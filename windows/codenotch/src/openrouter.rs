//! OpenRouter spend adapter.
//!
//! Endpoint: GET https://openrouter.ai/api/v1/key, authenticated with `Authorization: Bearer <key>`.
//! It describes the key itself: its own spending limit (`limit`, `limit_remaining`, `limit_reset`),
//! any budgets an organisation puts on its member (`budgets`, `effective_budget`), and what it has
//! spent today, this week, this month and in all. `/credits` would give the account balance, but only
//! to management keys, so it is not asked.
//!
//! Each limit is a ring window: the budget an organisation set, then the key's own limit. A key with
//! neither has nothing to fill a ring against, and its spend is reported as a note instead.
//!
//! Credentials are read-only and borrowed in precedence order:
//!   1. `OPENROUTER_API_KEY` environment variable
//!   2. `<config dir>/codenotch/openrouter.json` (`{"api_key":"..."}`)
//!   3. OpenCode `auth.json` (`"openrouter"` entry, written by `opencode auth login`)
//!   4. OpenCode's config `provider.openrouter.options.apiKey`, either the key itself or
//!      `{env:NAME}`. Codenotch started at sign-in has not read the shell's start-up files, so a
//!      variable missing from its environment is looked for in them (`export NAME=...`).

use crate::usage::{LimitWindow, UsageSnapshot};
use crate::AppState;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Emitter, Manager};

const API: &str = "https://openrouter.ai/api/v1";
const ENV_VAR: &str = "OPENROUTER_API_KEY";
const POLL_SECS: u64 = 300;
const BACKOFF_BASE_SECS: u64 = 60;
const BACKOFF_CAP_SECS: u64 = 900;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;

static REFRESH: AtomicBool = AtomicBool::new(false);
static BACKOFF_UNTIL: AtomicU64 = AtomicU64::new(0);
static CONSECUTIVE_429: AtomicU32 = AtomicU32::new(0);

pub fn request_refresh() {
    BACKOFF_UNTIL.store(0, Ordering::Relaxed);
    CONSECUTIVE_429.store(0, Ordering::Relaxed);
    REFRESH.store(true, Ordering::Relaxed);
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn store_path() -> PathBuf {
    crate::config::config_path().with_file_name("openrouter-usage.json")
}

fn key_file() -> PathBuf {
    crate::config::config_path().with_file_name("openrouter.json")
}

pub fn load_persisted() -> UsageSnapshot {
    std::fs::read_to_string(store_path())
        .ok()
        .and_then(|t| serde_json::from_str::<UsageSnapshot>(&t).ok())
        .map(|mut s| {
            if !s.windows.is_empty() {
                s.status = "stale".into();
            }
            BACKOFF_UNTIL.store(s.backoff_until, Ordering::Relaxed);
            s
        })
        .unwrap_or_default()
}

fn persist(s: &UsageSnapshot) {
    if let Ok(t) = serde_json::to_string_pretty(s) {
        let _ = std::fs::write(store_path(), t);
    }
}

// ---------------- Credentials (borrowed, never written) ----------------

/// `NAME=value` or `export NAME=value` in a dotenv or shell start-up file, quotes taken off.
fn parse_env_assignment(text: &str, name: &str) -> Option<String> {
    let mut found = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").map(str::trim_start).unwrap_or(line);
        let Some((k, v)) = line.split_once('=') else { continue };
        if k.trim() != name {
            continue;
        }
        let v = v.trim();
        let unquoted = if let Some(rest) = v.strip_prefix('"') {
            rest.split_once('"').map(|(inner, _)| inner).unwrap_or(rest)
        } else if let Some(rest) = v.strip_prefix('\'') {
            rest.split_once('\'').map(|(inner, _)| inner).unwrap_or(rest)
        } else {
            v.split('#').next().unwrap_or(v).trim()
        };
        // A value built from other variables is the shell's to expand, not ours
        if !unquoted.is_empty() && !unquoted.contains('$') {
            // The last assignment is the one the shell ends up with
            found = Some(unquoted.to_string());
        }
    }
    found
}

