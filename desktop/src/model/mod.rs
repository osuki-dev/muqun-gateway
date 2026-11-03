//! The thread boundary (architecture §2): commands go UI → core over a
//! channel, updates come core → UI through the [`inbox::Inbox`], and the
//! main thread applies them in [`drain`].

pub mod commands;
pub mod drain;
pub mod inbox;
pub mod projection;
pub mod stream;
pub mod turn;
pub mod view;
