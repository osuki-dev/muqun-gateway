//! Assistant tokens: bearer tokens minted for one pane/task (7 days) or one agent
//! (no expiry; replaced on its next start), which authorise `/mcp` and
//! nothing else. Only `hash_token()` of a
//! token is stored; the token itself is shown once, at issue.

use serde::{Deserialize, Serialize};

use crate::platform::store::sqlite::Migration;
use crate::tasks::domain::{PrincipalKind, TaskPrincipal};
use crate::tasks::ports::TaskFuture;

pub(crate) const MIGRATION: Migration = Migration {
    namespace: "mcp",
    steps: &["
CREATE TABLE assistant_tokens(
  id TEXT PRIMARY KEY, token_hash TEXT NOT NULL UNIQUE, binding TEXT NOT NULL,
  owner_key TEXT NOT NULL, label TEXT NOT NULL, issued_ms INTEGER NOT NULL,
  expires_ms INTEGER NOT NULL, last_used_ms INTEGER, revoked_ms INTEGER);
CREATE INDEX assistant_tokens_owner ON assistant_tokens(owner_key);
"],
};

pub(crate) const PANE_TOKEN_TTL_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// The stored `expires_ms` of a token that does not expire (an agent token,
/// revoked when its agent next starts or by `mcp revoke`); on the wire it is
/// `null`. The largest value the store's signed column holds; pass it as the TTL.
pub(crate) const NO_EXPIRY: u64 = i64::MAX as u64;

/// What a token was issued for. Everything the token's holder creates is
/// owned by [`TokenBinding::owner_key`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum TokenBinding {
    Pane {
        session_id: String,
        pane_id: String,
    },
    /// Legacy service credential. It has no authority to forward remotely.
    Agent {
        agent_id: String,
    },
    /// Explicit owner registration for a shared local provider. All of its
    /// sessions share this authority; it is never a per-session identity.
    Runtime {
        agent_id: String,
    },
    /// One immutable delegation, independent of a shared daemon's ancestry.
    Task {
        task_id: String,
    },
}

impl TokenBinding {
    pub(crate) fn owner_key(&self) -> String {
        match self {
            Self::Pane {
                session_id,
                pane_id,
            } => format!("pane:{session_id}\u{1f}{pane_id}"),
            Self::Agent { agent_id } => format!("agent:{agent_id}"),
            Self::Runtime { agent_id } => format!("runtime:{agent_id}"),
            Self::Task { task_id } => format!("task:{task_id}"),
        }
    }
}

/// A resolved, live assistant token: who is calling `/mcp`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct AssistantSession {
    pub token_id: String,
    pub binding: TokenBinding,
    pub label: String,
    /// `None` for a token that never expires (the wire has no 2^63).
    pub expires_ms: Option<u64>,
}

impl AssistantSession {
    pub(crate) fn principal(&self) -> TaskPrincipal {
        TaskPrincipal {
            kind: PrincipalKind::Assistant,
            id: self.binding.owner_key(),
        }
    }
}

/// A token as issued: the only time the token itself exists outside its holder.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct IssuedToken {
    pub token: String,
    pub session: AssistantSession,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct TokenRow {
    pub id: String,
    pub binding: TokenBinding,
    pub label: String,
    pub issued_ms: u64,
    /// `None` for a token that never expires.
    pub expires_ms: Option<u64>,
    pub last_used_ms: Option<u64>,
    pub revoked_ms: Option<u64>,
}

/// What a presented token turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Resolved {
    Live(AssistantSession),
    /// Known and not revoked, but past `expires_ms`: the holder should fetch
    /// a new one (`/mcp` answers 401 so hosts re-run their helper).
    Expired,
    /// Unknown or revoked.
    Invalid,
}

#[cfg(test)]
impl Resolved {
    pub(crate) fn live(self) -> Option<AssistantSession> {
        match self {
            Self::Live(session) => Some(session),
            _ => None,
        }
    }
}

pub(crate) trait AssistantTokenStore: Send + Sync {
    /// A `ttl_ms` of [`NO_EXPIRY`] issues a token that never expires.
    fn issue<'a>(
        &'a self,
        binding: TokenBinding,
        label: String,
        ttl_ms: u64,
    ) -> TaskFuture<'a, IssuedToken>;
    /// What `token` is; records the use of a live one.
    fn resolve<'a>(&'a self, token: &'a str) -> TaskFuture<'a, Resolved>;
    /// Whether a live token was revoked.
    fn revoke<'a>(&'a self, token_id: &'a str) -> TaskFuture<'a, bool>;
    /// How many live tokens for `binding` were revoked. Only tests revoke
    /// by binding today.
    #[cfg(test)]
    fn revoke_binding<'a>(&'a self, binding: &'a TokenBinding) -> TaskFuture<'a, usize>;
    /// How many live tokens were revoked.
    fn revoke_all<'a>(&'a self) -> TaskFuture<'a, usize>;
    /// Live tokens only, newest first.
    fn list<'a>(&'a self) -> TaskFuture<'a, Vec<TokenRow>>;
    /// Delete the tokens revoked or expired before `before_ms`. How many went.
    fn purge_dead<'a>(&'a self, before_ms: u64) -> TaskFuture<'a, usize>;
}
