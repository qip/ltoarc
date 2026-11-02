//! End-to-end CLI flow against a simulated library (no real hardware is touched).

use std::path::Path;
use std::process::{Command, Output};

fn ltoarc(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(dir.join("cat.db"))
        .arg("--sim")
        .arg(dir.join("world"))
        .arg("--sim-only")
        .args(args)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> String {
    let out = ltoarc(dir, args);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(out.status.success(), "ltoarc {args:?} failed:\n{text}");
    text
}

/// Capacity of a simulated cartridge: `mib` MiB plus what the copy of the ltoarc binary in
/// the bootstrap file after each label takes. A release build is put there (13 MiB or so);
/// a debug build is too large and goes without. So the cartridges hold as much in either.
fn capacity(mib: u64) -> String {
    let exe = std::fs::metadata(env!("CARGO_BIN_EXE_ltoarc")).unwrap().len();
    let binary = if exe <= ltoarc_engine::bootstrap::MAX_BINARY { exe.next_multiple_of(1 << 20) } else { 0 };
    ((mib << 20) + binary).to_string()
}

fn fails(dir: &Path, args: &[&str]) -> String {
    let out = ltoarc(dir, args);
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    assert!(!out.status.success(), "ltoarc {args:?} should fail:\n{text}");
    text
}

#[test]
fn scan_register_init_m8_and_stats() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let world = d.join("world");
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(&world)
        .args(["--drive", "LTO-8", "--drive", "LTO-6:unattached"])
        .args(["--tapes", "DC8001L8..DC8002L8", "--tapes", "DC7001L7", "--tapes", "DC5001L5", "--capacity", &capacity(128)])
        .status()
        .unwrap();
    assert!(status.success());

    let scan = ok(d, &["hw", "scan"]);
    assert!(scan.contains("lto8-1") && scan.contains("attached"));
    assert!(scan.contains("lto6-1") && scan.contains("unattached"));

    let reg = ok(d, &["tape", "register"]);
    assert!(reg.contains("DC7001L7") && reg.contains("LTO-7"));

    // Dry run does not touch the library.
    let dry = ok(d, &["tape", "init", "DC7001L7", "--m8"]);
    assert!(dry.contains("dry run"));
    assert!(ok(d, &["tape", "list"]).contains("lib1 slot 3"));

    let init = ok(d, &["tape", "init", "DC7001L7", "--m8", "--pool", "dashcam", "--yes"]);
    assert!(init.contains("LTO-7 M8 volume"), "{init}");
    let info = ok(d, &["tape", "info", "DC7001L7"]);
    assert!(info.contains("5Dh LTO-7 M8"), "{info}");
    assert!(info.contains("ltoarc volume"));

    // Re-initializing needs --force; M8 conversion is refused once written.
    assert!(fails(d, &["tape", "init", "DC7001L7", "--yes"]).contains("--force"));
    ok(d, &["tape", "unload"]);

    // LTO-5 media only works in the LTO-6 drive, which is not attached.
    let e = fails(d, &["tape", "load", "DC5001L5"]);
    assert!(e.contains("lto6-1 (unattached)"), "{e}");

    // An LTO-8 cartridge cannot become M8.
    assert!(fails(d, &["tape", "init", "DC8001L8", "--m8", "--yes"]).contains("cannot make DC8001L8 an M8"));
    ok(d, &["tape", "init", "DC8001L8", "--pool", "db", "--yes"]);
    ok(d, &["tape", "unload", "--slot", "10"]);
    assert!(ok(d, &["tape", "list"]).contains("lib1 slot 10"));

    let stats = ok(d, &["stats", "drive", "lto8-1", "--history", "20"]);
    assert!(stats.contains("init") && stats.contains("unload"));
    let tape_stats = ok(d, &["stats", "tape", "DC7001L7"]);
    assert!(tape_stats.contains("loads"));
}

fn have_fuse() -> bool {
    Path::new("/dev/fuse").exists() && Command::new("fusermount3").arg("-V").output().is_ok_and(|o| o.status.success())
}

#[test]
fn backup_restore_and_offline_mount() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8", "--capacity", &capacity(128)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    ok(d, &["tape", "init", "DC8001L8", "--yes"]);

    let src = d.join("src");
    std::fs::create_dir_all(src.join("a/b")).unwrap();
    std::fs::write(src.join("a/b/data.bin"), vec![7u8; 3 << 20]).unwrap();
    std::fs::write(src.join("a/note.txt"), "note").unwrap();
    std::os::unix::fs::symlink("a/note.txt", src.join("link")).unwrap();
    let src_s = src.to_str().unwrap();

    let out = ok(d, &["backup", src_s, "--tape", "DC8001L8", "--hash", "sha256", "--job", "t"]);
    assert!(out.contains("(complete, full)"), "{out}");
    assert!(ok(d, &["sessions"]).contains("DC8001L8"));
    assert!(ok(d, &["find", "*.bin"]).contains("DC8001L8@"));
    assert!(ok(d, &["verify", "DC8001L8"]).contains("DC8001L8: OK"));

    let dest = d.join("restored");
    let r = ok(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]);
    assert!(r.contains("hashes verified"), "{r}");
    assert_eq!(std::fs::read(dest.join("a/b/data.bin")).unwrap(), vec![7u8; 3 << 20]);
    assert_eq!(std::fs::read_link(dest.join("link")).unwrap(), Path::new("a/note.txt"));
    assert!(fails(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]).contains("--overwrite"));

    if !have_fuse() {
        eprintln!("skipping mount check: no FUSE");
        return;
    }
    let mnt = d.join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("mount")
        .arg(&mnt)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let inner = mnt.join(src.strip_prefix("/").unwrap());
    for _ in 0..100 {
        if inner.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let listing = |root: &Path| -> String {
        let o = Command::new("find").arg(".").args(["-printf", "%s %T@ %m %y %p %l\n"]).current_dir(root).output().unwrap();
        let mut lines: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(str::to_string).collect();
        lines.sort();
        lines.join("\n")
    };
    let same = listing(&src) == listing(&inner);
    let xattr = Command::new("getfattr").args(["--only-values", "-n", "user.ltoarc.sha256"]).arg(inner.join("a/note.txt")).output();
    let zeros = std::fs::read(inner.join("a/note.txt")).unwrap_or_default();
    let _ = Command::new("fusermount3").arg("-u").arg(&mnt).status();
    let _ = child.wait();
    assert!(same, "mounted tree differs from source:\n{}\n---\n{}", listing(&src), "(see mount)");
    if let Ok(x) = xattr {
        if x.status.success() {
            assert_eq!(String::from_utf8_lossy(&x.stdout), sha256_hex(b"note"));
        }
    }
    assert_eq!(zeros, vec![0u8; 4]);
}

fn sha256_hex(data: &[u8]) -> String {
    let out = Command::new("sha256sum").stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).spawn().and_then(|mut c| {
        use std::io::Write;
        c.stdin.take().unwrap().write_all(data)?;
        c.wait_with_output()
    });
    String::from_utf8_lossy(&out.unwrap().stdout).split_whitespace().next().unwrap_or("").to_string()
}

