//! LaunchAgent transitions through `launchctl`, shared by install, the
//! lifecycle commands and the updater.
//!
//! One rule matters more than the rest: **a transition never leaves the agent
//! unloaded when it was meant to be running.** `bootout` can return while
//! launchd is still tearing the old job down, and a `bootstrap` in that window
//! fails with "5: Input/output error" (or reports the label as still loaded).
//! v0.12.1's installer was left stopped by exactly that; 1dcbdf3 taught
//! `service install` to retry, and every other path that boots the agent back
//! in -- `start`, `restart`, the updater's start and its recovery start -- goes
//! through the same retrying bootstrap here. A restart whose bootout or
//! bootstrap still fails falls back to `kickstart -k` on whatever launchd has
//! loaded, so the gateway keeps running under its previous definition rather
//! than not at all.
//!
//! Compiled on Linux under `cfg(test)` so the transitions run against a fake
//! `launchctl` in CI, not only on a Mac.

use std::path::PathBuf;
use std::process::{Command, Output, Stdio};
use std::time::Duration;

use anyhow::{Context as _, Result};

use super::service::{ServiceAction, SERVICE_LABEL};

/// How a launchd transition waits and retries. Tests shrink every interval.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Timing {
    /// How long a stop waits for the old job to be gone. Past launchd's own
    /// `ExitTimeOut` (5 s by default, when SIGTERM becomes SIGKILL), with room.
    pub bootout_wait: Duration,
    pub poll: Duration,
    /// Bootstrap attempts before giving up.
    pub attempts: u32,
    /// The first retry delay; each later one doubles, up to `max_delay`.
    pub first_delay: Duration,
    pub max_delay: Duration,
}

impl Timing {
    pub(crate) const LAUNCHD: Self = Self {
        bootout_wait: Duration::from_secs(15),
        poll: Duration::from_millis(100),
        attempts: 10,
        first_delay: Duration::from_millis(250),
        max_delay: Duration::from_secs(1),
    };

    fn delay(&self, attempt: u32) -> Duration {
        self.first_delay
            .saturating_mul(1 << attempt.saturating_sub(1).min(16))
            .min(self.max_delay)
    }
}

/// One LaunchAgent in one launchd domain.
pub(crate) struct Launchctl {
    pub program: PathBuf,
    pub domain: String,
    pub plist: PathBuf,
    pub timing: Timing,
}

impl Launchctl {
    /// The real agent: `gui/<uid>/dev.osuki.muqun-gateway`.
    #[cfg(target_os = "macos")]
    pub(crate) fn system() -> Result<Self> {
        Ok(Self {
            program: PathBuf::from("launchctl"),
            // `unsafe` only because getuid is FFI; it cannot fail.
            domain: format!("gui/{}", unsafe { libc::getuid() }),
            plist: super::service::unit_path()?,
            timing: Timing::LAUNCHD,
        })
    }

    pub(crate) fn target(&self) -> String {
        format!("{}/{SERVICE_LABEL}", self.domain)
    }

    fn run(&self, args: &[&str]) -> Result<Output> {
        Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .output()
            .with_context(|| format!("failed to run launchctl {}", args.join(" ")))
    }

    fn checked(&self, args: &[&str]) -> Result<()> {
        let output = self.run(args)?;
        anyhow::ensure!(
            output.status.success(),
            "launchctl {} failed ({}): {}",
            args.join(" "),
            output.status,
            stderr(&output)
        );
        Ok(())
    }

