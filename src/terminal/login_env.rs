//! The environment the user actually has, for a gateway an init system started.
//!
//! A gateway started from a shell inherits that shell's environment. A gateway
//! started by launchd or systemd inherits almost nothing, and the two variables
//! it does not inherit are both load-bearing here.
//!
//! ## `PATH`
//!
//! Every external program this gateway runs -- `tmux`, `git` for worktrees, and
//! each agent executable in the catalog -- is spawned by name and found through
//! `PATH`. launchd hands a user agent `/usr/bin:/bin:/usr/sbin:/sbin` and
//! nothing else, and `/opt/homebrew/bin` is not on that list, so on a Mac where
//! tmux came from Homebrew -- which is nearly every Mac -- the tmux adapter
//! cannot spawn tmux at all the moment the gateway stops being started from a
//! shell.
//!
//! ## `LC_CTYPE`
//!
//! And once tmux *can* be spawned, the second half. tmux with no UTF-8 locale
//! replaces every byte it will not print with `_`, in its own output, including
//! the `\u{1f}` this adapter joins its `-F` fields with. So the format string
//! goes out with separators and the answer comes back as one field:
//!
//! ```text
//! $0\u{1f}0\u{1f}2        with a UTF-8 LC_CTYPE
//! $0_0_2                  with none
//! ```
//!
//! Every list parse then fails with "terminal backend returned an invalid
//! session", which is true and useless. Non-ASCII pane titles and working
//! directories are mangled the same way and, unlike the separator, silently:
//! a pane titled `✳ 修复 gateway` arrives as `_ __ _______`.
//!
//! ## Two halves, because either one alone leaves a hole
//!
//! [`for_unit_file`] is written into the LaunchAgent and the systemd unit at
//! install time, so the supervised process starts with the user's own
//! environment and that environment is a thing you can read in a file rather
//! than infer.
//!
//! [`adopt`] repairs the running process's environment at startup regardless.
//! That is what covers the installs that already exist: upgrading the binary
//! does not rewrite a unit file, and asking every user to re-run `service
//! install` to collect a fix they never knew they needed is not a fix. It also
//! covers the init systems this project has not met yet.
//!
//! ## Why a login shell, and not a list of likely values
//!
//! Because `/opt/homebrew/bin` is a guess, and the next machine keeps its tools
//! somewhere else. A login shell answers exactly: on macOS `/etc/zprofile` runs
//! `path_helper`, which reads `/etc/paths` and every file in `/etc/paths.d` --
//! Homebrew installs one there -- and on Linux the distribution's own profile
//! does the equivalent. One subprocess at startup, and nobody's package manager
//! hard-coded.
//!
//! Only `-l`, never `-i`: a login shell reads the profile, which is where these
//! belong, and an interactive one additionally reads `.zshrc`/`.bashrc`, which
//! is where prompts, completions and version managers live. Those are slow,
//! occasionally interactive, and not this process's business.

use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// How long the login shell gets to answer before it is killed and ignored.
///
/// A profile is normally a few milliseconds. This bound is not for the slow
/// case, it is for the wedged one: a profile that blocks on a prompt, a stale
/// network mount, or a version manager waiting on a lock would otherwise hang a
/// daemon that has not opened its listener yet, and it would hang it at boot,
/// where nobody is watching. Losing the answer degrades one backend; never
/// finishing startup loses the whole gateway.
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

const PATH_FENCE: &str = "__muqun_path__";
const CTYPE_FENCE: &str = "__muqun_ctype__";

/// The character encoding to fall back on when the user's own environment names
/// none.
///
/// Any UTF-8 locale will do -- tmux only consults it to decide whether a byte
/// is printable -- so this picks the spelling each platform is sure to have.
/// macOS accepts the bare `UTF-8`; `C.UTF-8` is the one Linux distributions
/// ship without a locale package installed.
const FALLBACK_CTYPE: &str = if cfg!(target_os = "macos") {
    "UTF-8"
} else {
    "C.UTF-8"
};

/// What a login shell reports about the environment this gateway needs.
#[derive(Debug, Default, PartialEq, Eq)]
struct LoginEnvironment {
    path: Option<String>,
    ctype: Option<String>,
}

