//! ltoarc command-line interface.

mod archive;
mod arrange;
mod cron;
mod ctx;
mod fit;
mod daemon;
mod hw;
mod jobs;
mod lease;
mod mount;
mod notify;
mod sim;
mod stats;
mod table;
mod tape;
mod ui;
mod web;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "ltoarc", version, about = "Tape-native LTO archiver")]
struct Cli {
    /// Catalog database (default: $LTOARC_CATALOG or ~/.local/share/ltoarc/catalog.db).
    #[arg(long, global = true)]
    catalog: Option<PathBuf>,
    /// Also use a simulated world (repeatable). Adds to `sim` entries in the config file.
    #[arg(long = "sim", global = true)]
    sims: Vec<PathBuf>,
    /// Only use simulated worlds; do not probe real SCSI devices.
    #[arg(long, global = true)]
    sim_only: bool,
    /// Config file (default: ~/.config/ltoarc/config.toml).
    #[arg(long, global = true)]
    config: Option<PathBuf>,
    /// Key file for encrypted sessions (repeatable); adds to `keys` in the config file.
    #[arg(long = "key", global = true)]
    keys: Vec<PathBuf>,
    /// When a cartridge needed is in no library, ask for it and wait this long (e.g. 2h)
    /// instead of stopping.
    #[arg(long, global = true)]
    wait_tapes: Option<String>,
    /// Jobs file for `job` and `daemon` (default: ~/.config/ltoarc/jobs.toml).
    #[arg(long, global = true)]
    jobs: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Tape libraries and drives.
    #[command(subcommand)]
    Hw(hw::HwCmd),
    /// Cartridges: registration, loading, inspection, initialization.
    #[command(subcommand)]
    Tape(tape::TapeCmd),
    /// Drive and cartridge wear statistics.
    #[command(subcommand)]
    Stats(stats::StatsCmd),
    /// Simulated hardware for testing.
    #[command(subcommand)]
    Sim(sim::SimCmd),
    /// Back up files and directories to a cartridge (one session).
    Backup(Box<archive::BackupArgs>),
    /// List backup sessions.
    Sessions {
        /// Only sessions on this cartridge.
        #[arg(long)]
        tape: Option<String>,
    },
    /// Write one archived file to standard output (e.g. a stream source back to its tool).
    Cat {
        path: PathBuf,
        /// Read only this copy.
        #[arg(long)]
        copy: Option<i64>,
        #[command(flatten)]
        view: archive::ViewArgs,
    },
    /// List archived entries of a directory.
    Ls {
        path: Option<PathBuf>,
        /// Long listing: mode, owner, size and modification time.
        #[arg(short, long)]
        long: bool,
        #[command(flatten)]
        view: archive::ViewArgs,
    },
    /// Find archived paths matching a glob (and where they are on tape).
    Find {
        pattern: String,
        /// Only below this archived directory.
        #[arg(long)]
        under: Option<PathBuf>,
        #[command(flatten)]
        view: archive::ViewArgs,
    },
    /// Restore archived paths.
    Restore(archive::RestoreArgs),
    /// Make another copy of sessions already on tape: read them in one drive, write them to
    /// another tape set in another drive (any generation, any library).
    Copy(archive::CopyArgs),
    /// Expire a job's old sessions by its retention rules (whole chains: a full session with
    /// the ones built on it). Cartridges left holding nothing still needed become reusable.
    Expire(archive::ExpireArgs),
    /// Delete source files of an append-only job that are archived unchanged with enough
    /// verified copies (dash cams, camera cards). Lists them unless --yes.
    Prune(archive::PruneArgs),
    /// Mount the archive read-only (FUSE). Offline: metadata and xattrs from the catalog, file
    /// contents read as zeros. Runs until unmounted with `fusermount3 -u DIR`.
    Mount {
        dir: PathBuf,
        /// Serve from the catalog only: file contents read as zeros (the default).
        #[arg(long, default_value_t = true)]
        offline: bool,
        /// Read file contents from tape on demand (loading cartridges as needed).
        #[arg(long)]
        online: bool,
        /// Online: memory for cached file data (default 2GiB).
        #[arg(long, default_value = "2GiB")]
        cache: String,
        /// Online: a read that cannot be served in this long fails with EIO (default 30m;
        /// loading and positioning a cartridge can take minutes).
        #[arg(long, default_value = "30m")]
        timeout: String,
        /// Reads fail with EIO instead of returning zeros (offline).
        #[arg(long)]
        eio: bool,
        #[command(flatten)]
        view: archive::ViewArgs,
    },
    /// Measure drive write/read speed on a cartridge (test data is removed afterwards) and,
    /// with --source, how fast a source directory reads.
    Calibrate {
        /// ltoarc-initialized cartridge to test on.
        #[arg(long)]
        tape: Option<String>,
        /// Drive to test (default: a free one that can write the cartridge).
        #[arg(long)]
        drive: Option<String>,
        /// Amount of test data (default 16GiB; more gives a steadier number).
        #[arg(long, default_value = "16GiB")]
        size: String,
        /// Stop after this many seconds (default 120).
        #[arg(long, default_value_t = 120)]
        seconds: u64,
        /// Also measure reading this directory.
        #[arg(long)]
        source: Option<PathBuf>,
        /// Write the test data through the kernel st driver (/dev/nstN) instead of SCSI generic.
        #[arg(long)]
        via_st: bool,
        /// Also map the rates the drive streams at without pausing: paced writes for this
        /// long at each rate (e.g. 30s), from its top speed down, each from a standstill.
        /// Prints the ranges for `[rates]` in the config, gap included (some drives cannot
        /// match speeds between their two ranges).
        #[arg(long)]
        sweep: Option<String>,
        /// Sweep in steps of this many MB/s (default 20).
        #[arg(long, default_value_t = 20.0)]
        sweep_step: f64,
        /// Sweep down to this rate in MB/s (default a sixth of the top speed, at least 40).
        #[arg(long)]
        sweep_low: Option<f64>,
        /// Actually write the test data (otherwise only says what it would do).
        #[arg(long)]
        yes: bool,
    },
    /// Measure how fast sources read (scan rate, sequential, parallel, small files) and store
    /// it for the scheduler. Run once per filesystem; repeat after hardware changes.
    Bench {
        sources: Vec<PathBuf>,
        /// Read at most this much per test (default 8GiB).
        #[arg(long, default_value = "8GiB")]
        size: String,
        /// Stop each test after this many seconds (default 20).
        #[arg(long, default_value_t = 20)]
        seconds: u64,
        /// Show stored source and drive benchmarks.
        #[arg(long)]
        list: bool,
    },
    /// Encryption keys.
    #[command(subcommand)]
    Key(KeyCmd),
    /// Scheduled jobs from the jobs file: list them, run one now, see past runs.
    #[command(subcommand)]
    Job(jobs::JobCmd),
    /// The scheduler (ltoarcd): run it, or ask the running one for status, runs, logs.
    #[command(subcommand)]
    Daemon(daemon::DaemonCmd),
    /// Read a cartridge back and check every chunk and file hash against the catalog.
    Verify {
        barcode: String,
        /// Drive to read in (default: a free one that can read its generation).
        #[arg(long)]
        drive: Option<String>,
        /// Read at full speed in bursts, instead of slowing the drive to the checks' pace.
        #[arg(long)]
        no_pace: bool,
    },
}

