//! The terminal panel's machinery: a pty running the owner's shell
//! (`platform/pty.rs`), libghostty-vt as the emulator (`engine.rs`) on its own
//! thread (`session.rs`), and the frame the UI draws (`grid.rs`).

pub mod daemon;
pub mod engine;
pub mod grid;
pub mod input;
pub mod session;
