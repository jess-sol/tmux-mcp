/// Live process info from `/proc`. Read on demand, never cached.
///
/// Given a shell PID (from tmux `#{pane_pid}`), derives:
/// - CWD: `readlink /proc/{pid}/cwd`
/// - Process name: `/proc/{pid}/comm`
/// - Foreground process: walk single-child chains to find the leaf

use std::path::PathBuf;

/// Live process info for a shell PID.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub comm: String,
    pub cwd: Option<PathBuf>,
    pub foreground: Option<ForegroundProcess>,
}

/// The leaf process in a single-child chain from the shell.
#[derive(Debug, Clone)]
pub struct ForegroundProcess {
    pub pid: u32,
    pub comm: String,
}

/// Read live process info for a PID.
/// Returns `None` if the process doesn't exist.
pub fn proc_info(pid: u32) -> Option<ProcInfo> {
    let comm = read_comm(pid)?;
    let cwd = read_cwd(pid);
    let foreground = find_foreground(pid);

    Some(ProcInfo { pid, comm, cwd, foreground })
}

/// Read `/proc/{pid}/comm` — the process name (max 16 chars, kernel-truncated).
fn read_comm(pid: u32) -> Option<String> {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim().to_string())
}

/// Read `/proc/{pid}/cwd` — symlink to the current working directory.
fn read_cwd(pid: u32) -> Option<PathBuf> {
    std::fs::read_link(format!("/proc/{pid}/cwd")).ok()
}

/// Read `/proc/{pid}/task/{pid}/children` — space-separated child PIDs.
fn read_children(pid: u32) -> Vec<u32> {
    let path = format!("/proc/{pid}/task/{pid}/children");
    let Ok(contents) = std::fs::read_to_string(&path) else {
        return vec![];
    };
    contents
        .split_whitespace()
        .filter_map(|s| s.parse().ok())
        .collect()
}

/// Walk single-child chains to find the foreground process.
///
/// Starting from `pid`, if it has exactly one child, recurse into that child.
/// Continue until a process has 0 or 2+ children. If we moved at all from
/// the starting pid, return the leaf. Returns `None` if the process has
/// no children (shell is idle at prompt).
pub fn find_foreground(pid: u32) -> Option<ForegroundProcess> {
    let mut current = pid;

    loop {
        let children = read_children(current);
        match children.len() {
            1 => current = children[0],
            _ => break,
        }
    }

    if current == pid {
        // Never moved — shell has no children (idle) or multiple children
        return None;
    }

    let comm = read_comm(current)?;
    Some(ForegroundProcess { pid: current, comm })
}
