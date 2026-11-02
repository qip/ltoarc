//! The web interface, served by ltoarcd (`web = "ADDR"` in [daemon], or `--web ADDR`).
//!
//! Pages (web/*, built into the binary) over a JSON API on the catalog and the scheduler:
//! drives and libraries, cartridges, sessions, the archive as of any time, jobs and their
//! runs with live logs. Actions (run a job, restore, verify, load and unload, moving
//! cartridges between slots, inventory, wear check, cleaning, an ad-hoc backup or its plan)
//! run as child `ltoarc` commands that
//! the daemon tracks, recorded as runs of the pseudo-job `@KIND` with their logs.
//!
//! Access needs the token in the state directory (`ltoarc daemon url` prints a link that
//! logs in with it): sent as a cookie (HttpOnly, SameSite=Strict) or as `Authorization:
//! Bearer TOKEN`. Requests that change anything are POSTs carrying `X-Ltoarc: 1`, which a
//! page on another site cannot send. Bind to loopback, or put a TLS proxy in front.

use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use chrono::Local;
use globset::Glob;
use ltoarc_catalog::{Catalog, DriveRow, LibraryRow, ObjectRow, SessionRow, StatsRow, View, VolumeRow, ROOT_PATH};
use serde_json::{json, Value};
use tiny_http::{Header, Method, Request, Response, Server};

use crate::daemon::{Shared, Task};
use crate::lease::{Leases, Priority};

pub struct Web {
    pub shared: Arc<Mutex<Shared>>,
    pub catalog: PathBuf,
    pub state: PathBuf,
    pub exe: PathBuf,
    pub child_args: Vec<OsString>,
    pub lock_dir: PathBuf,
    pub token: String,
}

const INDEX: &str = include_str!("../web/index.html");
const APP: &str = include_str!("../web/app.js");
const STYLE: &str = include_str!("../web/style.css");
const ICON: &str = include_str!("../web/icon.svg");

/// Entries a listing returns at most.
const LIST_MAX: usize = 5000;
/// Log bytes a request returns at most.
const LOG_CHUNK: u64 = 256 << 10;

/// Serve on `addr` with a few worker threads (each with its own catalog connection); returns
/// the address bound.
pub fn start(addr: &str, web: Web) -> Result<String> {
    let server = Arc::new(Server::http(addr).map_err(|e| anyhow!("web interface on {addr}: {e}"))?);
    let bound = server.server_addr().to_ip().map_or_else(|| addr.to_string(), |a| a.to_string());
    let web = Arc::new(web);
    for _ in 0..4 {
        let (server, web) = (server.clone(), web.clone());
        std::thread::spawn(move || {
            let cat = match Catalog::open(&web.catalog) {
                Ok(c) => c,
                Err(e) => return eprintln!("ltoarcd web: {e:#}"),
            };
            let w = Worker { leases: Leases::new(web.lock_dir.clone(), Priority::Interactive, "ltoarcd web"), web: &web, cat };
            while let Ok(rq) = server.recv() {
                w.handle(rq);
            }
        });
    }
    Ok(bound)
}

/// An error with the HTTP status to answer it with.
#[derive(Debug)]
struct Status(u16, String);

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.1)
    }
}

impl std::error::Error for Status {}

fn http(code: u16, msg: impl Into<String>) -> anyhow::Error {
    Status(code, msg.into()).into()
}

type Reply = Response<std::io::Cursor<Vec<u8>>>;

fn header(name: &str, value: &str) -> Header {
    Header::from_bytes(name.as_bytes(), value.as_bytes()).expect("valid header")
}

fn reply(code: u16, ctype: &str, body: Vec<u8>) -> Reply {
    Response::from_data(body).with_status_code(code).with_header(header("Content-Type", ctype))
}

fn json_reply(code: u16, v: &Value) -> Reply {
    reply(code, "application/json", serde_json::to_vec(v).unwrap_or_default()).with_header(header("Cache-Control", "no-store"))
}

fn error_reply(code: u16, msg: &str) -> Reply {
    json_reply(code, &json!({ "error": msg }))
}

fn request_header<'r>(rq: &'r Request, name: &str) -> Option<&'r str> {
    rq.headers().iter().find(|h| h.field.as_str().as_str().eq_ignore_ascii_case(name)).map(|h| h.value.as_str())
}

fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < b.len() => match u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap_or("zz"), 16) {
                Ok(v) => {
                    out.push(v);
                    i += 2;
                }
                Err(_) => out.push(b'%'),
            },
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn parse_query(q: &str) -> HashMap<String, String> {
    q.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode(k), decode(v))).collect()
}

fn arg<'q>(q: &'q HashMap<String, String>, k: &str) -> Result<&'q str> {
    q.get(k).map(String::as_str).filter(|v| !v.is_empty()).ok_or_else(|| http(400, format!("missing {k}")))
}

fn text(b: &Value, k: &str) -> Result<Option<String>> {
    match &b[k] {
        Value::Null => Ok(None),
        Value::String(s) if s.trim().is_empty() => Ok(None),
        Value::String(s) => Ok(Some(s.trim().to_string())),
        _ => Err(http(400, format!("{k}: expected text"))),
    }
}

