//! ltoarcd: runs the jobs of the jobs file on their schedules.
//!
//! Each run is a child `ltoarc job run NAME` (hooks, step, retention, notifications happen
//! there) with its output in a log under the state directory; one instance of a job runs at
//! a time and at most `parallel` jobs at once (drives are shared through their leases, so a
//! job that needs a busy drive waits its turn by priority). A Unix socket answers `status`,
//! `run NAME`, `log NAME [LINES]`, `reload` and `stop`; `ltoarc daemon ...` talks to it. On
//! SIGTERM or `stop` it lets running jobs finish, then exits. With `web` set it also serves
//! the web interface (crate::web), whose tasks (restores, verifies, ...) it runs and
//! records alongside the job runs.

use std::collections::{HashMap, VecDeque};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, bail, Context as _, Result};
use chrono::{DateTime, Local};
use clap::Subcommand;

use crate::ctx::Ctx;
use crate::jobs::{self, JobsFile};

#[derive(Subcommand)]
pub enum DaemonCmd {
    /// Run the scheduler in the foreground (what `ltoarcd` does).
    Run {
        /// Also serve the web interface on this address (overrides `web` in the jobs file).
        #[arg(long)]
        web: Option<String>,
    },
    /// What runs, what is queued, last and next runs.
    Status,
    /// Run a job now (queued if it is running).
    Trigger { name: String },
    /// The latest log of a job.
    Log {
        name: String,
        /// Show this many lines from its end.
        #[arg(long, default_value_t = 40)]
        lines: usize,
    },
    /// Re-read the jobs file.
    Reload,
    /// Stop after the running jobs finish.
    Stop,
    /// The link that logs in to the web interface (it carries the access token; anyone who
    /// can read the daemon's state directory can get it).
    Url,
}

static TERMINATE: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    TERMINATE.store(true, Ordering::SeqCst);
}

pub fn socket_path(jobs: &JobsFile) -> PathBuf {
    if let Some(s) = &jobs.daemon.socket {
        return s.clone();
    }
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(d) => PathBuf::from(d).join("ltoarc/ltoarcd.sock"),
        None => PathBuf::from("/run/ltoarc/ltoarcd.sock"),
    }
}

pub fn state_dir(jobs: &JobsFile) -> PathBuf {
    if let Some(s) = &jobs.daemon.state {
        return s.clone();
    }
    std::env::var_os("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| crate::ctx::home().join(".local/state"))
        .join("ltoarc")
}

pub(crate) struct Running {
    pub child: Child,
    pub since: DateTime<Local>,
    pub log: PathBuf,
}

/// Something run on request from the web interface: a child `ltoarc` command, recorded as a
/// run of the pseudo-job `@KIND`.
pub(crate) struct Task {
    /// Its job_runs row.
    pub run_id: i64,
    pub kind: String,
    pub title: String,
    pub child: Child,
    pub since: DateTime<Local>,
    pub log: PathBuf,
}

#[derive(Default)]
pub(crate) struct Shared {
    pub jobs: JobsFile,
    pub queue: VecDeque<String>,
    pub running: HashMap<String, Running>,
    pub tasks: Vec<Task>,
    pub next: HashMap<String, DateTime<Local>>,
    pub stopping: bool,
    pub reload: bool,
    pub parallel: usize,
    /// The address the web interface listens on, once started.
    pub web: Option<String>,
}

fn schedule_all(sh: &mut Shared) {
    let now = Local::now();
    sh.next.clear();
    for (name, j) in &sh.jobs.job {
        if let Some(t) = j.schedule.as_deref().and_then(|s| crate::cron::Schedule::parse(s).ok()).and_then(|s| s.next_after(now)) {
            sh.next.insert(name.clone(), t);
        }
    }
}

fn latest_log(dir: &Path) -> Option<PathBuf> {
    let mut logs: Vec<PathBuf> = std::fs::read_dir(dir).ok()?.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "log")).collect();
    logs.sort();
    logs.pop()
}

/// Start `ltoarc ARGS` with its output in `log`, in a process group of its own (so it can be
/// stopped with everything it started, and a Ctrl-C meant for the daemon does not reach it).
pub(crate) fn spawn_ltoarc(exe: &Path, child_args: &[OsString], args: &[OsString], log: &Path, env: &[(&str, &OsString)]) -> Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let out = std::fs::File::create(log).with_context(|| format!("creating {}", log.display()))?;
    let err = out.try_clone()?;
    let mut cmd = Command::new(exe);
    cmd.args(child_args).args(args).env("LTOARC_LOG", log).stdin(Stdio::null()).stdout(out).stderr(err).process_group(0);
    for (k, v) in env {
        cmd.env(k, v);
    }
    Ok(cmd.spawn()?)
}

