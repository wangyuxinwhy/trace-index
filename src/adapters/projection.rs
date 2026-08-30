//! Adapter-neutral facts produced from a complete Source projection.
//!
//! Loop membership, dual-track pairing, and tool-call/output correlation can
//! depend on surrounding Records. A projector therefore consumes an ordered
//! Record stream and emits Sessions; the storage boundary validates and
//! publishes the five-object domain contract.

/// Content digest for deduplication.
///
/// The 32 raw bytes rather than their hex spelling: the value is only ever
/// compared for equality, and hex doubled both the index and every key
/// comparison in it. It is not a public column, so the encoding is free to be
/// the compact one.
pub(crate) type ContentHash = [u8; 32];

/// Text retained by an Adapter, bounded for predictable indexing cost.
///
/// The digest is over the complete input. Adapters use it while pairing
/// Runtime Records; private storage combines it with the published prefix
/// length when deduplicating Semantic text. The optional text is the bounded
/// value that can enter the Item's Semantic value.
#[derive(Debug, Clone)]
pub(crate) struct BoundedText {
    pub hash: ContentHash,
    pub full_bytes: u64,
    pub estimated_tokens: u64,
    pub text: Option<String>,
}

impl BoundedText {
    pub(crate) fn bounded(text: &str, max_bytes: usize) -> Self {
        let visible = if text.len() <= max_bytes {
            text.to_owned()
        } else {
            let mut boundary = max_bytes;
            while boundary > 0 && !text.is_char_boundary(boundary) {
                boundary -= 1;
            }
            text[..boundary].to_owned()
        };
        Self {
            hash: *blake3::hash(text.as_bytes()).as_bytes(),
            full_bytes: u64::try_from(text.len()).unwrap_or(u64::MAX),
            estimated_tokens: crate::domain::estimate_tokens(text),
            text: Some(visible),
        }
    }

    pub(crate) fn content(&self) -> Option<crate::domain::TextContent> {
        self.text.as_ref().map(|value| crate::domain::TextContent {
            value: value.clone(),
            full_bytes: self.full_bytes,
            estimated_tokens: self.estimated_tokens,
        })
    }
}

/// The parsed shape of one text, produced by `syntax` and written verbatim.
///
/// Local ids are indices into these vectors; `persist` maps them to row ids.
/// The extractor never sees a database, so parsing stays a pure function of
/// (text, language) and can be tested without one.
#[derive(Debug, Default, Clone)]
pub(crate) struct SyntaxProjection {
    pub fragments: Vec<FragmentProjection>,
    pub statements: Vec<StatementProjection>,
    pub invocations: Vec<InvocationProjection>,
    pub redirects: Vec<RedirectProjection>,
}

#[derive(Debug, Clone)]
pub(crate) struct FragmentProjection {
    pub parent: Option<usize>,
    pub content: BoundedText,
    pub parse_status: &'static str,
}

#[derive(Debug, Clone)]
pub(crate) struct StatementProjection {
    pub fragment: usize,
    pub parent: Option<usize>,
    pub start_byte: u32,
    pub end_byte: u32,
    /// Shell composition, present only for shell statements.
    pub shell: Option<ShellStatement>,
}

#[derive(Debug, Clone)]
pub(crate) struct ShellStatement {
    pub connector: &'static str,
    pub pipeline_id: Option<u32>,
    pub pipeline_pos: Option<u32>,
}

#[derive(Debug, Clone)]
pub(crate) struct InvocationProjection {
    pub statement: usize,
    pub program: String,
    /// Ordered argument words as a JSON array. Shell words, not a runtime argv.
    pub argv: String,
}

#[derive(Debug, Clone)]
pub(crate) struct RedirectProjection {
    pub statement: usize,
    pub source_fd_raw: Option<String>,
    pub operator: String,
    pub target_raw: String,
    pub start_byte: u32,
    pub end_byte: u32,
}

