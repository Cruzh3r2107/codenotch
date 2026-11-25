//! Jump back to the right terminal: from the session's Claude CLI process PID, walk the parent chain
//! to the hosting terminal window, then SetForegroundWindow + FlashWindowEx. Returns false on failure (the page reports it).

#[cfg(windows)]
pub fn focus_terminal(claude_pid: u32) -> bool {
    use std::collections::HashMap;
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FlashWindowEx, GetWindowTextLengthW, GetWindowThreadProcessId, IsIconic,
        IsWindowVisible, SetForegroundWindow, ShowWindow, FLASHWINFO, FLASHW_ALL, SW_RESTORE,
    };

    if claude_pid == 0 {
        return false;
    }

    // 1) Full pid -> ppid snapshot
    let mut ppid_map: HashMap<u32, u32> = HashMap::new();
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                ppid_map.insert(entry.th32ProcessID, entry.th32ParentProcessID);
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }

    // 2) claude's ancestor chain (itself included), at most 8 levels: node → shell → WindowsTerminal/conhost host…
    let mut chain: Vec<u32> = vec![claude_pid];
    let mut cur = claude_pid;
    for _ in 0..8 {
        match ppid_map.get(&cur) {
            Some(&p) if p != 0 && !chain.contains(&p) => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }

    // 3) Enumerate visible top-level windows
    struct Cand {
        hwnd: isize,
        pid: u32,
    }
    let mut wins: Vec<Cand> = Vec::new();
    unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<(isize, u32)>);
        if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            v.push((hwnd.0 as isize, pid));
        }
        BOOL(1)
    }
    let mut raw: Vec<(isize, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }
    for (h, p) in raw {
        wins.push(Cand { hwnd: h, pid: p });
    }

    // 4) Score: the window's PID is on the ancestor chain (higher up = the real terminal host = higher
    //    score), or the window PID's parent is on the chain (the classic conhost case).
    let score_of = |pid: u32| -> Option<usize> {
        if let Some(i) = chain.iter().position(|&c| c == pid) {
            return Some(i);
        }
        if let Some(&pp) = ppid_map.get(&pid) {
            if let Some(i) = chain.iter().position(|&c| c == pp) {
                return Some(i);
            }
        }
        None
    };
    let best = wins
        .iter()
        .filter_map(|w| score_of(w.pid).map(|s| (s, w.hwnd)))
        .max_by_key(|(s, _)| *s);

    let Some((_, hwnd_raw)) = best else {
        return false;
    };
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut core::ffi::c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        let fi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL,
            uCount: 2,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&fi);
    }
    true
}

/// Linux: the same walk up the process tree, through `/proc`, then the companion GNOME Shell extension
/// raises the window (see `linux_shell`): under Wayland an app may not raise another app's window.
#[cfg(target_os = "linux")]
pub fn focus_terminal(claude_pid: u32) -> bool {
    if claude_pid == 0 {
        return false;
    }
    let maps = proc_maps();
    let chain = chain_of(claude_pid, &maps.ppid);
    linux_shell::activate(&chain, "")
}

#[cfg(not(any(windows, target_os = "linux")))]
pub fn focus_terminal(_claude_pid: u32) -> bool {
    false
}

// ---------------- Process and foreground helpers shared by seen-clears-it and the desktop jump-back ----------------

#[cfg(any(windows, target_os = "linux"))]
pub struct ProcMaps {
    pub ppid: std::collections::HashMap<u32, u32>,
    pub name: std::collections::HashMap<u32, String>, // lower-case exe name
}

#[cfg(windows)]
pub fn proc_maps() -> ProcMaps {
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    let mut m = ProcMaps {
        ppid: Default::default(),
        name: Default::default(),
    };
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return m;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                m.ppid.insert(e.th32ProcessID, e.th32ParentProcessID);
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(260);
                m.name.insert(
                    e.th32ProcessID,
                    String::from_utf16_lossy(&e.szExeFile[..len]).to_lowercase(),
                );
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = windows::Win32::Foundation::CloseHandle(snap);
    }
    m
}