#[test]
fn cartridge_without_barcode() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let spec = d.join("spec.toml");
    let cap = capacity(64);
    std::fs::write(
        &spec,
        format!(
            r#"
        [[libraries]]
        name = "lib1"
        slots = 6
        ie_slots = 1
        drives = [{{ generation = "LTO-8" }}]
        tapes = [
          {{ barcode = "DC8001L8", cartridge = "LTO-8", capacity = {cap} }},
          {{ cartridge = "LTO-7", capacity = {cap}, slot = 3 }},
        ]
        "#
        ),
    )
    .unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world")).arg("--spec").arg(&spec).status().unwrap();
    assert!(st.success());
    ok(d, &["hw", "scan"]);
    let reg = ok(d, &["tape", "register"]);
    assert!(reg.contains("slot 4: cartridge without a readable barcode") && reg.contains("--slot 4 --name"), "{reg}");
    assert!(fails(d, &["tape", "register", "--slot", "1", "--name", "X"]).contains("readable barcode"));
    ok(d, &["tape", "register", "--slot", "4", "--name", "PRGP04"]);
    // One name per cartridge.
    let twice = fails(d, &["tape", "register", "--slot", "4", "--name", "OTHER"]);
    assert!(twice.contains("has PRGP04 there already") && twice.contains("unregister PRGP04"), "{twice}");
    ok(d, &["tape", "register", "--slot", "4", "--name", "PRGP04"]);
    let init = ok(d, &["tape", "init", "PRGP04", "--pool", "test", "--yes"]);
    assert!(init.contains("LTO-7 volume"), "{init}");
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f"), "data").unwrap();
    ok(d, &["backup", src.to_str().unwrap(), "--tape", "PRGP04"]);
    let un = ok(d, &["tape", "unload"]);
    assert!(un.contains("PRGP04 returned to lib1 slot 4"), "{un}");
    let list = ok(d, &["tape", "list"]);
    assert!(list.lines().any(|l| l.starts_with("PRGP04") && l.contains("lib1 slot 4")), "{list}");
    let reg = ok(d, &["tape", "register"]);
    assert!(reg.contains("PRGP04") && !reg.contains("slot 4: cartridge without"), "{reg}");
    assert!(ok(d, &["verify", "PRGP04"]).contains("PRGP04: OK"));
    // Unregistering: a cartridge that holds sessions needs --force, and --yes without a terminal.
    assert!(fails(d, &["tape", "unregister", "PRGP04"]).contains("holds 1 session(s); add --force"));
    let ask = fails(d, &["tape", "unregister", "PRGP04", "--force"]);
    assert!(ask.contains("1 held nowhere else") && ask.contains("add --yes to forget them"), "{ask}");
    let gone = ok(d, &["tape", "unregister", "PRGP04", "--force", "--yes"]);
    assert!(gone.contains("PRGP04 removed from the catalog; forgot 1 session(s)"), "{gone}");
    assert!(ok(d, &["sessions"]).contains("no sessions"));
    // It was in the drive (verified); unloaded, it can be named again.
    let un = ok(d, &["tape", "unload"]);
    assert!(un.contains("slot 4"), "{un}");
    ok(d, &["tape", "register", "--slot", "4", "--name", "PRGP05"]);
    // A barcoded one comes back with the next inventory.
    let bc = ok(d, &["tape", "unregister", "DC8001L8"]);
    assert!(bc.contains("DC8001L8 removed") && bc.contains("next `ltoarc tape register` records it again"), "{bc}");
    assert!(ok(d, &["tape", "register"]).contains("DC8001L8"));
}

/// Moving cartridges between slots: one at a time, several at once through a free slot, out
/// to the I/E slot, and a cartridge without barcode, which is followed by its slot.
#[test]
fn move_cartridges_between_slots() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let spec = d.join("spec.toml");
    std::fs::write(
        &spec,
        r#"
        [[libraries]]
        name = "lib1"
        slots = 5
        ie_slots = 1
        drives = [{ generation = "LTO-8" }]
        tapes = [
          { barcode = "DC8001L8", cartridge = "LTO-8" },
          { barcode = "DC8002L8", cartridge = "LTO-8" },
          { barcode = "DC8003L8", cartridge = "LTO-8" },
          { cartridge = "LTO-8", slot = 3 },
        ]
        "#,
    )
    .unwrap();
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world")).arg("--spec").arg(&spec).status().unwrap();
    assert!(st.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    ok(d, &["tape", "register", "--slot", "4", "--name", "NOLABEL"]);
    let at = |bc: &str| {
        let list = ok(d, &["tape", "list"]);
        list.lines().find(|l| l.starts_with(bc)).unwrap_or_else(|| panic!("{bc} not in {list}")).split("lib1 ").nth(1).unwrap().split("  ").next().unwrap().to_string()
    };
    assert_eq!(at("DC8003L8"), "slot 3");

    let out = ok(d, &["tape", "move", "DC8001L8", "5"]);
    assert!(out.contains("DC8001L8 moved from lib1 slot 1 to slot 5"), "{out}");
    assert_eq!(at("DC8001L8"), "slot 5");
    // A taken place needs --swap, or the other cartridge moved too.
    let e = fails(d, &["tape", "move", "DC8002L8", "slot5"]);
    assert!(e.contains("lib1 slot 5 holds DC8001L8; add --swap") && e.contains("DC8001L8=PLACE"), "{e}");
    assert!(fails(d, &["tape", "move", "DC8002L8", "9"]).contains("lib1 has no slot 9"));
    // Changing places goes through a free slot; a dry run moves nothing.
    let dry = ok(d, &["tape", "move", "DC8002L8=5", "DC8001L8=2", "--dry-run"]);
    assert!(dry.contains("would move") && dry.contains("dry run"), "{dry}");
    assert_eq!(at("DC8002L8"), "slot 2");
    ok(d, &["tape", "move", "DC8002L8", "5", "--swap"]);
    assert_eq!((at("DC8001L8"), at("DC8002L8")), ("slot 2".to_string(), "slot 5".to_string()));
    // Three in turn, the one without barcode among them, and one out to the I/E slot.
    ok(d, &["tape", "move", "DC8001L8=3", "DC8003L8=4", "NOLABEL=2", "DC8002L8=ie"]);
    assert_eq!((at("DC8001L8"), at("DC8003L8"), at("NOLABEL"), at("DC8002L8")), ("slot 3".into(), "slot 4".into(), "slot 2".into(), "I/E 1".into()));
    // What the catalog says is where the cartridges are: the next inventory agrees.
    let reg = ok(d, &["tape", "register"]);
    assert!(!reg.contains("without a readable barcode") && !reg.contains("no longer in"), "{reg}");
    assert_eq!(at("NOLABEL"), "slot 2");
    // A full library has no room to exchange cartridges through.
    ok(d, &["tape", "move", "DC8002L8", "1"]);
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world2")).args(["--slots", "2", "--ie", "0", "--tapes", "DC8001L8..DC8002L8"]).status().unwrap();
    assert!(st.success());
    let full = |args: &[&str]| {
        let mut all = vec!["--sim-only"];
        all.extend(args);
        Command::new(env!("CARGO_BIN_EXE_ltoarc")).arg("--catalog").arg(d.join("cat2.db")).arg("--sim").arg(d.join("world2")).args(&all).output().unwrap()
    };
    full(&["hw", "scan"]);
    full(&["tape", "register"]);
    let out = full(&["tape", "move", "DC8001L8", "2", "--swap"]);
    assert!(String::from_utf8_lossy(&out.stderr).contains("no free slot to exchange cartridges through"), "{out:?}");
    // A cartridge in a drive is unloaded, not moved.
    ok(d, &["tape", "load", "DC8003L8"]);
    assert!(fails(d, &["tape", "move", "DC8003L8", "4"]).contains("tape unload --slot N"));
}

fn noise(seed: u64, len: usize) -> Vec<u8> {
    let mut x = seed | 1;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect()
}

