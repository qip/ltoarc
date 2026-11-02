//! Cron schedules ("min hour day-of-month month day-of-week", in local time) for jobs.
//!
//! Fields take `*`, numbers, ranges `a-b`, steps `*/n` or `a-b/n`, and lists `a,b,c`; day of
//! week is 0-7 (0 and 7 are Sunday). As in cron, when both day fields are restricted a day
//! matches either. `@hourly`, `@daily`, `@weekly`, `@monthly` and `@yearly` are shorthands.

use anyhow::{anyhow, bail, Result};
use chrono::{DateTime, Datelike, Duration, Local, TimeZone, Timelike};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Schedule {
    minutes: Vec<bool>,
    hours: Vec<bool>,
    days: Vec<bool>,
    months: Vec<bool>,
    weekdays: Vec<bool>,
    days_restricted: bool,
    weekdays_restricted: bool,
}

fn field(spec: &str, lo: u32, hi: u32) -> Result<(Vec<bool>, bool)> {
    let mut set = vec![false; hi as usize + 1];
    for part in spec.split(',') {
        let (range, step) = match part.split_once('/') {
            Some((r, s)) => (r, s.parse::<u32>().map_err(|_| anyhow!("bad step in '{part}'"))?),
            None => (part, 1),
        };
        if step == 0 {
            bail!("step 0 in '{part}'");
        }
        let (a, b) = if range == "*" {
            (lo, hi)
        } else if let Some((a, b)) = range.split_once('-') {
            (a.parse().map_err(|_| anyhow!("bad range '{range}'"))?, b.parse().map_err(|_| anyhow!("bad range '{range}'"))?)
        } else {
            let v: u32 = range.parse().map_err(|_| anyhow!("bad value '{range}'"))?;
            // "5/15": from 5 on, every 15.
            (v, if part.contains('/') { hi } else { v })
        };
        if a < lo || b > hi || a > b {
            bail!("'{part}' is outside {lo}-{hi}");
        }
        let mut v = a;
        while v <= b {
            set[v as usize] = true;
            v += step;
        }
    }
    Ok((set, spec != "*"))
}

impl Schedule {
    pub fn parse(spec: &str) -> Result<Schedule> {
        let spec = match spec.trim() {
            "@hourly" => "0 * * * *",
            "@daily" | "@midnight" => "0 0 * * *",
            "@weekly" => "0 0 * * 0",
            "@monthly" => "0 0 1 * *",
            "@yearly" | "@annually" => "0 0 1 1 *",
            s => s,
        };
        let f: Vec<&str> = spec.split_whitespace().collect();
        let [mi, h, dom, mo, dow] = f.as_slice() else { bail!("'{spec}': a schedule has 5 fields (minute hour day-of-month month day-of-week)") };
        let (minutes, _) = field(mi, 0, 59)?;
        let (hours, _) = field(h, 0, 23)?;
        let (days, days_restricted) = field(dom, 1, 31)?;
        let (months, _) = field(mo, 1, 12)?;
        let (mut weekdays, weekdays_restricted) = field(dow, 0, 7)?;
        if weekdays[7] {
            weekdays[0] = true;
        }
        Ok(Schedule { minutes, hours, days, months, weekdays, days_restricted, weekdays_restricted })
    }

    fn day_matches(&self, t: &DateTime<Local>) -> bool {
        let dom = self.days[t.day() as usize];
        let dow = self.weekdays[t.weekday().num_days_from_sunday() as usize];
        match (self.days_restricted, self.weekdays_restricted) {
            (true, true) => dom || dow,
            (true, false) => dom,
            (false, true) => dow,
            (false, false) => true,
        }
    }

    /// The first time after `after` (to the minute) the schedule fires.
    pub fn next_after(&self, after: DateTime<Local>) -> Option<DateTime<Local>> {
        let start = after.with_second(0)?.with_nanosecond(0)? + Duration::minutes(1);
        let mut t = start;
        // Within five years, or never (e.g. 31 February).
        let limit = start + Duration::days(5 * 366);
        while t < limit {
            if !self.months[t.month() as usize] || !self.day_matches(&t) {
                // To the next day's midnight.
                let next = (t.date_naive() + Duration::days(1)).and_hms_opt(0, 0, 0)?;
                t = Local.from_local_datetime(&next).earliest().unwrap_or(t + Duration::hours(1));
                continue;
            }
            if !self.hours[t.hour() as usize] {
                t = t.with_minute(0)? + Duration::hours(1);
                continue;
            }
            if self.minutes[t.minute() as usize] {
                return Some(t);
            }
            t += Duration::minutes(1);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> DateTime<Local> {
        Local.from_local_datetime(&chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M").unwrap()).unwrap()
    }

    fn next(spec: &str, after: &str) -> String {
        Schedule::parse(spec).unwrap().next_after(at(after)).unwrap().format("%Y-%m-%d %H:%M %a").to_string()
    }

    #[test]
    fn schedules() {
        assert_eq!(next("30 1 * * *", "2026-10-31 20:00"), "2026-11-01 01:30 Sun");
        assert_eq!(next("30 1 * * *", "2026-10-31 01:00"), "2026-10-31 01:30 Sat");
        assert_eq!(next("*/15 * * * *", "2026-10-31 20:07"), "2026-10-31 20:15 Sat");
        assert_eq!(next("0 9-17/4 * * 1-5", "2026-10-31 12:00"), "2026-11-02 09:00 Mon");
        assert_eq!(next("0 0 * * 7", "2026-10-31 12:00"), "2026-11-01 00:00 Sun");
        assert_eq!(next("0 3 1,15 * *", "2026-10-02 00:00"), "2026-10-15 03:00 Thu");
        // Both day fields restricted: either matches.
        assert_eq!(next("0 0 13 * 5", "2026-10-31 00:00"), "2026-11-06 00:00 Fri");
        assert_eq!(next("@monthly", "2026-10-31 12:00"), "2026-11-01 00:00 Sun");
        assert_eq!(next("5/20 * * * *", "2026-10-31 12:30"), "2026-10-31 12:45 Sat");
        assert!(Schedule::parse("0 0 31 2 *").unwrap().next_after(at("2026-01-01 00:00")).is_none());
        assert!(Schedule::parse("61 * * * *").is_err());
        assert!(Schedule::parse("* * *").is_err());
    }
}
