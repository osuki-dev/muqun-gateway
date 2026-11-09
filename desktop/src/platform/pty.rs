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

/// How the shell ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// It exited with this status.
    Code(i32),
    /// A signal killed it.
    Signal(i32),
}

/// Told, on a thread of its own, how the shell ended once it has.
pub type OnExit = Box<dyn FnOnce(Exit) + Send>;

#[cfg(all(unix, test))]
pub use unix::spawn_command;
#[cfg(unix)]
pub use unix::{spawn_shell, Child, Pty};

#[cfg(not(unix))]
pub use other::{spawn_shell, Child, Pty};

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
    use std::sync::{Arc, Mutex};

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
        /// Its drop ends every [`Reader`]: they wait on the pipe's other end.
        _stop: io::PipeWriter,
        stopped: io::PipeReader,
    }

    /// A second handle on the master for a reader thread. It reads to the
    /// end of the shell's output, or until its [`Pty`] is dropped, so the
    /// thread never outlives the panel's shell (a job the shell left behind
    /// may hold the terminal open long after).
    pub struct Reader {
        master: File,
        stopped: io::PipeReader,
    }

    impl Read for Reader {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut fds = [
                libc::pollfd {
                    fd: self.master.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
                libc::pollfd {
                    fd: self.stopped.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                },
            ];
            loop {
                // SAFETY: poll reads and writes the two pollfds above.
                let rc = unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) };
                if rc < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(err);
                }
                if fds[1].revents != 0 {
                    return Ok(0);
                }
                if fds[0].revents != 0 {
                    return self.master.read(buf);
                }
            }
        }
    }

    impl Pty {
        /// A reader for a thread of its own; see [`Reader`].
        pub fn reader(&self) -> io::Result<Reader> {
            Ok(Reader {
                master: self.master.try_clone()?,
                stopped: self.stopped.try_clone()?,
            })
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

    /// The shell process. A thread waits for it and reaps it as it ends;
    /// dropped while it runs, it is hung up.
    pub struct Child {
        pub(super) pid: libc::pid_t,
        /// Set once the pid is reaped (and may name another process).
        reaped: Arc<Mutex<bool>>,
    }

    impl Drop for Child {
        fn drop(&mut self) {
            let reaped = self.reaped.lock().unwrap_or_else(|e| e.into_inner());
            if !*reaped {
                // SIGHUP, as a terminal closing does; the shell passes it on
                // to its jobs. The waiting thread reaps it.
                // SAFETY: kill(2) on our own child, not yet reaped (the
                // lock keeps the waiter from reaping it meanwhile).
                unsafe {
                    libc::kill(self.pid, libc::SIGHUP);
                }
            }
        }
    }

    impl Child {
        /// Wait for `pid` on a thread; `on_exit` hears how it ended.
        fn watch(pid: libc::pid_t, on_exit: super::OnExit) -> io::Result<Self> {
            let reaped = Arc::new(Mutex::new(false));
            let flag = reaped.clone();
            std::thread::Builder::new()
                .name("muqun-term-wait".into())
                .spawn(move || {
                    // Wait without reaping, so the pid stays this child's
                    // while `Child::drop` may still signal it.
                    loop {
                        // SAFETY: waitid fills the zeroed siginfo_t.
                        let rc = unsafe {
                            let mut info: libc::siginfo_t = std::mem::zeroed();
                            libc::waitid(
                                libc::P_PID,
                                pid as libc::id_t,
                                &mut info,
                                libc::WEXITED | libc::WNOWAIT,
                            )
                        };
                        if rc == 0
                            || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted
                        {
                            break;
                        }
                    }
                    let mut status = 0;
                    {
                        let mut reaped = flag.lock().unwrap_or_else(|e| e.into_inner());
                        // SAFETY: reaps our own exited child.
                        while unsafe { libc::waitpid(pid, &mut status, 0) } < 0 {
                            if io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                                break;
                            }
                        }
                        *reaped = true;
                    }
                    on_exit(if libc::WIFSIGNALED(status) {
                        super::Exit::Signal(libc::WTERMSIG(status))
                    } else {
                        super::Exit::Code(libc::WEXITSTATUS(status))
                    });
                })?;
            Ok(Self { pid, reaped })
        }
    }

    /// Open a pty of `size` and start the owner's login shell in `cwd`, with
    /// `TERM=xterm-256color` and truecolor advertised.
    pub fn spawn_shell(
        cwd: &Path,
        size: WinSize,
        on_exit: super::OnExit,
    ) -> io::Result<(Pty, Child)> {
        let shell = super::user_shell();
        let name = shell
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "sh".into());
        let mut cmd = Command::new(&shell);
        // A login shell, as a terminal emulator starts one: `-zsh` as argv[0].
        cmd.arg0(format!("-{name}")).current_dir(cwd);
        spawn(cmd, size, on_exit)
    }

    /// `cmd` on a pty, as [`spawn_shell`] runs the shell.
    #[cfg(test)]
    pub fn spawn_command(
        cmd: Command,
        size: WinSize,
        on_exit: super::OnExit,
    ) -> io::Result<(Pty, Child)> {
        spawn(cmd, size, on_exit)
    }

    fn spawn(mut cmd: Command, size: WinSize, on_exit: super::OnExit) -> io::Result<(Pty, Child)> {
        let mut master: libc::c_int = -1;
        let mut slave: libc::c_int = -1;
        #[cfg(target_os = "macos")]
        let mut ws = winsize(size);
        #[cfg(not(target_os = "macos"))]
        let ws = winsize(size);
        // SAFETY: openpty writes two fds; name and termios may be null.
        let rc = unsafe {
            #[cfg(target_os = "macos")]
            let winp = &mut ws as *mut libc::winsize;
            #[cfg(not(target_os = "macos"))]
            let winp = &ws as *const libc::winsize;

            libc::openpty(
                &mut master,
                &mut slave,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                winp,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: openpty succeeded, so both are fresh fds we own.
        let (master, slave) =
            unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
        // SAFETY: FD_CLOEXEC on our own fds: neither the shell nor anything
        // else this process starts keeps them (a stray copy of the slave
        // would hold the terminal open after the shell ends). The shell gets
        // the slave as its 0, 1 and 2, which exec keeps.
        unsafe {
            libc::fcntl(master.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
            libc::fcntl(slave.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
        }

        cmd.env("TERM", "xterm-256color")
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
        let (stopped, stop) = io::pipe()?;
        let child = cmd.spawn()?;
        // Not waited on through std: `Child::watch` reaps it.
        let pid = child.id() as libc::pid_t;
        drop(cmd);
        Ok((
            Pty {
                master: File::from(master),
                _stop: stop,
                stopped,
            },
            Child::watch(pid, on_exit)?,
        ))
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::io::Read;
    use std::process::Command;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::{spawn_command, Exit, WinSize};

    const SIZE: WinSize = WinSize {
        cols: 80,
        rows: 24,
        px_w: 640,
        px_h: 384,
    };

    fn sh(script: &str) -> Command {
        let mut cmd = Command::new("/bin/sh");
        cmd.args(["-c", script]);
        cmd
    }

    #[test]
    fn the_shell_is_reaped_with_its_status_and_the_reader_stops_with_the_pty() {
        let (tx, rx) = mpsc::channel();
        // A job left holding the terminal (it ignores the hangup), as
        // `cmd &` in a shell that then exits does.
        let script = "trap '' HUP; sleep 3 & exit 3";
        let (pty, child) = spawn_command(
            sh(script),
            SIZE,
            Box::new(move |exit| tx.send(exit).unwrap()),
        )
        .unwrap();
        let pid = child.pid;
        let mut reader = pty.reader().unwrap();
        let (read_tx, read_rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            while !matches!(reader.read(&mut buf), Ok(0) | Err(_)) {}
            read_tx.send(()).unwrap();
        });
        assert_eq!(rx.recv_timeout(Duration::from_secs(5)), Ok(Exit::Code(3)));
        // Reaped: no zombie left behind.
        // SAFETY: kill with signal 0 only checks the pid.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "the exited shell is still in the process table");
        // The job keeps the terminal open, so the reader waits on...
        assert!(read_rx.recv_timeout(Duration::from_millis(300)).is_err());
        // ...until the panel drops the pty.
        drop((pty, child));
        assert!(read_rx.recv_timeout(Duration::from_secs(1)).is_ok());
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

    pub fn spawn_shell(_: &Path, _: WinSize, _: super::OnExit) -> io::Result<(Pty, Child)> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "the terminal panel needs a Unix pty (ConPTY is not wired yet)",
        ))
    }
}
