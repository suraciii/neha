//! The normalized usage of one Model call.
//!
//! Every Provider reports usage differently. The Space normalizes it into
//! five measurements so budgets and reports read one shape.

use serde::{Deserialize, Serialize};

/// The normalized usage of one Model call.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Usage {
    /// Input tokens that were not cached.
    pub uncached_input: u64,
    /// Input tokens read from the provider's cache.
    pub cache_read: u64,
    /// Input tokens written to the provider's cache.
    pub cache_write: u64,
    /// Output tokens.
    pub output: u64,
    /// Output tokens that were reasoning.
    pub reasoning: u64,
}

impl std::ops::Add for Usage {
    type Output = Self;

    fn add(self, other: Self) -> Self {
        Self {
            uncached_input: self.uncached_input.saturating_add(other.uncached_input),
            cache_read: self.cache_read.saturating_add(other.cache_read),
            cache_write: self.cache_write.saturating_add(other.cache_write),
            output: self.output.saturating_add(other.output),
            reasoning: self.reasoning.saturating_add(other.reasoning),
        }
    }
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Self) {
        *self = *self + other;
    }
}

impl Usage {
    /// Returns the tokens a usage budget counts.
    #[must_use]
    pub fn budgeted(self) -> u64 {
        self.uncached_input
            .saturating_add(self.cache_read)
            .saturating_add(self.cache_write)
            .saturating_add(self.output)
    }
}