fn parse_key_json(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text).ok()?;
    v.get("api_key")
        .or_else(|| v.get("apiKey"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

/// A variable as the user's login shell would have it, for a process started without one.
fn shell_variable(name: &str) -> Option<(String, String)> {
    let home = dirs::home_dir()?;
    let mut files: Vec<PathBuf> = [".bashrc", ".bash_profile", ".profile", ".zshrc", ".zshenv"]
        .iter()
        .map(|f| home.join(f))
        .collect();
    if let Ok(dir) = std::fs::read_dir(home.join(".config").join("environment.d")) {
        let mut confs: Vec<PathBuf> = dir.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "conf")).collect();
        confs.sort();
        files.extend(confs);
    }
    files.into_iter().find_map(|path| {
        let text = std::fs::read_to_string(&path).ok()?;
        let value = parse_env_assignment(&text, name)?;
        let shown = path.strip_prefix(&home).map(|p| format!("~/{}", p.display())).unwrap_or_else(|_| path.display().to_string());
        Some((shown, value))
    })
}

fn opencode_auth_key() -> Option<String> {
    let mut paths = Vec::new();
    if let Some(d) = dirs::data_local_dir() {
        paths.push(d.join("opencode").join("auth.json"));
    }
    if let Some(h) = dirs::home_dir() {
        paths.push(h.join(".local").join("share").join("opencode").join("auth.json"));
    }
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else { continue };
        let Ok(root) = serde_json::from_str::<Value>(&text) else { continue };
        let Some(entry) = root.get("openrouter") else { continue };
        let key = entry
            .as_str()
            .or_else(|| entry.get("key").and_then(Value::as_str))
            .or_else(|| entry.get("apiKey").and_then(Value::as_str))
            .map(str::trim)
            .filter(|s| !s.is_empty());
        if let Some(k) = key {
            return Some(k.to_string());
        }
    }
    None
}

/// What OpenCode's config says the key is: the key itself, or `{env:NAME}` naming a variable.
enum ConfigKey {
    Literal(String),
    Env(String),
}

fn parse_opencode_config(text: &str) -> Option<ConfigKey> {
    let v: Value = serde_json::from_str(text).ok()?;
    let raw = v.pointer("/provider/openrouter/options/apiKey")?.as_str()?.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.strip_prefix("{env:").and_then(|r| r.strip_suffix('}')) {
        Some(name) if !name.trim().is_empty() => Some(ConfigKey::Env(name.trim().to_string())),
        Some(_) => None,
        None => Some(ConfigKey::Literal(raw.to_string())),
    }
}

fn opencode_config_key() -> Option<(String, String)> {
    let dir = dirs::config_dir()?.join("opencode");
    // `opencode.jsonc` may carry comments, which a JSON reader refuses; it is then simply skipped
    for file in ["opencode.json", "config.json", "opencode.jsonc"] {
        let Ok(text) = std::fs::read_to_string(dir.join(file)) else { continue };
        match parse_opencode_config(&text) {
            Some(ConfigKey::Literal(k)) => return Some((format!("opencode/{file}"), k)),
            Some(ConfigKey::Env(name)) => {
                if let Some(k) = env_nonempty(&name) {
                    return Some((format!("opencode/{file} → ${name}"), k));
                }
                if let Some((where_, k)) = shell_variable(&name) {
                    return Some((format!("opencode/{file} → ${name} in {where_}"), k));
                }
            }
            None => {}
        }
    }
    None
}

fn credential() -> Option<(String, String)> {
    if let Some(k) = env_nonempty(ENV_VAR) {
        return Some((ENV_VAR.into(), k));
    }
    if let Some(k) = std::fs::read_to_string(key_file()).ok().as_deref().and_then(parse_key_json) {
        return Some(("openrouter.json".into(), k));
    }
    if let Some(k) = opencode_auth_key() {
        return Some(("opencode/auth.json".into(), k));
    }
    if let Some(found) = opencode_config_key() {
        return Some(found);
    }
    // Not configured in OpenCode, but exported for the shell all the same
    shell_variable(ENV_VAR).map(|(where_, k)| (format!("${ENV_VAR} in {where_}"), k))
}

