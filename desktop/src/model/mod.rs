//! The thread boundary (architecture §2): commands go UI → core over a
//! channel, updates come core → UI through the [`inbox::Inbox`], and the UI
//! thread folds them into its state (`ui::bridge`).

pub mod commands;
pub mod compose;
pub mod inbox;
pub mod projection;
pub mod projects;
pub mod stream;
pub mod turn;
pub mod types;
