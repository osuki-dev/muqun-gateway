//! Finishing the job when OpenCode's `DELETE /api/shell/{id}` only pretends.
//!
//! OpenCode answers 204 and leaves the record `running` when the shell's
//! process has been reparented (the wrapping bash exited and the real child
//! now hangs off the OpenCode service). The Gateway shares the host, so it
//! signals that pid itself -- but only after proving the pid belongs to the
//! shell, because a stale or reused pid must never be killed.

use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Signal {
    Term,
    Kill,
}

/// What `/proc/<pid>` says about a process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub ppid: u32,
    /// `/proc/<pid>/cmdline` with NULs turned into spaces.
    pub cmdline: String,
}

pub fn shell_pid(shell: &Value) -> Option<u32> {
    shell
        .get("pid")
        .and_then(Value::as_u64)
        .and_then(|p| u32::try_from(p).ok())
        .filter(|p| *p > 1 && i32::try_from(*p).is_ok())
}

pub fn is_running(shell: &Value) -> bool {
    shell.get("status").and_then(Value::as_str) == Some("running")
}

/// Decide whether a shell that survived `DELETE` should be signalled.
///
/// `Some(Signal::Term)` only when the shell is still `running`, has a numeric
/// pid, and the process is a plausible child: its parent is the OpenCode
/// service, or its command line starts like the shell's `command`.
pub fn should_force_kill(
    shell_after_delete: &Value,
    service_pid: Option<u32>,
    proc_info: Option<&ProcInfo>,
) -> Option<Signal> {
    if !is_running(shell_after_delete) {
        return None;
    }
    let pid = shell_pid(shell_after_delete)?;
    if pid == std::process::id() || service_pid == Some(pid) {
        return None;
    }
    let info = proc_info?;
    let parent_matches = service_pid.is_some_and(|s| s == info.ppid);
    let command = shell_after_delete
        .get("command")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let cmdline = info.cmdline.trim();
    let cmd_matches = !command.is_empty()
        && !cmdline.is_empty()
        && (cmdline.starts_with(command) || command.starts_with(cmdline));
    (parent_matches || cmd_matches).then_some(Signal::Term)
}

#[cfg(target_os = "linux")]
pub fn read_proc_info(pid: u32) -> Option<ProcInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // `pid (comm) state ppid ...`; comm may contain spaces and parens, so
    // split after the last `)`.
    let rest = stat.rsplit_once(')')?.1;
    let ppid = rest.split_whitespace().nth(1)?.parse().ok()?;
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let cmdline = String::from_utf8_lossy(&raw)
        .split('\0')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    Some(ProcInfo { ppid, cmdline })
}

#[cfg(not(target_os = "linux"))]
pub fn read_proc_info(_pid: u32) -> Option<ProcInfo> {
    None
}

pub fn send_signal(pid: u32, signal: Signal) -> bool {
    let sig = match signal {
        Signal::Term => libc::SIGTERM,
        Signal::Kill => libc::SIGKILL,
    };
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: kill(2) takes plain integers and has no memory-safety contract.
    unsafe { libc::kill(pid, sig) == 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn running() -> Value {
        json!({"id": "shl_1", "status": "running", "pid": 3000714,
               "command": "mkdir x; bun -e 'Bun.serve({})'"})
    }

    fn info(ppid: u32, cmdline: &str) -> ProcInfo {
        ProcInfo {
            ppid,
            cmdline: cmdline.into(),
        }
    }

    #[test]
    fn running_child_of_the_service_is_terminated() {
        let got = should_force_kill(&running(), Some(1234), Some(&info(1234, "bun -e x")));
        assert_eq!(got, Some(Signal::Term));
    }

    #[test]
    fn matching_command_line_is_enough_without_a_service_pid() {
        let got = should_force_kill(
            &running(),
            None,
            Some(&info(77, "mkdir x; bun -e 'Bun.serve({})'")),
        );
        assert_eq!(got, Some(Signal::Term));
    }

    #[test]
    fn exited_shell_is_left_alone() {
        let mut shell = running();
        shell["status"] = json!("exited");
        assert_eq!(
            should_force_kill(&shell, Some(1234), Some(&info(1234, "bun"))),
            None
        );
    }

    #[test]
    fn foreign_parent_and_unrelated_command_is_never_killed() {
        let got = should_force_kill(&running(), Some(1234), Some(&info(999, "nginx -g daemon")));
        assert_eq!(got, None);
    }

    #[test]
    fn missing_pid_or_proc_info_is_never_killed() {
        let mut shell = running();
        shell.as_object_mut().unwrap().remove("pid");
        assert_eq!(
            should_force_kill(&shell, Some(1234), Some(&info(1234, "bun"))),
            None
        );
        assert_eq!(should_force_kill(&running(), Some(1234), None), None);
    }

    #[test]
    fn the_service_itself_and_pid_one_are_refused() {
        let svc = json!({"status": "running", "pid": 1234, "command": "x"});
        assert_eq!(
            should_force_kill(&svc, Some(1234), Some(&info(1234, "x"))),
            None
        );
        let init = json!({"status": "running", "pid": 1, "command": "x"});
        assert_eq!(should_force_kill(&init, None, Some(&info(1, "x"))), None);
    }
}
