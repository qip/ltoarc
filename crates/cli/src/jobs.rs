//! Jobs (`[job.NAME]` in the jobs file) and running one: hooks, the session or maintenance
//! step, retention, a catalog copy, the run's record and notifications.
//!
//! The steps run as child ltoarc processes with the same global options, so a step that
//! fails or crashes still lets the post hooks, the run record and the notification happen.
//! Hooks and commands may use {job}, {run} (this run's id, a timestamp such as
//! 20261031T013000), {prev_run} (the last successful run's id) and {date}.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail, Context as _, Result};
use clap::Subcommand;
use serde::{Deserialize, Serialize};

use crate::ctx::Ctx;
use crate::notify::{self, NotifyCfg};

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JobsFile {
    #[serde(default)]
    pub daemon: DaemonCfg,
    #[serde(default)]
    pub notify: NotifyCfg,
    #[serde(default)]
    pub job: BTreeMap<String, Job>,
}

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DaemonCfg {
    /// Control socket (default $XDG_RUNTIME_DIR/ltoarc/ltoarcd.sock, or /run/ltoarc).
    pub socket: Option<PathBuf>,
    /// Run logs (default ~/.local/state/ltoarc).
    pub state: Option<PathBuf>,
    /// Jobs running at once (default 2; drives are shared through their leases anyway).
    pub parallel: Option<usize>,
    /// After every backup or copy job, a consistent copy of the whole catalog here.
    pub catalog_copy: Option<PathBuf>,
    /// Serve the web interface on this address (e.g. "127.0.0.1:8470"); `ltoarc daemon url`
    /// prints the link to log in with.
    pub web: Option<String>,
}

fn backup_kind() -> String {
    "backup".into()
}

#[derive(Debug, Default, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Job {
    /// backup (default), scrub, check, copy.
    #[serde(default = "backup_kind")]
    pub kind: String,
    /// Cron schedule; without one the job runs only when asked (`ltoarc daemon run NAME`).
    pub schedule: Option<String>,
    // backup
    #[serde(default)]
    pub sources: Vec<String>,
    pub tape: Option<String>,
    pub tape2: Option<String>,
    pub level: Option<String>,
    /// For incremental and differential jobs: run a full session when the last one is
    /// older than this (e.g. 30d).
    pub full_every: Option<String>,
    #[serde(default)]
    pub map: Vec<String>,
    /// Stream sources: name -> command; archived as /.ltoarc/streams/<job>/<name>.
    #[serde(default)]
    pub streams: BTreeMap<String, String>,
    pub settle: Option<String>,
    /// Retention after each successful run (`ltoarc expire`).
    pub keep_fulls: Option<usize>,
    pub keep: Option<String>,
    /// Append-only jobs: delete source files with this many verified copies.
    pub prune_copies: Option<usize>,
    /// Backup and copy jobs: start each cartridge afresh instead of appending (rotation
    /// schemes); what the cartridges held is overwritten without asking.
    #[serde(default)]
    pub overwrite: bool,
    /// Backup jobs: delete the source files once the session is complete (moving them to
    /// tape; see `backup --delete-source`). Not with `map` (snapshots).
    #[serde(default)]
    pub delete_source: bool,
    // scrub
    /// Cartridges to scrub (default: all holding sessions).
    pub tapes: Option<String>,
    /// Cartridges verified per run (default 1), those verified longest ago first.
    pub per_run: Option<usize>,
    /// Only cartridges not verified for this long (default 90d).
    pub older_than: Option<String>,
    // check
    #[serde(default)]
    pub retire: bool,
    #[serde(default)]
    pub clean: bool,
    // copy
    pub from: Option<String>,
    pub to: Option<String>,
    #[serde(default)]
    pub from_source: bool,
    // all kinds
    #[serde(default)]
    pub pre: Vec<String>,
    /// Always run, with LTOARC_STATUS=ok or failed.
    #[serde(default)]
    pub post: Vec<String>,
    /// More options for the step's command (e.g. ["--hash", "sha256", "--write-back"]).
    #[serde(default)]
    pub args: Vec<String>,
}

pub fn default_path() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::ctx::home().join(".config"))
        .join("ltoarc/jobs.toml")
}

