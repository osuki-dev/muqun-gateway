//! One Muqun per desktop session: a second launch hands its request (focus,
//! and a file to open) to the running one and exits before it starts any
//! agent.
//!
//! On Unix the running instance listens on a socket in `$XDG_RUNTIME_DIR`
//! (else the temporary directory), named after the window title so builds
//! from several worktrees (`MUQUN_WINDOW_TITLE`) still run side by side. The
//! request carries the launcher's activation token, without which a Wayland
//! compositor would not let the running window take focus. Windows has no
//! implementation yet: every launch runs on its own.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// What a second launch asks of the running instance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// `XDG_ACTIVATION_TOKEN` / `DESKTOP_STARTUP_ID` the launcher gave it.
    pub token: Option<String>,
    /// A file named on its command line, for the viewer.
    pub open: Option<PathBuf>,
}

impl Request {
    /// This launch's request: its activation token and `open`.
    pub fn here(open: Option<PathBuf>) -> Self {
        let token = ["XDG_ACTIVATION_TOKEN", "DESKTOP_STARTUP_ID"]
            .iter()
            .find_map(|key| std::env::var(key).ok().filter(|v| !v.is_empty()));
        Self { token, open }
    }
}

/// The outcome of [`claim`].
pub enum Claim {
    /// This process is the instance; [`Primary::serve`] answers later launches.
    Primary(Primary),
    /// Another instance took the request; this process should exit.
    Forwarded,
}

/// The listening end, held by the running instance.
pub struct Primary {
    #[cfg(unix)]
    listener: Option<(std::os::unix::net::UnixListener, PathBuf)>,
}

/// Become the instance for `name`, or hand `request` to the one that is.
/// Any failure (no runtime directory, a socket that cannot be bound) runs
/// this launch on its own rather than refusing to start.
pub fn claim(name: &str, request: &Request) -> Claim {
    imp::claim(name, request)
}

/// The socket this process listens on, removed by [`release`].
#[cfg_attr(not(unix), allow(dead_code))]
static SOCKET: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();

/// Stop being the instance: remove the socket, so the next launch starts
/// without first trying a dead one. Called on the way out (a crash leaves
/// it, and the next launch replaces it).
pub fn release() {
    if let Some(path) = SOCKET.get() {
        let _ = std::fs::remove_file(path);
    }
}

impl Primary {
    /// Answer later launches: `on_request` runs on a thread of its own for
    /// each one.
    pub fn serve(self, on_request: impl Fn(Request) + Send + 'static) {
        imp::serve(self, on_request);
    }
}

#[cfg(unix)]
fn socket_path(name: &str) -> PathBuf {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    match dirs::runtime_dir() {
        Some(dir) => dir.join(format!("{}-{slug}.sock", crate::branding::APP_ID)),
        // No per-user runtime directory: the user id keeps users apart.
        None => std::env::temp_dir().join(format!(
            "{}-{}-{slug}.sock",
            crate::branding::APP_ID,
            unsafe { libc::getuid() }
        )),
    }
}

#[cfg(unix)]
mod imp {
    use std::io::{BufRead as _, BufReader, Write as _};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::time::Duration;

    use super::{socket_path, Claim, Primary, Request};

    pub fn claim(name: &str, request: &Request) -> Claim {
        let path = socket_path(name);
        if let Ok(stream) = UnixStream::connect(&path) {
            match forward(stream, request) {
                Ok(()) => return Claim::Forwarded,
                Err(err) => tracing::warn!("the running Muqun did not answer ({err}); starting"),
            }
        }
        // Nobody answered: a socket left by a crash, or none at all.
        let _ = std::fs::remove_file(&path);
        match UnixListener::bind(&path) {
            Ok(listener) => Claim::Primary(Primary {
                listener: Some((listener, path)),
            }),
            Err(err) => {
                tracing::warn!(path = %path.display(), "single instance unavailable: {err}");
                Claim::Primary(Primary { listener: None })
            }
        }
    }

    fn forward(mut stream: UnixStream, request: &Request) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(3)))?;
        stream.set_write_timeout(Some(Duration::from_secs(3)))?;
        let mut line = serde_json::to_vec(request)?;
        line.push(b'\n');
        stream.write_all(&line)?;
        let mut answer = String::new();
        BufReader::new(stream).read_line(&mut answer)?;
        if answer.trim() == "ok" {
            Ok(())
        } else {
            Err(std::io::Error::other("no acknowledgement"))
        }
    }

    pub fn serve(primary: Primary, on_request: impl Fn(Request) + Send + 'static) {
        let Some((listener, path)) = primary.listener else {
            return;
        };
        let spawned = std::thread::Builder::new()
            .name("muqun-instance".into())
            .spawn(move || {
                let _ = super::SOCKET.set(path);
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { continue };
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
                    let mut line = String::new();
                    if BufReader::new(&stream).read_line(&mut line).is_err() {
                        continue;
                    }
                    match serde_json::from_str::<Request>(line.trim()) {
                        Ok(request) => {
                            let _ = stream.write_all(b"ok\n");
                            on_request(request);
                        }
                        Err(err) => tracing::debug!("ignored an instance request: {err}"),
                    }
                }
            });
        if let Err(err) = spawned {
            tracing::warn!("could not listen for other launches: {err}");
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use super::{Claim, Primary, Request};

    pub fn claim(_: &str, _: &Request) -> Claim {
        Claim::Primary(Primary {})
    }

    pub fn serve(_: Primary, _: impl Fn(Request) + Send + 'static) {}
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// A second launch reaches the first and is answered; the first sees
    /// exactly what was asked.
    #[test]
    fn a_second_launch_is_handed_to_the_first() {
        let name = format!("test-{}", std::process::id());
        let Claim::Primary(primary) = claim(&name, &Request::default()) else {
            panic!("the first launch is the instance");
        };
        let (tx, rx) = std::sync::mpsc::channel();
        primary.serve(move |request| {
            let _ = tx.send(request);
        });
        let ask = Request {
            token: Some("t".into()),
            open: Some("/tmp/notes.md".into()),
        };
        assert!(matches!(claim(&name, &ask), Claim::Forwarded));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(3)).unwrap(),
            ask
        );
        let _ = std::fs::remove_file(socket_path(&name));
    }
}