#[test]
fn backup_spans_a_tape_set() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8004L8", "--capacity", &capacity(48)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    ok(d, &["tape", "init", "DC8001L8", "--pool", "span", "--yes"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    for i in 0..3u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, 28 << 20)).unwrap();
    }
    for i in 0..50u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 9000)).unwrap();
    }
    let src_s = src.to_str().unwrap();
    // Only the first cartridge is labelled; the others are blank and get labelled on the way.
    let out = ok(d, &["backup", src_s, "--tape", "DC8001L8..DC8004L8", "--buffer", "8MiB"]);
    assert!(out.contains("DC8001L8 is full; continuing on DC8002L8"), "{out}");
    assert!(out.contains("DC8002L8 is blank; labelling it"), "{out}");
    for bc in ["DC8001L8", "DC8002L8", "DC8003L8"] {
        assert!(out.contains(&format!("tape      {bc} blocks")), "{out}");
    }
    assert!(!out.contains("tape      DC8004L8") && !out.contains("continuing on DC8004L8"), "{out}");
    assert!(out.contains("(complete, full)"), "{out}");
    assert!(ok(d, &["sessions"]).contains("DC8001L8,DC8002L8,DC8003L8"));
    let list = ok(d, &["tape", "list"]);
    assert!(list.lines().any(|l| l.contains("DC8001L8") && l.contains("full")), "{list}");

    // Restore swaps cartridges in the drive as it goes.
    let dest = d.join("restored");
    let r = ok(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]);
    assert!(r.contains("from DC8001L8, DC8002L8, DC8003L8"), "{r}");
    for i in 0..3u64 {
        assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, 28 << 20), "big{i}.bin differs");
    }
    assert_eq!(std::fs::read(dest.join("small/7.txt")).unwrap(), noise(107, 9000));
    ok(d, &["tape", "unload"]);
    assert!(ok(d, &["verify", "DC8002L8"]).contains("DC8002L8: OK"));
    assert!(ok(d, &["tape", "recover", "DC8002L8"]).contains("clean, nothing to recover"));
    // Cartridges labelled by init or on the way carry the bootstrap file after the label.
    let scan = ok(d, &["tape", "scan", "DC8002L8"]);
    assert!(scan.contains("1 already known") && scan.contains("bootstrap file, 0 other"), "{scan}");
}

#[test]
fn encrypted_backup_needs_its_key() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8002L8", "--capacity", &capacity(128)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let key = d.join("backup.key");
    let k = ok(d, &["key", "new", key.to_str().unwrap()]);
    assert!(fails(d, &["key", "new", key.to_str().unwrap()]).contains("creating key file"), "never overwrites a key");
    let id = ok(d, &["key", "id", key.to_str().unwrap()]);
    assert!(k.contains(id.trim()));
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"private").unwrap();
    let src_s = src.to_str().unwrap();
    ok(d, &["backup", src_s, "--tape", "DC8001L8", "--encrypt", key.to_str().unwrap()]);
    let dest = d.join("out");
    let e = fails(d, &["restore", src_s, "--to", dest.to_str().unwrap()]);
    assert!(e.contains(&format!("encrypted with key {}", id.trim())), "{e}");
    ok(d, &["--key", key.to_str().unwrap(), "restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]);
    assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"private");
    assert!(ok(d, &["--key", key.to_str().unwrap(), "verify", "DC8001L8"]).contains("DC8001L8: OK"));
    // Erasing the cartridge removes its sessions from the catalog, so it can start afresh.
    let init = ok(d, &["tape", "init", "DC8001L8", "--force", "--yes"]);
    assert!(init.contains("removed 1 session(s)"), "{init}");
    assert!(ok(d, &["sessions"]).contains("no sessions"));
    ok(d, &["backup", src_s, "--tape", "DC8001L8"]);
}

/// A range without media identifiers spans generations: each cartridge goes to a drive that
/// can write it, and restore reads each in a drive that can read it.
#[test]
fn tape_set_mixes_generations_and_drives() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6"])
        .args(["--tapes", "DC0001L8", "--tapes", "DC0002L6", "--tapes", "DC0003L5", "--capacity", &capacity(48)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..3u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, 28 << 20)).unwrap();
    }
    let src_s = src.to_str().unwrap();
    let out = ok(d, &["backup", src_s, "--tape", "DC0001..DC0003", "--buffer", "8MiB"]);
    assert!(out.contains("DC0001L8 is full; continuing on DC0002L6"), "{out}");
    assert!(out.contains("continuing in drive lto6-1"), "{out}");
    assert!(out.contains("DC0002L6 is full; continuing on DC0003L5"), "{out}");
    assert!(out.contains("(complete, full)"), "{out}");
    let dest = d.join("restored");
    let r = ok(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]);
    // The LTO-8 drive cannot read L6, so that cartridge is read in the LTO-6 drive.
    assert!(r.contains("reading DC0001L8 in drive lto8-1") && r.contains("reading DC0002L6 in drive lto6-1"), "{r}");
    for i in 0..3u64 {
        assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, 28 << 20), "big{i}.bin differs");
    }
}

#[test]
fn parallel_drives_share_a_tape_set() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-8", "--tapes", "DC8001L8..DC8004L8", "--capacity", &capacity(48)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    for i in 0..5u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, (14 + 2 * i as usize) << 20)).unwrap();
    }
    for i in 0..60u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 9000)).unwrap();
    }
    let src_s = src.to_str().unwrap();
    let out = ok(d, &["backup", src_s, "--tape", "DC8001..DC8004", "--drives", "2", "--buffer", "16MiB"]);
    assert!(out.contains("lto8-2 writes DC8002L8 in parallel"), "{out}");
    assert!(out.contains("(complete, full)"), "{out}");
    assert!(out.contains("drive     lto8-1: ") && out.contains("drive     lto8-2: "), "{out}");
    assert!(out.matches("tape      DC800").count() >= 3, "both drives went on to further cartridges:\n{out}");
    let dest = d.join("restored");
    ok(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s]);
    for i in 0..5u64 {
        assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, (14 + 2 * i as usize) << 20), "big{i}.bin differs");
    }
    assert_eq!(std::fs::read(dest.join("small/7.txt")).unwrap(), noise(107, 9000));
    assert!(fails(d, &["backup", src_s, "--tape", "DC8001..DC8004", "--drives", "2", "--drive-compression"]).contains("compression must stay off"));
}

#[test]
fn copy_between_tape_sets() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6", "--tapes", "DC8001L8..DC8003L8", "--tapes", "DC5001L5..DC5004L5", "--capacity", &capacity(40)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    for i in 0..3u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, 18 << 20)).unwrap();
    }
    for i in 0..40u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 7000)).unwrap();
    }
    let src_s = src.to_str().unwrap();
    let b = ok(d, &["backup", src_s, "--tape", "DC8001..DC8003", "--buffer", "8MiB"]);
    assert!(b.contains("DC8001L8 is full"), "{b}");
    let c = ok(d, &["copy", "--from", "DC8001..DC8003", "--to", "DC5001..DC5004", "--buffer", "8MiB"]);
    assert!(c.contains("from drive lto8-1 to drive lto6-1"), "{c}");
    assert!(c.contains("copy 1 -> copy 2 on DC5001L5,DC5002L5"), "{c}");
    assert!(!c.contains("ERROR"), "{c}");
    let dest = d.join("restored");
    let r = ok(d, &["restore", src_s, "--to", dest.to_str().unwrap(), "--strip-prefix", src_s, "--copy", "2"]);
    assert!(r.contains("from DC5001L5"), "{r}");
    assert!(!r.contains("DC800"), "copy 2 is read from the LTO-5 cartridges only:\n{r}");
    for i in 0..3u64 {
        assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, 18 << 20), "big{i}.bin differs");
    }
    assert_eq!(std::fs::read(dest.join("small/7.txt")).unwrap(), noise(107, 7000));
    assert!(ok(d, &["verify", "DC5002L5"]).contains("DC5002L5: OK"));
}