pub fn load(path: Option<&std::path::Path>) -> Result<JobsFile> {
    let p = path.map(PathBuf::from).unwrap_or_else(default_path);
    // No jobs file at the default place means no jobs (the scheduler may serve only the web
    // interface); one named with --jobs has to exist.
    let text = match std::fs::read_to_string(&p) {
        Ok(t) => t,
        Err(e) if path.is_none() && e.kind() == std::io::ErrorKind::NotFound => return Ok(JobsFile::default()),
        Err(e) => return Err(e).with_context(|| format!("reading jobs file {}", p.display())),
    };
    let jobs: JobsFile = toml::from_str(&text).with_context(|| format!("parsing {}", p.display()))?;
    for (name, j) in &jobs.job {
        if !["backup", "scrub", "check", "copy"].contains(&j.kind.as_str()) {
            bail!("job {name}: unknown kind '{}' (backup, scrub, check, copy)", j.kind);
        }
        if let Some(s) = &j.schedule {
            crate::cron::Schedule::parse(s).with_context(|| format!("job {name}: schedule"))?;
        }
        if j.kind == "backup" && j.tape.is_none() {
            bail!("job {name}: a backup job needs tape = \"...\"");
        }
    }
    Ok(jobs)
}

#[derive(Subcommand)]
pub enum JobCmd {
    /// List jobs with their schedules, last and next runs.
    List,
    /// Run a job now, in this process (hooks, step, retention, notifications).
    Run { name: String },
    /// Show a job's recent runs.
    Runs {
        name: String,
        /// Show this many runs, newest first.
        #[arg(long, default_value_t = 10)]
        last: usize,
    },
}

pub fn run_cmd(ctx: &Ctx, jobs_path: Option<&std::path::Path>, cmd: JobCmd) -> Result<()> {
    let jobs = load(jobs_path)?;
    match cmd {
        JobCmd::List => {
            let mut t = crate::table::Table::new(&["JOB", "KIND", "SCHEDULE", "LAST RUN", "STATUS", "NEXT"]);
            for (name, j) in &jobs.job {
                let last = ctx.cat.job_runs(name, 1)?.into_iter().next();
                let next = j
                    .schedule
                    .as_deref()
                    .and_then(|s| crate::cron::Schedule::parse(s).ok())
                    .and_then(|s| s.next_after(chrono::Local::now()))
                    .map_or("-".into(), |t| t.format("%Y-%m-%d %H:%M").to_string());
                t.row(vec![
                    name.clone(),
                    j.kind.clone(),
                    j.schedule.clone().unwrap_or_else(|| "on request".into()),
                    last.as_ref().map_or("-".into(), |r| crate::table::when(r.started)),
                    last.as_ref().map_or("-".into(), |r| r.status.clone()),
                    next,
                ]);
            }
            t.print();
            Ok(())
        }
        JobCmd::Run { name } => {
            let ok = run_job(ctx, &jobs, &name)?;
            if !ok {
                bail!("job {name} failed");
            }
            Ok(())
        }
        JobCmd::Runs { name, last } => {
            let mut t = crate::table::Table::new(&["RUN", "STARTED", "ENDED", "STATUS", "SUMMARY"]);
            for r in ctx.cat.job_runs(&name, last)? {
                t.row(vec![
                    r.run.clone(),
                    crate::table::when(r.started),
                    r.ended.map_or("-".into(), crate::table::when),
                    r.status.clone(),
                    r.summary.clone().unwrap_or_default(),
                ]);
            }
            t.print();
            Ok(())
        }
    }
}

fn expand(s: &str, vars: &[(&str, String)]) -> String {
    let mut out = s.to_string();
    for (k, v) in vars {
        out = out.replace(&format!("{{{k}}}"), v);
    }
    out
}

/// Run a hook with the run's variables in its environment.
fn hook(cmd: &str, vars: &[(&str, String)], status: Option<&str>) -> Result<()> {
    let line = expand(cmd, vars);
    println!("hook      {line}");
    let mut c = Command::new("/bin/sh");
    c.arg("-c").arg(&line);
    for (k, v) in vars {
        c.env(format!("LTOARC_{}", k.to_ascii_uppercase()), v);
    }
    if let Some(s) = status {
        c.env("LTOARC_STATUS", s);
    }
    let st = c.status().with_context(|| format!("running hook `{line}`"))?;
    if !st.success() {
        bail!("hook `{line}` failed ({st})");
    }
    Ok(())
}

/// Run `ltoarc ARGS` (with this process's global options), echoing its output and calling
/// `watch` with each line. True when it succeeded.
fn step(ctx: &Ctx, args: &[String], watch: &mut dyn FnMut(&str)) -> Result<bool> {
    println!("step      ltoarc {}", args.join(" "));
    let exe = std::env::current_exe()?;
    let exe = if exe.file_name().is_some_and(|n| n != "ltoarc") { exe.with_file_name("ltoarc") } else { exe };
    let mut child = Command::new(&exe)
        .args(&ctx.child_args)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("starting {}", exe.display()))?;
    let (tx, rx) = std::sync::mpsc::channel::<String>();
    let mut readers = Vec::new();
    for pipe in [child.stdout.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>), child.stderr.take().map(|p| Box::new(p) as Box<dyn std::io::Read + Send>)]
        .into_iter()
        .flatten()
    {
        let tx = tx.clone();
        readers.push(std::thread::spawn(move || {
            for line in BufReader::new(pipe).lines().map_while(Result::ok) {
                if tx.send(line).is_err() {
                    break;
                }
            }
        }));
    }
    drop(tx);
    for line in rx {
        println!("  {line}");
        watch(&line);
    }
    for r in readers {
        let _ = r.join();
    }
    Ok(child.wait()?.success())
}

