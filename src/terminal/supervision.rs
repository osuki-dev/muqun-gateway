//! Which supervisor, if any, manages a given gateway process.
//!
//! Two supervision mechanisms exist for this binary and they do not know
//! about each other: the CLI's own `start`/`stop` (a self-daemonized child
//! plus a pid file), and whatever service manager an operator wired up by
//! hand -- on Linux, typically a systemd unit with `Restart=always`. Left
//! alone, they fight. A real incident shows the shape of the fight: `stop`
//! killed a unit's gateway through the port scan, a hand `start` grabbed the
//! state directory one second later, and the unit then lost the lock race
//! every three seconds for six days -- 179k failed restarts of journal spam,
//! ending in a gateway nobody was supervising.
//!
//! The fix is not to pick a winner here, but to refuse to fight: when the
//! process this command is about to kill, or the process holding the state
//! directory, belongs to a systemd *service*, name the unit and hand the
//! operator the matching `systemctl` command instead. Killing a
//! `Restart=always` service by pid is never what anyone means -- the
//! supervisor immediately undoes it, or worse, loses the lock race to
//! whatever gets started next.
//!
//! Detection reads `/proc/<pid>/cgroup`, which systemd keeps authoritative
//! for every process it manages, and applies two filters:
//!
//! - the *leaf* cgroup must be a `.service`. Every process in a login
//!   session lives somewhere under `user@<uid>.service`, but terminal
//!   children sit in a `.scope` leaf -- only processes systemd itself
//!   started sit in a `.service` leaf.
//! - the unit name must contain `muqun-gateway`. A gateway started through a
//!   Herdr plugin action inherits *Herdr's* service cgroup when Herdr itself
//!   runs as a unit; telling someone to `systemctl restart herdr` to manage
//!   the gateway would be wrong, so such processes keep the normal
//!   kill-by-pid path.
//!
//! macOS has no `/proc` and no systemd; there this module reports nothing
//! and every code path behaves exactly as before. (launchd coexistence is a
//! separate problem for whoever first wires the gateway into launchd.)

/// A systemd service found to be managing a gateway process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemdUnit {
    /// The unit's full name, e.g. `dev.osuki.muqun-gateway.service`.
    pub unit: String,
    /// Whether the unit runs under the user manager (`systemctl --user`)
    /// rather than the system one.
    pub user_manager: bool,
}

impl SystemdUnit {
    /// The exact command an operator should run instead of whatever this
    /// process was about to do by pid.
    pub fn systemctl(&self, verb: &str) -> String {
        if self.user_manager {
            format!("systemctl --user {verb} {}", self.unit)
        } else {
            format!("systemctl {verb} {}", self.unit)
        }
    }
}

/// The systemd service managing `pid`, when that service is a muqun-gateway
/// unit. `None` on other platforms, for unmanaged processes, for processes
/// that died between the caller finding the pid and this reading it, and for
/// services that are not this gateway's.
pub fn managing_gateway_unit(pid: u32) -> Option<SystemdUnit> {
    #[cfg(target_os = "linux")]
    {
        let contents = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
        gateway_unit_from_cgroup(&contents)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = pid;
        None
    }
}

/// Pure parse of `/proc/<pid>/cgroup` contents.
///
/// Only Linux reads a cgroup, so a non-Linux build has no caller for this
/// outside its own tests -- which do run everywhere, and are the reason the
/// parse is a separate function at all. Handles both the unified v2
/// layout (one `0::/...` line) and legacy v1 (one line per controller); the
/// first line whose leaf is a muqun-gateway `.service` wins.
#[cfg(any(target_os = "linux", test))]
fn gateway_unit_from_cgroup(contents: &str) -> Option<SystemdUnit> {
    for line in contents.lines() {
        // "hierarchy:controllers:path" in both v1 and v2; the path itself
        // contains no ':' because systemd escapes unit names into cgroup
        // component names.
        let path = line.splitn(3, ':').nth(2)?;
        let leaf = path.rsplit('/').find(|component| !component.is_empty())?;
        if leaf.ends_with(".service") && leaf.contains("muqun-gateway") {
            return Some(SystemdUnit {
                unit: leaf.to_string(),
                user_manager: path.split('/').any(|part| part.starts_with("user@")),
            });
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The incident case: a unit-managed gateway must be recognised so that
    /// `stop` refuses to kill it and refusals can name the systemctl command.
    #[test]
    fn a_user_service_leaf_names_the_unit_and_the_user_manager() {
        let unit = gateway_unit_from_cgroup(
            "0::/user.slice/user-1000.slice/user@1000.service/app.slice/dev.osuki.muqun-gateway.service\n",
        )
        .expect("a user-manager gateway service went undetected");
        assert_eq!(unit.unit, "dev.osuki.muqun-gateway.service");
        assert!(unit.user_manager);
        assert_eq!(
            unit.systemctl("stop"),
            "systemctl --user stop dev.osuki.muqun-gateway.service"
        );
    }

    /// Every login-session process lives under `user@<uid>.service`; only a
    /// `.service` *leaf* means systemd started it. A terminal-spawned gateway
    /// sits in a `.scope` and must keep the ordinary kill-by-pid path.
    #[test]
    fn a_terminal_scope_is_not_treated_as_managed() {
        assert_eq!(
            gateway_unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/app-alacritty-1234.scope\n",
            ),
            None
        );
    }

    /// A gateway started through a Herdr plugin action runs in *Herdr's*
    /// service cgroup. Advising `systemctl restart` against Herdr's unit
    /// would restart the wrong program, so it must not match.
    #[test]
    fn another_programs_service_cgroup_is_not_claimed() {
        assert_eq!(
            gateway_unit_from_cgroup(
                "0::/user.slice/user-1000.slice/user@1000.service/app.slice/herdr.service\n",
            ),
            None
        );
    }

    /// A system-manager unit produces a hint without `--user`.
    #[test]
    fn a_system_service_hints_without_the_user_flag() {
        let unit = gateway_unit_from_cgroup("0::/system.slice/muqun-gateway.service\n")
            .expect("a system-manager gateway service went undetected");
        assert!(!unit.user_manager);
        assert_eq!(
            unit.systemctl("restart"),
            "systemctl restart muqun-gateway.service"
        );
    }

    /// Legacy cgroup v1 mounts one line per controller; the unit must still
    /// be found among them.
    #[test]
    fn cgroup_v1_controller_lines_are_parsed() {
        let contents = "12:pids:/user.slice/user-1000.slice/user@1000.service/dev.osuki.muqun-gateway.service\n\
                        11:cpu,cpuacct:/user.slice\n\
                        1:name=systemd:/user.slice/user-1000.slice/user@1000.service/dev.osuki.muqun-gateway.service\n";
        let unit = gateway_unit_from_cgroup(contents).expect("v1 layout went undetected");
        assert_eq!(unit.unit, "dev.osuki.muqun-gateway.service");
        assert!(unit.user_manager);
    }

    /// Garbage in `/proc` (or an empty file for a raced pid) must never panic
    /// or misreport; it just means "not managed".
    #[test]
    fn malformed_contents_report_nothing() {
        assert_eq!(gateway_unit_from_cgroup(""), None);
        assert_eq!(gateway_unit_from_cgroup("not a cgroup line"), None);
        assert_eq!(gateway_unit_from_cgroup("0::/"), None);
    }
}