/// Levels from the command line, retention, reuse of an expired cartridge, and pruning an
/// append-only source once it has verified copies.
#[test]
fn levels_retention_and_prune() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC0001L8..DC0004L8", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("sub")).unwrap();
    std::fs::write(src.join("a.bin"), noise(1, 3 << 20)).unwrap();
    std::fs::write(src.join("sub/b.txt"), b"first").unwrap();
    let s = src.to_str().unwrap();
    assert!(fails(d, &["backup", s, "--tape", "DC0001L8", "--level", "incremental"]).contains("needs --job"));
    // No full session yet: the first incremental one runs as a full one.
    let f = ok(d, &["backup", s, "--tape", "DC0001L8", "--job", "j", "--level", "incremental"]);
    assert!(f.contains("had no full session yet") && f.contains("(complete, full)"), "{f}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(src.join("sub/b.txt"), b"second").unwrap();
    std::fs::remove_file(src.join("a.bin")).unwrap();
    let i = ok(d, &["backup", s, "--tape", "DC0001L8", "--job", "j", "--level", "incremental"]);
    assert!(i.contains("(complete, incremental after") && i.contains("deleted   1 entries"), "{i}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    ok(d, &["backup", s, "--tape", "DC0002L8", "--job", "j"]);
    let list = ok(d, &["sessions"]);
    assert_eq!(list.matches(" j ").count(), 3, "{list}");
    // Keep one chain: the full + incremental pair on DC0001L8 expires, and the cartridge
    // holds nothing else, so it can be reused.
    let dry = ok(d, &["expire", "--job", "j", "--keep-fulls", "1"]);
    assert!(dry.contains("2 session(s) would expire"), "{dry}");
    let e = ok(d, &["expire", "--job", "j", "--keep-fulls", "1", "--yes"]);
    assert!(e.contains("reusable now") && e.contains("DC0001L8"), "{e}");
    assert!(ok(d, &["sessions"]).contains("expired"));
    let r = ok(d, &["backup", s, "--tape", "DC0001L8", "--job", "other"]);
    assert!(r.contains("holds only expired sessions; relabelling it"), "{r}");

    // Append-only: prune deletes what has two verified copies; one copy is not enough.
    let cam = d.join("cam");
    std::fs::create_dir_all(&cam).unwrap();
    std::fs::write(cam.join("0001.mp4"), noise(2, 500_000)).unwrap();
    let c = cam.to_str().unwrap();
    ok(d, &["backup", c, "--tape", "DC0003L8", "--job", "cam", "--level", "append"]);
    let p = ok(d, &["prune", "--job", "cam", "--copies", "1"]);
    assert!(p.contains("would delete 0 file(s)") && p.contains("1 archived file(s) still lack 1 verified copies"), "{p}");
    ok(d, &["verify", "DC0003L8"]);
    let p = ok(d, &["prune", "--job", "cam", "--copies", "1"]);
    assert!(p.contains("0001.mp4") && p.contains("would delete 1 file(s)"), "{p}");
    assert!(ok(d, &["prune", "--job", "cam", "--copies", "2"]).contains("would delete 0 file(s)"));
    ok(d, &["prune", "--job", "cam", "--copies", "1", "--yes"]);
    assert!(!cam.join("0001.mp4").exists());
    assert!(fails(d, &["prune", "--job", "j"]).contains("append-only"));
}

/// The parts (copy, part, barcode) of the one session in the catalog at `dir`.
fn parts(dir: &Path) -> Vec<(i64, i64, String)> {
    let cat = ltoarc_catalog::Catalog::open(&dir.join("cat.db")).unwrap();
    let session = cat.sessions().unwrap().pop().expect("a session");
    let vols = cat.volumes().unwrap();
    cat.session_volumes(session.id)
        .unwrap()
        .iter()
        .map(|sv| (sv.copy, sv.seq, vols.iter().find(|v| v.id == sv.volume_id).unwrap().barcode.clone()))
        .collect()
}

/// Each copy of a tee spans its own tape set and numbers its own parts: copy 2's first
/// cartridge is part 1, in the catalog and on tape, and copy 2 alone restores across both.
#[test]
fn tee_copies_count_their_own_parts() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6", "--tapes", "DC8001L8..DC8003L8", "--tapes", "DC6001L6..DC6003L6", "--capacity", "64MiB"])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..4u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, 18 << 20)).unwrap();
    }
    let s = src.to_str().unwrap();
    let out = ok(d, &["backup", s, "--tape", "DC8001L8..", "--tape2", "DC6001L6..DC6003L6", "--buffer", "8MiB"]);
    assert!(!out.contains("writing to DC6001L6"), "{out}");
    assert!(out.contains("writing to DC8002L8 (cartridge 2 of copy 1)"), "{out}");
    assert!(out.contains("writing to DC6002L6 (cartridge 2 of copy 2)"), "{out}");
    assert!(!out.contains("tape      DC8003L8") && !out.contains("tape      DC6003L6"), "{out}");
    assert!(out.contains("(complete, full)"), "{out}");
    let want = |bc: &[(i64, i64, &str)]| bc.iter().map(|&(c, p, b)| (c, p, b.to_string())).collect::<Vec<_>>();
    assert_eq!(parts(d), want(&[(1, 1, "DC8001L8"), (1, 2, "DC8002L8"), (2, 1, "DC6001L6"), (2, 2, "DC6002L6")]));

    let dest = d.join("restored");
    let r = ok(d, &["restore", s, "--to", dest.to_str().unwrap(), "--strip-prefix", s, "--copy", "2"]);
    assert!(r.contains("from DC6001L6, DC6002L6"), "{r}");
    for i in 0..4u64 {
        assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, 18 << 20), "big{i}.bin differs");
    }

    // A scan of copy 2's second cartridge into an empty catalog reads part 2 from its SESSION_BEGIN.
    ok(d, &["tape", "unload", "--drive", "lto6-1"]);
    ok(d, &["tape", "unload", "--drive", "lto8-1"]);
    let fresh = d.join("fresh");
    std::fs::create_dir_all(&fresh).unwrap();
    std::os::unix::fs::symlink(d.join("world"), fresh.join("world")).unwrap();
    ok(&fresh, &["hw", "scan"]);
    ok(&fresh, &["tape", "register"]);
    ok(&fresh, &["tape", "scan", "DC6002L6"]);
    assert_eq!(parts(&fresh), want(&[(2, 2, "DC6002L6")]));
}

