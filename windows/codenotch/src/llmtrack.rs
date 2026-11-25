//! llmtrack: a link, not a quota. llmtrack (a local session tracker, `llmtrack serve`) records every
//! session of every coding tool, but no limit, so it has no ring to fill. Its notch cell opens the
//! dashboard, and its card lists the last few sessions of any tool.
//!
//! The dashboard is found the way llmtrack documents it: `llmtrack url` prints its address (a custom
//! port included). Only a `http://127.0.0.1:<port>/` answer is accepted; anything else falls back to
//! the default port. Sessions come from that server's `/data`.

use crate::usage::UsageSnapshot;
use crate::AppState;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager};

const DEFAULT_URL: &str = "http://127.0.0.1:8377/";
const POLL_SECS: u64 = 30;
/// The card's rows: the most recent sessions
const ROWS: usize = 3;

static REFRESH: AtomicBool = AtomicBool::new(false);
static URL: Mutex<Option<String>> = Mutex::new(None);

pub fn request_refresh() {
    REFRESH.store(true, Ordering::Relaxed);
}

#[derive(Clone, Debug, Default, Serialize, PartialEq)]
pub struct Session {
    pub tool: String,
    pub title: String,
    pub dir: String,
    pub tokens: String,
    /// Last activity, ms since the epoch
    pub when: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Snapshot {
    /// "ok", "absent" (llmtrack not installed), "offline" (installed, its server not answering)
    pub status: String,
    pub url: String,
    pub sessions: Vec<Session>,
    pub fetched_at: u64,
    pub note: String,
}

/// The settings list and the tray read every provider as a usage snapshot.
pub fn as_usage(s: &Snapshot) -> UsageSnapshot {
    UsageSnapshot {
        status: match s.status.as_str() {
            "ok" => "none",
            "absent" => "absent",
            _ => "error",
        }
        .into(),
        fetched_at: s.fetched_at,
        note: s.note.clone(),
        ..Default::default()
    }
}

fn executable() -> Option<std::path::PathBuf> {
    let on_path = std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths).map(|d| d.join("llmtrack")).find(|p| p.is_file())
    });
    on_path.or_else(|| Some(std::path::PathBuf::from("/snap/bin/llmtrack")).filter(|p| p.exists()))
}

/// `http://127.0.0.1:<port>` with or without a trailing slash, and nothing else: the link is opened
/// in the browser, so an answer pointing anywhere but this machine is not taken.
fn local_url(text: &str) -> Option<String> {
    let line = text.lines().map(str::trim).find(|l| !l.is_empty())?;
    let port = line.strip_prefix("http://127.0.0.1:")?.trim_end_matches('/');
    let port: u16 = port.parse().ok().filter(|p| *p > 0)?;
    Some(format!("http://127.0.0.1:{port}/"))
}

fn discover() -> String {
    let found = executable().and_then(|exe| {
        let out = std::process::Command::new(exe).arg("url").output().ok()?;
        local_url(&String::from_utf8_lossy(&out.stdout))
    });
    found.unwrap_or_else(|| DEFAULT_URL.to_string())
}

/// Newest first, at most `ROWS`. Each row is one session, its `when` in seconds.
fn parse_sessions(body: &Value) -> Vec<Session> {
    let mut rows: Vec<Session> = body
        .get("rows")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let when = r.get("when").and_then(Value::as_f64).filter(|w| w.is_finite() && *w > 0.0)?;
            let text = |k: &str| r.get(k).and_then(Value::as_str).map(str::trim).unwrap_or("").to_string();
            let title = [text("title"), text("session")].into_iter().find(|t| !t.is_empty()).unwrap_or_else(|| "(untitled)".into());
            Some(Session { tool: text("tool"), title, dir: text("dir"), tokens: text("tokens_s"), when: (when * 1000.0) as u64 })
        })
        .collect();
    rows.sort_by(|a, b| b.when.cmp(&a.when));
    rows.truncate(ROWS);
    rows
}

