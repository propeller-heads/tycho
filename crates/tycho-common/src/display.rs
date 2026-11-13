use std::fmt::{Display, Formatter};

use tracing::Value;

/// Wrapper that makes `Option<T>` implement `Display`.
pub struct DisplayOption<'a, T>(&'a Option<T>);

impl<'a, T: Display> Display for DisplayOption<'a, T> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self.0 {
            Some(inner) => write!(f, "{}", inner),
            None => write!(f, "None"),
        }
    }
}

/// Convenience function so you can write `%opt(value)` in `tracing` logs.
#[expect(clippy::ref_option, reason = "public API; changing the signature is a breaking change")]
pub fn opt<T: Display>(val: &Option<T>) -> impl Value + '_ {
    tracing::field::display(DisplayOption(val))
}