fn flag(b: &Value, k: &str) -> bool {
    b[k].as_bool().unwrap_or(false)
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// "drwxr-xr-x" for an object.
fn mode_string(kind: &str, mode: i64) -> String {
    let t = match kind {
        "dir" => 'd',
        "symlink" => 'l',
        "fifo" => 'p',
        "char" => 'c',
        "block" => 'b',
        "socket" => 's',
        _ => '-',
    };
    let mut s = String::from(t);
    for shift in [6, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

fn object_json(o: &ObjectRow) -> Value {
    json!({
        "id": o.id, "session": o.session_id, "kind": o.kind, "size": o.size,
        "mtime": o.mtime_ns.div_euclid(1_000_000_000), "mode": mode_string(&o.kind, o.mode),
        "user": o.user.clone().unwrap_or_else(|| o.uid.to_string()), "group": o.grp.clone().unwrap_or_else(|| o.gid.to_string()),
        "status": o.status, "target": o.target.as_deref().map(lossy), "by_reference": o.data_from.is_some(),
    })
}

fn session_status(s: &SessionRow) -> &str {
    if s.expired_at.is_some() { "expired" } else { &s.status }
}

fn stats_json(rows: &[StatsRow], keys: &[&str], drives: &[DriveRow]) -> Value {
    rows.iter()
        .map(|r| {
            let mut data = serde_json::Map::new();
            for k in keys {
                if let Some(v) = r.data.get(*k) {
                    data.insert((*k).into(), v.clone());
                }
            }
            json!({
                "at": r.taken_at, "event": r.event,
                "drive": r.drive_id.and_then(|id| drives.iter().find(|d| d.id == id)).map(|d| d.name.clone()),
                "data": data,
            })
        })
        .collect()
}

const DRIVE_KEYS: &[&str] = &[
    "lifetime_volume_loads",
    "lifetime_cleanings",
    "lifetime_power_on_hours",
    "lifetime_head_hours",
    "head_hours_since_cleaning",
    "lifetime_meters",
    "hard_write_errors",
    "hard_read_errors",
    "tapealerts",
];
const TAPE_KEYS: &[&str] = &[
    "load_count",
    "mib_written_life",
    "mib_read_life",
    "bom_passes",
    "mot_passes",
    "recovered_write_errors",
    "unrecovered_write_errors",
    "recovered_read_errors",
    "unrecovered_read_errors",
    "volume_lifetime_remaining",
    "remaining_mib",
];

struct Worker<'w> {
    web: &'w Web,
    cat: Catalog,
    leases: Leases,
}

impl Worker<'_> {
    fn handle(&self, mut rq: Request) {
        let url = rq.url().to_string();
        let (path, query) = url.split_once('?').unwrap_or((&url, ""));
        let q = parse_query(query);
        let method = rq.method().clone();
        let resp = match (&method, path) {
            (Method::Get, "/") | (Method::Get, "/index.html") => reply(200, "text/html; charset=utf-8", INDEX.into()),
            (Method::Get, "/app.js") => reply(200, "text/javascript; charset=utf-8", APP.into()),
            (Method::Get, "/style.css") => reply(200, "text/css; charset=utf-8", STYLE.into()),
            (Method::Get, "/icon.svg") => reply(200, "image/svg+xml", ICON.into()),
            (Method::Post, "/api/login") => self.login(&mut rq),
            (Method::Post, "/api/logout") => {
                json_reply(200, &json!({})).with_header(header("Set-Cookie", "ltoarc=; HttpOnly; SameSite=Strict; Path=/; Max-Age=0"))
            }
            (m, p) if p.starts_with("/api/") => {
                if !self.authorized(&rq) {
                    error_reply(401, "log in first")
                } else if *m == Method::Post && request_header(&rq, "X-Ltoarc") != Some("1") {
                    error_reply(403, "requests that change something need the X-Ltoarc: 1 header")
                } else {
                    let body = if *m == Method::Post { read_json(&mut rq) } else { Ok(Value::Null) };
                    match body.and_then(|b| self.api(m, p, &q, b)) {
                        Ok(v) => json_reply(200, &v),
                        Err(e) => match e.downcast_ref::<Status>() {
                            Some(s) => error_reply(s.0, &s.1),
                            None => error_reply(400, &format!("{e:#}")),
                        },
                    }
                }
            }
            _ => error_reply(404, "not found"),
        };
        let resp = resp
            .with_header(header("X-Content-Type-Options", "nosniff"))
            .with_header(header("Referrer-Policy", "no-referrer"))
            .with_header(header(
                "Content-Security-Policy",
                "default-src 'self'; img-src 'self' data:; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
            ));
        let _ = rq.respond(resp);
    }

    fn authorized(&self, rq: &Request) -> bool {
        let bearer = request_header(rq, "Authorization").and_then(|v| v.strip_prefix("Bearer ")).map(str::trim);
        let cookie = request_header(rq, "Cookie")
            .and_then(|c| c.split(';').map(str::trim).find_map(|kv| kv.strip_prefix("ltoarc=")));
        bearer.or(cookie).is_some_and(|t| same(t, &self.web.token))
    }

    fn login(&self, rq: &mut Request) -> Reply {
        let token = read_json(rq).ok().and_then(|b| b["token"].as_str().map(str::to_string)).unwrap_or_default();
        if !same(token.trim(), &self.web.token) {
            return error_reply(401, "wrong token");
        }
        json_reply(200, &json!({}))
            .with_header(header("Set-Cookie", &format!("ltoarc={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000", self.web.token)))
    }

    fn api(&self, m: &Method, path: &str, q: &HashMap<String, String>, body: Value) -> Result<Value> {
        match (m, path) {
            (Method::Get, "/api/overview") => self.overview(),
            (Method::Get, "/api/library") => self.library(),
            (Method::Get, "/api/volumes") => self.volumes(),
            (Method::Get, "/api/volume") => self.volume(arg(q, "barcode")?),
            (Method::Get, "/api/sessions") => self.sessions(),
            (Method::Get, "/api/session") => self.session(arg(q, "id")?),
            (Method::Get, "/api/ls") => self.ls(q),
            (Method::Get, "/api/stat") => self.stat(q),
            (Method::Get, "/api/find") => self.find(q),
            (Method::Get, "/api/jobs") => self.jobs(),
            (Method::Get, "/api/runs") => self.runs(q),
            (Method::Get, "/api/log") => self.log(q),
            (Method::Get, "/api/drive") => self.drive(arg(q, "name")?),
            (Method::Post, "/api/run-job") => {
                let name = text(&body, "name")?.ok_or_else(|| http(400, "missing name"))?;
                crate::daemon::queue(&mut self.web.shared.lock().unwrap(), &name).map_err(|e| http(409, e.to_string()))?;
                Ok(json!({ "queued": name }))
            }
            (Method::Post, "/api/task") => self.task(&body),
            (Method::Post, "/api/stop") => self.stop(&body),
            _ => Err(http(404, "no such call")),
        }
    }

    // --- hardware and cartridges ----------------------------------------------------------

    fn drive_json(&self, d: &DriveRow, libs: &[LibraryRow], vols: &[VolumeRow]) -> Result<Value> {
        let lib = d.library_id.and_then(|id| libs.iter().find(|l| l.id == id));
        let loaded = vols.iter().find(|v| v.library_id.is_some() && v.library_id == d.library_id && v.element.is_some() && v.element == d.element);
        let last = self.cat.drive_stats(d.id, 1)?.into_iter().next();
        let alerts: Vec<i64> = last
            .as_ref()
            .and_then(|r| r.data.get("tapealerts"))
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(|x| x["flag"].as_i64()).collect())
            .unwrap_or_default();
        let waiting: Vec<Value> =
            self.leases.waiting_for(d).into_iter().map(|(pid, what, since)| json!({ "pid": pid, "what": what, "since": since as i64 })).collect();
        Ok(json!({
            "name": d.name, "serial": d.serial, "vendor": d.vendor, "product": d.product, "revision": d.revision,
            "generation": d.generation, "state": d.state, "backend": d.backend,
            "library": lib.map(|l| l.name.clone()),
            "bay": lib.zip(d.element).map(|(l, e)| l.element_name(e)),
            "loaded": loaded.map(|v| v.barcode.clone()),
            "holder": (d.state == "attached").then(|| self.leases.holder(d)).flatten(),
            "waiting": waiting,
            "stats": stats_json(last.as_slice(), DRIVE_KEYS, &[]).get(0).cloned(),
            "needs_cleaning": alerts.contains(&20) || alerts.contains(&21),
        }))
    }

    fn volume_json(&self, v: &VolumeRow, libs: &[LibraryRow], sessions: &[SessionRow]) -> Result<Value> {
        let lib = v.library_id.and_then(|id| libs.iter().find(|l| l.id == id));
        let parts = self.cat.volume_sessions(v.id)?;
        let live: Vec<&SessionRow> =
            sessions.iter().filter(|s| s.expired_at.is_none() && parts.iter().any(|p| p.session_id == s.id)).collect();
        let room = crate::fit::room(&self.cat, v, false)?;
        let cap = crate::fit::capacity(v);
        let last = self.cat.volume_stats(v.id, 1)?.into_iter().next();
        Ok(json!({
            "barcode": v.barcode, "generation": v.generation, "kind": v.kind, "state": v.state, "pool": v.pool,
            "labelled": v.labelled, "uuid": v.uuid,
            "library": lib.map(|l| l.name.clone()),
            "place": lib.zip(v.element).map(|(l, e)| l.element_name(e)),
            "capacity": cap,
            "room": room.as_ref().map(|r| r.bytes), "room_how": room.as_ref().map(|r| r.how),
            "sessions": live.len(),
            "last_written": live.iter().filter_map(|s| s.ended).max(),
            "verified_at": parts.iter().filter_map(|p| p.verified_at).max(),
            "life_percent": last.as_ref().and_then(|r| r.data.get("volume_lifetime_remaining")).cloned(),
            "loads": last.as_ref().and_then(|r| r.data.get("load_count")).cloned(),
            "writable": crate::ctx::writable(v),
        }))
    }

    fn overview(&self) -> Result<Value> {
        let libs = self.cat.libraries()?;
        let vols = self.cat.volumes()?;
        let drives: Vec<Value> = self.cat.drives()?.iter().map(|d| self.drive_json(d, &libs, &vols)).collect::<Result<_>>()?;
        let mut states: BTreeMap<String, usize> = BTreeMap::new();
        for v in &vols {
            *states.entry(if v.kind == "data" { v.state.clone() } else { v.kind.clone() }).or_default() += 1;
        }
        let sessions = self.cat.sessions()?;
        let live: Vec<&SessionRow> = sessions.iter().filter(|s| s.expired_at.is_none()).collect();
        let recent: Vec<Value> = sessions.iter().rev().take(6).map(|s| self.session_brief(s, &vols)).collect::<Result<_>>()?;
        let events: Vec<Value> = self
            .cat
            .events(None, 15)?
            .into_iter()
            .map(|(at, kind, subject, message)| json!({ "at": at, "kind": kind, "subject": subject, "message": message }))
            .collect();
        let sh = self.web.shared.lock().unwrap();
        let next = sh.next.iter().min_by_key(|(_, t)| **t).map(|(n, t)| json!({ "job": n, "at": t.timestamp() }));
        let daemon = json!({ "jobs": sh.jobs.job.len(), "queued": sh.queue.len(), "stopping": sh.stopping, "parallel": sh.parallel, "next": next });
        drop(sh);
        Ok(json!({
            "host": std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default().trim(),
            "version": env!("CARGO_PKG_VERSION"),
            "now": Local::now().timestamp(),
            "daemon": daemon,
            "libraries": libs.iter().map(|l| json!({ "name": l.name, "vendor": l.vendor, "product": l.product, "slots": l.slots, "ie_slots": l.ie_slots, "bays": l.bays,
                "cartridges": vols.iter().filter(|v| v.library_id == Some(l.id) && v.element.is_some()).count() })).collect::<Vec<_>>(),
            "drives": drives,
            "cartridges": { "count": vols.len(), "states": states },
            "sessions": { "count": live.len(), "files": live.iter().map(|s| s.files).sum::<i64>(), "bytes": live.iter().map(|s| s.bytes).sum::<i64>(),
                "stored": live.iter().map(|s| s.stored_bytes).sum::<i64>() },
            "running": self.running()?,
            "runs": self.cat.job_runs_of(None, 10)?.iter().map(run_json).collect::<Vec<_>>(),
            "recent_sessions": recent,
            "events": events,
        }))
    }

    fn library(&self) -> Result<Value> {
        let libs = self.cat.libraries()?;
        let vols = self.cat.volumes()?;
        let drives = self.cat.drives()?;
        let sessions = self.cat.sessions()?;
        let mut out = Vec::new();
        for l in &libs {
            let mut elements = Vec::new();
            let mut add = |kind: &str, base: Option<i64>, count: i64| -> Result<()> {
                let Some(base) = base else { return Ok(()) };
                for n in 1..=count {
                    let addr = base + n - 1;
                    let v = vols.iter().find(|v| v.library_id == Some(l.id) && v.element == Some(addr));
                    let d = drives.iter().find(|d| d.library_id == Some(l.id) && d.element == Some(addr));
                    elements.push(json!({
                        "kind": kind, "n": n, "name": l.element_name(addr),
                        "volume": v.map(|v| self.volume_json(v, &libs, &sessions)).transpose()?,
                        "drive": d.map(|d| d.name.clone()), "attached": d.map(|d| d.state == "attached"),
                    }));
                }
                Ok(())
            };
            add("drive", l.bay_base, l.bays)?;
            add("ie", l.ie_base, l.ie_slots)?;
            add("slot", l.slot_base, l.slots)?;
            out.push(json!({
                "name": l.name, "vendor": l.vendor, "product": l.product, "serial": l.serial, "backend": l.backend, "last_seen": l.last_seen,
                "elements": elements, "layout": magazines(l.slots, l.ie_slots),
            }));
        }
        let outside: Vec<Value> =
            vols.iter().filter(|v| v.library_id.is_none() || v.element.is_none()).map(|v| self.volume_json(v, &libs, &sessions)).collect::<Result<_>>()?;
        Ok(json!({ "libraries": out, "outside": outside }))
    }

    fn volumes(&self) -> Result<Value> {
        let libs = self.cat.libraries()?;
        let sessions = self.cat.sessions()?;
        let vols: Vec<Value> = self.cat.volumes()?.iter().map(|v| self.volume_json(v, &libs, &sessions)).collect::<Result<_>>()?;
        Ok(json!({ "volumes": vols }))
    }

    fn volume(&self, barcode: &str) -> Result<Value> {
        let v = self.cat.volume(barcode)?.ok_or_else(|| http(404, format!("no cartridge {barcode}")))?;
        let libs = self.cat.libraries()?;
        let sessions = self.cat.sessions()?;
        let drives = self.cat.drives()?;
        let mut j = self.volume_json(&v, &libs, &sessions)?;
        let parts: Vec<Value> = self
            .cat
            .volume_sessions(v.id)?
            .iter()
            .map(|p| {
                let s = sessions.iter().find(|s| s.id == p.session_id);
                json!({
                    "session": s.map(|s| s.uuid[..8].to_string()), "job": s.and_then(|s| s.job.clone()), "level": s.map(|s| s.level.clone()),
                    "started": s.map(|s| s.started), "status": s.map(|s| session_status(s).to_string()),
                    "copy": p.copy, "seq": p.seq, "start": p.start_object, "end": p.end_object, "catalog": p.catalog_object, "verified_at": p.verified_at,
                })
            })
            .collect();
        j["parts"] = parts.into();
        j["stats"] = stats_json(&self.cat.volume_stats(v.id, 60)?, TAPE_KEYS, &drives);
        j["events"] = self
            .cat
            .events(Some(barcode), 30)?
            .into_iter()
            .map(|(at, kind, _, message)| json!({ "at": at, "kind": kind, "message": message }))
            .collect::<Vec<_>>()
            .into();
        j["drives"] = drives.iter().filter(|d| d.state == "attached").map(|d| d.name.clone()).collect::<Vec<_>>().into();
        Ok(j)
    }

    fn drive(&self, name: &str) -> Result<Value> {
        let d = self.cat.drive(name)?.ok_or_else(|| http(404, format!("no drive {name}")))?;
        let mut j = self.drive_json(&d, &self.cat.libraries()?, &self.cat.volumes()?)?;
        j["history"] = stats_json(&self.cat.drive_stats(d.id, 60)?, DRIVE_KEYS, &[]);
        Ok(j)
    }

    // --- sessions ------------------------------------------------------------------------

    fn session_brief(&self, s: &SessionRow, vols: &[VolumeRow]) -> Result<Value> {
        let parts = self.cat.session_volumes(s.id)?;
        let tapes: Vec<Value> = parts
            .iter()
            .map(|p| json!({ "barcode": vols.iter().find(|v| v.id == p.volume_id).map(|v| v.barcode.clone()), "copy": p.copy }))
            .collect();
        let mut copies: Vec<i64> = parts.iter().map(|p| p.copy).collect();
        copies.sort();
        copies.dedup();
        let stats: Value = s.stats.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
        // A partial session may be covered by a later run; what only it holds matters when
        // choosing what to copy.
        let only_here = if s.status == "partial" && s.expired_at.is_none() { Some(self.cat.uncovered_entries(s.id)?) } else { None };
        Ok(json!({
            "id": s.id, "uuid": s.uuid, "short": &s.uuid[..8.min(s.uuid.len())], "job": s.job, "level": s.level, "started": s.started, "ended": s.ended,
            "status": session_status(s), "files": s.files, "bytes": s.bytes, "stored": s.stored_bytes, "tapes": tapes, "copies": copies,
            "encrypted": s.encryption.is_some(), "writers": stats.get("writers").cloned(), "only_here": only_here,
        }))
    }

    fn sessions(&self) -> Result<Value> {
        let vols = self.cat.volumes()?;
        let list: Vec<Value> = self.cat.sessions()?.iter().rev().map(|s| self.session_brief(s, &vols)).collect::<Result<_>>()?;
        let drives = self.cat.drives()?.iter().filter(|d| d.state == "attached").count();
        Ok(json!({ "sessions": list, "attached_drives": drives }))
    }

    fn session(&self, key: &str) -> Result<Value> {
        let s = self.cat.session(key)?.ok_or_else(|| http(404, format!("no session {key}")))?;
        let vols = self.cat.volumes()?;
        let all = self.cat.sessions()?;
        let mut j = self.session_brief(&s, &vols)?;
        j["host"] = s.host.clone().into();
        j["sources"] = serde_json::from_str::<Value>(&s.sources).unwrap_or(Value::Null);
        j["stats"] = s.stats.as_deref().and_then(|t| serde_json::from_str(t).ok()).unwrap_or(Value::Null);
        j["parent"] = s.parent_id.and_then(|p| all.iter().find(|x| x.id == p)).map(|p| p.uuid[..8].to_string()).into();
        j["children"] = all.iter().filter(|x| x.parent_id == Some(s.id)).map(|x| x.uuid[..8].to_string()).collect::<Vec<_>>().into();
        j["expired_at"] = s.expired_at.into();
        j["parts"] = self
            .cat
            .session_volumes(s.id)?
            .iter()
            .map(|p| {
                json!({
                    "barcode": vols.iter().find(|v| v.id == p.volume_id).map(|v| v.barcode.clone()),
                    "copy": p.copy, "seq": p.seq, "start": p.start_object, "end": p.end_object, "catalog": p.catalog_object, "verified_at": p.verified_at,
                })
            })
            .collect::<Vec<_>>()
            .into();
        Ok(j)
    }

    // --- the archive ---------------------------------------------------------------------

    fn view(&self, q: &HashMap<String, String>) -> Result<View> {
        let as_of = q.get("as_of").filter(|s| !s.is_empty()).map(|s| crate::archive::parse_time(s)).transpose()?;
        let session = match q.get("session").filter(|s| !s.is_empty()) {
            Some(s) => Some(self.cat.session(s)?.ok_or_else(|| http(404, format!("no session {s}")))?.id),
            None => None,
        };
        Ok(View { as_of, session })
    }

    fn path_id(&self, path: &str) -> Result<i64> {
        if path == "/" {
            return Ok(ROOT_PATH);
        }
        self.cat.lookup_path(path.trim_end_matches('/').as_bytes())?.ok_or_else(|| http(404, format!("{path} is not in the catalog")))
    }

    fn ls(&self, q: &HashMap<String, String>) -> Result<Value> {
        let path = q.get("path").map(String::as_str).filter(|p| p.starts_with('/')).unwrap_or("/");
        let view = self.view(q)?;
        let id = self.path_id(path)?;
        let mut entries: Vec<(Vec<u8>, Option<ObjectRow>)> = self.cat.entries(id, view)?.into_iter().map(|(_, n, o)| (n, o)).collect();
        entries.sort_by(|a, b| {
            let dir = |o: &Option<ObjectRow>| o.as_ref().is_none_or(|o| o.kind == "dir");
            dir(&b.1).cmp(&dir(&a.1)).then_with(|| a.0.cmp(&b.0))
        });
        let total = entries.len();
        let base = path.trim_end_matches('/');
        let list: Vec<Value> = entries
            .iter()
            .take(LIST_MAX)
            .map(|(name, o)| {
                let name = lossy(name);
                let full = format!("{base}/{name}");
                match o {
                    Some(o) => {
                        let mut j = object_json(o);
                        j["name"] = name.into();
                        j["path"] = full.into();
                        j
                    }
                    None => json!({ "name": name, "path": full, "kind": "dir", "implied": true }),
                }
            })
            .collect();
        let me = if id == ROOT_PATH { None } else { self.cat.latest(id, view)? };
        Ok(json!({ "path": path, "entries": list, "total": total, "self": me.as_ref().map(object_json) }))
    }

    fn stat(&self, q: &HashMap<String, String>) -> Result<Value> {
        let path = arg(q, "path")?;
        let view = self.view(q)?;
        let id = self.path_id(path)?;
        let sessions = self.cat.sessions()?;
        let versions = self.cat.path_versions(id)?;
        let current = match q.get("object").and_then(|o| o.parse::<i64>().ok()) {
            Some(oid) => versions.iter().find(|o| o.id == oid).cloned(),
            None => self.cat.latest(id, view)?,
        };
        let detail = match &current {
            Some(o) => {
                let data = o.data_from.unwrap_or(o.id);
                let hashes: Vec<Value> =
                    self.cat.hashes(o.id)?.iter().map(|h| json!({ "profile": h.profile, "kind": h.kind, "algo": h.algo, "digest": h.digest })).collect();
                let locations: Vec<Value> = self
                    .cat
                    .object_locations(o.id)?
                    .into_iter()
                    .map(|(copy, barcode, part, block)| json!({ "copy": copy, "barcode": barcode, "partition": part, "block": block }))
                    .collect();
                let xattrs: Vec<Value> = o
                    .xattrs
                    .as_deref()
                    .and_then(|x| ltoarc_format::record::from_cbor::<Vec<(serde_bytes::ByteBuf, serde_bytes::ByteBuf)>>(x).ok())
                    .unwrap_or_default()
                    .iter()
                    .map(|(n, v)| json!({ "name": lossy(n), "value": lossy(v) }))
                    .collect();
                let from = if data != o.id {
                    let src = self.cat.object(data)?;
                    src.map(|s| -> Result<Value> {
                        let p = self.cat.full_paths([s.path_id])?.remove(&s.path_id).map(|p| lossy(&p));
                        let sess = sessions.iter().find(|x| x.id == s.session_id).map(|x| x.uuid[..8].to_string());
                        Ok(json!({ "path": p, "session": sess }))
                    })
                    .transpose()?
                } else {
                    None
                };
                let mut j = object_json(o);
                j["hashes"] = hashes.into();
                j["locations"] = locations.into();
                j["xattrs"] = xattrs.into();
                j["data_from"] = from.into();
                j["chunks"] = self.cat.chunk_placements(data)?.iter().filter(|c| c.placement.copy == 1).count().into();
                Some(j)
            }
            None => None,
        };
        let versions: Vec<Value> = versions
            .iter()
            .map(|o| {
                let s = sessions.iter().find(|s| s.id == o.session_id);
                let mut j = object_json(o);
                j["session_short"] = s.map(|s| s.uuid[..8].to_string()).into();
                j["started"] = s.map(|s| s.started).into();
                j["session_status"] = s.map(|s| session_status(s).to_string()).into();
                j["job"] = s.and_then(|s| s.job.clone()).into();
                j
            })
            .collect();
        Ok(json!({ "path": path, "current": detail, "versions": versions }))
    }

    fn find(&self, q: &HashMap<String, String>) -> Result<Value> {
        let pattern = arg(q, "pattern")?;
        let view = self.view(q)?;
        let root = match q.get("under").filter(|u| u.starts_with('/') && u.as_str() != "/") {
            Some(u) => self.path_id(u)?,
            None => ROOT_PATH,
        };
        let pat = if pattern.contains('/') { pattern.to_string() } else { format!("**/{pattern}") };
        let m = Glob::new(&pat).map_err(|e| http(400, e.to_string()))?.compile_matcher();
        let mut out = Vec::new();
        let mut total = 0;
        for (path, o) in self.cat.resolve(root, view)? {
            let p = lossy(&path);
            if m.is_match(&p) {
                total += 1;
                if out.len() < LIST_MAX {
                    let mut j = object_json(&o);
                    j["path"] = p.into();
                    out.push(j);
                }
            }
        }
        Ok(json!({ "matches": out, "total": total }))
    }

    // --- jobs, runs, tasks ---------------------------------------------------------------

    fn running(&self) -> Result<Vec<Value>> {
        let sh = self.web.shared.lock().unwrap();
        let mut out = Vec::new();
        for (name, r) in &sh.running {
            let run = self.cat.job_runs(name, 1)?.into_iter().next().filter(|x| x.log.as_deref() == Some(&*r.log.to_string_lossy()));
            out.push(json!({ "kind": "job", "job": name, "title": format!("job {name}"), "since": r.since.timestamp(), "run": run.map(|x| x.id),
                "progress": progress(&r.log) }));
        }
        for t in &sh.tasks {
            out.push(json!({ "kind": "task", "job": format!("@{}", t.kind), "title": t.title, "since": t.since.timestamp(), "run": t.run_id,
                "progress": progress(&t.log) }));
        }
        for name in &sh.queue {
            out.push(json!({ "kind": "queued", "job": name, "title": format!("job {name} (queued)") }));
        }
        Ok(out)
    }

    fn jobs(&self) -> Result<Value> {
        let sh = self.web.shared.lock().unwrap();
        let mut out = Vec::new();
        for (name, j) in &sh.jobs.job {
            let state = match sh.running.get(name) {
                Some(r) => json!({ "running": r.since.timestamp() }),
                None if sh.queue.contains(name) => json!("queued"),
                None => json!("idle"),
            };
            let last = self.cat.job_runs(name, 1)?.into_iter().next();
            out.push(json!({
                "name": name, "kind": j.kind, "schedule": j.schedule, "next": sh.next.get(name).map(|t| t.timestamp()),
                "state": state, "last": last.as_ref().map(run_json), "config": serde_json::to_value(j)?,
            }));
        }
        drop(sh);
        Ok(json!({ "jobs": out, "running": self.running()? }))
    }

    fn runs(&self, q: &HashMap<String, String>) -> Result<Value> {
        let limit = q.get("limit").and_then(|l| l.parse().ok()).unwrap_or(100).min(1000);
        let runs = self.cat.job_runs_of(q.get("job").map(String::as_str).filter(|j| !j.is_empty()), limit)?;
        Ok(json!({ "runs": runs.iter().map(run_json).collect::<Vec<_>>() }))
    }

    /// A run's log from byte `from` on (the last LOG_CHUNK bytes when it starts at 0 and the
    /// log is longer). Only logs under the state directory are served.
    fn log(&self, q: &HashMap<String, String>) -> Result<Value> {
        let id: i64 = arg(q, "run")?.parse().map_err(|_| http(400, "run: expected a number"))?;
        let run = self.cat.job_run(id)?.ok_or_else(|| http(404, format!("no run {id}")))?;
        let path = PathBuf::from(run.log.clone().ok_or_else(|| http(404, "that run kept no log"))?);
        let logs = std::fs::canonicalize(self.web.state.join("logs"))?;
        let real = std::fs::canonicalize(&path).map_err(|e| http(404, format!("{}: {e}", path.display())))?;
        if !real.starts_with(&logs) {
            return Err(http(403, "that log is not in the daemon's state directory"));
        }
        let mut f = std::fs::File::open(&real)?;
        let size = f.metadata()?.len();
        let mut from: u64 = q.get("from").and_then(|f| f.parse().ok()).unwrap_or(0).min(size);
        let skipped = from == 0 && size > LOG_CHUNK;
        if skipped {
            from = size - LOG_CHUNK;
        }
        f.seek(SeekFrom::Start(from))?;
        let mut buf = Vec::new();
        f.take(LOG_CHUNK).read_to_end(&mut buf)?;
        let running = self.web.shared.lock().unwrap().tasks.iter().any(|t| t.run_id == id) || (run.ended.is_none() && run.status == "running");
        Ok(json!({
            "run": run_json(&run), "text": lossy(&buf), "from": from, "offset": from + buf.len() as u64, "size": size,
            "skipped": skipped, "running": running,
        }))
    }

    /// Start a task: a child `ltoarc` command chosen from a fixed set, with checked values
    /// (option values go as --opt=VALUE, positional ones after --).
    fn task(&self, b: &Value) -> Result<Value> {
        let kind = text(b, "kind")?.ok_or_else(|| http(400, "missing kind"))?;
        let need = |k: &str| -> Result<String> { text(b, k)?.ok_or_else(|| http(400, format!("missing {k}"))) };
        let opt = |args: &mut Vec<OsString>, name: &str, k: &str| -> Result<()> {
            if let Some(v) = text(b, k)? {
                args.push(format!("--{name}={v}").into());
            }
            Ok(())
        };
        let drive = |k: &str| -> Result<Option<String>> {
            match text(b, k)? {
                Some(d) => Ok(Some(self.cat.drive(&d)?.ok_or_else(|| http(400, format!("no drive {d}")))?.name)),
                None => Ok(None),
            }
        };
        let paths = |k: &str| -> Result<Vec<String>> {
            let list: Vec<String> = b[k].as_array().map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect()).unwrap_or_default();
            if list.is_empty() || list.iter().any(|p| !p.starts_with('/')) {
                return Err(http(400, format!("{k}: expected absolute paths")));
            }
            Ok(list)
        };
        let mut args: Vec<OsString> = Vec::new();
        let title = match kind.as_str() {
            "restore" => {
                let ps = paths("paths")?;
                let to = need("to")?;
                if !to.starts_with('/') {
                    return Err(http(400, "to: expected an absolute path"));
                }
                args.extend(["restore".into(), format!("--to={to}").into()]);
                opt(&mut args, "strip-prefix", "strip_prefix")?;
                opt(&mut args, "as-of", "as_of")?;
                opt(&mut args, "session", "session")?;
                if let Some(c) = b["copy"].as_i64() {
                    args.push(format!("--copy={c}").into());
                }
                if flag(b, "overwrite") {
                    args.push("--overwrite".into());
                }
                if flag(b, "dry_run") {
                    args.push("--dry-run".into());
                }
                args.push("--".into());
                args.extend(ps.iter().map(OsString::from));
                let what = if ps.len() == 1 { ps[0].clone() } else { format!("{} paths", ps.len()) };
                format!("{}restore {what} to {to}", if flag(b, "dry_run") { "plan: " } else { "" })
            }
            "verify" => {
                let bc = need("barcode")?;
                let v = self.cat.volume(&bc)?.ok_or_else(|| http(400, format!("no cartridge {bc}")))?;
                args.push("verify".into());
                if let Some(d) = drive("drive")? {
                    args.push(format!("--drive={d}").into());
                }
                args.extend(["--".into(), v.barcode.clone().into()]);
                format!("verify {}", v.barcode)
            }
            "backup" => {
                let ps = paths("sources")?;
                let tape = need("tape")?;
                args.extend(["backup".into(), format!("--tape={tape}").into()]);
                opt(&mut args, "tape2", "tape2")?;
                opt(&mut args, "job", "job")?;
                opt(&mut args, "level", "level")?;
                let dry = flag(b, "dry_run");
                if dry {
                    args.extend(["--dry-run".into(), "--plan=scan".into()]);
                }
                if flag(b, "overwrite") {
                    args.extend(["--overwrite".into(), "--yes".into()]);
                }
                if flag(b, "resume") {
                    args.push("--resume".into());
                }
                if flag(b, "delete_source") && !dry {
                    args.push("--delete-source".into());
                }
                args.push("--".into());
                args.extend(ps.iter().map(OsString::from));
                format!("{}back up {} to {tape}", if dry { "plan: " } else { "" }, ps.join(", "))
            }
            "copy" => {
                // Another copy of the chosen sessions: from the cartridges holding them (or
                // the source disks) to the cartridges `to`.
                let keys: Vec<String> = b["sessions"].as_array().map(|a| a.iter().filter_map(|k| k.as_str().map(str::to_string)).collect()).unwrap_or_default();
                if keys.is_empty() {
                    return Err(http(400, "choose the sessions to copy"));
                }
                let mut rows = Vec::new();
                for k in &keys {
                    rows.push(self.cat.session(k)?.ok_or_else(|| http(400, format!("no session {k}")))?);
                }
                let to = need("to")?;
                args.extend(["copy".into(), format!("--to={to}").into()]);
                let from_source = flag(b, "from_source");
                if from_source {
                    args.push("--from-source".into());
                } else {
                    let mut from: Vec<String> = Vec::new();
                    let vols = self.cat.volumes()?;
                    for s in &rows {
                        for sv in self.cat.session_volumes(s.id)? {
                            if let Some(v) = vols.iter().find(|v| v.id == sv.volume_id) {
                                if !from.contains(&v.barcode) {
                                    from.push(v.barcode.clone());
                                }
                            }
                        }
                    }
                    if from.is_empty() {
                        return Err(http(400, "the chosen sessions are on no cartridge"));
                    }
                    args.push(format!("--from={}", from.join(",")).into());
                }
                for s in &rows {
                    args.push(format!("--session={}", s.uuid).into());
                }
                opt(&mut args, "pool", "pool")?;
                let dry = flag(b, "dry_run");
                if dry {
                    args.push("--dry-run".into());
                }
                if flag(b, "overwrite") {
                    args.extend(["--overwrite".into(), "--yes".into()]);
                }
                let what = match rows.as_slice() {
                    [one] => format!("session {}", &one.uuid[..8]),
                    many => format!("{} sessions", many.len()),
                };
                format!("{}copy {what} to {to}{}", if dry { "plan: " } else { "" }, if from_source { " from the source disks" } else { "" })
            }
            "inventory" => {
                args.extend(["tape".into(), "register".into()]);
                "read the library inventories".into()
            }
            "check" => {
                args.extend(["stats".into(), "check".into()]);
                if flag(b, "retire") {
                    args.push("--retire".into());
                }
                if flag(b, "clean") {
                    args.push("--clean".into());
                }
                "check wear".into()
            }
            "load" => {
                let bc = need("barcode")?;
                let v = self.cat.volume(&bc)?.ok_or_else(|| http(400, format!("no cartridge {bc}")))?;
                args.extend(["tape".into(), "load".into()]);
                let d = drive("drive")?;
                if let Some(d) = &d {
                    args.push(format!("--drive={d}").into());
                }
                args.extend(["--".into(), v.barcode.clone().into()]);
                format!("load {}{}", v.barcode, d.map(|d| format!(" into {d}")).unwrap_or_default())
            }
            "unload" => {
                let d = drive("drive")?.ok_or_else(|| http(400, "missing drive"))?;
                args.extend(["tape".into(), "unload".into(), format!("--drive={d}").into()]);
                match b["slot"].as_i64() {
                    Some(n) => {
                        args.push(format!("--slot={n}").into());
                        format!("unload {d} to slot {n}")
                    }
                    None => format!("unload {d}"),
                }
            }
            "move" => {
                // Cartridges to other slots: [{barcode, place}], place as `tape move` takes it.
                let list = b["moves"].as_array().filter(|a| !a.is_empty()).ok_or_else(|| http(400, "missing moves"))?;
                let mut moves = Vec::new();
                for m in list {
                    let bc = text(m, "barcode")?.ok_or_else(|| http(400, "missing barcode"))?;
                    let v = self.cat.volume(&bc)?.ok_or_else(|| http(400, format!("no cartridge {bc}")))?;
                    let place = text(m, "place")?.ok_or_else(|| http(400, "missing place"))?;
                    let p: crate::arrange::Place = place.parse().map_err(|e: anyhow::Error| http(400, e.to_string()))?;
                    moves.push((v.barcode, p));
                }
                args.extend(["tape".into(), "move".into()]);
                if flag(b, "swap") {
                    args.push("--swap".into());
                }
                args.push("--".into());
                args.extend(moves.iter().map(|(bc, p)| OsString::from(format!("{bc}={}", p.arg()))));
                match moves.as_slice() {
                    [(bc, p)] => format!("move {bc} to {p}"),
                    many => format!("move {} cartridges", many.len()),
                }
            }
            "clean" => {
                let d = drive("drive")?.ok_or_else(|| http(400, "missing drive"))?;
                args.extend(["hw".into(), "clean".into(), "--".into(), d.clone().into()]);
                format!("clean {d}")
            }
            k => return Err(http(400, format!("no task kind {k}"))),
        };
        let mut sh = self.web.shared.lock().unwrap();
        if sh.stopping {
            return Err(http(409, "the scheduler is stopping"));
        }
        let now = Local::now();
        let log = self.web.state.join("logs").join(format!("@{kind}")).join(format!("{}.log", now.format("%Y%m%dT%H%M%S%.3f")));
        // A task's run is named by what it does.
        let run_id = self.cat.start_job_run(&format!("@{kind}"), &title, Some(&log.to_string_lossy()))?;
        let title_env = OsString::from(&title);
        let child = match crate::daemon::spawn_ltoarc(&self.web.exe, &self.web.child_args, &args, &log, &[("LTOARC_TASK", &title_env)]) {
            Ok(c) => c,
            Err(e) => {
                self.cat.end_job_run(run_id, "failed", &format!("could not start: {e:#}"))?;
                return Err(e);
            }
        };
        println!("ltoarcd: task {title} (pid {}); log {}", child.id(), log.display());
        sh.tasks.push(Task { run_id, kind: kind.clone(), title: title.clone(), child, since: now, log });
        Ok(json!({ "run": run_id, "title": title }))
    }

    fn stop(&self, b: &Value) -> Result<Value> {
        let sh = self.web.shared.lock().unwrap();
        if let Some(id) = b["run"].as_i64() {
            if let Some(t) = sh.tasks.iter().find(|t| t.run_id == id) {
                crate::daemon::terminate(&t.child);
                return Ok(json!({ "stopping": t.title }));
            }
            let run = self.cat.job_run(id)?.ok_or_else(|| http(404, format!("no run {id}")))?;
            if let Some(r) = sh.running.get(&run.job).filter(|r| run.log.as_deref() == Some(&*r.log.to_string_lossy())) {
                crate::daemon::terminate(&r.child);
                return Ok(json!({ "stopping": run.job }));
            }
        }
        Err(http(404, "nothing running by that id"))
    }
}