/// Ask a child started by `spawn_ltoarc` and everything it started to stop.
pub(crate) fn terminate(child: &Child) {
    // SAFETY: a signal to the child's own process group.
    unsafe {
        libc::kill(-(child.id() as i32), libc::SIGTERM);
    }
}

/// The line of a log that says how it ended: an error, else the result line of a backup or
/// restore, else the last line that is not a drive report.
fn summary_of(log: &Path) -> String {
    let text = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = text.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
    let last = |f: &dyn Fn(&str) -> bool| lines.iter().rev().find(|l| f(l)).copied();
    last(&|l| l.starts_with("error"))
        .or_else(|| last(&|l| l.starts_with("session ") || l.starts_with("restored ")))
        .or_else(|| last(&|l| !["drive ", "dest ", "source "].iter().any(|p| l.starts_with(p))))
        .or_else(|| lines.last().copied())
        .map(|l| l.chars().take(200).collect())
        .unwrap_or_default()
}

fn answer(line: &str, sh: &Mutex<Shared>, cat: &ltoarc_catalog::Catalog, jobs_path: Option<&Path>, state: &Path) -> String {
    let mut words = line.split_whitespace();
    match words.next() {
        Some("status") => {
            let sh = sh.lock().unwrap();
            let mut t = crate::table::Table::new(&["JOB", "KIND", "STATE", "LAST RUN", "RESULT", "NEXT"]);
            for (name, j) in &sh.jobs.job {
                let state = match sh.running.get(name) {
                    Some(r) => format!("running since {}", r.since.format("%H:%M")),
                    None if sh.queue.contains(name) => "queued".into(),
                    None => "idle".into(),
                };
                let last = cat.job_runs(name, 1).ok().and_then(|r| r.into_iter().next());
                t.row(vec![
                    name.clone(),
                    j.kind.clone(),
                    state,
                    last.as_ref().map_or("-".into(), |r| crate::table::when(r.started)),
                    last.as_ref().map_or("-".into(), |r| format!("{}{}", r.status, r.summary.as_deref().map(|s| format!(": {s}")).unwrap_or_default())),
                    sh.next.get(name).map_or("on request".into(), |t| t.format("%Y-%m-%d %H:%M").to_string()),
                ]);
            }
            let mut out = t.render();
            for task in &sh.tasks {
                out += &format!("task: {} (since {})\n", task.title, task.since.format("%H:%M"));
            }
            if sh.stopping {
                out += "stopping once the running jobs finish\n";
            }
            out
        }
        Some("run") => {
            let Some(name) = words.next() else { return "usage: run NAME\n".into() };
            match queue(&mut sh.lock().unwrap(), name) {
                Ok(()) => format!("queued {name}\n"),
                Err(e) => format!("{e}\n"),
            }
        }
        Some("log") => {
            let Some(name) = words.next() else { return "usage: log NAME [LINES]\n".into() };
            let n: usize = words.next().and_then(|w| w.parse().ok()).unwrap_or(40);
            match latest_log(&state.join("logs").join(name)) {
                Some(p) => {
                    let text = std::fs::read_to_string(&p).unwrap_or_default();
                    let lines: Vec<&str> = text.lines().collect();
                    format!("{}\n{}\n", p.display(), lines[lines.len().saturating_sub(n)..].join("\n"))
                }
                None => format!("no log for {name}\n"),
            }
        }
        Some("reload") => match jobs::load(jobs_path) {
            Ok(j) => {
                let mut sh = sh.lock().unwrap();
                sh.jobs = j;
                sh.reload = true;
                format!("reloaded {} job(s)\n", sh.jobs.job.len())
            }
            Err(e) => format!("jobs file not reloaded: {e:#}\n"),
        },
        Some("stop") => {
            let mut sh = sh.lock().unwrap();
            sh.stopping = true;
            format!("stopping after {} running job(s)\n", sh.running.len() + sh.tasks.len())
        }
        Some("web") => format!("{}\n", sh.lock().unwrap().web.as_deref().unwrap_or("none")),
        _ => "commands: status, run NAME, log NAME [LINES], reload, stop, web\n".into(),
    }
}

/// Queue a run of job `name`.
pub(crate) fn queue(sh: &mut Shared, name: &str) -> Result<()> {
    if !sh.jobs.job.contains_key(name) {
        bail!("no job {name}");
    }
    if sh.stopping {
        bail!("the scheduler is stopping");
    }
    if sh.queue.iter().any(|q| q == name) {
        bail!("{name} is already queued");
    }
    sh.queue.push_back(name.to_string());
    Ok(())
}