pub fn present() -> bool {
    credential().is_some()
}

pub fn probe() -> String {
    match credential() {
        Some((src, _)) => format!("openrouter: credential from {src}"),
        None => format!("openrouter: absent (no {ENV_VAR}, openrouter.json, or OpenCode openrouter key)"),
    }
}

// ---------------- API Fetch & Parsing ----------------

enum FetchError {
    Auth,
    RateLimited(Option<u64>),
    Other(String),
}

fn get_json(path: &str, token: &str) -> Result<Value, FetchError> {
    let resp = ureq::get(&format!("{API}{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/json")
        .timeout(Duration::from_secs(15))
        .call();
    match resp {
        Ok(r) => r.into_json().map_err(|e| FetchError::Other(format!("invalid JSON: {e}"))),
        Err(ureq::Error::Status(401 | 403, _)) => Err(FetchError::Auth),
        Err(ureq::Error::Status(429, r)) => {
            let secs = r.header("Retry-After").and_then(|s| s.trim().parse::<u64>().ok());
            Err(FetchError::RateLimited(secs))
        }
        Err(ureq::Error::Status(code, _)) => Err(FetchError::Other(format!("HTTP {code}"))),
        Err(err) => Err(FetchError::Other(err.to_string())),
    }
}

fn usd(amount: f64) -> String {
    format!("${amount:.2}")
}

fn finite(v: Option<&Value>) -> Option<f64> {
    v.and_then(Value::as_f64).filter(|n| n.is_finite())
}

/// How long one period of a reset interval lasts, for the pace marker. `None` when it never resets.
fn interval_ms(interval: Option<&str>) -> Option<u64> {
    match interval?.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" => Some(DAY_MS),
        "weekly" | "week" => Some(7 * DAY_MS),
        "monthly" | "month" => Some(30 * DAY_MS),
        _ => None,
    }
}

fn interval_name(interval: Option<&str>) -> &'static str {
    match interval.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("daily" | "day") => "Daily",
        Some("weekly" | "week") => "Weekly",
        Some("monthly" | "month") => "Monthly",
        _ => "Total",
    }
}

/// The start of the next daily, weekly (Monday) or monthly period, in UTC, which is how OpenRouter
/// resets a key's own limit; it does not say when, as it does for a budget.
fn next_utc_reset(interval: Option<&str>, now: chrono::DateTime<chrono::Utc>) -> Option<u64> {
    use chrono::{Datelike, Duration as D, NaiveDate, TimeZone};
    let today = now.date_naive();
    let next: NaiveDate = match interval?.trim().to_ascii_lowercase().as_str() {
        "daily" | "day" => today + D::days(1),
        "weekly" | "week" => today + D::days(7 - today.weekday().num_days_from_monday() as i64),
        "monthly" | "month" => {
            let (y, m) = if today.month() == 12 { (today.year() + 1, 1) } else { (today.year(), today.month() + 1) };
            NaiveDate::from_ymd_opt(y, m, 1)?
        }
        _ => return None,
    };
    let at = chrono::Utc.from_utc_datetime(&next.and_hms_opt(0, 0, 0)?);
    Some(at.timestamp_millis().max(0) as u64)
}

/// A budget an organisation set on this member or key: `limit_usd`, `spend_usd`, `resets_at`.
fn budget_window(b: &Value, id: &str) -> Option<LimitWindow> {
    let limit = finite(b.get("limit_usd")).filter(|l| *l > 0.0)?;
    let spent = finite(b.get("spend_usd"))
        .or_else(|| finite(b.get("remaining_usd")).map(|r| limit - r))?
        .max(0.0);
    let interval = b.get("reset_interval").and_then(Value::as_str);
    let resets_at = b
        .get("resets_at")
        .and_then(Value::as_str)
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis().max(0) as u64);
    Some(LimitWindow {
        id: id.into(),
        label: format!("{} budget ({} / {})", interval_name(interval), usd(spent), usd(limit)),
        used: (spent / limit).clamp(0.0, 1.0),
        resets_at,
        duration_ms: interval_ms(interval),
        ..Default::default()
    })
}