    /// `launchctl print` of the job, when launchd has it loaded.
    pub(crate) fn print(&self) -> Option<String> {
        let output = self.run(&["print", &self.target()]).ok()?;
        output
            .status
            .success()
            .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub(crate) fn loaded(&self) -> bool {
        self.print().is_some()
    }

    pub(crate) fn loaded_pid(&self) -> Option<u32> {
        parse_launchd_pid(&self.print()?)
    }

    fn wait_until(&self, limit: Duration, mut done: impl FnMut() -> bool) -> bool {
        let deadline = std::time::Instant::now() + limit;
        loop {
            if done() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(self.timing.poll);
        }
    }

    /// Boot the job out and wait until launchd has released the label and the
    /// old process is gone. `Ok(false)` means bootout returned but the job
    /// outlived the wait.
    fn bootout(&self) -> Result<bool> {
        let pid = self.loaded_pid();
        self.checked(&["bootout", &self.target()])?;
        Ok(self.wait_until(self.timing.bootout_wait, || {
            !self.loaded() && !pid.is_some_and(process_alive)
        }))
    }

    /// `bootstrap`, retried while launchd is still releasing a previous job.
    pub(crate) fn bootstrap(&self) -> Result<()> {
        let plist = self.plist.display().to_string();
        let mut last_error = String::new();
        for attempt in 1..=self.timing.attempts {
            let output = self.run(&["bootstrap", &self.domain, &plist])?;
            if output.status.success() {
                return Ok(());
            }
            last_error = format!("{} ({})", stderr(&output), output.status);
            // Anything else -- a plist launchd will not parse, no GUI session
            // to load into -- will not change by waiting, so report it now.
            if !retryable(&output) {
                anyhow::bail!("launchctl bootstrap failed: {last_error}");
            }
            if attempt < self.timing.attempts {
                std::thread::sleep(self.timing.delay(attempt));
            }
        }
        anyhow::bail!(
            "launchctl bootstrap failed after {} attempts: {last_error}; \
             launchd has not released the previous gateway yet",
            self.timing.attempts
        )
    }

    /// Reinstall over whatever is loaded: bootout (unchecked; usually nothing
    /// is loaded), wait, bootstrap with retries.
    pub(crate) fn reload(&self) -> Result<()> {
        let pid = self.loaded_pid();
        let _ = self.run(&["bootout", &self.target()]);
        self.wait_until(self.timing.bootout_wait, || {
            !self.loaded() && !pid.is_some_and(process_alive)
        });
        self.bootstrap()
    }

    pub(crate) fn control(&self, action: ServiceAction) -> Result<()> {
        if action == ServiceAction::Stop {
            if self.loaded() {
                anyhow::ensure!(self.bootout()?, "launchd has not stopped the gateway yet");
            }
            return Ok(());
        }
        if action == ServiceAction::Restart && self.loaded() {
            // Still loaded after a failed or slow bootout: restart the job
            // launchd has rather than leave it to a bootstrap that cannot
            // succeed. Unloaded either way: bootstrap below.
            if !self.bootout().unwrap_or(false) && self.loaded() {
                return self.kickstart(true);
            }
        } else if action == ServiceAction::Start && self.loaded() {
            return self.kickstart(false);
        }
        match self.bootstrap() {
            Ok(()) => Ok(()),
            // The last attempt may have lost only to the old job lingering;
            // if launchd has a job loaded now, run that one.
            Err(_) if self.loaded() => self.kickstart(action == ServiceAction::Restart),
            Err(error) => Err(error).context(
                "the gateway LaunchAgent is not loaded; run `muqun-gateway start` to retry",
            ),
        }
    }

    fn kickstart(&self, kill: bool) -> Result<()> {
        let target = self.target();
        if kill {
            self.checked(&["kickstart", "-k", &target])
        } else {
            self.checked(&["kickstart", &target])
        }
    }
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr)
        .chars()
        .take(512)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// launchd still holding the previous job: EIO (exit 5), or the label not yet
/// released ("service already loaded").
fn retryable(output: &Output) -> bool {
    let text = String::from_utf8_lossy(&output.stderr);
    output.status.code() == Some(5)
        || text.contains("Input/output error")
        || text.contains("already loaded")
}

/// The job's own `pid = N` line. Only the top-level one: nested sections
/// (endpoints, spawn info) are indented further and are not the job's pid.
pub(crate) fn parse_launchd_pid(print: &str) -> Option<u32> {
    print
        .lines()
        .find_map(|line| line.strip_prefix("\tpid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

pub(crate) fn process_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 only checks that the pid exists and may be signalled.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// A scriptable `launchctl` for tests. Every call is appended to `calls`; the
/// job counts as loaded while the `loaded` file exists. `bootstrap`, `bootout`
/// and `kickstart` take their exit codes, one per call, from files of the same
/// name (missing or exhausted means 0).
#[cfg(all(test, unix))]
pub(crate) mod fake {
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    const SCRIPT: &str = r#"#!/bin/sh
dir=$(dirname "$0")
[ "$1" = ready ] && exit 0
printf '%s\n' "$*" >> "$dir/calls"
next() {
  code=0
  if [ -s "$dir/$1" ]; then
    code=$(head -n 1 "$dir/$1")
    tail -n +2 "$dir/$1" > "$dir/$1.rest"
    mv "$dir/$1.rest" "$dir/$1"
  fi
}
case "$1" in
  print)
    [ -f "$dir/loaded" ] || { echo 'Could not find service' >&2; exit 113; }
    printf '%s = {\n\tstate = running\n\tprogram = %s\n' "$2" "$(cat "$dir/program" 2>/dev/null)"
    [ -s "$dir/pid" ] && printf '\tpid = %s\n' "$(cat "$dir/pid")"
    echo '}' ;;
  bootout)
    next bootout
    [ "$code" = 0 ] || { echo "Boot-out failed: $code: Operation not permitted" >&2; exit "$code"; }
    rm -f "$dir/loaded" ;;
  bootstrap)
    next bootstrap
    case "$code" in
      0) touch "$dir/loaded" ;;
      5) echo 'Bootstrap failed: 5: Input/output error' >&2; exit 5 ;;
      *) echo "Bootstrap failed: $code: Invalid property list" >&2; exit "$code" ;;
    esac ;;
  kickstart)
    [ -f "$dir/loaded" ] || { echo 'Could not find service' >&2; exit 113; }
    next kickstart
    exit "$code" ;;