/// Where the slots of a library sit, to draw it as it looks: magazines on the left and right
/// of the robot, four cartridges to a row from the front of the library to the back, rows
/// counted from the bottom, the lower magazines first and the left side before the right.
/// The I/E slots (mailslots) take the front of the bottom rows on the left, and slot numbers
/// go on past them. So are the IBM 3573 (TS3100, TS3200; Dell TL2000, TL4000), HPE MSL2024
/// and MSL4048 and the 8-slot autoloaders built like them numbered.
///
/// {"left": [magazine], "right": [magazine]}, the upper magazine first; a magazine is its
/// rows, the top one first; a row is four places from the front, each {"kind": "slot" or
/// "ie", "n"}, or null where the library reports no slot. None for libraries this cannot
/// fit (more than 96 places, or more I/E slots than the bottom magazine has rows).
fn magazines(slots: i64, ie: i64) -> Option<Value> {
    const ROW: i64 = 4;
    let rows = (slots + ie + 2 * ROW - 1) / (2 * ROW);
    if slots < 1 || ie < 0 || slots + ie > 96 || ie > rows.min(3) {
        return None;
    }
    // Each side bottom row first, front to back.
    let mut sides = vec![vec![Value::Null; (rows * ROW) as usize]; 2];
    for i in 0..ie {
        sides[0][(i * ROW) as usize] = json!({ "kind": "ie", "n": i + 1 });
    }
    let mut n = 1;
    for cell in sides.iter_mut().flatten() {
        if cell.is_null() && n <= slots {
            *cell = json!({ "kind": "slot", "n": n });
            n += 1;
        }
    }
    // Magazines of three rows, as in 2U and 4U libraries; else one per side.
    let per = (if rows % 3 == 0 { 3 } else { rows }) as usize;
    let side = |cells: &[Value]| -> Value {
        let rows: Vec<Value> = cells.chunks(ROW as usize).map(|r| Value::from(r.to_vec())).collect();
        rows.chunks(per).rev().map(|m| m.iter().rev().cloned().collect::<Vec<_>>()).collect::<Vec<_>>().into()
    };
    Some(json!({ "left": side(&sides[0]), "right": side(&sides[1]) }))
}