/// Ask a login shell of the user's own shell what it has.
///
/// `None` -- or a field left `None` -- when there is no shell to ask, it fails,
/// it takes too long, or it says nothing useful. Every one of those is a reason
/// to keep what is already in hand, never a reason to fail.
fn probe_login_shell() -> Option<LoginEnvironment> {
    let shell = probe_shell_for(std::env::var("SHELL").ok().as_deref());
    // `printf` rather than `echo`: `echo` is a builtin with three incompatible
    // dialects across shells, and one of them would eat a `\` in a directory
    // name. The locale is whichever of the three the shell would itself obey,
    // in the order the C library resolves them.
    let script = format!(
        "printf '\\n{PATH_FENCE}%s\\n{CTYPE_FENCE}%s\\n' \
         \"$PATH\" \"${{LC_ALL:-${{LC_CTYPE:-$LANG}}}}\""
    );
    let mut child = Command::new(shell)
        .arg("-lc")
        .arg(script)
        // A profile that tries to read from the terminal gets EOF and moves on,
        // instead of blocking until the timeout for a keystroke nobody is there
        // to type.
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;

    let deadline = Instant::now() + PROBE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            // Timed out, or the wait itself failed. Kill it and take the answer
            // we do not have: half an environment is worse than none.
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }

    let mut stdout = String::new();
    child.stdout.take()?.read_to_string(&mut stdout).ok()?;
    Some(parse_probe(&stdout))
}

/// Which shell to ask.
///
/// `$SHELL` when it speaks POSIX, because that is where the user's own profile
/// lives. Otherwise `/bin/sh`: the probe script is POSIX -- `${LC_ALL:-…}` is
/// a syntax error to fish, which would answer with nothing and cost a fish user
/// the whole repair -- and `/bin/sh -l` still reads `/etc/profile`, which on
/// macOS runs the same `path_helper` that puts Homebrew on the path.
fn probe_shell_for(shell: Option<&str>) -> String {
    const POSIX_SHELLS: &[&str] = &["sh", "bash", "zsh", "ksh", "mksh", "dash", "ash"];
    match shell {
        Some(shell) if POSIX_SHELLS.contains(&shell.rsplit('/').next().unwrap_or_default()) => {
            shell.to_owned()
        }
        _ => "/bin/sh".to_owned(),
    }
}

/// The fenced values out of whatever the profile printed.
///
/// Fenced, because `.zprofile` files print things -- version banners, a warning
/// about a missing tool, `fortune` -- and that output arrives on the same
/// stdout. Reading "the last line" would work until the day a profile ends with
/// an `echo`.
fn parse_probe(output: &str) -> LoginEnvironment {
    let fenced = |fence: &str| {
        output
            .lines()
            .find_map(|line| line.strip_prefix(fence))
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
    };
    LoginEnvironment {
        path: fenced(PATH_FENCE),
        ctype: fenced(CTYPE_FENCE),
    }
}

/// `current` with everything from `extra` it does not already have, appended.
///
/// Appended and not prepended, and order otherwise preserved: the environment
/// the process was given decides which `git` wins, because someone who put a
/// directory ahead of another meant it. This only ever adds a way to find a
/// program that could not be found at all.
pub fn merged(current: &str, extra: &str) -> String {
    let mut entries: Vec<&str> = current
        .split(':')
        .filter(|entry| !entry.is_empty())
        .collect();
    for entry in extra.split(':').filter(|entry| !entry.is_empty()) {
        if !entries.contains(&entry) {
            entries.push(entry);
        }
    }
    entries.join(":")
}

/// Whether a locale name means UTF-8.
///
/// Spelled every way the platforms spell it: `en_US.UTF-8`, `C.utf8`, and the
/// bare `UTF-8` macOS accepts. Anything else -- `C`, `POSIX`, an empty string,
/// a latin-1 locale -- is a locale under which tmux will replace bytes.
fn is_utf8(locale: &str) -> bool {
    let upper = locale.to_ascii_uppercase().replace('-', "");
    upper.contains("UTF8")
}

/// The character encoding already in force, if it is one that works.
fn effective_ctype() -> Option<String> {
    // The order the C library resolves them in, so this agrees with what tmux
    // will actually see.
    ["LC_ALL", "LC_CTYPE", "LANG"]
        .into_iter()
        .find_map(|name| std::env::var(name).ok().filter(|value| !value.is_empty()))
        .filter(|value| is_utf8(value))
}

/// What to write into a unit file for a service installed right now.
///
/// These are the installing process's own values, because [`adopt`] has already
/// repaired them before any subcommand runs. Naming them separately is not
/// ceremony: what goes into a unit file outlives the shell that wrote it, and a
/// reader of `service.rs` should see where the values came from rather than an
/// anonymous environment read.
pub fn for_unit_file() -> (String, String) {
    (
        std::env::var("PATH").unwrap_or_default(),
        effective_ctype().unwrap_or_else(|| FALLBACK_CTYPE.to_owned()),
    )
}

