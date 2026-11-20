//! Shared process monitor for the bundled Desktop and Gateway installers.
use std::{
    io::Read,
    os::{fd::OwnedFd, unix::net::UnixStream},
    process::{Command, Stdio},
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Downloading { received: u64, total: u64 },
    Verifying,
    Installing,
    Starting,
}

fn parse_progress(line: &[u8]) -> Option<Phase> {
    let mut words = std::str::from_utf8(line).ok()?.split_ascii_whitespace();
    if words.next()? != "MUQUN_PROGRESS" {
        return None;
    }
    let status = match words.next()? {
        "download" => Phase::Downloading {
            received: words.next()?.parse().ok()?,
            total: words.next()?.parse().ok()?,
        },
        "verify" => Phase::Verifying,
        "install" => Phase::Installing,
        "start" => Phase::Starting,
        _ => return None,
    };
    words.next().is_none().then_some(status)
}

pub fn run(
    mut command: Command,
    cancel: &AtomicBool,
    deadline: Option<Duration>,
    progress: impl Fn(Phase),
) -> anyhow::Result<()> {
    anyhow::ensure!(!cancel.load(Ordering::Acquire), "installation cancelled");
    let (mut events, output) = UnixStream::pair()?;
    events.set_nonblocking(true)?;
    command
        .env("MUQUN_INSTALL_PROGRESS", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::from(OwnedFd::from(output)));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // SAFETY: setsid is the only post-fork operation; no allocation or
        // locking. Optional installer prompts must not read Desktop's TTY.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    Err(std::io::Error::last_os_error())
                } else {
                    Ok(())
                }
            });
        }
    }
    let mut child = command.spawn()?;
    drop(command);
    let started = Instant::now();
    let mut buffer = [0u8; 4096];
    let mut line = Vec::new();
    loop {
        let exit = child.try_wait();
        // Drain before returning a completed child so short phase transitions
        // reach the UI. Never wait on a descriptor inherited by a service.
        while let Ok(count @ 1..) = events.read(&mut buffer) {
            for byte in &buffer[..count] {
                if *byte == b'\n' {
                    if line.len() <= 128 {
                        if let Some(status) = parse_progress(&line) {
                            progress(status);
                        }
                    }
                    line.clear();
                } else if line.len() <= 128 {
                    line.push(*byte);
                }
            }
        }
        if let Some(exit) = exit? {
            anyhow::ensure!(exit.success(), "installer exited with {exit}");
            return Ok(());
        }
        if cancel.load(Ordering::Acquire) || deadline.is_some_and(|d| started.elapsed() >= d) {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("installer cancelled or timed out");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}