/// A folder renamed between a full and an incremental session leaves the latest view, in
/// ls and the mount, and stays in the full session's view.
#[test]
fn renamed_folder_leaves_the_latest_view() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world")).args(["--drive", "LTO-8", "--tapes", "DC8001L8", "--capacity", "64MiB"]).status().unwrap();
    assert!(st.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    let year = src.join("photos/2025");
    std::fs::create_dir_all(year.join("kyoto")).unwrap();
    std::fs::create_dir_all(year.join("osaka")).unwrap();
    for n in 1..=3 {
        std::fs::write(year.join(format!("kyoto/DSC0220{n}.ARW")), noise(n, 20_000)).unwrap();
    }
    std::fs::write(year.join("osaka/a.jpg"), b"a").unwrap();
    let map = format!("{}=/tank/data", src.display());
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC8001L8", "--job", "data", "--map", &map]);
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::rename(year.join("kyoto"), year.join("kyoto-autumn")).unwrap();
    let i = ok(d, &["backup", s, "--tape", "DC8001L8", "--job", "data", "--level", "incremental", "--map", &map]);
    assert!(i.contains("deleted   4 entries"), "{i}");

    let l = ok(d, &["ls", "-l", "/tank/data/photos/2025"]);
    assert!(l.contains(" kyoto-autumn\n") && l.contains(" osaka\n") && !l.contains(" kyoto\n"), "{l}");
    assert!(fails(d, &["ls", "/tank/data/photos/2025/kyoto"]).contains("has no version in this view"));
    assert_eq!(ok(d, &["ls", "/tank/data/photos/2025/kyoto-autumn"]), "DSC02201.ARW\nDSC02202.ARW\nDSC02203.ARW\n");
    assert!(!ok(d, &["find", "*.ARW"]).contains("/kyoto/"));
    let full = ok(d, &["sessions"]).lines().find(|l| l.contains(" full ")).unwrap().split_whitespace().next().unwrap().to_string();
    assert_eq!(ok(d, &["ls", "/tank/data/photos/2025", "--session", &full]), "kyoto/\nosaka/\n");
    assert_eq!(ok(d, &["ls", "/tank/data/photos/2025/kyoto", "--session", &full]), "DSC02201.ARW\nDSC02202.ARW\nDSC02203.ARW\n");

    if !have_fuse() {
        eprintln!("skipping mount check: no FUSE");
        return;
    }
    let mnt = d.join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("mount")
        .arg(&mnt)
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let inner = mnt.join("tank/data/photos/2025");
    for _ in 0..100 {
        if inner.exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let mut names: Vec<String> = std::fs::read_dir(&inner).map(|r| r.map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect()).unwrap_or_default();
    names.sort();
    let old_exists = inner.join("kyoto").exists();
    let _ = Command::new("fusermount3").arg("-u").arg(&mnt).status();
    let _ = child.wait();
    assert_eq!(names, ["kyoto-autumn", "osaka"]);
    assert!(!old_exists);
}

/// Two copies at once: copy 1 to LTO-8 cartridges, copy 2 to LTO-5 cartridges in the LTO-6
/// drive, one read of the source; either copy restores everything.
#[test]
fn tee_two_copies_at_once() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6", "--tapes", "DC8001L8..DC8002L8", "--tapes", "DC5001L5..DC5003L5", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    for i in 0..3u64 {
        std::fs::write(src.join(format!("big{i}.bin")), noise(i, 16 << 20)).unwrap();
    }
    for i in 0..30u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 5000)).unwrap();
    }
    let s = src.to_str().unwrap();
    let out = ok(d, &["backup", s, "--tape", "DC8001..DC8002", "--tape2", "DC5001..DC5003", "--buffer", "8MiB"]);
    assert!(out.contains("copy 2    to DC5001L5 in drive lto6-1"), "{out}");
    assert!(out.contains("drive lto8-1 is matched to drive lto6-1"), "{out}");
    assert!(out.contains("DC8001L8 (copy 1)") && out.contains("DC5001L5 (copy 2)"), "{out}");
    assert!(out.contains("(complete, full)"), "{out}");
    for copy in ["1", "2"] {
        let dest = d.join(format!("restored{copy}"));
        let r = ok(d, &["restore", s, "--to", dest.to_str().unwrap(), "--strip-prefix", s, "--copy", copy]);
        assert!(!r.contains(if copy == "1" { "DC500" } else { "DC800" }), "copy {copy} read only its own cartridges:\n{r}");
        for i in 0..3u64 {
            assert!(std::fs::read(dest.join(format!("big{i}.bin"))).unwrap() == noise(i, 16 << 20), "copy {copy}: big{i}.bin");
        }
    }
    ok(d, &["verify", "DC5001L5"]);
    assert!(ok(d, &["sessions"]).contains("DC5001L5"));
}

/// After a session with copy 2 in another drive, each cartridge's room is still its own
/// (the statistics read after the session are filed under the cartridge they came from).
#[test]
fn room_after_two_copies() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6", "--tapes", "DC0001L8", "--tapes", "DC0002L6"])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.txt"), b"two copies").unwrap();
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC0001L8", "--tape2", "DC0002L6"]);
    let plan = ok(d, &["backup", s, "--tape", "DC0001L8", "--tape2", "DC0002L6", "--dry-run"]);
    let room = |bc: &str| plan.lines().find(|l| l.trim_start().starts_with(bc)).unwrap_or_default().to_string();
    assert!(room("DC0001L8").contains("11.9 TB"), "{plan}");
    assert!(room("DC0002L6").contains("2.5 TB"), "{plan}");
}

/// A later copy from the source disks, then a restore from that copy alone.
#[test]
fn copy_from_source_disks() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--drive", "LTO-6", "--tapes", "DC8001L8", "--tapes", "DC5001L5..DC5002L5", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    std::fs::write(src.join("big.bin"), noise(1, 20 << 20)).unwrap();
    for i in 0..20u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 4000)).unwrap();
    }
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC8001L8"]);
    let c = ok(d, &["copy", "--from-source", "--from", "DC8001L8", "--to", "DC5001..DC5002", "--buffer", "8MiB"]);
    assert!(c.contains("as copy 2 in drive lto6-1") && c.contains("copy 2 on DC5001L5"), "{c}");
    assert!(!c.contains("MISSING"), "{c}");
    ok(d, &["verify", "DC5001L5"]);
    let dest = d.join("restored");
    let r = ok(d, &["restore", s, "--to", dest.to_str().unwrap(), "--strip-prefix", s, "--copy", "2"]);
    assert!(!r.contains("DC8001L8"), "{r}");
    assert!(std::fs::read(dest.join("big.bin")).unwrap() == noise(1, 20 << 20));
    assert_eq!(std::fs::read(dest.join("small/7.txt")).unwrap(), noise(107, 4000));
}

/// Online mount: file contents come from tape, the cartridge loaded on demand; concurrent
/// reads are served from one pass.
#[test]
fn online_mount_reads_from_tape() {
    if !have_fuse() {
        eprintln!("skipping online mount: no FUSE");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8", "--capacity", &capacity(128)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(src.join("small")).unwrap();
    std::fs::write(src.join("big.bin"), noise(1, 40 << 20)).unwrap();
    for i in 0..40u64 {
        std::fs::write(src.join(format!("small/{i}.txt")), noise(100 + i, 3000 + i as usize)).unwrap();
    }
    {
        // Sparse: data, a large hole, data.
        use std::io::{Seek, SeekFrom, Write};
        let mut f = std::fs::File::create(src.join("sparse.img")).unwrap();
        f.write_all(&noise(7, 1 << 20)).unwrap();
        f.seek(SeekFrom::Start(64 << 20)).unwrap();
        f.write_all(&noise(8, 1 << 20)).unwrap();
    }
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC8001L8"]);
    ok(d, &["tape", "unload"]);
    let mnt = d.join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let log = d.join("mount.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("--sim")
        .arg(d.join("world"))
        .arg("--sim-only")
        .args(["mount", "--online", "--cache", "256MiB"])
        .arg(&mnt)
        .stderr(std::fs::File::create(&log).unwrap())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let inner = mnt.join(src.strip_prefix("/").unwrap());
    for _ in 0..200 {
        if inner.join("big.bin").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let checks = std::thread::scope(|sc| {
        let big = sc.spawn(|| std::fs::read(inner.join("big.bin")).ok() == Some(noise(1, 40 << 20)));
        let small = sc.spawn(|| (0..40u64).all(|i| std::fs::read(inner.join(format!("small/{i}.txt"))).ok() == Some(noise(100 + i, 3000 + i as usize))));
        let sparse = sc.spawn(|| std::fs::read(inner.join("sparse.img")).ok().map(|b| (b.len(), b[..1 << 20] == noise(7, 1 << 20)[..], b[2 << 20..(2 << 20) + 4096].iter().all(|&x| x == 0), b[64 << 20..] == noise(8, 1 << 20)[..])));
        (big.join().unwrap(), small.join().unwrap(), sparse.join().unwrap())
    });
    let _ = Command::new("fusermount3").arg("-u").arg(&mnt).status();
    let _ = child.wait();
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert!(checks.0, "big.bin differs\n{text}");
    assert!(checks.1, "small files differ\n{text}");
    assert_eq!(checks.2, Some(((65 << 20), true, true, true)), "sparse.img\n{text}");
    assert!(text.contains("from DC8001L8 in drive lto8-1"), "{text}");
    // The small files share a few packs: each read once, not once per file.
    assert!(text.matches("reading ").count() < 20, "{text}");
}

/// Wear: a drive asking for cleaning is cleaned with the library's cleaning cartridge; a
/// cartridge past its configured load limit is retired and no longer written.
#[test]
fn wear_check_cleans_and_retires() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let world = d.join("world");
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(&world)
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8002L8", "--tapes", "CLN001CU", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    assert!(ok(d, &["stats", "check"]).contains("drive lto8-1: OK"));
    let alert = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "tapealert"]).arg(&world).args(["--drive", "SIM8D0001", "--flag", "20"]).status().unwrap();
    assert!(alert.success());
    assert!(ok(d, &["stats", "check"]).contains("NEEDS CLEANING: the drive asks for cleaning now"));
    let c = ok(d, &["stats", "check", "--clean"]);
    assert!(c.contains("cleaning drive lto8-1 with CLN001CU") && c.contains("CLN001CU back in slot"), "{c}");
    assert!(ok(d, &["stats", "check"]).contains("drive lto8-1: OK"));
    assert!(ok(d, &["stats", "drive", "lto8-1"]).contains("cleanings            1"));

    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), noise(1, 1 << 20)).unwrap();
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC8001L8"]);
    let cfg = d.join("config.toml");
    std::fs::write(&cfg, "[wear]\ntape_max_loads = 1\n").unwrap();
    let cfg_s = cfg.to_str().unwrap();
    let r = ok(d, &["--config", cfg_s, "stats", "check", "--retire"]);
    assert!(r.contains("DC8001L8: RETIRE: 1 loads (limit 1)") && r.contains("1 cartridge(s) marked retiring"), "{r}");
    // Still read, no longer written: the next backup goes to DC8002L8.
    let b = ok(d, &["backup", s, "--tape", "DC8001L8..DC8002L8"]);
    assert!(b.contains("DC8002L8") && !b.contains("tape      DC8001L8"), "{b}");
    ok(d, &["verify", "DC8001L8"]);
}