#[derive(Subcommand)]
enum KeyCmd {
    /// Create a key file (readable only by you). Keep a copy apart from the cartridges:
    /// without it, sessions encrypted with it cannot be read.
    New { file: PathBuf },
    /// Show the id of a key file (sessions record the id of the key they need).
    Id { file: PathBuf },
}

fn key(cmd: KeyCmd) -> anyhow::Result<()> {
    use ltoarc_engine::crypt::Key;
    match cmd {
        KeyCmd::New { file } => {
            let k = Key::generate();
            k.save_new(&file)?;
            println!("key {} written to {}", k.id(), file.display());
        }
        KeyCmd::Id { file } => println!("{}", Key::load(&file)?.id()),
    }
    Ok(())
}

fn main() {
    let argv0 = std::env::args().next().unwrap_or_default();
    if argv0.rsplit('/').next() == Some("mount.ltoarc") {
        if let Err(e) = mount_helper() {
            eprintln!("mount.ltoarc: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // `ltoarcd [OPTIONS]` is `ltoarc [OPTIONS] daemon run`.
    let cli = if argv0.rsplit('/').next() == Some("ltoarcd") {
        let mut args: Vec<String> = std::env::args().collect();
        args[0] = "ltoarc".into();
        // `--web ADDR` belongs to `daemon run`, after it.
        let own: Vec<String> = match args.iter().position(|a| a == "--web" || a.starts_with("--web=")) {
            Some(i) => {
                let n = if args[i] == "--web" { 2 } else { 1 };
                args.drain(i..(i + n).min(args.len())).collect()
            }
            None => Vec::new(),
        };
        args.extend(["daemon".into(), "run".into()]);
        args.extend(own);
        Cli::parse_from(args)
    } else {
        Cli::parse()
    };
    if let Err(e) = run(cli) {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    if let Cmd::Sim(cmd) = cli.cmd {
        return sim::run(cmd);
    }
    if let Cmd::Key(cmd) = cli.cmd {
        return key(cmd);
    }
    // Asking a running ltoarcd needs no catalog.
    if matches!(&cli.cmd, Cmd::Daemon(c) if !matches!(c, daemon::DaemonCmd::Run { .. })) {
        let Cmd::Daemon(cmd) = cli.cmd else { unreachable!() };
        return daemon::client_cmd(cli.jobs.as_deref(), cmd);
    }
    // The same global options for child processes (job steps).
    let mut child_args: Vec<std::ffi::OsString> = Vec::new();
    let mut pass = |flag: &str, v: &std::path::Path| {
        child_args.push(flag.into());
        child_args.push(std::fs::canonicalize(v).unwrap_or_else(|_| v.to_path_buf()).into());
    };
    if let Some(c) = &cli.catalog {
        pass("--catalog", c);
    }
    if let Some(c) = &cli.config {
        pass("--config", c);
    }
    for s in &cli.sims {
        pass("--sim", s);
    }
    for k in &cli.keys {
        pass("--key", k);
    }
    if let Some(j) = &cli.jobs {
        pass("--jobs", j);
    }
    if cli.sim_only {
        child_args.push("--sim-only".into());
    }
    if let Some(w) = &cli.wait_tapes {
        child_args.push("--wait-tapes".into());
        child_args.push(w.into());
    }
    let jobs_path = cli.jobs.clone();
    let mut ctx = ctx::Ctx::open(cli.catalog, cli.config, cli.sims, cli.sim_only, &cli.keys)?;
    ctx.child_args = child_args;
    // Who waits first for a busy drive: someone waiting for data, then backups and copies,
    // then verification.
    use lease::Priority::*;
    let (priority, what) = match &cli.cmd {
        Cmd::Backup(a) => (Backup, format!("backup to {}", a.tape)),
        Cmd::Copy(a) => (Backup, format!("copy to {}", a.to)),
        Cmd::Verify { barcode, .. } => (Scrub, format!("verify {barcode}")),
        Cmd::Calibrate { .. } => (Scrub, "calibrate".to_string()),
        Cmd::Restore(_) | Cmd::Cat { .. } => (Interactive, "restore".to_string()),
        Cmd::Mount { .. } => (Interactive, "mount".to_string()),
        _ => (Interactive, "ltoarc".to_string()),
    };
    ctx.leases.set_purpose(priority, &what);
    ctx.wait_tapes = cli.wait_tapes.as_deref().map(archive::parse_duration).transpose()?;
    let ctx = ctx;
    match cli.cmd {
        Cmd::Hw(cmd) => hw::run(&ctx, cmd),
        Cmd::Tape(cmd) => tape::run(&ctx, cmd),
        Cmd::Stats(cmd) => stats::run(&ctx, cmd),
        Cmd::Sim(_) | Cmd::Key(_) => unreachable!(),
        Cmd::Backup(a) => archive::backup(&ctx, *a),
        Cmd::Sessions { tape } => archive::sessions(&ctx, tape.as_deref()),
        Cmd::Cat { path, copy, view } => archive::cat(&ctx, &path, view.view(&ctx)?, copy),
        Cmd::Ls { path, long, view } => archive::ls(&ctx, path, long, view.view(&ctx)?),
        Cmd::Find { pattern, under, view } => archive::find(&ctx, &pattern, under, view.view(&ctx)?),
        Cmd::Restore(a) => archive::restore(&ctx, a),
        Cmd::Copy(a) => archive::copy(&ctx, a),
        Cmd::Expire(a) => archive::expire(&ctx, a),
        Cmd::Job(cmd) => jobs::run_cmd(&ctx, jobs_path.as_deref(), cmd),
        Cmd::Daemon(cmd) => daemon::run_cmd(&ctx, jobs_path.as_deref(), cmd),
        Cmd::Prune(a) => archive::prune(&ctx, a),
        Cmd::Verify { barcode, drive, no_pace } => archive::verify(&ctx, &barcode, drive.as_deref(), no_pace),
        Cmd::Calibrate { tape, drive, size, seconds, source, via_st, sweep, sweep_step, sweep_low, yes } => archive::calibrate(
            &ctx,
            &archive::CalibrateArgs { tape, drive, size, seconds, source, via_st, sweep, sweep_step, sweep_low, yes },
        ),
        Cmd::Bench { sources, size, seconds, list } => archive::bench(&ctx, &sources, &size, seconds, list),
        Cmd::Mount { dir, online: true, cache, timeout, eio, view, .. } => {
            let opts = ltoarc_fs::Options { view: view.view(&ctx)?, eio };
            let cat = ltoarc_catalog::Catalog::open(&ctx.catalog_path)?;
            let fetcher = mount::Fetcher::new(sim::parse_size(&cache)?, archive::parse_duration(&timeout)?);
            let session = ltoarc_fs::spawn_online(cat, &dir, opts, fetcher.clone())?;
            eprintln!("serving {} at {} (online: file contents from tape; unmount with `fusermount3 -u {}`)", ctx.catalog_path.display(), dir.display(), dir.display());
            mount::serve(&ctx, &fetcher, ltoarc_engine::restore::default_read_buffer(), &|| session.guard.is_finished())?;
            session.join()?;
            Ok(())
        }
        Cmd::Mount { dir, eio, view, .. } => {
            let opts = ltoarc_fs::Options { view: view.view(&ctx)?, eio };
            let cat = ltoarc_catalog::Catalog::open(&ctx.catalog_path)?;
            eprintln!("serving {} at {} (offline; unmount with `fusermount3 -u {}`)", ctx.catalog_path.display(), dir.display(), dir.display());
            ltoarc_fs::mount(cat, &dir, opts)?;
            Ok(())
        }
    }
}

/// `mount -t ltoarc CATALOG DIR -o opt,...` runs `mount.ltoarc CATALOG DIR -o opt,...`.
/// Options: offline, online, cache=SIZE, eio, as_of=TIME, session=ID, catalog=PATH. CATALOG may be "ltoarc" or
/// "none" for the default catalog. Starts `ltoarc mount` detached and waits for the mount.
fn mount_helper() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut pos = Vec::new();
    let mut opts = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-o" => opts.extend(it.next().map(|o| o.split(',').map(str::to_string).collect::<Vec<_>>()).unwrap_or_default()),
            s if s.starts_with('-') => {} // -n, -v, -s and friends from mount(8)
            s => pos.push(s.to_string()),
        }
    }
    let [source, dir] = pos.as_slice() else { anyhow::bail!("usage: mount.ltoarc CATALOG DIR [-o options]") };
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe.with_file_name("ltoarc"));
    if source != "ltoarc" && source != "none" {
        cmd.arg("--catalog").arg(source);
    }
    cmd.arg("mount").arg(dir);
    for o in &opts {
        match o.split_once('=') {
            Some(("as_of", v)) => {
                cmd.arg("--as-of").arg(v);
            }
            Some(("session", v)) => {
                cmd.arg("--session").arg(v);
            }
            Some(("catalog", v)) => {
                cmd.arg("--catalog").arg(v);
            }
            None if o == "eio" => {
                cmd.arg("--eio");
            }
            None if o == "online" => {
                cmd.arg("--online");
            }
            Some(("cache", v)) => {
                cmd.arg("--cache").arg(v);
            }
            _ => {} // offline, ro, defaults, etc.
        }
    }
    use std::os::unix::process::CommandExt;
    unsafe {
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let mut child = cmd.stdin(std::process::Stdio::null()).spawn()?;
    let target = std::fs::canonicalize(dir)?;
    for _ in 0..100 {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("ltoarc mount exited early ({status})");
        }
        let mounts = std::fs::read_to_string("/proc/self/mountinfo")?;
        if mounts.lines().any(|l| l.split(' ').nth(4) == Some(target.to_str().unwrap_or(""))) {
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    anyhow::bail!("timed out waiting for the mount at {dir}")
}