/// The key's own spending limit, set where the key was made.
fn key_limit_window(d: &Value, now: chrono::DateTime<chrono::Utc>) -> Option<LimitWindow> {
    let limit = finite(d.get("limit")).filter(|l| *l > 0.0)?;
    let interval = d.get("limit_reset").and_then(Value::as_str);
    let spent = match finite(d.get("limit_remaining")) {
        Some(remaining) => limit - remaining,
        // Without the remainder, what was spent in the limit's own period
        None => finite(d.get(match interval_name(interval) {
            "Daily" => "usage_daily",
            "Weekly" => "usage_weekly",
            "Monthly" => "usage_monthly",
            _ => "usage",
        }))?,
    }
    .max(0.0);
    Some(LimitWindow {
        id: "key".into(),
        label: format!("{} key limit ({} / {})", interval_name(interval), usd(spent), usd(limit)),
        used: (spent / limit).clamp(0.0, 1.0),
        resets_at: next_utc_reset(interval, now),
        duration_ms: interval_ms(interval),
        ..Default::default()
    })
}

/// Every limit on the key, tightest first so `windows[0]` drives the ring.
fn parse_windows(d: &Value, now: chrono::DateTime<chrono::Utc>) -> Vec<LimitWindow> {
    let mut windows = Vec::new();
    // `budgets` lists each one; `effective_budget` is the one that binds, and is only read when the
    // list is missing, so the same budget is never shown twice
    match d.get("budgets").and_then(Value::as_array).filter(|a| !a.is_empty()) {
        Some(list) => {
            for (i, b) in list.iter().enumerate() {
                let scope = b.get("scope").and_then(Value::as_str).unwrap_or("budget");
                if let Some(w) = budget_window(b, &format!("budget_{scope}_{i}")) {
                    windows.push(w);
                }
            }
        }
        None => {
            if let Some(w) = d.get("effective_budget").and_then(|b| budget_window(b, "budget")) {
                windows.push(w);
            }
        }
    }
    if let Some(w) = key_limit_window(d, now) {
        windows.push(w);
    }
    windows.sort_by(|a, b| b.used.total_cmp(&a.used));
    windows
}

/// Spend in each period, for the card: what there is to say about a key with no limit at all.
fn spend_note(d: &Value) -> String {
    let parts: Vec<String> = [("usage_daily", "today"), ("usage_weekly", "this week"), ("usage_monthly", "this month")]
        .iter()
        .filter_map(|(field, name)| finite(d.get(*field)).map(|v| format!("{} {name}", usd(v))))
        .collect();
    let mut note = parts.join(" · ");
    if d.get("is_free_tier").and_then(Value::as_bool) == Some(true) {
        note = if note.is_empty() { "Free tier".into() } else { format!("Free tier · {note}") };
    }
    note
}

