//! codenotch-hook: the minimal client Claude Code's hooks call.
//! Duty: report the event plus stdin JSON to the main app. It never starts the app: as on the Mac,
//! Codenotch runs when it is opened or at sign-in, and a hook that finds it closed gives up.
//! Iron rule: never block Claude Code — ~2 s total budget, and every failure exits 0 silently.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const DEFAULT_PORT: u16 = 48666;
const MAX_STDIN: u64 = 256 * 1024;

fn main() {
    let event = std::env::args().nth(1).unwrap_or_else(|| "ping".into());

    // The hook's stdin is the JSON Claude Code provides (session_id / cwd / prompt / message…)
    let mut body = String::new();
    let _ = std::io::stdin().take(MAX_STDIN).read_to_string(&mut body);

    let port = read_port();
    let token = read_token();
    let ppid = parent_pid();

    // Not running: give up quietly — never affect Claude Code
    let _ = send(port, &token, &event, ppid, &body);
}

/// Pulls auth token out of %APPDATA%\codenotch\auth_token (hand-rolled scan, no dependency)
fn read_token() -> String {
    let path = match std::env::var("APPDATA") {
        Ok(a) => format!("{a}\\codenotch\\auth_token"),
        Err(_) => return String::new(),
    };
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Pulls "port": N out of %APPDATA%\codenotch\config.json (hand-rolled scan, no dependency)
fn read_port() -> u16 {
    let path = match std::env::var("APPDATA") {
        Ok(a) => format!("{a}\\codenotch\\config.json"),
        Err(_) => return DEFAULT_PORT,
    };
    let Ok(txt) = std::fs::read_to_string(path) else {
        return DEFAULT_PORT;
    };
    if let Some(i) = txt.find("\"port\"") {
        let digits: String = txt[i + 6..]
            .chars()
            .skip_while(|c| !c.is_ascii_digit())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        if let Ok(p) = digits.parse() {
            return p;
        }
    }
    DEFAULT_PORT
}

fn build_request(event: &str, ppid: u32, token: &str, body: &str) -> String {
    let auth_header = if !token.is_empty() {
        format!("X-Codenotch-Token: {}\r\n", token)
    } else {
        String::new()
    };
    format!(
        "POST /event?e={}&ppid={} HTTP/1.1\r\nHost: 127.0.0.1\r\nContent-Type: application/json\r\nContent-Length: {}\r\n{}Connection: close\r\n\r\n{}",
        event,
        ppid,
        body.len(),
        auth_header,
        body
    )
}

fn send(port: u16, token: &str, event: &str, ppid: u32, body: &str) -> std::io::Result<()> {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_millis(300))?;
    s.set_write_timeout(Some(Duration::from_millis(700)))?;
    s.set_read_timeout(Some(Duration::from_millis(700)))?;
    let req = build_request(event, ppid, token, body);
    s.write_all(req.as_bytes())?;
    let mut buf = [0u8; 64];
    let _ = s.read(&mut buf); // wait for a response fragment to confirm delivery; failure does not matter
    Ok(())
}

/// Parent process PID (≈ the Claude Code CLI process) via NtQueryInformationProcess, no dependency
#[cfg(windows)]
fn parent_pid() -> u32 {
    #[repr(C)]
    struct Pbi {
        exit_status: isize,
        peb: usize,
        affinity_mask: usize,
        base_priority: isize,
        unique_process_id: usize,
        inherited_from_unique_process_id: usize,
    }
    extern "system" {
        fn NtQueryInformationProcess(
            handle: isize,
            class: u32,
            info: *mut Pbi,
            len: u32,
            ret_len: *mut u32,
        ) -> i32;
    }
    unsafe {
        let mut pbi = std::mem::zeroed::<Pbi>();
        let mut ret = 0u32;
        // -1 = GetCurrentProcess()
        if NtQueryInformationProcess(
            -1,
            0,
            &mut pbi,
            std::mem::size_of::<Pbi>() as u32,
            &mut ret,
        ) == 0
        {
            return pbi.inherited_from_unique_process_id as u32;
        }
    }
    0
}

/// The Claude Code process. Claude Code runs a hook through a shell (`sh -c …`), so the hook's own
/// parent is usually that shell, gone as soon as the hook is — useless for finding the terminal.
/// Walk up past any shells to the first process that is not one.
#[cfg(not(windows))]
fn parent_pid() -> u32 {
    past_shells(std::os::unix::process::parent_id(), stat_of)
}

#[cfg(any(test, not(windows)))]
fn past_shells(start: u32, stat: impl Fn(u32) -> Option<(String, u32)>) -> u32 {
    const SHELLS: [&str; 6] = ["sh", "dash", "bash", "zsh", "fish", "busybox"];
    let mut pid = start;
    for _ in 0..4 {
        let Some((name, ppid)) = stat(pid) else { break };
        if !SHELLS.contains(&name.as_str()) || ppid <= 1 {
            break;
        }
        pid = ppid;
    }
    pid
}

/// (`comm`, parent) from `/proc/<pid>/stat`; the fields after `comm` are found from its last `)`.
#[cfg(not(windows))]
fn stat_of(pid: u32) -> Option<(String, u32)> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(any(test, not(windows)))]
fn parse_stat(stat: &str) -> Option<(String, u32)> {
    let name = stat.get(stat.find('(')? + 1..stat.rfind(')')?)?.to_string();
    let mut rest = stat.get(stat.rfind(')')? + 1..)?.split_whitespace();
    rest.next()?;
    Some((name, rest.next()?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hook_reports_claude_not_the_shell_that_ran_it() {
        // hook ← sh 300 ← claude 200 ← bash 100 (the terminal's shell)
        let tree = |pid: u32| match pid {
            300 => Some(("sh".to_string(), 200)),
            200 => Some(("claude".to_string(), 100)),
            100 => Some(("bash".to_string(), 50)),
            _ => None,
        };
        assert_eq!(past_shells(300, tree), 200);
        // Run straight from claude, with no shell between
        assert_eq!(past_shells(200, tree), 200);
        // A process that is gone already stays as it is
        assert_eq!(past_shells(999, tree), 999);
        assert_eq!(parse_stat("42 (node (x)) S 7 1 1"), Some(("node (x)".into(), 7)));
    }

    #[test]
    fn test_build_request_with_token() {
        let req = build_request("session_start", 1234, "secret123", "{}");
        assert!(req.contains("POST /event?e=session_start&ppid=1234 HTTP/1.1\r\n"));
        assert!(req.contains("X-Codenotch-Token: secret123\r\n"));
        assert!(req.contains("Content-Length: 2\r\n"));
    }

    #[test]
    fn test_build_request_without_token() {
        let req = build_request("running", 5678, "", "{}");
        assert!(req.contains("POST /event?e=running&ppid=5678 HTTP/1.1\r\n"));
        assert!(!req.contains("X-Codenotch-Token"));
    }
}