esac
"#;

    pub(crate) struct Fake {
        pub dir: PathBuf,
    }

    impl Fake {
        pub(crate) fn new() -> Self {
            let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("target/launchctl-fixtures")
                .join(uuid::Uuid::new_v4().to_string());
            std::fs::create_dir_all(&dir).unwrap();
            let program = dir.join("launchctl");
            std::fs::write(&program, SCRIPT).unwrap();
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o700)).unwrap();
            // Another test thread forking while the script was open for
            // writing holds that descriptor until its exec, and running the
            // script meanwhile fails with ETXTBSY. Wait that out once here.
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            while Command::new(&program).arg("ready").status().is_err() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "fake launchctl never became executable"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Self { dir }
        }

        pub(crate) fn launchctl(&self) -> Launchctl {
            Launchctl {
                program: self.dir.join("launchctl"),
                domain: String::from("gui/501"),
                plist: self.dir.join("agent.plist"),
                timing: Timing {
                    bootout_wait: Duration::from_millis(50),
                    poll: Duration::from_millis(5),
                    attempts: 10,
                    first_delay: Duration::ZERO,
                    max_delay: Duration::ZERO,
                },
            }
        }

        pub(crate) fn set(&self, name: &str, contents: &str) {
            std::fs::write(self.dir.join(name), contents).unwrap();
        }

        pub(crate) fn loaded(&self) -> bool {
            self.dir.join("loaded").exists()
        }

        pub(crate) fn load(&self) {
            self.set("loaded", "");
        }

        /// The subcommands run, in order.
        pub(crate) fn calls(&self) -> Vec<String> {
            std::fs::read_to_string(self.dir.join("calls"))
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.starts_with("print "))
                .map(|line| {
                    line.split_whitespace()
                        .take_while(|word| !word.starts_with("gui/"))
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .collect()
        }
    }

    impl Drop for Fake {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::fake::Fake;
    use super::*;

    #[test]
    fn the_job_pid_is_read_from_its_own_line_and_nowhere_else() {
        let print = "gui/501/dev.osuki.muqun-gateway = {\n\
                     \tactive count = 1\n\
                     \tstate = running\n\
                     \tendpoints = {\n\
                     \t\tpid = 7\n\
                     \t}\n\
                     \tpid = 99542\n\
                     }\n";
        assert_eq!(parse_launchd_pid(print), Some(99542));
        assert_eq!(parse_launchd_pid("\tstate = not running\n"), None);
    }

    #[test]
    fn start_retries_a_bootstrap_launchd_refuses_with_eio_until_it_succeeds() {
        let fake = Fake::new();
        fake.set("bootstrap", "5\n5\n5\n0\n");
        fake.launchctl().control(ServiceAction::Start).unwrap();
        assert!(fake.loaded());
        assert_eq!(fake.calls(), ["bootstrap"; 4]);
    }

    #[test]
    fn restart_boots_out_then_retries_bootstrap_through_the_eio_window() {
        let fake = Fake::new();
        fake.load();
        fake.set("bootstrap", "5\n5\n0\n");
        fake.launchctl().control(ServiceAction::Restart).unwrap();
        assert!(fake.loaded());
        assert_eq!(
            fake.calls(),
            ["bootout", "bootstrap", "bootstrap", "bootstrap"]
        );
    }

    #[test]
    fn a_bootstrap_error_that_waiting_cannot_fix_is_reported_at_once() {
        let fake = Fake::new();
        fake.set("bootstrap", "78\n");
        let error = fake.launchctl().control(ServiceAction::Start).unwrap_err();
        assert!(
            format!("{error:#}").contains("Invalid property list"),
            "{error:#}"
        );
        assert_eq!(fake.calls(), ["bootstrap"]);
    }

    #[test]
    fn retries_are_bounded_and_say_launchd_still_holds_the_old_job() {
        let fake = Fake::new();
        fake.set("bootstrap", &"5\n".repeat(20));
        let error = fake.launchctl().control(ServiceAction::Start).unwrap_err();
        assert!(
            format!("{error:#}").contains("after 10 attempts"),
            "{error:#}"
        );
        assert_eq!(fake.calls().len(), 10);
        assert!(!fake.loaded());
    }

    #[test]
    fn restart_falls_back_to_kickstart_when_bootout_fails_and_never_unloads() {
        let fake = Fake::new();
        fake.load();
        fake.set("bootout", "1\n");
        fake.launchctl().control(ServiceAction::Restart).unwrap();
        assert!(fake.loaded());
        assert_eq!(fake.calls(), ["bootout", "kickstart -k"]);
    }

    #[test]
    fn stop_reports_a_failed_bootout_and_leaves_the_job_alone() {
        let fake = Fake::new();
        fake.load();
        fake.set("bootout", "1\n");
        assert!(fake.launchctl().control(ServiceAction::Stop).is_err());
        assert!(fake.loaded());
        assert_eq!(fake.calls(), ["bootout"]);
    }

    #[test]
    fn start_kickstarts_a_loaded_job_and_stop_of_an_unloaded_one_is_a_no_op() {
        let fake = Fake::new();
        fake.launchctl().control(ServiceAction::Stop).unwrap();
        fake.load();
        fake.launchctl().control(ServiceAction::Start).unwrap();
        assert_eq!(fake.calls(), ["kickstart"]);
        fake.launchctl().control(ServiceAction::Stop).unwrap();
        assert!(!fake.loaded());
    }

    #[test]
    fn reload_retries_like_every_other_bootstrap() {
        let fake = Fake::new();
        fake.load();
        fake.set("bootout", "0\n");
        fake.set("bootstrap", "5\n0\n");
        fake.launchctl().reload().unwrap();
        assert!(fake.loaded());
        assert_eq!(fake.calls(), ["bootout", "bootstrap", "bootstrap"]);
    }

    #[test]
    fn backoff_doubles_up_to_its_ceiling() {
        let timing = Timing::LAUNCHD;
        assert_eq!(timing.delay(1), Duration::from_millis(250));
        assert_eq!(timing.delay(2), Duration::from_millis(500));
        assert_eq!(timing.delay(3), Duration::from_secs(1));
        assert_eq!(timing.delay(9), Duration::from_secs(1));
    }
}