#[derive(Debug)]
pub(crate) enum ItemDetail {
    Message {
        has_images: bool,
    },
    Reasoning {
        representation: crate::domain::ReasoningRepresentation,
    },
    ToolCall {
        call_id: String,
        name: Option<String>,
        cmd: Option<String>,
        /// What language `cmd` is written in, as the Adapter declares it from
        /// the Runtime's own source. `None` means the Adapter makes no claim,
        /// and nothing is parsed — a tool whose argument merely looks like a
        /// command is not one.
        ///
        /// This is a declaration, not a detection: the Adapter states it, and
        /// the `syntax` module parses. Sniffing the text would reproduce the
        /// mistake that once invented two runtime markers no runtime has.
        cmd_lang: Option<&'static str>,
        workdir: Option<String>,
        args: Option<BoundedText>,
        /// Filled by the syntax pass between projection and persistence.
        syntax: Option<SyntaxProjection>,
    },
    ToolOutput {
        call_id: String,
        output: Option<BoundedText>,
        facts: ToolOutputFacts,
    },
    Misc,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct ToolOutputFacts {
    pub exit_code: Option<i64>,
    pub nonzero_exit: Option<bool>,
    pub duration_ms: Option<u64>,
    pub output_tokens: Option<u64>,
    pub truncated: Option<bool>,
}

#[derive(Debug)]
pub(crate) struct ItemProjection {
    /// Timeline position: the line number where this item was first observed.
    ///
    /// When a runtime writes one logical fact twice, this is the earlier of the
    /// two, so ordering reflects when the thing actually happened.
    pub seq: u64,
    /// Line number of the primary physical witness. Differs from `seq` when the
    /// user-interface record was seen before its model-input twin.
    pub record_seq: u64,
    /// Line number of the second physical witness, when one was paired.
    pub ui_seq: Option<u64>,
    pub ts_ms: Option<i64>,
    pub semantic_role: String,
    pub basis: String,
    pub preview: Option<BoundedText>,
    pub detail: ItemDetail,
    /// Runtime-native identity of the Session referenced by a delegation,
    /// subagent activity, or subagent report.
    ///
    /// This is private projection evidence. Persistence resolves it to the
    /// public numeric Session id selected by `semantic_role`; adapters do not
    /// need to know database identities.
    pub linked_session_native_id: Option<String>,
    /// Whether this item's text enters the conversation full-text index.
    pub searchable: bool,
}

#[derive(Debug)]
pub(crate) struct LoopProjection {
    pub native_id: Option<String>,
    pub start_seq: u64,
    pub end_record_seq: Option<u64>,
    pub outcome: Option<crate::domain::LoopOutcome>,
    pub model: Option<crate::domain::Model>,
    pub usage: Option<crate::domain::Usage>,
    pub items: Vec<ItemProjection>,
}

/// Normalizes one Runtime usage report into the Loop contract.
///
/// Codex reports input with cache reads already included. Claude and Pi report
/// uncached input, cache reads, and cache writes separately, so their parts are
/// added once here. Optional cache fields remain absent when the Runtime did
/// not report them; a reported zero remains zero.
pub(crate) fn normalized_usage(
    input: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    output: Option<u64>,
    reasoning: Option<u64>,
    input_includes_cache: bool,
) -> Option<crate::domain::Usage> {
    let mut input = input?;
    let output = output?;
    if !input_includes_cache {
        input = input
            .saturating_add(cached.unwrap_or(0))
            .saturating_add(cache_write.unwrap_or(0));
    }
    Some(crate::domain::Usage {
        input,
        cached: cached.filter(|value| *value <= input),
        cache_write: cache_write.filter(|value| *value <= input),
        output,
        reasoning: reasoning.filter(|value| *value <= output),
    })
}

pub(crate) fn add_usage(
    current: Option<crate::domain::Usage>,
    sample: crate::domain::Usage,
) -> crate::domain::Usage {
    let Some(current) = current else {
        return sample;
    };
    crate::domain::Usage {
        input: current.input.saturating_add(sample.input),
        cached: add_optional(current.cached, sample.cached),
        cache_write: add_optional(current.cache_write, sample.cache_write),
        output: current.output.saturating_add(sample.output),
        reasoning: add_optional(current.reasoning, sample.reasoning),
    }
}

fn add_optional(left: Option<u64>, right: Option<u64>) -> Option<u64> {
    match (left, right) {
        (Some(left), Some(right)) => Some(left.saturating_add(right)),
        (None, _) | (_, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{add_usage, normalized_usage};

    #[test]
    fn normalizes_cache_subsets_without_adding_them_twice() {
        let usage = normalized_usage(Some(10), Some(20), Some(3), Some(7), Some(2), false)
            .expect("complete usage");
        assert_eq!(usage.input, 33);
        assert_eq!(usage.cached, Some(20));
        assert_eq!(usage.cache_write, Some(3));
        assert_eq!(usage.output, 7);
        assert_eq!(usage.reasoning, Some(2));
    }

    #[test]
    fn an_unreported_optional_subset_keeps_the_loop_total_unknown() {
        let first =
            normalized_usage(Some(10), Some(4), None, Some(2), Some(1), true).expect("first usage");
        let second =
            normalized_usage(Some(20), None, None, Some(3), Some(1), true).expect("second usage");
        let total = add_usage(Some(first), second);
        assert_eq!(total.input, 30);
        assert_eq!(total.cached, None);
        assert_eq!(total.cache_write, None);
        assert_eq!(total.output, 5);
        assert_eq!(total.reasoning, Some(2));
    }
}

#[derive(Debug)]
pub(crate) struct SessionProjection {
    pub session_uuid: String,
    /// Whether a Runtime structure directly established `session_uuid`.
    /// Incomplete adapter builders may exist while scanning, but publication
    /// rejects a Source that never supplies this evidence.
    pub identity_confirmed: bool,
    pub start_seq: u64,
    pub started_at: Option<i64>,
    pub cwd: Option<String>,
    pub forked_from_native_id: Option<String>,
    pub forked_from_locator: Option<String>,
    pub forked_from_record_seq: Option<u64>,
    pub delegated_from_native_id: Option<String>,
    pub delegated_from_record_seq: Option<u64>,
    pub title: Option<String>,
    /// Session-scoped facts for which no Loop membership is structurally
    /// established. They publish with null `loop_id` and `loop_position`.
    pub items: Vec<ItemProjection>,
    /// Loops in their structural order inside this Source.
    pub loops: Vec<LoopProjection>,
}

/// Physical classification of one JSONL Record, produced before semantic
/// projection so malformed input can still be described precisely.
#[derive(Debug)]
pub(crate) struct RecordFacts {
    pub ts_ms: Option<i64>,
    pub native_type: String,
    pub parse_status: &'static str,
    pub parse_error: Option<String>,
}

/// Joins a Runtime's two-level Record type into one native discriminant.
///
/// Adapters route on the pair, never on the refinement alone, and the
/// refinement is drawn from a different field in each runtime. One value keeps
/// the only correct comparison — against `source_adapter` — the natural one.
pub(crate) fn native_type(kind: &str, refinement: Option<&str>) -> String {
    match refinement {
        Some(refinement) => format!("{kind}/{refinement}"),
        None => kind.to_owned(),
    }
}

impl RecordFacts {
    pub(crate) fn malformed(error: String) -> Self {
        Self {
            ts_ms: None,
            native_type: "unknown".to_owned(),
            parse_status: "malformed",
            parse_error: Some(error),
        }
    }

    pub(crate) fn oversized() -> Self {
        Self {
            ts_ms: None,
            native_type: "unknown".to_owned(),
            parse_status: "oversized",
            parse_error: Some("record exceeded the configured maximum size".to_owned()),
        }
    }
}