fn read_once(prev: &UsageSnapshot) -> UsageSnapshot {
    let Some((_, token)) = credential() else {
        return UsageSnapshot { status: "absent".into(), ..Default::default() };
    };
    let now = now_ms();
    let until = BACKOFF_UNTIL.load(Ordering::Relaxed);
    if until > now {
        let mut s = prev.clone();
        if s.status != "ok" && !s.windows.is_empty() {
            s.status = "stale".into();
        }
        s.backoff_until = until;
        return s;
    }

    match get_json("/key", &token) {
        Ok(body) => {
            CONSECUTIVE_429.store(0, Ordering::Relaxed);
            BACKOFF_UNTIL.store(0, Ordering::Relaxed);
            let data = body.get("data").unwrap_or(&body);
            let windows = parse_windows(data, chrono::Utc::now());
            let note = spend_note(data);
            if windows.is_empty() {
                return UsageSnapshot {
                    status: "none".into(),
                    fetched_at: now,
                    note: if note.is_empty() { "No spending limit or budget on this key".into() } else { format!("No limit on this key · {note}") },
                    ..Default::default()
                };
            }
            UsageSnapshot { status: "ok".into(), windows, fetched_at: now, note, backoff_until: 0, ..Default::default() }
        }
        Err(FetchError::Auth) => UsageSnapshot {
            status: "needsAuth".into(),
            note: format!("OpenRouter API key was rejected — check {ENV_VAR} or OpenCode's openrouter key"),
            ..Default::default()
        },
        Err(FetchError::RateLimited(retry_after)) => {
            let n = CONSECUTIVE_429.fetch_add(1, Ordering::Relaxed);
            let secs = retry_after
                .unwrap_or_else(|| BACKOFF_BASE_SECS.saturating_mul(1u64 << n.min(4)))
                .clamp(BACKOFF_BASE_SECS, BACKOFF_CAP_SECS);
            let deadline = now + secs * 1000;
            BACKOFF_UNTIL.store(deadline, Ordering::Relaxed);
            let mut s = prev.clone();
            s.status = if s.windows.is_empty() { "backoff" } else { "stale" }.into();
            s.backoff_until = deadline;
            s.note = format!("Rate limited — retrying in {secs}s");
            s
        }
        Err(FetchError::Other(msg)) => {
            let mut s = prev.clone();
            s.status = if s.windows.is_empty() { "error" } else { "stale" }.into();
            s.note = format!("Live read failed ({msg})");
            s
        }
    }
}

