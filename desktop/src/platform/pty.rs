//! A pseudo-terminal running the owner's own shell, for the terminal panel
//! (`term/`). Unix only for now: Windows (ConPTY) answers with an error the
//! panel shows.

/// The size the shell is told about (TIOCSWINSZ), in cells and pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WinSize {
    pub cols: u16,
    pub rows: u16,
    pub px_w: u16,
    pub px_h: u16,
}

#[cfg(unix)]
pub use unix::spawn_shell;

#[cfg(not(unix))]
pub use other::spawn_shell;

/// The shell to run: `$SHELL`, else the passwd entry's, else `/bin/sh`.
#[cfg(unix)]
fn user_shell() -> std::path::PathBuf {
    if let Some(shell) = std::env::var_os("SHELL").filter(|s| !s.is_empty()) {
        return shell.into();
    }
    // SAFETY: getpwuid returns a pointer into static storage or null; it is
    // read at once, on this thread, before anything else calls it.
    unsafe {
        let pw = libc::getpwuid(libc::getuid());
        if !pw.is_null() && !(*pw).pw_shell.is_null() {
            let shell = std::ffi::CStr::from_ptr((*pw).pw_shell);
            if let Ok(shell) = shell.to_str() {
                if !shell.is_empty() {
                    return shell.into();
                }
            }
        }
    }
    "/bin/sh".into()
}

#[cfg(unix)]
mod unix {
    use std::fs::File;
    use std::io::{self, Read, Write};
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::process::CommandExt;
    use std::path::Path;
    use std::process::{Command, Stdio};

    use super::WinSize;

    fn winsize(size: WinSize) -> libc::winsize {
        libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.px_w,
            ws_ypixel: size.px_h,
        }
    }

    /// The master side: read what the shell prints, write what the owner types.
    pub struct Pty {
        master: File,
    }

    impl Pty {
        /// A second handle on the master for a reader thread.
        pub fn reader(&self) -> io::Result<impl Read + Send + 'static> {
            self.master.try_clone()
        }

        /// Write everything, retrying on interrupts (the fd is blocking).
        pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
            self.master.write_all(bytes)
        }

        /// Tell the shell its new size; the kernel sends it SIGWINCH.
        pub fn resize(&self, size: WinSize) {
            let ws = winsize(size);
            // SAFETY: TIOCSWINSZ reads one `winsize` from the pointer.
            unsafe {
                libc::ioctl(self.master.as_raw_fd(), libc::TIOCSWINSZ, &ws);
            }
        }
    }

    /// The shell process; hung up and reaped on drop.
    pub struct Child {
        child: Option<std::process::Child>,
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let Some(mut child) = self.child.take() else {
                return;
            };
            // SIGHUP, as a terminal closing does; the shell passes it on
            // to its jobs. Reaped on a thread so closing never blocks.
            // SAFETY: plain kill(2) on our own child's pid.
            unsafe {
                libc::kill(child.id() as libc::pid_t, libc::SIGHUP);
            }
            let _ = std::thread::Builder::new()
                .name("muqun-term-reap".into())
                .spawn(move || {
                    let _ = child.wait();
                });
        }
    }

    /// Open a pty of `size` and start the owner's login shell in `cwd`, with
    /// `TERM=xterm-256color` and truecolor advertised.
    pub fn spawn_shell(cwd: &Path, size: WinSize) -> io::Result<(Pty, Child)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        let ws = winsize(size);
        // SAFETY: openpty writes two fds; name and termios may be null.
        let rc = unsafe {
            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null(),
                &ws,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty succeeded, so both are fresh fds we own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: FD_CLOEXEC on our own fd, so the shell doesn't inherit it.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        let shell = super::user_shell();
        let name = shell
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sh".into());
        let mut cmd = Command::new(&shell);
        // A login shell, as a terminal emulator starts one: `-zsh` as argv[0].
        cmd.arg0(format!("-{name}"))
            .current_dir(cwd)
            .env("TERM", "xterm-256color")
            .env("COLORTERM", "truecolor")
            .env("TERM_PROGRAM", "muqun-desktop")
            .env_remove("TERM_PROGRAM_VERSION")
            .stdin(Stdio::from(slave.try_clone()?))
            .stdout(Stdio::from(slave.try_clone()?))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec: a new
        // session, and the pty (now fd 0) as its controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let child = cmd.spawn()?;
        Ok((
            Pty {
                master: File::from(master),
            },
            Child { child: Some(child) },
        ))
    }
}

#[cfg(not(unix))]
mod other {
    use std::io;
    use std::path::Path;

    use super::WinSize;

    pub struct Pty;
    pub struct Child;

    impl Pty {
        pub fn reader(&self) -> io::Result<io::Empty> {
            Ok(io::empty())
        }
        pub fn write_all(&mut self, _: &[u8]) -> io::Result<()> {
            Ok(())
        }
        pub fn resize(&self, _: WinSize) {}
    }

    pub fn spawn_shell(_: &Path, _: WinSize) -> io::Result<(Pty, Child)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the terminal panel needs a Unix pty (ConPTY is not wired yet)",
        ))
    }
}
