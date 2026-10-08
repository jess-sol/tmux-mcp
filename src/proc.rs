/// Live process info from `/proc`. Read on demand, never cached.
///
/// Given a shell PID (from tmux `#{pane_pid}`), derives:
/// - CWD: `readlink /proc/{pid}/cwd`
/// - Process name: `/proc/{pid}/comm`
/// - Foreground process: the terminal's foreground process group

use std::path::PathBuf;

/// Live process info for a shell PID.
#[derive(Debug, Clone)]
pub struct ProcInfo {
    pub pid: u32,
    pub comm: String,
    pub cwd: Option<PathBuf>,
    pub foreground: Option<ForegroundProcess>,
}

/// The leader of the terminal's foreground process group, when the shell has
/// handed the terminal to a job.
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

/// Read `tpgid` from `/proc/{pid}/stat`: the foreground process group of the
/// process's controlling terminal.
fn read_tpgid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // comm is parenthesized and may itself contain spaces or parens.
    let after_comm = &stat[stat.rfind(')')? + 1..];
    // state ppid pgrp session tty_nr tpgid
    after_comm.split_whitespace().nth(5)?.parse().ok()
}

/// The job holding the terminal of the shell `pid`, or `None` while the shell
/// holds it itself (idle at its prompt). The kernel tracks this, so prompt
/// helpers and background jobs — children of the shell outside its
/// foreground group — are never mistaken for it.
pub fn find_foreground(pid: u32) -> Option<ForegroundProcess> {
    let tpgid = read_tpgid(pid)?;
    if tpgid == pid {
        return None;
    }
    let comm = read_comm(tpgid)?;
    Some(ForegroundProcess { pid: tpgid, comm })
}