pub fn start(app: AppHandle) {
    std::thread::spawn(move || {
        {
            let st = app.state::<AppState>();
            let snap = st.openrouter.lock().unwrap().clone();
            let _ = app.emit("openrouter", &snap);
        }
        let mut last_poll = 0u64;
        loop {
            let forced = REFRESH.swap(false, Ordering::Relaxed);
            let due = now_ms().saturating_sub(last_poll) >= POLL_SECS * 1000;
            if forced || due || last_poll == 0 {
                let prev = app.state::<AppState>().openrouter.lock().unwrap().clone();
                let snap = read_once(&prev);
                last_poll = now_ms();
                if snap.status != "absent" {
                    persist(&snap);
                } else {
                    let _ = std::fs::remove_file(store_path());
                }
                {
                    let st = app.state::<AppState>();
                    *st.openrouter.lock().unwrap() = snap.clone();
                }
                let _ = app.emit("openrouter", &snap);
                crate::tray::refresh_menu(&app);
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn at(y: i32, m: u32, d: u32) -> chrono::DateTime<chrono::Utc> {
        chrono::Utc.with_ymd_and_hms(y, m, d, 15, 30, 0).unwrap()
    }

    /// The shape OpenRouter returned for a real member key (identifiers removed).
    const MEMBER_KEY: &str = r#"{"data":{
        "limit": null, "limit_reset": null, "limit_remaining": null,
        "budgets": [{"scope":"member","limit_usd":250,"spend_usd":214.0227585,
                     "remaining_usd":35.977,"reset_interval":"monthly",
                     "resets_at":"2026-12-01T00:00:00.000Z"}],
        "effective_budget": {"scope":"member","limit_usd":250,"spend_usd":214.0227585,
                     "reset_interval":"monthly","resets_at":"2026-12-01T00:00:00.000Z"},
        "usage": 392.41, "usage_daily": 0, "usage_weekly": 1.5, "usage_monthly": 214.0227585,
        "is_free_tier": false}}"#;

    #[test]
    fn an_organisation_budget_is_one_ring_window() {
        let body: Value = serde_json::from_str(MEMBER_KEY).unwrap();
        let ws = parse_windows(&body["data"], at(2026, 11, 24));
        assert_eq!(ws.len(), 1, "effective_budget repeats budgets[0] and is not shown twice");
        assert_eq!(ws[0].label, "Monthly budget ($214.02 / $250.00)");
        assert!((ws[0].used - 214.0227585 / 250.0).abs() < 1e-9);
        assert_eq!(ws[0].resets_at, Some(chrono::Utc.with_ymd_and_hms(2026, 12, 1, 0, 0, 0).unwrap().timestamp_millis() as u64));
        assert_eq!(ws[0].duration_ms, Some(30 * DAY_MS));
        assert_eq!(spend_note(&body["data"]), "$0.00 today · $1.50 this week · $214.02 this month");
    }

    #[test]
    fn a_key_limit_is_read_from_its_remainder_and_resets_at_utc_midnight() {
        let d: Value = serde_json::from_str(r#"{"limit":10,"limit_remaining":2.5,"limit_reset":"weekly","usage_weekly":7}"#).unwrap();
        // 2026-11-24 is a Tuesday: the week starts again on Monday the 30th
        let ws = parse_windows(&d, at(2026, 11, 24));
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].id, "key");
        assert_eq!(ws[0].label, "Weekly key limit ($7.50 / $10.00)");
        assert!((ws[0].used - 0.75).abs() < 1e-9);
        assert_eq!(ws[0].resets_at, Some(chrono::Utc.with_ymd_and_hms(2026, 11, 30, 0, 0, 0).unwrap().timestamp_millis() as u64));
        let monthly: Value = serde_json::from_str(r#"{"limit":10,"limit_reset":"monthly","usage_monthly":4}"#).unwrap();
        let ws = parse_windows(&monthly, at(2026, 12, 9));
        assert_eq!(ws[0].label, "Monthly key limit ($4.00 / $10.00)");
        assert_eq!(ws[0].resets_at, Some(chrono::Utc.with_ymd_and_hms(2027, 1, 1, 0, 0, 0).unwrap().timestamp_millis() as u64));
    }

    #[test]
    fn the_tightest_limit_leads_and_a_key_without_one_has_none() {
        let d: Value = serde_json::from_str(
            r#"{"limit":100,"limit_remaining":90,"limit_reset":null,
                "budgets":[{"scope":"member","limit_usd":50,"spend_usd":40,"reset_interval":"monthly"}]}"#,
        )
        .unwrap();
        let ws = parse_windows(&d, at(2026, 11, 24));
        assert_eq!(ws.iter().map(|w| w.id.as_str()).collect::<Vec<_>>(), ["budget_member_0", "key"]);
        assert_eq!(ws[1].label, "Total key limit ($10.00 / $100.00)");
        assert_eq!(ws[1].resets_at, None);
        let none: Value = serde_json::from_str(r#"{"limit":null,"budgets":[],"usage_monthly":3}"#).unwrap();
        assert!(parse_windows(&none, at(2026, 11, 24)).is_empty());
    }

    #[test]
    fn finds_the_key_in_shell_files_and_opencode_config() {
        let rc = "# OpenRouter\nexport OPENROUTER_API_KEY=\"sk-or-first\"\nalias ll='ls -l'\nexport OPENROUTER_API_KEY='sk-or-last' # used\n";
        assert_eq!(parse_env_assignment(rc, "OPENROUTER_API_KEY"), Some("sk-or-last".into()));
        assert_eq!(parse_env_assignment("OPENROUTER_API_KEY=$OTHER\n", "OPENROUTER_API_KEY"), None);
        assert_eq!(parse_env_assignment("OPENROUTER_API_KEY_OLD=x\n", "OPENROUTER_API_KEY"), None);
        let by_env = r#"{"provider":{"openrouter":{"options":{"apiKey":"{env:OPENROUTER_API_KEY}"}}}}"#;
        assert!(matches!(parse_opencode_config(by_env), Some(ConfigKey::Env(n)) if n == "OPENROUTER_API_KEY"));
        let literal = r#"{"provider":{"openrouter":{"options":{"apiKey":"sk-or-lit"}}}}"#;
        assert!(matches!(parse_opencode_config(literal), Some(ConfigKey::Literal(k)) if k == "sk-or-lit"));
        assert!(parse_opencode_config(r#"{"provider":{"ollama":{}}}"#).is_none());
        assert_eq!(parse_key_json(r#"{"api_key":" sk-or-json "}"#), Some("sk-or-json".into()));
    }
}
