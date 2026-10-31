pub mod driver;
pub use driver::ClaudeDriver;

pub(crate) mod native_sessions;

mod recovery;

#[cfg(all(test, unix))]
mod tests;