fn run_json(r: &ltoarc_catalog::JobRun) -> Value {
    json!({ "id": r.id, "job": r.job, "run": r.run, "started": r.started, "ended": r.ended, "status": r.status, "summary": r.summary, "has_log": r.log.is_some() })
}

/// The last progress line of a running command's log ("  1234 entries, 5.6 GB, 151.0 MB/s").
fn progress(log: &Path) -> Option<String> {
    let mut f = std::fs::File::open(log).ok()?;
    let size = f.metadata().ok()?.len();
    f.seek(SeekFrom::Start(size.saturating_sub(16 << 10))).ok()?;
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = lossy(&buf);
    text.lines().rev().map(str::trim).find(|l| !l.is_empty()).map(|l| l.chars().take(200).collect())
}

fn read_json(rq: &mut Request) -> Result<Value> {
    let mut body = Vec::new();
    rq.as_reader().take(1 << 20).read_to_end(&mut body)?;
    if body.is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_slice(&body).map_err(|e| http(400, format!("bad JSON: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_decoding() {
        let q = parse_query("path=%2Fdata%2Fa+b&as_of=2026-10-30%2018%3A00&x");
        assert_eq!(q["path"], "/data/a b");
        assert_eq!(q["as_of"], "2026-10-30 18:00");
        assert_eq!(q["x"], "");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%e2%9c%93"), "✓");
    }

    /// A side as text, a line per row from the top: "M1 1 2 3"; magazines apart by "|".
    fn drawn(side: &Value) -> Vec<String> {
        let cell = |c: &Value| match c["kind"].as_str() {
            Some("ie") => format!("M{}", c["n"]),
            Some(_) => c["n"].to_string(),
            None => "-".into(),
        };
        let mags: Vec<Vec<String>> =
            side.as_array().unwrap().iter().map(|m| m.as_array().unwrap().iter().map(|r| r.as_array().unwrap().iter().map(cell).collect::<Vec<_>>().join(" ")).collect()).collect();
        mags.join(&"|".to_string())
    }

    #[test]
    fn magazine_layouts() {
        // HPE 1/8 G2 with the mailslot on; numbered from the front, the right side mirrored on screen.
        let l = magazines(7, 1).unwrap();
        assert_eq!(drawn(&l["left"]), ["M1 1 2 3"]);
        assert_eq!(drawn(&l["right"]), ["4 5 6 7"]);
        // 2U (TS3100, TL2000, MSL2024), with and without the I/E slot.
        let l = magazines(23, 1).unwrap();
        assert_eq!(drawn(&l["left"]), ["8 9 10 11", "4 5 6 7", "M1 1 2 3"]);
        assert_eq!(drawn(&l["right"]), ["20 21 22 23", "16 17 18 19", "12 13 14 15"]);
        assert_eq!(drawn(&magazines(24, 0).unwrap()["left"]), ["9 10 11 12", "5 6 7 8", "1 2 3 4"]);
        // 4U (TS3200, TL4000, MSL4048) with the three-slot I/E station: two magazines a side.
        let l = magazines(45, 3).unwrap();
        assert_eq!(drawn(&l["left"]), ["18 19 20 21", "14 15 16 17", "10 11 12 13", "|", "M3 7 8 9", "M2 4 5 6", "M1 1 2 3"]);
        assert_eq!(drawn(&l["right"])[..3], ["42 43 44 45", "38 39 40 41", "34 35 36 37"]);
        assert_eq!(drawn(&l["right"])[4..], ["30 31 32 33", "26 27 28 29", "22 23 24 25"]);
        // Fewer slots reported than there are places (reserved ones): the last are blank.
        assert_eq!(drawn(&magazines(21, 1).unwrap()["right"])[0], "20 21 - -");
        // Too many I/E slots for the magazine, or too large a library: no layout.
        assert!(magazines(5, 3).is_none());
        assert!(magazines(200, 0).is_none());
    }

    #[test]
    fn modes() {
        assert_eq!(mode_string("dir", 0o755), "drwxr-xr-x");
        assert_eq!(mode_string("file", 0o640), "-rw-r-----");
    }
}