#[cfg(windows)]
pub fn fg_pid() -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return 0;
        }
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        pid
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub fn chain_of(pid: u32, ppid: &std::collections::HashMap<u32, u32>) -> Vec<u32> {
    let mut chain = vec![pid];
    let mut cur = pid;
    for _ in 0..8 {
        match ppid.get(&cur) {
            Some(&p) if p != 0 && !chain.contains(&p) => {
                chain.push(p);
                cur = p;
            }
            _ => break,
        }
    }
    chain
}

/// Whether the foreground process belongs to a session's terminal window (itself on the chain, or its parent — the conhost case)
#[cfg(any(windows, target_os = "linux"))]
pub fn pid_hits_chain(pid: u32, chain: &[u32], maps: &ProcMaps) -> bool {
    chain.contains(&pid)
        || maps
            .ppid
            .get(&pid)
            .map(|p| chain.contains(p))
            .unwrap_or(false)
}

/// Focus the Claude desktop app's main window (the jump-back target for desktop sessions: the largest visible window whose process name contains claude)
#[cfg(windows)]
pub fn focus_claude_desktop() -> bool {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, FlashWindowEx, GetWindowRect, GetWindowTextLengthW, GetWindowThreadProcessId,
        IsIconic, IsWindowVisible, SetForegroundWindow, ShowWindow, FLASHWINFO, FLASHW_ALL,
        SW_RESTORE,
    };
    let maps = proc_maps();
    unsafe extern "system" fn cb(hwnd: HWND, l: LPARAM) -> BOOL {
        let v = &mut *(l.0 as *mut Vec<(isize, u32)>);
        if IsWindowVisible(hwnd).as_bool() && GetWindowTextLengthW(hwnd) > 0 {
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            v.push((hwnd.0 as isize, pid));
        }
        BOOL(1)
    }
    let mut wins: Vec<(isize, u32)> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut wins as *mut _ as isize));
    }
    let mut best: Option<(isize, i64)> = None;
    for (h, pid) in wins {
        let Some(name) = maps.name.get(&pid) else {
            continue;
        };
        if !name.contains("claude") || name.contains("codenotch") {
            continue;
        }
        let mut r = RECT::default();
        let area = unsafe {
            if GetWindowRect(HWND(h as *mut core::ffi::c_void), &mut r).is_ok() {
                ((r.right - r.left) as i64) * ((r.bottom - r.top) as i64)
            } else {
                0
            }
        };
        if best.map(|(_, a)| area > a).unwrap_or(true) {
            best = Some((h, area));
        }
    }
    let Some((h, _)) = best else {
        return false;
    };
    unsafe {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        if IsIconic(hwnd).as_bool() {
            let _ = ShowWindow(hwnd, SW_RESTORE);
        }
        let _ = SetForegroundWindow(hwnd);
        let fi = FLASHWINFO {
            cbSize: std::mem::size_of::<FLASHWINFO>() as u32,
            hwnd,
            dwFlags: FLASHW_ALL,
            uCount: 2,
            dwTimeout: 0,
        };
        let _ = FlashWindowEx(&fi);
    }
    true
}

#[cfg(not(windows))]
pub fn focus_claude_desktop() -> bool {
    false
}

// ---------------- Linux ----------------

/// Every process's parent and name, from `/proc/<pid>/stat`. The name is the kernel's `comm`
/// (at most 15 characters), lower-cased as the Windows names are.
#[cfg(target_os = "linux")]
pub fn proc_maps() -> ProcMaps {
    let mut m = ProcMaps { ppid: Default::default(), name: Default::default() };
    let Ok(dir) = std::fs::read_dir("/proc") else { return m };
    for entry in dir.flatten() {
        let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse::<u32>().ok()) else { continue };
        if let Some((name, ppid)) = stat_of(pid) {
            m.ppid.insert(pid, ppid);
            m.name.insert(pid, name.to_lowercase());
        }
    }
    m
}

