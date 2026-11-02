//! Retention: which of a job's sessions have outlived its policy. Sessions expire in whole
//! chains, a full session together with the incremental and differential ones that build on
//! it, so no remaining session depends on an expired one; the newest chain never expires.
//! Append-only sessions stand alone, each its own chain.

use ltoarc_catalog::SessionRow;

#[derive(Debug, Clone, Copy, Default)]
pub struct Policy {
    /// Keep this many of the newest chains.
    pub keep_fulls: Option<usize>,
    /// Keep chains with a session started in the last this many seconds.
    pub keep_secs: Option<i64>,
}

/// A job's sessions (oldest first) grouped into chains.
pub fn chains(sessions: &[SessionRow]) -> Vec<Vec<&SessionRow>> {
    let mut out: Vec<Vec<&SessionRow>> = Vec::new();
    for s in sessions {
        match out.last_mut() {
            Some(c) if s.level != "full" && s.level != "append" && c[0].level != "append" => c.push(s),
            _ => out.push(vec![s]),
        }
    }
    out
}

/// Sessions the policy expires (ids), given the job's live sessions oldest first. A chain
/// expires when every rule given says so; with no rule, nothing expires.
pub fn expired(sessions: &[SessionRow], policy: Policy, now: i64) -> Vec<i64> {
    if policy.keep_fulls.is_none() && policy.keep_secs.is_none() {
        return Vec::new();
    }
    let chains = chains(sessions);
    let n = chains.len();
    let mut out = Vec::new();
    for (i, chain) in chains.iter().enumerate() {
        let newest = chain.iter().map(|s| s.started).max().unwrap_or(0);
        let by_count = policy.keep_fulls.is_none_or(|k| i + k < n);
        let by_age = policy.keep_secs.is_none_or(|k| newest < now - k);
        if i + 1 < n && by_count && by_age {
            out.extend(chain.iter().map(|s| s.id));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(id: i64, level: &str, started: i64) -> SessionRow {
        SessionRow { id, level: level.into(), started, ..Default::default() }
    }

    #[test]
    fn whole_chains_expire() {
        let day = 86400;
        let all = vec![
            s(1, "full", 0),
            s(2, "incremental", day),
            s(3, "differential", 2 * day),
            s(4, "full", 10 * day),
            s(5, "incremental", 11 * day),
            s(6, "full", 20 * day),
        ];
        assert_eq!(chains(&all).len(), 3);
        assert_eq!(expired(&all, Policy { keep_fulls: Some(2), keep_secs: None }, 21 * day), [1, 2, 3]);
        assert_eq!(expired(&all, Policy { keep_fulls: Some(1), keep_secs: None }, 21 * day), [1, 2, 3, 4, 5]);
        // The second chain's newest session is 10 days old: kept by a 15-day rule.
        assert_eq!(expired(&all, Policy { keep_fulls: None, keep_secs: Some(15 * day) }, 21 * day), [1, 2, 3]);
        assert_eq!(expired(&all, Policy { keep_fulls: Some(1), keep_secs: Some(15 * day) }, 21 * day), [1, 2, 3]);
        // The newest chain stays whatever the rules.
        assert_eq!(expired(&all, Policy { keep_fulls: Some(0), keep_secs: Some(0) }, 21 * day), [1, 2, 3, 4, 5]);
        assert!(expired(&all, Policy::default(), 21 * day).is_empty());
        let app = vec![s(1, "append", 0), s(2, "append", day), s(3, "append", 2 * day)];
        assert_eq!(expired(&app, Policy { keep_fulls: None, keep_secs: Some(day) }, 3 * day), [1, 2]);
    }
}