/// Cartridges a scrub verifies this run: those verified longest ago (never first).
fn scrub_targets(ctx: &Ctx, job: &Job) -> Result<Vec<String>> {
    let pool: Vec<ltoarc_catalog::VolumeRow> = match &job.tapes {
        Some(t) => crate::tape::tape_set(ctx, t)?,
        None => ctx.cat.volumes()?,
    };
    let older = job.older_than.as_deref().map(crate::archive::parse_duration).transpose()?.unwrap_or(std::time::Duration::from_secs(90 * 86400));
    let cutoff = ltoarc_catalog::now() - older.as_secs() as i64;
    let mut due: Vec<(i64, String)> = Vec::new();
    for v in pool.into_iter().filter(|v| v.kind == "data" && v.state != "expired") {
        let parts = ctx.cat.volume_sessions(v.id)?;
        if parts.is_empty() {
            continue;
        }
        // The cartridge's last full verification: its least recently verified part.
        let last = parts.iter().map(|p| p.verified_at.unwrap_or(0)).min().unwrap_or(0);
        if last < cutoff {
            due.push((last, v.barcode));
        }
    }
    due.sort();
    Ok(due.into_iter().take(job.per_run.unwrap_or(1)).map(|(_, b)| b).collect())
}

/// Run job `name`. Returns whether it succeeded (errors of the job itself are reported,
/// recorded and notified; Err only when it could not be run at all).
pub fn run_job(ctx: &Ctx, jobs: &JobsFile, name: &str) -> Result<bool> {
    let job = jobs.job.get(name).ok_or_else(|| anyhow!("no job {name} in the jobs file"))?;
    let run = chrono::Local::now().format("%Y%m%dT%H%M%S").to_string();
    let prev = ctx.cat.job_runs(name, 100)?.into_iter().find(|r| r.status == "ok").map(|r| r.run).unwrap_or_default();
    let vars: Vec<(&str, String)> =
        vec![("job", name.to_string()), ("run", run.clone()), ("prev_run", prev), ("date", chrono::Local::now().format("%Y-%m-%d").to_string())];
    let log = std::env::var("LTOARC_LOG").ok();
    let rid = ctx.cat.start_job_run(name, &run, log.as_deref())?;
    println!("job       {name} run {run} ({})", job.kind);
    let notify_cfg = jobs.notify.clone();
    let mut requests = |line: &str| {
        if let Some(r) = line.strip_prefix("OPERATOR: ") {
            notify::send(&notify_cfg, "request", name, &format!("ltoarc {name}: cartridge needed"), r);
        }
    };
    let mut wear_lines: Vec<String> = Vec::new();
    let result = (|| -> Result<String> {
        for h in &job.pre {
            hook(h, &vars, None)?;
        }
        match job.kind.as_str() {
            "backup" => {
                let mut level = job.level.clone().unwrap_or_else(|| "full".into());
                if let (Some(every), "incremental" | "differential") = (&job.full_every, level.as_str()) {
                    let every = crate::archive::parse_duration(every)?.as_secs() as i64;
                    let last_full = ctx.cat.job_sessions(name)?.into_iter().filter(|s| s.level == "full").map(|s| s.started).max();
                    if last_full.is_none_or(|t| t < ltoarc_catalog::now() - every) {
                        println!("level     full (the last full session is older than {})", job.full_every.as_deref().unwrap_or(""));
                        level = "full".into();
                    }
                }
                let mut args: Vec<String> = vec!["backup".into()];
                args.extend(job.sources.iter().map(|s| expand(s, &vars)));
                args.extend(["--tape".into(), job.tape.clone().unwrap_or_default(), "--job".into(), name.into(), "--level".into(), level]);
                if let Some(t2) = &job.tape2 {
                    args.extend(["--tape2".into(), t2.clone()]);
                }
                for m in &job.map {
                    args.extend(["--map".into(), expand(m, &vars)]);
                }
                for (sname, cmd) in &job.streams {
                    args.extend(["--stream".into(), format!("/.ltoarc/streams/{name}/{sname}={}", expand(cmd, &vars))]);
                }
                if let Some(s) = &job.settle {
                    args.extend(["--settle".into(), s.clone()]);
                }
                if job.overwrite {
                    args.extend(["--overwrite".into(), "--yes".into()]);
                }
                if job.delete_source {
                    args.push("--delete-source".into());
                }
                args.extend(job.args.iter().map(|a| expand(a, &vars)));
                let mut summary = String::new();
                let ok = step(ctx, &args, &mut |l| {
                    requests(l);
                    if l.starts_with("session ") || l.starts_with("data ") {
                        summary = format!("{summary}{}{}", if summary.is_empty() { "" } else { "; " }, l.split_whitespace().collect::<Vec<_>>().join(" "));
                    }
                })?;
                if !ok {
                    bail!("the backup failed");
                }
                if job.keep_fulls.is_some() || job.keep.is_some() {
                    let mut a: Vec<String> = vec!["expire".into(), "--job".into(), name.into(), "--yes".into()];
                    if let Some(k) = job.keep_fulls {
                        a.extend(["--keep-fulls".into(), k.to_string()]);
                    }
                    if let Some(k) = &job.keep {
                        a.extend(["--keep".into(), k.clone()]);
                    }
                    if !step(ctx, &a, &mut |_| {})? {
                        bail!("retention failed");
                    }
                }
                if let Some(n) = job.prune_copies {
                    if !step(ctx, &["prune".into(), "--job".into(), name.into(), "--copies".into(), n.to_string(), "--yes".into()], &mut |_| {})? {
                        bail!("pruning the sources failed");
                    }
                }
                Ok(summary)
            }
            "scrub" => {
                let targets = scrub_targets(ctx, job)?;
                if targets.is_empty() {
                    return Ok("nothing due".into());
                }
                let mut bad = Vec::new();
                for bc in &targets {
                    let mut a: Vec<String> = vec!["verify".into(), bc.clone()];
                    a.extend(job.args.iter().cloned());
                    if !step(ctx, &a, &mut requests)? {
                        bad.push(bc.clone());
                    }
                }
                if !bad.is_empty() {
                    bail!("verify failed on {}", bad.join(", "));
                }
                Ok(format!("verified {}", targets.join(", ")))
            }
            "check" => {
                let mut a: Vec<String> = vec!["stats".into(), "check".into()];
                if job.retire {
                    a.push("--retire".into());
                }
                if job.clean {
                    a.push("--clean".into());
                }
                let ok = step(ctx, &a, &mut |l| {
                    if l.contains("NEEDS CLEANING") || l.contains("RETIRE") || l.contains("suspect") {
                        wear_lines.push(l.to_string());
                    }
                })?;
                if !ok {
                    bail!("the wear check failed");
                }
                Ok(if wear_lines.is_empty() { "all within limits".into() } else { format!("{} finding(s)", wear_lines.len()) })
            }
            "copy" => {
                let mut a: Vec<String> = vec!["copy".into(), "--to".into(), job.to.clone().ok_or_else(|| anyhow!("a copy job needs to = \"...\""))?];
                if let Some(f) = &job.from {
                    a.extend(["--from".into(), f.clone()]);
                }
                if job.from_source {
                    a.push("--from-source".into());
                }
                if job.overwrite {
                    a.extend(["--overwrite".into(), "--yes".into()]);
                }
                a.extend(job.args.iter().cloned());
                if !step(ctx, &a, &mut requests)? {
                    bail!("the copy failed");
                }
                Ok("copied".into())
            }
            other => bail!("unknown kind {other}"),
        }
    })();
    let status = if result.is_ok() { "ok" } else { "failed" };
    // Post hooks run whatever happened.
    let mut post_err = None;
    for h in &job.post {
        if let Err(e) = hook(h, &vars, Some(status)) {
            post_err.get_or_insert(e);
        }
    }
    if let (Some(dest), true, "backup" | "copy") = (&jobs.daemon.catalog_copy, result.is_ok(), job.kind.as_str()) {
        match ctx.cat.copy_to(dest) {
            Ok(()) => println!("catalog   copied to {}", dest.display()),
            Err(e) => eprintln!("warning: catalog copy to {}: {e:#}", dest.display()),
        }
    }
    let (status, summary) = match (&result, &post_err) {
        (Ok(s), None) => ("ok", s.clone()),
        (Ok(_), Some(e)) => ("failed", format!("{e:#}")),
        (Err(e), _) => ("failed", format!("{e:#}")),
    };
    ctx.cat.end_job_run(rid, status, &summary)?;
    println!("job       {name} {status}: {summary}");
    if !wear_lines.is_empty() {
        notify::send(&jobs.notify, "wear", name, &format!("ltoarc {name}: wear check"), &wear_lines.join("\n"));
    }
    let event = if status == "ok" { "done" } else { "failed" };
    notify::send(&jobs.notify, event, name, &format!("ltoarc {name}: {status}"), &summary);
    Ok(status == "ok")
}
