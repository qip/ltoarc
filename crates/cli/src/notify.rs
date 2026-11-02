//! Notifications (`[notify]` in the jobs file): job results, requests to insert cartridges,
//! cleaning and wear. Sent best effort, by webhook (JSON POST), ntfy, or a shell command;
//! a failure to notify is reported and otherwise ignored.

use std::io::Write;
use std::process::{Command, Stdio};

use serde::Deserialize;

#[derive(Debug, Default, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotifyCfg {
    /// URL that gets a JSON POST: {"event", "job", "title", "message", "host", "time"}.
    pub webhook: Option<String>,
    /// ntfy topic URL (e.g. https://ntfy.sh/my-tapes).
    pub ntfy: Option<String>,
    /// Shell command; LTOARC_EVENT, LTOARC_JOB, LTOARC_TITLE and LTOARC_MESSAGE are set and
    /// the message is also on its standard input.
    pub command: Option<String>,
    /// Events to send: done, failed, request, wear (default: all but done).
    pub on: Option<Vec<String>>,
}

impl NotifyCfg {
    fn wants(&self, event: &str) -> bool {
        match &self.on {
            Some(list) => list.iter().any(|e| e == event),
            None => event != "done",
        }
    }
}

fn host() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname").map(|s| s.trim().to_string()).unwrap_or_default()
}

fn run(cmd: &mut Command, stdin: &[u8]) -> Result<(), String> {
    let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::piped()).spawn().map_err(|e| e.to_string())?;
    if let Some(mut i) = child.stdin.take() {
        let _ = i.write_all(stdin);
    }
    let out = child.wait_with_output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Send `event` about `job` everywhere configured for it.
pub fn send(cfg: &NotifyCfg, event: &str, job: &str, title: &str, message: &str) {
    if !cfg.wants(event) {
        return;
    }
    let time = chrono::Local::now().to_rfc3339();
    if let Some(url) = &cfg.webhook {
        let body = serde_json::json!({"event": event, "job": job, "title": title, "message": message, "host": host(), "time": time}).to_string();
        let r = run(Command::new("curl").args(["-fsS", "-m", "20", "-H", "Content-Type: application/json", "--data-binary", "@-", url]), body.as_bytes());
        if let Err(e) = r {
            eprintln!("warning: webhook notification failed: {e}");
        }
    }
    if let Some(url) = &cfg.ntfy {
        let prio = match event {
            "failed" | "request" => "high",
            "wear" => "default",
            _ => "low",
        };
        let r = run(
            Command::new("curl").args(["-fsS", "-m", "20", "-H", &format!("Title: {title}"), "-H", &format!("Priority: {prio}"), "-H", &format!("Tags: ltoarc,{event}"), "--data-binary", "@-", url]),
            message.as_bytes(),
        );
        if let Err(e) = r {
            eprintln!("warning: ntfy notification failed: {e}");
        }
    }
    if let Some(cmd) = &cfg.command {
        let r = run(
            Command::new("/bin/sh").arg("-c").arg(cmd).env("LTOARC_EVENT", event).env("LTOARC_JOB", job).env("LTOARC_TITLE", title).env("LTOARC_MESSAGE", message),
            message.as_bytes(),
        );
        if let Err(e) = r {
            eprintln!("warning: notification command failed: {e}");
        }
    }
}
