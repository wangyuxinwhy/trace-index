//! Bounded text carried by the domain model.
//!
//! `TextContent` always describes the complete logical text even when `value`
//! is only a bounded prefix. The public SQL encoding can store that structure in
//! `blobs` and place `{blob_id}` in `items.semantic`; callers should not confuse
//! that access optimization with the domain value defined here.

use serde::{Deserialize, Serialize};

/// Deterministic estimator used by every published `estimated_tokens` value.
///
/// ASCII characters contribute one token per four characters, rounded up.
/// Every non-ASCII Unicode scalar contributes one token. Runtime-reported
/// token counts remain separate facts because their tokenizers and scopes
/// differ from this corpus-wide size estimate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct TextContent {
    /// Text published by Trace Index. This can be a bounded prefix.
    pub(crate) value: String,
    /// UTF-8 bytes in the complete text before Trace Index applied its bound.
    pub(crate) full_bytes: u64,
    /// Deterministic estimate over that same complete text.
    pub(crate) estimated_tokens: u64,
}

#[must_use]
pub(crate) fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0_u64;
    let mut non_ascii = 0_u64;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
    }
    ascii.div_ceil(4) + non_ascii
}

#[cfg(test)]
mod tests {
    use super::estimate_tokens;

    #[test]
    fn estimates_ascii_and_unicode_without_using_utf8_width() {
        assert_eq!(estimate_tokens(""), 0);
        assert_eq!(estimate_tokens("a"), 1);
        assert_eq!(estimate_tokens("abcdefgh"), 2);
        assert_eq!(estimate_tokens("abcd你好"), 3);
        assert_eq!(estimate_tokens("你好"), 2);
    }
}
