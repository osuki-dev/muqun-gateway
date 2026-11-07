//! Light/dark preference without Blinc.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ColorScheme {
    Light,
    Dark,
}

impl ColorScheme {
    #[allow(dead_code)]
    pub fn is_dark(self) -> bool {
        matches!(self, Self::Dark)
    }
}