/// (`comm`, parent) for one process. `comm` sits in parentheses and may itself hold spaces or `)`,
/// so the fields after it are found from the last `)`.
#[cfg(target_os = "linux")]
pub fn stat_of(pid: u32) -> Option<(String, u32)> {
    parse_stat(&std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

#[cfg(any(test, target_os = "linux"))]
fn parse_stat(stat: &str) -> Option<(String, u32)> {
    let open = stat.find('(')?;
    let close = stat.rfind(')')?;
    let name = stat.get(open + 1..close)?.to_string();
    let mut rest = stat.get(close + 1..)?.split_whitespace();
    let _state = rest.next()?;
    let ppid = rest.next()?.parse().ok()?;
    Some((name, ppid))
}

/// The process owning the focused window, as the companion extension reports it; 0 when it is not
/// installed, so seen-clears-it simply never fires.
#[cfg(target_os = "linux")]
pub fn fg_pid() -> u32 {
    linux_shell::focused_pid()
}

/// The companion GNOME Shell extension (`linux/gnome-shell-extension/codenotch-focus@codenotch`),
/// over the session bus. Every call is short and fails quietly when the extension is absent.
#[cfg(target_os = "linux")]
pub mod linux_shell {
    use gtk::gio;
    use gtk::glib::{self, ToVariant};

    const NAME: &str = "com.codenotch.Focus";
    const PATH: &str = "/com/codenotch/Focus";
    const TIMEOUT_MS: i32 = 500;

    fn call(method: &str, args: Option<&glib::Variant>) -> Option<glib::Variant> {
        let bus = gio::bus_get_sync(gio::BusType::Session, gio::Cancellable::NONE).ok()?;
        bus.call_sync(
            Some(NAME),
            PATH,
            NAME,
            method,
            args,
            None,
            gio::DBusCallFlags::NO_AUTO_START,
            TIMEOUT_MS,
            gio::Cancellable::NONE,
        )
        .ok()
    }

    pub fn focused_pid() -> u32 {
        call("FocusedPid", None).and_then(|v| v.get::<(u32,)>()).map(|(p,)| p).unwrap_or(0)
    }

    pub fn activate(chain: &[u32], hint: &str) -> bool {
        let args = (chain.to_vec(), hint.to_string()).to_variant();
        call("Activate", Some(&args)).and_then(|v| v.get::<(bool,)>()).map(|(b,)| b).unwrap_or(false)
    }

    /// For the doctor: whether the extension answers at all.
    pub fn available() -> bool {
        call("FocusedPid", None).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_process_name_may_hold_spaces_and_parentheses() {
        assert_eq!(parse_stat("81792 (claude) S 19878 81792 19878"), Some(("claude".into(), 19878)));
        // `comm` is whatever the process set, so the fields after it are found from the last `)`
        assert_eq!(parse_stat("42 (Web Content (x)) R 7 42 7"), Some(("Web Content (x)".into(), 7)));
        assert_eq!(parse_stat("garbage"), None);
        assert_eq!(parse_stat("1 (init) S"), None);
    }

    #[cfg(any(windows, target_os = "linux"))]
    #[test]
    fn a_session_is_seen_from_its_terminal_window() {
        // claude 100 → bash 90 → ptyxis-agent 80 → ptyxis 70 (the window) → systemd 1
        let ppid: std::collections::HashMap<u32, u32> = [(100, 90), (90, 80), (80, 70), (70, 1), (55, 70)].into();
        let maps = ProcMaps { ppid: ppid.clone(), name: Default::default() };
        let chain = chain_of(100, &ppid);
        assert_eq!(chain, [100, 90, 80, 70, 1]);
        assert!(pid_hits_chain(70, &chain, &maps), "the terminal's own window");
        assert!(pid_hits_chain(55, &chain, &maps), "a window whose parent is on the chain");
        assert!(!pid_hits_chain(999, &chain, &maps), "an unrelated window");
    }
}