fn read_once() -> Snapshot {
    if executable().is_none() {
        return Snapshot { status: "absent".into(), ..Default::default() };
    }
    // Read and released before discovering: the guard of a one-statement `lock().clone().unwrap_or_else`
    // lives to the end of the statement, and locking again inside it deadlocked this thread for good
    let known = URL.lock().unwrap().clone();
    let url = match known {
        Some(u) => u,
        None => {
            let u = discover();
            *URL.lock().unwrap() = Some(u.clone());
            u
        }
    };
    let now = crate::now_ms();
    let fetched = ureq::get(&format!("{url}data"))
        .timeout(Duration::from_secs(5))
        .call()
        .map_err(|e| e.to_string())
        .and_then(|r| r.into_json::<Value>().map_err(|e| e.to_string()));
    match fetched {
        Ok(body) => {
            let sessions = parse_sessions(&body);
            let note = if sessions.is_empty() { "No sessions recorded yet".into() } else { String::new() };
            Snapshot { status: "ok".into(), url, sessions, fetched_at: now, note }
        }
        Err(e) => {
            // Asked again next time: the server may have come back on another port
            *URL.lock().unwrap() = None;
            crate::applog(&format!("llmtrack: {url}data unreadable: {e}"));
            Snapshot {
                status: "offline".into(),
                url,
                fetched_at: now,
                note: "llmtrack's dashboard is not running — click to start it".into(),
                ..Default::default()
            }
        }
    }
}

/// The dashboard in the default browser. Not running, it is started first, as llmtrack's own
/// launcher does (`llmtrack serve`), and given a few seconds to answer. `async`, so that wait is never
/// spent on the window's own thread.
#[tauri::command(async)]
pub fn open_llmtrack(app: AppHandle) -> Result<(), String> {
    let snap = app.state::<AppState>().llmtrack.lock().unwrap().clone();
    let url = if snap.status == "ok" {
        snap.url
    } else {
        let exe = executable().ok_or("llmtrack is not installed")?;
        let url = discover();
        let up = |u: &str| ureq::get(&format!("{u}data")).timeout(Duration::from_secs(2)).call().is_ok();
        if !up(&url) {
            let port = url.trim_end_matches('/').rsplit(':').next().unwrap_or("8377").to_string();
            std::process::Command::new(exe)
                .args(["serve", "--port", &port])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
                .map_err(|e| format!("could not start llmtrack: {e}"))?;
            if !(0..20).any(|_| {
                std::thread::sleep(Duration::from_millis(250));
                up(&url)
            }) {
                return Err("llmtrack did not start".into());
            }
            request_refresh();
        }
        url
    };
    #[cfg(windows)]
    let opened = std::process::Command::new("cmd").args(["/C", "start", "", &url]).spawn();
    #[cfg(not(windows))]
    let opened = std::process::Command::new("xdg-open").arg(&url).spawn();
    opened.map(|_| ()).map_err(|e| e.to_string())
}

#[tauri::command]
pub fn get_llmtrack(state: tauri::State<AppState>) -> Snapshot {
    state.llmtrack.lock().unwrap().clone()
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        let mut last_poll = 0u64;
        loop {
            let forced = REFRESH.swap(false, Ordering::Relaxed);
            if forced || last_poll == 0 || crate::now_ms().saturating_sub(last_poll) >= POLL_SECS * 1000 {
                let snap = read_once();
                last_poll = crate::now_ms();
                *app.state::<AppState>().llmtrack.lock().unwrap() = snap.clone();
                let _ = app.emit("llmtrack", &snap);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_local_address_is_taken() {
        assert_eq!(local_url("http://127.0.0.1:8377\n"), Some("http://127.0.0.1:8377/".into()));
        assert_eq!(local_url("\nhttp://127.0.0.1:9000/\n"), Some("http://127.0.0.1:9000/".into()));
        assert_eq!(local_url("http://example.com:8377/"), None);
        assert_eq!(local_url("http://127.0.0.1:8377/../x"), None);
        assert_eq!(local_url("http://127.0.0.1:0/"), None);
        assert_eq!(local_url(""), None);
    }

    #[test]
    fn the_newest_three_sessions_of_any_tool() {
        let rows: Vec<Value> = (0..8)
            .map(|i| {
                serde_json::json!({
                    "tool": if i % 2 == 0 { "claude" } else { "opencode" },
                    "title": if i == 7 { "" } else { "work" }, "session": format!("s{i}"),
                    "dir": "~/canonical", "tokens_s": "1.2M", "when": 1_795_000_000.0 + i as f64 * 60.0
                })
            })
            .chain([serde_json::json!({"tool": "codex", "title": "no time"})])
            .collect();
        let got = parse_sessions(&serde_json::json!({ "rows": rows }));
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].tool, "opencode");
        assert_eq!(got[0].title, "s7", "an empty title falls back to the session name");
        assert_eq!(got[0].when, 1_795_000_420_000);
        assert!(got.windows(2).all(|w| w[0].when >= w[1].when));
        assert!(got.iter().all(|s| s.tool != "codex"), "a row with no time is left out");
    }
}