/// The web interface's access token (created on first use, readable by the daemon's user only).
pub(crate) fn web_token(state: &Path) -> Result<String> {
    let p = state.join("web.token");
    if let Ok(t) = std::fs::read_to_string(&p) {
        if t.trim().len() >= 32 {
            return Ok(t.trim().to_string());
        }
    }
    std::fs::create_dir_all(state)?;
    let t = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
    let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(&p).with_context(|| format!("creating {}", p.display()))?;
    f.write_all(t.as_bytes())?;
    Ok(t)
}

/// The scheduler, in the foreground.
pub fn serve(ctx: &Ctx, jobs_path: Option<&Path>, web: Option<String>) -> Result<()> {
    let jobs = jobs::load(jobs_path)?;
    let sock = socket_path(&jobs);
    let state = state_dir(&jobs);
    let parallel = jobs.daemon.parallel.unwrap_or(2).max(1);
    let web = web.or_else(|| jobs.daemon.web.clone());
    std::fs::create_dir_all(state.join("logs"))?;
    if let Some(dir) = sock.parent() {
        std::fs::create_dir_all(dir)?;
    }
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            bail!("an ltoarcd already listens on {}", sock.display());
        }
        std::fs::remove_file(&sock)?;
    }
    let listener = UnixListener::bind(&sock).with_context(|| format!("listening on {}", sock.display()))?;
    let _ = std::fs::set_permissions(&sock, std::os::unix::fs::PermissionsExt::from_mode(0o660));
    // SAFETY: the handler only stores to an atomic.
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as extern "C" fn(libc::c_int) as libc::sighandler_t);
    }
    let shared = Arc::new(Mutex::new(Shared { jobs, parallel, ..Default::default() }));
    schedule_all(&mut shared.lock().unwrap());
    println!("ltoarcd: {} job(s), socket {}, logs in {}", shared.lock().unwrap().jobs.job.len(), sock.display(), state.join("logs").display());
    {
        let (shared, catalog_path, jobs_path, state) = (shared.clone(), ctx.catalog_path.clone(), jobs_path.map(Path::to_path_buf), state.clone());
        std::thread::spawn(move || {
            let Ok(cat) = ltoarc_catalog::Catalog::open(&catalog_path) else { return };
            for conn in listener.incoming().flatten() {
                let mut line = String::new();
                let mut r = BufReader::new(&conn);
                if r.read_line(&mut line).is_err() {
                    continue;
                }
                let reply = answer(line.trim(), &shared, &cat, jobs_path.as_deref(), &state);
                let _ = (&conn).write_all(reply.as_bytes());
            }
        });
    }
    let exe = std::env::current_exe()?.with_file_name("ltoarc");
    if let Some(addr) = &web {
        let w = crate::web::Web {
            shared: shared.clone(),
            catalog: ctx.catalog_path.clone(),
            state: state.clone(),
            exe: exe.clone(),
            child_args: ctx.child_args.clone(),
            lock_dir: ctx.leases.real_dir().to_path_buf(),
            token: web_token(&state)?,
        };
        let bound = crate::web::start(addr, w)?;
        println!("ltoarcd: web interface on http://{bound}/ (`ltoarc daemon url` prints the login link)");
        shared.lock().unwrap().web = Some(bound);
    }
    loop {
        let mut sh = shared.lock().unwrap();
        if TERMINATE.load(Ordering::SeqCst) && !sh.stopping {
            sh.stopping = true;
            println!("ltoarcd: stopping once {} running job(s) finish", sh.running.len() + sh.tasks.len());
        }
        if std::mem::take(&mut sh.reload) {
            schedule_all(&mut sh);
            println!("ltoarcd: reloaded the jobs file");
        }
        // Finished runs.
        let done: Vec<String> = sh.running.iter_mut().filter_map(|(n, r)| matches!(r.child.try_wait(), Ok(Some(_))).then(|| n.clone())).collect();
        for name in done {
            let mut r = sh.running.remove(&name).expect("listed");
            let st = r.child.wait()?;
            println!("ltoarcd: {name} finished ({}); log {}", if st.success() { "ok".to_string() } else { st.to_string() }, r.log.display());
            // A run stopped or killed before it could record its end.
            let log = r.log.to_string_lossy();
            if let Some(run) = ctx.cat.job_runs(&name, 5)?.into_iter().find(|x| x.status == "running" && x.log.as_deref() == Some(&*log)) {
                let why = if st.signal().is_some() { format!("stopped ({st})") } else { format!("ended without a result ({st})") };
                ctx.cat.end_job_run(run.id, "failed", &why)?;
            }
        }
        // Finished tasks.
        let mut i = 0;
        while i < sh.tasks.len() {
            match sh.tasks[i].child.try_wait() {
                Ok(Some(st)) => {
                    let t = sh.tasks.remove(i);
                    let status = if st.success() { "ok" } else { "failed" };
                    if let Err(e) = ctx.cat.end_job_run(t.run_id, status, &summary_of(&t.log)) {
                        eprintln!("ltoarcd: recording the end of task {}: {e:#}", t.title);
                    }
                    println!("ltoarcd: task {} finished ({status}); log {}", t.title, t.log.display());
                }
                _ => i += 1,
            }
        }
        // Due runs.
        let now = Local::now();
        let due: Vec<String> = sh.next.iter().filter(|(_, t)| **t <= now).map(|(n, _)| n.clone()).collect();
        for name in due {
            if let Some(t) = sh.jobs.job.get(&name).and_then(|j| j.schedule.as_deref()).and_then(|s| crate::cron::Schedule::parse(s).ok()).and_then(|s| s.next_after(now)) {
                sh.next.insert(name.clone(), t);
            }
            if !sh.queue.contains(&name) && !sh.running.contains_key(&name) && !sh.stopping {
                sh.queue.push_back(name);
            }
        }
        // Start what may start: one instance per job, `parallel` at once.
        while sh.running.len() < parallel && !sh.stopping {
            let Some(pos) = sh.queue.iter().position(|n| !sh.running.contains_key(n)) else { break };
            let name = sh.queue.remove(pos).expect("position");
            let log = state.join("logs").join(&name).join(format!("{}.log", Local::now().format("%Y%m%dT%H%M%S")));
            let args: Vec<OsString> = vec!["job".into(), "run".into(), name.clone().into()];
            let child = spawn_ltoarc(&exe, &ctx.child_args, &args, &log, &[]).with_context(|| format!("starting job {name}"))?;
            println!("ltoarcd: started {name} (pid {}); log {}", child.id(), log.display());
            sh.running.insert(name, Running { child, since: Local::now(), log });
        }
        if sh.stopping && sh.running.is_empty() && sh.tasks.is_empty() {
            println!("ltoarcd: stopped");
            let _ = std::fs::remove_file(&sock);
            return Ok(());
        }
        drop(sh);
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Send one request to the daemon and print its answer.
pub fn client(jobs_path: Option<&Path>, request: &str) -> Result<()> {
    let jobs = jobs::load(jobs_path).unwrap_or_default();
    print!("{}", ask(&jobs, request)?);
    Ok(())
}

/// Send one request to the daemon and return its answer.
fn ask(jobs: &JobsFile, request: &str) -> Result<String> {
    let sock = socket_path(jobs);
    let mut s = UnixStream::connect(&sock).map_err(|e| anyhow!("no ltoarcd on {} ({e}); start it with `ltoarcd` or `ltoarc daemon run`", sock.display()))?;
    s.write_all(format!("{request}\n").as_bytes())?;
    s.shutdown(std::net::Shutdown::Write)?;
    let mut reply = String::new();
    std::io::Read::read_to_string(&mut s, &mut reply)?;
    Ok(reply)
}

pub fn run_cmd(ctx: &Ctx, jobs_path: Option<&Path>, cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Run { web } => serve(ctx, jobs_path, web),
        other => client_cmd(jobs_path, other),
    }
}