/// Repair this process's environment, and say what that changed.
///
/// The returned notes are empty every time the gateway was started from a
/// terminal, because a shell has already given it everything. The caller logs
/// them, so a normal start stays quiet and a supervised one says exactly what
/// it had to put right.
pub fn adopt() -> Vec<String> {
    if std::env::var("MUQUN_NO_LOGIN_ENV").is_ok() {
        return Vec::new();
    }
    let mut notes = Vec::new();
    let login = probe_login_shell().unwrap_or_default();

    let current = std::env::var("PATH").unwrap_or_default();
    if let Some(from_shell) = login.path {
        let widened = merged(&current, &from_shell);
        if widened != current {
            notes.push(format!("PATH: added {}", added_entries(&current, &widened)));
            std::env::set_var("PATH", &widened);
        }
    }

    if effective_ctype().is_none() {
        // The login shell's own, when it has a usable one, so a machine that
        // works in Chinese keeps working in Chinese. Otherwise any UTF-8
        // locale: this is set to stop tmux mangling bytes, not to choose a
        // language.
        let ctype = login
            .ctype
            .filter(|value| is_utf8(value))
            .unwrap_or_else(|| FALLBACK_CTYPE.to_owned());
        notes.push(format!(
            "LC_CTYPE={ctype}: nothing in this environment named a UTF-8 locale, \
             and tmux replaces every byte it will not print"
        ));
        std::env::set_var("LC_CTYPE", &ctype);
    }

    notes
}

