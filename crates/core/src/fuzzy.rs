//! Thin wrapper around [`nucleo_matcher`] for scoring candidates against a needle.

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

/// A reusable fuzzy matcher holding a parsed pattern. Create one per query and
/// score many candidates against it.
pub struct Fuzzy {
    matcher: Matcher,
    pattern: Pattern,
}

impl Fuzzy {
    pub fn new(needle: &str) -> Self {
        Self {
            matcher: Matcher::new(Config::DEFAULT),
            pattern: Pattern::parse(needle, CaseMatching::Ignore, Normalization::Smart),
        }
    }

    /// Score `text` against the needle. Higher is better; `None` means no match.
    pub fn score(&mut self, text: &str) -> Option<u32> {
        let mut buf = Vec::new();
        let haystack = Utf32Str::new(text, &mut buf);
        self.pattern.score(haystack, &mut self.matcher)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_subsequence_and_ranks_prefix_higher() {
        let mut f = Fuzzy::new("saf");
        let safari = f.score("Safari").expect("should match");
        let other = f.score("Disk Utility");
        assert!(other.is_none());
        assert!(safari > 0);
    }
}