/// A reader that stops part-way through a large file does not stall the mount: the tape side
/// gives the rest up and serves the next reads.
#[test]
fn online_mount_survives_abandoned_reads() {
    if !have_fuse() {
        eprintln!("skipping online mount: no FUSE");
        return;
    }
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8", "--capacity", &capacity(128)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("big.bin"), noise(1, 40 << 20)).unwrap();
    std::fs::write(src.join("other.bin"), noise(2, 6 << 20)).unwrap();
    let s = src.to_str().unwrap();
    ok(d, &["backup", s, "--tape", "DC8001L8"]);
    let mnt = d.join("mnt");
    std::fs::create_dir(&mnt).unwrap();
    let log = d.join("mount.log");
    let mut child = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("--sim")
        .arg(d.join("world"))
        .arg("--sim-only")
        .args(["mount", "--online", "--cache", "8MiB", "--timeout", "60s"])
        .arg(&mnt)
        .stderr(std::fs::File::create(&log).unwrap())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let inner = mnt.join(src.strip_prefix("/").unwrap());
    for _ in 0..200 {
        if inner.join("big.bin").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let head = {
        use std::io::Read;
        let mut f = std::fs::File::open(inner.join("big.bin")).unwrap();
        let mut b = vec![0u8; 1 << 20];
        f.read_exact(&mut b).unwrap();
        b
    };
    let started = std::time::Instant::now();
    let other = std::fs::read(inner.join("other.bin")).ok();
    let took = started.elapsed();
    let _ = Command::new("fusermount3").arg("-u").arg(&mnt).status();
    let _ = child.wait();
    let text = std::fs::read_to_string(&log).unwrap_or_default();
    assert_eq!(head, noise(1, 40 << 20)[..1 << 20], "{text}");
    assert!(other == Some(noise(2, 6 << 20)), "other.bin\n{text}");
    assert!(took < std::time::Duration::from_secs(40), "took {took:?}\n{text}");
    assert!(text.contains("readers stopped taking data"), "{text}");
}

/// The harness: jobs run with hooks, levels, retention, a catalog copy and notifications;
/// a failing job still runs its post hook and is reported; ltoarcd runs a job on request
/// and answers on its socket.
#[test]
fn jobs_and_daemon() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8003L8", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("a.bin"), noise(1, 2 << 20)).unwrap();
    let p = |x: &str| d.join(x).display().to_string();
    let jobs = format!(
        r#"
[daemon]
socket = "{sock}"
state = "{state}"
parallel = 1
catalog_copy = "{copy}"

[notify]
command = 'echo "$LTOARC_EVENT $LTOARC_JOB $LTOARC_TITLE" >> {notes}'
on = ["done", "failed", "request", "wear"]

[job.data]
sources = ["{src}"]
tape = "DC8001L8..DC8003L8"
level = "incremental"
full_every = "30d"
keep_fulls = 1
pre = ["echo pre {{job}} {{run}} prev={{prev_run}} >> {hooks}"]
post = ['echo "post $LTOARC_STATUS" >> {hooks}']

[job.dump]
tape = "DC8001L8..DC8003L8"
streams = {{ tarred = "tar -C {src} -cf - ." }}

[job.broken]
sources = ["/no/such/dir"]
tape = "DC8001L8..DC8003L8"
post = ['echo "post $LTOARC_STATUS" >> {hooks}']

[job.scrub]
kind = "scrub"
older_than = "0s"
per_run = 1
"#,
        sock = p("ltoarcd.sock"),
        state = p("state"),
        copy = p("catalog-copy.db"),
        notes = p("notes.log"),
        src = src.display(),
        hooks = p("hooks.log"),
    );
    std::fs::write(d.join("jobs.toml"), jobs).unwrap();
    let j = p("jobs.toml");
    let first = ok(d, &["--jobs", &j, "job", "run", "data"]);
    assert!(first.contains("(complete, full)") && first.contains("job       data ok"), "{first}");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    std::fs::write(src.join("b.txt"), b"new").unwrap();
    let second = ok(d, &["--jobs", &j, "job", "run", "data"]);
    assert!(second.contains("(complete, incremental after"), "{second}");
    let hooks = std::fs::read_to_string(d.join("hooks.log")).unwrap();
    let lines: Vec<&str> = hooks.lines().collect();
    assert_eq!(lines.len(), 4, "{hooks}");
    assert!(lines[0].starts_with("pre data 20") && lines[0].ends_with("prev="), "{hooks}");
    assert!(lines[2].contains(&format!("prev={}", lines[0].split_whitespace().nth(2).unwrap())), "the second run knows the first:\n{hooks}");
    assert_eq!((lines[1], lines[3]), ("post ok", "post ok"));
    assert!(d.join("catalog-copy.db").exists());
    assert!(ok(d, &["--jobs", &j, "job", "runs", "data"]).matches(" ok ").count() >= 2);
    // A failing job: post hook, record, notification.
    assert!(fails(d, &["--jobs", &j, "job", "run", "broken"]).contains("job broken failed"));
    assert!(std::fs::read_to_string(d.join("hooks.log")).unwrap().ends_with("post failed\n"));
    ok(d, &["--jobs", &j, "job", "run", "scrub"]);
    let notes = std::fs::read_to_string(d.join("notes.log")).unwrap();
    assert!(notes.contains("done data") && notes.contains("failed broken") && notes.contains("done scrub"), "{notes}");
    assert!(ok(d, &["--jobs", &j, "job", "list"]).contains("scrub"));

    // The daemon: a run on request, status, log, stop.
    let mut daemon = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("--sim")
        .arg(d.join("world"))
        .args(["--sim-only", "--jobs", &j, "daemon", "run"])
        .stdout(std::fs::File::create(d.join("daemon.log")).unwrap())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..100 {
        if d.join("ltoarcd.sock").exists() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(ok(d, &["--jobs", &j, "daemon", "status"]).contains("dump"));
    assert!(ok(d, &["--jobs", &j, "daemon", "trigger", "dump"]).contains("queued dump"));
    let mut done = false;
    for _ in 0..600 {
        if ok(d, &["--jobs", &j, "job", "runs", "dump"]).contains(" ok ") {
            done = true;
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let log = ok(d, &["--jobs", &j, "daemon", "log", "dump"]);
    assert!(ok(d, &["--jobs", &j, "daemon", "stop"]).contains("stopping"));
    let _ = daemon.wait();
    assert!(done, "dump never finished:\n{log}");
    assert!(log.contains("/.ltoarc/streams/dump/tarred"), "{log}");
    let tarred = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("--sim")
        .arg(d.join("world"))
        .args(["--sim-only", "cat", "/.ltoarc/streams/dump/tarred"])
        .output()
        .unwrap();
    assert!(tarred.status.success());
    assert!(tarred.stdout.windows(5).any(|w| w == b"a.bin"), "the archived tar holds a.bin");
}

/// Open ranges and the capacity plan: which cartridges of DC8001.. a session of known size
/// takes, or by how much they fall short; overwriting instead of appending, with the loss
/// spelled out and confirmed.
#[test]
fn open_ranges_plan_and_overwrite() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8005L8", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    for i in 1..=5 {
        ok(d, &["tape", "init", &format!("DC800{i}L8"), "--yes"]);
    }
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..4u64 {
        std::fs::write(src.join(format!("f{i}.bin")), noise(i, 20 << 20)).unwrap();
    }
    let s = src.to_str().unwrap();
    let plan = ok(d, &["backup", s, "--tape", "DC8002..", "--plan", "scan", "--dry-run"]);
    assert!(plan.contains("about 83.9 MB to write"), "{plan}");
    assert!(plan.contains("DC8002L8") && plan.contains("DC8003L8") && plan.contains("(empty)"), "{plan}");
    assert!(plan.contains("2 cartridge(s) of 4") && plan.contains("not needed: DC8004L8, DC8005L8"), "{plan}");
    assert!(ok(d, &["sessions"]).contains("no sessions"), "a dry run writes nothing");
    let short = ok(d, &["backup", s, "--tape", "DC8005..", "--plan", "scan", "--dry-run"]);
    assert!(short.contains("SHORT by") && short.contains("more cartridge(s) like DC8005L8"), "{short}");

    // Overwrite: refused without --yes when not at a terminal, after saying what goes.
    std::fs::write(d.join("small.txt"), b"first").unwrap();
    let small = d.join("small.txt");
    ok(d, &["backup", small.to_str().unwrap(), "--tape", "DC8001L8"]);
    let refused = fails(d, &["backup", s, "--tape", "DC8001L8..DC8003L8", "--plan", "scan", "--overwrite"]);
    assert!(refused.contains("DC8001L8: 1 session(s)") && refused.contains("1 held nowhere else") && refused.contains("add --yes"), "{refused}");
    assert!(refused.contains("WARNING   1 session(s) would be lost"), "{refused}");
    let o = ok(d, &["backup", s, "--tape", "DC8001L8..DC8003L8", "--plan", "scan", "--overwrite", "--yes"]);
    assert!(o.contains("DC8001L8: rewriting it from the start (forgot 1 session(s)"), "{o}");
    assert!(o.contains("(complete, full)"), "{o}");
    let list = ok(d, &["sessions"]);
    assert_eq!(list.lines().count(), 2, "only the new session is left:\n{list}");
    let gone = fails(d, &["restore", small.to_str().unwrap(), "--to", d.join("r").to_str().unwrap()]);
    assert!(gone.contains("nothing archived") || gone.contains("not in the catalog"), "{gone}");
    ok(d, &["verify", "DC8001L8"]);
}

/// --delete-source moves the files to tape: once the session is complete they are deleted
/// (then the directories left empty), and they restore from the archive.
#[test]
fn delete_source_after_a_complete_session() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let status = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .args(["sim", "create"])
        .arg(d.join("world"))
        .args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8002L8", "--capacity", &capacity(64)])
        .status()
        .unwrap();
    assert!(status.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("cam");
    std::fs::create_dir_all(src.join("day1/sub")).unwrap();
    std::fs::write(src.join("day1/a.mp4"), noise(1, 3 << 20)).unwrap();
    std::fs::write(src.join("day1/sub/b.mp4"), noise(2, 1 << 20)).unwrap();
    std::fs::write(src.join("c.txt"), b"small").unwrap();
    std::fs::write(src.join("keep.tmp"), b"excluded, so not archived").unwrap();
    let orig = d.join("orig");
    assert!(Command::new("cp").arg("-a").arg(&src).arg(&orig).status().unwrap().success());
    let s = src.to_str().unwrap();
    // Not with --map: the files read would be a snapshot's.
    assert!(fails(d, &["backup", s, "--tape", "DC8001L8", "--delete-source", "--map", &format!("{s}=/x")]).contains("snapshot"));
    let out = ok(d, &["backup", s, "--tape", "DC8001L8", "--exclude", "*.tmp", "--delete-source"]);
    assert!(out.contains("deleted   3 files (4.2 MB) and 2 empty directories from the sources"), "{out}");
    let left: Vec<String> = std::fs::read_dir(&src).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
    assert_eq!(left, vec!["keep.tmp".to_string()], "only the excluded file stays (and the source itself)");
    let dest = d.join("dest");
    ok(d, &["restore", s, "--to", dest.to_str().unwrap(), "--strip-prefix", d.to_str().unwrap()]);
    for f in ["day1/a.mp4", "day1/sub/b.mp4", "c.txt"] {
        assert_eq!(std::fs::read(dest.join("cam").join(f)).unwrap(), std::fs::read(orig.join(f)).unwrap(), "{f}");
    }
    // A session that ends partial deletes nothing.
    std::fs::write(src.join("big.bin"), noise(3, 90 << 20)).unwrap();
    let out = ok(d, &["backup", s, "--tape", "DC8002L8", "--delete-source"]);
    assert!(out.contains("deleted   nothing: the session is partial"), "{out}");
    assert!(src.join("big.bin").exists());
}

/// One HTTP/1.1 request to the web interface: (status, body).
fn http(addr: &str, method: &str, path: &str, token: Option<&str>, body: Option<&str>) -> (u16, String) {
    use std::io::{Read, Write};
    let mut s = std::net::TcpStream::connect(addr).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n");
    if let Some(t) = token {
        req += &format!("Authorization: Bearer {t}\r\n");
    }
    if let Some(b) = body {
        req += &format!("Content-Type: application/json\r\nX-Ltoarc: 1\r\nContent-Length: {}\r\n\r\n{b}", b.len());
    } else {
        req += "\r\n";
    }
    s.write_all(req.as_bytes()).unwrap();
    let mut out = String::new();
    s.read_to_string(&mut out).unwrap();
    let code = out.split_whitespace().nth(1).unwrap().parse().unwrap();
    (code, out.split_once("\r\n\r\n").map(|x| x.1.to_string()).unwrap_or_default())
}

/// ltoarcd's web interface: token required, POSTs need X-Ltoarc, and a restore started
/// from it runs as a task with its log.
#[test]
fn web_interface_runs_a_restore() {
    use std::io::BufRead;
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world")).args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8002L8", "--capacity", &capacity(64)]).status().unwrap();
    assert!(st.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("f.bin"), noise(5, 300_000)).unwrap();
    ok(d, &["backup", src.to_str().unwrap(), "--tape", "DC8001L8"]);
    let jobs = d.join("jobs.toml");
    std::fs::write(&jobs, format!("[daemon]\nsocket = \"{0}/d.sock\"\nstate = \"{0}/state\"\n", d.display())).unwrap();
    /// Stops the daemon however the test ends.
    struct Reap(std::process::Child);
    impl Drop for Reap {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let daemon = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--catalog")
        .arg(d.join("cat.db"))
        .arg("--sim")
        .arg(d.join("world"))
        .arg("--sim-only")
        .arg("--jobs")
        .arg(&jobs)
        .args(["daemon", "run", "--web", "127.0.0.1:0"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut daemon = Reap(daemon);
    let mut lines = std::io::BufReader::new(daemon.0.stdout.take().unwrap()).lines();
    let addr = loop {
        let l = lines.next().expect("daemon output").unwrap();
        if let Some(rest) = l.split("web interface on http://").nth(1) {
            break rest.split('/').next().unwrap().to_string();
        }
    };
    let token = std::fs::read_to_string(d.join("state/web.token")).unwrap();
    let t = Some(token.trim());
    assert_eq!(http(&addr, "GET", "/api/overview", None, None).0, 401);
    let (code, body) = http(&addr, "GET", "/api/overview", t, None);
    assert_eq!(code, 200);
    assert!(body.contains("\"lto8-1\"") && body.contains("DC8001L8"), "{body}");
    let (code, page) = http(&addr, "GET", "/", None, None);
    assert!(code == 200 && page.contains("app.js"));
    // A change needs the header a page on another site cannot send.
    let mut s = std::net::TcpStream::connect(&addr).unwrap();
    std::io::Write::write_all(&mut s, format!("POST /api/task HTTP/1.1\r\nHost: {addr}\r\nAuthorization: Bearer {}\r\nConnection: close\r\nContent-Length: 2\r\n\r\n{{}}", token.trim()).as_bytes()).unwrap();
    let mut out = String::new();
    std::io::Read::read_to_string(&mut s, &mut out).unwrap();
    assert!(out.starts_with("HTTP/1.1 403"), "{out}");
    let dest = d.join("dest");
    let req = format!(r#"{{"kind":"restore","paths":["{}"],"to":"{}","strip_prefix":"{}"}}"#, src.display(), dest.display(), d.display());
    let (code, body) = http(&addr, "POST", "/api/task", t, Some(&req));
    assert_eq!(code, 200, "{body}");
    let run: serde_json::Value = serde_json::from_str(&body).unwrap();
    let id = run["run"].as_i64().unwrap();
    let mut log = serde_json::Value::Null;
    for _ in 0..100 {
        let (_, b) = http(&addr, "GET", &format!("/api/log?run={id}"), t, None);
        log = serde_json::from_str(&b).unwrap();
        if log["running"] == false && log["run"]["status"] != "running" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(log["run"]["status"], "ok", "{log}");
    assert!(log["text"].as_str().unwrap().contains("restored 1 files"), "{log}");
    assert_eq!(std::fs::read(dest.join("src/f.bin")).unwrap(), noise(5, 300_000));
    // Copying chosen sessions: planned only here (one drive).
    let (_, sessions) = http(&addr, "GET", "/api/sessions", t, None);
    let sessions: serde_json::Value = serde_json::from_str(&sessions).unwrap();
    let uuid = sessions["sessions"][0]["uuid"].as_str().unwrap().to_string();
    let req = format!(r#"{{"kind":"copy","sessions":["{uuid}"],"to":"DC8002L8","dry_run":true}}"#);
    let (code, body) = http(&addr, "POST", "/api/task", t, Some(&req));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains(&format!("plan: copy session {}", &uuid[..8])), "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["run"].as_i64().unwrap();
    for _ in 0..100 {
        let (_, b) = http(&addr, "GET", &format!("/api/log?run={id}"), t, None);
        log = serde_json::from_str(&b).unwrap();
        if log["running"] == false && log["run"]["status"] != "running" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(log["run"]["status"], "ok", "{log}");
    assert!(log["text"].as_str().unwrap().contains("DC8002L8") && log["text"].as_str().unwrap().contains("to write"), "{log}");
    assert_eq!(http(&addr, "POST", "/api/task", t, Some(r#"{"kind":"copy","sessions":[],"to":"DC8002L8"}"#)).0, 400);
    // The library as it looks (the I/E slot at the bottom front of the left magazine), and a
    // cartridge moved in it.
    let (_, lib) = http(&addr, "GET", "/api/library", t, None);
    let lib: serde_json::Value = serde_json::from_str(&lib).unwrap();
    assert_eq!(lib["libraries"][0]["layout"]["left"][0][2][0], serde_json::json!({ "kind": "ie", "n": 1 }), "{lib}");
    assert_eq!(http(&addr, "POST", "/api/task", t, Some(r#"{"kind":"move","moves":[{"barcode":"DC8002L8","place":"drive"}]}"#)).0, 400);
    let (code, body) = http(&addr, "POST", "/api/task", t, Some(r#"{"kind":"move","moves":[{"barcode":"DC8002L8","place":"7"}]}"#));
    assert_eq!(code, 200, "{body}");
    assert!(body.contains("move DC8002L8 to slot 7"), "{body}");
    let id = serde_json::from_str::<serde_json::Value>(&body).unwrap()["run"].as_i64().unwrap();
    for _ in 0..100 {
        let (_, b) = http(&addr, "GET", &format!("/api/log?run={id}"), t, None);
        log = serde_json::from_str(&b).unwrap();
        if log["running"] == false && log["run"]["status"] != "running" {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    assert_eq!(log["run"]["status"], "ok", "{log}");
    assert!(ok(d, &["tape", "list"]).lines().any(|l| l.starts_with("DC8002L8") && l.contains("lib1 slot 7")));
    let (_, runs) = http(&addr, "GET", "/api/runs?job=@restore", t, None);
    assert!(runs.contains("restore ") && runs.contains("\"ok\""), "{runs}");
    // Asking the daemon opens no catalog (this one names none, and must not touch the user's).
    let stop = Command::new(env!("CARGO_BIN_EXE_ltoarc"))
        .arg("--jobs")
        .arg(&jobs)
        .args(["daemon", "stop"])
        .env("XDG_DATA_HOME", d.join("nowhere"))
        .output()
        .unwrap();
    assert!(stop.status.success(), "{}", String::from_utf8_lossy(&stop.stderr));
    assert!(!d.join("nowhere").exists(), "daemon stop opened a catalog");
    let rest: Vec<String> = lines.map_while(Result::ok).collect();
    assert!(rest.iter().any(|l| l.contains("ltoarcd: stopped")), "{rest:?}");
    assert!(daemon.0.wait().unwrap().success());
}

/// A session cut short (out of cartridges here; interrupted works alike) is continued with
/// --resume: what it holds complete is not written again, and everything restores.
#[test]
fn resume_continues_a_partial_session() {
    let tmp = tempfile::tempdir().unwrap();
    let d = tmp.path();
    let st = Command::new(env!("CARGO_BIN_EXE_ltoarc")).args(["sim", "create"]).arg(d.join("world")).args(["--drive", "LTO-8", "--tapes", "DC8001L8..DC8003L8", "--capacity", &capacity(64)]).status().unwrap();
    assert!(st.success());
    ok(d, &["hw", "scan"]);
    ok(d, &["tape", "register"]);
    let src = d.join("src");
    std::fs::create_dir_all(&src).unwrap();
    for i in 0..3u64 {
        std::fs::write(src.join(format!("f{i}.bin")), noise(10 + i, 30 << 20)).unwrap();
    }
    let s = src.to_str().unwrap();
    assert!(fails(d, &["backup", s, "--tape", "DC8001L8", "--resume"]).contains("nothing to resume"));
    let first = ok(d, &["backup", s, "--tape", "DC8001L8"]);
    assert!(first.contains("(partial, full)"), "{first}");
    let plan = ok(d, &["backup", s, "--tape", "DC8002L8..DC8003L8", "--plan", "scan", "--dry-run"]);
    assert!(plan.contains("ended partial") && plan.contains("--resume would save only what it lacks"), "{plan}");
    let plan = ok(d, &["backup", s, "--tape", "DC8002L8..DC8003L8", "--plan", "scan", "--dry-run", "--resume"]);
    assert!(plan.contains("unchanged 2 entries (31.5 MB) are as the sessions this one builds on hold them") && plan.contains("about 62.9 MB to write"), "{plan}");
    let second = ok(d, &["backup", s, "--tape", "DC8002L8..DC8003L8", "--resume"]);
    assert!(second.contains("(complete, incremental after") && second.contains("unchanged 2 entries"), "{second}");
    let dest = d.join("dest");
    ok(d, &["restore", s, "--to", dest.to_str().unwrap(), "--strip-prefix", d.to_str().unwrap()]);
    for i in 0..3u64 {
        assert_eq!(std::fs::read(dest.join(format!("src/f{i}.bin"))).unwrap(), noise(10 + i, 30 << 20));
    }
}