/// What widening `PATH` gained, for the one log line it writes.
fn added_entries(before: &str, after: &str) -> String {
    let had: Vec<&str> = before.split(':').collect();
    after
        .split(':')
        .filter(|entry| !entry.is_empty() && !had.contains(entry))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Whether `program` can be found on `path`, and where.
///
/// Used to report a missing backend executable in terms the reader can act on,
/// rather than leaving them with "unavailable" and no idea which of the two
/// halves -- the program, or this process's view of the filesystem -- is the
/// one that is wrong.
pub fn lookup(program: &str, path: &str) -> Option<PathBuf> {
    // An explicit path is not a `PATH` search at all; honour it as written so a
    // configured `/opt/tmux/bin/tmux` is never silently replaced by another one.
    if program.contains('/') {
        let candidate = PathBuf::from(program);
        return is_executable(&candidate).then_some(candidate);
    }
    path.split(':')
        .filter(|entry| !entry.is_empty())
        .map(|entry| PathBuf::from(entry).join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(candidate: &std::path::Path) -> bool {
    let Ok(metadata) = std::fs::metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_chatty_profile_does_not_become_the_environment() {
        // The reason the values are fenced: a profile that ends in an `echo` is
        // ordinary, and "the last line of stdout" would silently become a
        // greeting instead of a PATH.
        let output = format!(
            "Welcome back!\n\n{PATH_FENCE}/opt/homebrew/bin:/usr/bin\n\
             {CTYPE_FENCE}zh_CN.UTF-8\nnode: v22.1.0\n"
        );
        assert_eq!(
            parse_probe(&output),
            LoginEnvironment {
                path: Some("/opt/homebrew/bin:/usr/bin".into()),
                ctype: Some("zh_CN.UTF-8".into()),
            }
        );
    }

    #[test]
    fn a_profile_that_answers_with_nothing_yields_nothing() {
        assert_eq!(parse_probe(""), LoginEnvironment::default());
        assert_eq!(
            parse_probe("bash: /etc/profile: permission denied\n"),
            LoginEnvironment::default()
        );
        // A fence with nothing behind it is not an answer either: adopting an
        // empty PATH would take away the four directories launchd did give us,
        // and an empty locale is the broken case this module exists for.
        assert_eq!(
            parse_probe(&format!("{PATH_FENCE}\n{CTYPE_FENCE}\n")),
            LoginEnvironment::default()
        );
    }

    #[test]
    fn only_a_posix_shell_is_asked_and_anything_else_falls_back_to_sh() {
        // fish is the one people actually use, and `${LC_ALL:-…}` is a syntax
        // error to it. Asking it would look like a machine with no profile.
        assert_eq!(probe_shell_for(Some("/opt/homebrew/bin/fish")), "/bin/sh");
        assert_eq!(probe_shell_for(Some("/usr/bin/nu")), "/bin/sh");
        assert_eq!(probe_shell_for(None), "/bin/sh");
        assert_eq!(probe_shell_for(Some("")), "/bin/sh");
        assert_eq!(probe_shell_for(Some("/bin/zsh")), "/bin/zsh");
        assert_eq!(
            probe_shell_for(Some("/opt/homebrew/bin/bash")),
            "/opt/homebrew/bin/bash"
        );
    }

    #[test]
    fn a_shell_with_no_locale_set_still_answers_about_the_path() {
        // `${LC_ALL:-${LC_CTYPE:-$LANG}}` expands to nothing on a machine that
        // sets none of the three, which must not cost us the PATH on the line
        // above it.
        let output = format!("{PATH_FENCE}/usr/bin\n{CTYPE_FENCE}\n");
        assert_eq!(
            parse_probe(&output),
            LoginEnvironment {
                path: Some("/usr/bin".into()),
                ctype: None,
            }
        );
    }

    #[test]
    fn merging_appends_what_is_missing_and_keeps_precedence() {
        // launchd's four, plus the one Homebrew directory that was missing.
        let launchd = "/usr/bin:/bin:/usr/sbin:/sbin";
        let login = "/opt/homebrew/bin:/usr/bin:/bin:/usr/sbin:/sbin";
        assert_eq!(
            merged(launchd, login),
            "/usr/bin:/bin:/usr/sbin:/sbin:/opt/homebrew/bin"
        );
    }

    #[test]
    fn merging_a_path_that_is_already_complete_changes_nothing() {
        // The started-from-a-terminal case, which must stay silent: `adopt`
        // decides it has nothing to say by comparing against this.
        let path = "/opt/homebrew/bin:/usr/bin:/bin";
        assert_eq!(merged(path, "/usr/bin:/opt/homebrew/bin"), path);
    }

    #[test]
    fn merging_drops_the_empty_entries_a_trailing_colon_leaves() {
        // A trailing colon means "the current directory" to some shells, and
        // this process should never search it.
        assert_eq!(merged("/usr/bin:", ":/bin::"), "/usr/bin:/bin");
    }

    #[test]
    fn every_spelling_of_utf8_counts_and_nothing_else_does() {
        for good in ["en_US.UTF-8", "zh_CN.utf8", "UTF-8", "C.UTF-8"] {
            assert!(is_utf8(good), "{good} should count as UTF-8");
        }
        // `C` and `POSIX` are exactly the locales tmux mangles bytes under, and
        // an empty value is what launchd leaves behind.
        for bad in ["C", "POSIX", "", "en_US.ISO8859-1"] {
            assert!(!is_utf8(bad), "{bad} should not count as UTF-8");
        }
    }

    #[test]
    fn lookup_finds_a_program_and_reports_a_missing_one() {
        let dir = std::env::temp_dir().join(format!("muqun-lookup-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let program = dir.join("tmuxish");
        std::fs::write(&program, b"#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = format!("/nonexistent:{}", dir.display());
        assert_eq!(lookup("tmuxish", &path).as_deref(), Some(program.as_path()));
        assert_eq!(lookup("tmuxish", "/nonexistent"), None);
        // A directory named like the program is not the program.
        assert_eq!(lookup("muqun-lookup-does-not-exist", &path), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn lookup_honours_a_path_that_was_written_out_in_full() {
        assert_eq!(
            lookup("/bin/sh", "/nowhere").as_deref(),
            Some(std::path::Path::new("/bin/sh"))
        );
        assert_eq!(lookup("/bin/definitely-not-here", "/bin"), None);
    }

    #[test]
    fn the_added_entries_are_the_ones_worth_naming() {
        assert_eq!(
            added_entries(
                "/usr/bin:/bin",
                "/usr/bin:/bin:/opt/homebrew/bin:/usr/local/bin"
            ),
            "/opt/homebrew/bin, /usr/local/bin"
        );
        assert_eq!(added_entries("/usr/bin", "/usr/bin"), "");
    }

    #[test]
    fn a_real_login_shell_answers_with_a_path() {
        // Guarded rather than asserted: a build host may have no usable login
        // shell, and this test exists to catch the flags being wrong, not to
        // require a shell.
        if let Some(environment) = probe_login_shell() {
            if let Some(path) = environment.path {
                assert!(path.contains('/'), "{path} does not look like a PATH");
                assert!(!path.contains(PATH_FENCE));
            }
        }
    }
}