/// The commands that only talk to a running ltoarcd (they open no catalog).
pub fn client_cmd(jobs_path: Option<&Path>, cmd: DaemonCmd) -> Result<()> {
    match cmd {
        DaemonCmd::Run { .. } => bail!("`daemon run` is the daemon itself"),
        DaemonCmd::Status => client(jobs_path, "status"),
        DaemonCmd::Trigger { name } => client(jobs_path, &format!("run {name}")),
        DaemonCmd::Log { name, lines } => client(jobs_path, &format!("log {name} {lines}")),
        DaemonCmd::Reload => client(jobs_path, "reload"),
        DaemonCmd::Stop => client(jobs_path, "stop"),
        DaemonCmd::Url => {
            let jobs = jobs::load(jobs_path)?;
            let state = state_dir(&jobs);
            // The address the running ltoarcd serves (`--web` included); with none running, or
            // one too old to say, the jobs file's.
            let addr = match ask(&jobs, "web").map(|r| r.trim().to_string()) {
                Ok(a) if a == "none" => bail!("the running ltoarcd serves no web interface; start it with --web ADDRESS, or set web in [daemon] of the jobs file"),
                Ok(a) if a.parse::<std::net::SocketAddr>().is_ok() => a,
                _ => jobs.daemon.web.clone().unwrap_or_else(|| "127.0.0.1:8470".into()),
            };
            // A wildcard address: link to this host by name.
            let addr = match addr.rsplit_once(':') {
                Some(("0.0.0.0" | "[::]", port)) => format!("{}:{port}", std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim()),
                _ => addr,
            };
            let token = std::fs::read_to_string(state.join("web.token"))
                .map_err(|e| anyhow!("no web token in {} ({e}); is ltoarcd running with the web interface?", state.display()))?;
            println!("http://{addr}/#login={}", token.trim());
            Ok(())
        }
    }
}
