//! Terminal backend construction: registry wiring, endpoint rendering, and the
//! adapter-facing error translation.

use axum::http::StatusCode;
use axum::response::Json;
use serde_json::Value;

use crate::backend::{
    BackendError, BackendKind, BackendRegistry, TerminalBackend, TmuxWireIds, TMUX_PROGRAM,
};
use crate::{api_error, default_socket_path, login_env, Config, SessionConfig};

pub(crate) fn default_backend_socket(backend: BackendKind) -> String {
    backend_registry().default_socket(backend)
}

pub(crate) fn default_backend_label(backend: BackendKind) -> &'static str {
    backend_registry().default_label(backend)
}

pub(crate) fn backend_registry() -> BackendRegistry {
    BackendRegistry::new(
        std::env::var("HERDR_SOCKET_PATH")
            .ok()
            .unwrap_or_else(default_socket_path),
    )
}

pub(crate) fn backend_endpoint(session: &SessionConfig) -> String {
    backend_registry()
        .endpoint(session.backend, &session.socket_path)
        .into_owned()
}

/// The program a session's backend has to spawn, and whether this process can
/// see it.
///
/// Reported because "unavailable" is not a diagnosis. A tmux backend has
/// exactly two ways to be unreachable -- no tmux server, or no tmux -- and only
/// one of them is visible to the person reading, since `tmux -V` in their own
/// shell answers happily while the daemon's `PATH` cannot find it at all. herdr
/// has nothing to look up: it is a socket, and the endpoint column already
/// names it.
pub(crate) fn backend_program_state(session: &SessionConfig) -> String {
    if session.backend != BackendKind::Tmux {
        return String::new();
    }
    let path = std::env::var("PATH").unwrap_or_default();
    match login_env::lookup(TMUX_PROGRAM, &path) {
        Some(found) => format!("tmux={}", found.display()),
        None => format!("tmux=NOT FOUND on PATH={path}"),
    }
}

/// Say so at startup when a configured backend's program cannot be found.
///
/// Without this the only symptom is one "terminal backend is unavailable" line
/// per poll, forever, naming a session id and nothing else -- which is what
/// this failure looked like for the whole time it went undiagnosed. It is a
/// warning and not a refusal: a gateway with a reachable herdr session and an
/// unreachable tmux one is still worth starting, and the app orders the
/// unreachable one last anyway.
pub(crate) fn warn_about_missing_backend_programs(config: &Config) {
    let path = std::env::var("PATH").unwrap_or_default();
    for session in &config.sessions {
        if session.backend != BackendKind::Tmux {
            continue;
        }
        if login_env::lookup(TMUX_PROGRAM, &path).is_none() {
            tracing::warn!(
                "session {}: backend=tmux, but no `tmux` on PATH={path}\n  \
                 This gateway cannot drive tmux until it can find it. If tmux works in your\n  \
                 shell but not here, the service is running with a different PATH: reinstall\n  \
                 it with `muqun-gateway service install`.",
                session.id
            );
        }
    }
}

/// The only call site that turns a session into a live backend. tmux ids
/// collide with URL percent-encoding past pane 9 (see `backend::tmux_wire`),
/// so a tmux session is wrapped here to translate ids to and from wire form
/// before any handler sees them -- a future handler that reaches its backend
/// through this function gets the translation automatically, with nothing to
/// remember. herdr sessions are returned unwrapped: herdr's own ids already
/// work today and must not change.
pub(crate) fn terminal_backend(session: &SessionConfig) -> Box<dyn TerminalBackend> {
    let backend = backend_registry().connect(session.backend, &session.socket_path);
    match session.backend {
        BackendKind::Tmux => Box::new(TmuxWireIds::new(backend)),
        BackendKind::Herdr => backend,
    }
}

pub(crate) fn backend_api_error(error: BackendError) -> (StatusCode, Json<Value>) {
    let (status, code, message) = match &error {
        BackendError::InvalidTarget(_) => (
            StatusCode::NOT_FOUND,
            "backend_target_not_found",
            "terminal target not found",
        ),
        BackendError::Unavailable => (
            StatusCode::BAD_GATEWAY,
            "backend_unavailable",
            "terminal backend is unavailable",
        ),
        BackendError::InvalidResponse(_)
        | BackendError::Refused { .. }
        | BackendError::Unsupported(_) => (
            StatusCode::BAD_GATEWAY,
            "backend_error",
            "terminal backend request failed",
        ),
    };
    // Adapter diagnostics may name a local socket or tmux target. Keep them in
    // the host log and return only a stable, non-sensitive API error.
    tracing::warn!("terminal backend request failed: {error}");
    api_error(status, code, message)
}
