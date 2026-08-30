//! Detects the source runtime and dispatches records to its projector.

use anyhow::{Result, bail};
use serde_json::Value;

use super::projection::{RecordFacts, SessionProjection};
use super::runtime::claude::ClaudeProjector;
use super::runtime::codex::CodexProjector;
use super::runtime::pi::PiProjector;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AdapterKind {
    Codex,
    Pi,
    Claude,
}

impl AdapterKind {
    pub(crate) const fn name(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Pi => "pi",
            Self::Claude => "claude",
        }
    }
}

pub(crate) fn detect(bytes: &[u8]) -> Result<AdapterKind> {
    let value: Value = serde_json::from_slice(bytes).map_err(|error| {
        anyhow::anyhow!("cannot detect trace adapter from malformed JSON: {error}")
    })?;
    let record_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    if record_type == "session"
        && value.get("id").and_then(Value::as_str).is_some()
        && value.get("cwd").and_then(Value::as_str).is_some()
    {
        return Ok(AdapterKind::Pi);
    }
    if record_type == "message"
        && value.get("id").and_then(Value::as_str).is_some()
        && value.get("message").and_then(Value::as_object).is_some()
    {
        return Ok(AdapterKind::Pi);
    }
    // Claude Code stamps the session on nearly every record, including the
    // control-plane ones a file can open with. The exception is a subagent
    // launch record, which names the agent instead.
    if value.get("sessionId").and_then(Value::as_str).is_some() {
        return Ok(AdapterKind::Claude);
    }
    if matches!(record_type, "started" | "result")
        && value.get("agentId").and_then(Value::as_str).is_some()
    {
        return Ok(AdapterKind::Claude);
    }
    if matches!(
        record_type,
        "session_meta"
            | "turn_context"
            | "response_item"
            | "event_msg"
            | "world_state"
            | "compacted"
    ) {
        return Ok(AdapterKind::Codex);
    }
    bail!("unsupported trace format: first record type is {record_type:?}")
}

pub(crate) fn record_facts(adapter: AdapterKind, value: &Value) -> RecordFacts {
    match adapter {
        AdapterKind::Codex => super::runtime::codex::record_facts(value),
        AdapterKind::Pi => super::runtime::pi::record_facts(value),
        AdapterKind::Claude => super::runtime::claude::record_facts(value),
    }
}

/// Runtime-specific stateful projector.
///
/// Projection is per-source rather than per-record: turn membership,
/// dual-track pairing and call/output correlation all need context a single
/// line does not carry.
#[derive(Debug)]
pub(crate) enum Projector {
    // Boxed: a projector holds a whole in-flight session, and the two runtimes
    // carry very different amounts of state.
    Codex(Box<CodexProjector>),
    Pi(Box<PiProjector>),
    Claude(Box<ClaudeProjector>),
}

impl Projector {
    pub(crate) fn new(adapter: AdapterKind, max_text_bytes: usize) -> Self {
        match adapter {
            AdapterKind::Codex => Self::Codex(Box::new(CodexProjector::new(max_text_bytes))),
            AdapterKind::Pi => Self::Pi(Box::new(PiProjector::new(max_text_bytes))),
            AdapterKind::Claude => Self::Claude(Box::new(ClaudeProjector::new(max_text_bytes))),
        }
    }

    pub(crate) fn push(&mut self, seq: u64, value: &Value) {
        match self {
            Self::Codex(projector) => projector.push(seq, value),
            Self::Pi(projector) => projector.push(seq, value),
            Self::Claude(projector) => projector.push(seq, value),
        }
    }

    /// Hands over sessions that are already closed, so a long source is not
    /// held in memory all at once.
    pub(crate) fn drain_completed(&mut self) -> Vec<SessionProjection> {
        match self {
            Self::Codex(projector) => projector.drain_completed(),
            Self::Pi(projector) => projector.drain_completed(),
            Self::Claude(projector) => projector.drain_completed(),
        }
    }

    pub(crate) fn finish(self) -> Vec<SessionProjection> {
        match self {
            Self::Codex(projector) => projector.finish(),
            Self::Pi(projector) => projector.finish(),
            Self::Claude(projector) => projector.finish(),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{AdapterKind, detect, record_facts};

    #[test]
    fn detects_codex_and_pi_headers() {
        assert_eq!(
            detect(br#"{"type":"session_meta","payload":{"id":"s"}}"#).unwrap(),
            AdapterKind::Codex
        );
        assert_eq!(
            detect(br#"{"type":"session","id":"s","cwd":"/tmp"}"#).unwrap(),
            AdapterKind::Pi
        );
        assert_eq!(
            detect(br#"{"type":"message","id":"e","message":{"role":"assistant"}}"#).unwrap(),
            AdapterKind::Pi
        );
    }

    #[test]
    fn detects_claude_from_any_opening_record() {
        // Claude Code files open with whatever the session wrote first, so
        // detection rests on the session stamp rather than a header record.
        for opening in [
            br#"{"type":"user","sessionId":"s","uuid":"u","message":{"role":"user"}}"#.as_slice(),
            br#"{"type":"mode","mode":"default","sessionId":"s"}"#.as_slice(),
            br#"{"type":"queue-operation","operation":"add","sessionId":"s"}"#.as_slice(),
        ] {
            assert_eq!(detect(opening).unwrap(), AdapterKind::Claude);
        }
        // A subagent launch record names the agent instead of the session.
        assert_eq!(
            detect(br#"{"type":"started","key":"v2:a","agentId":"a1"}"#).unwrap(),
            AdapterKind::Claude
        );
    }

    #[test]
    fn each_adapter_reads_its_native_type_refinement_from_its_own_field() {
        // The refinement is `payload.type` in Codex, `message.role` in Pi and
        // one of four fields in Claude Code. Identical strings across adapters
        // would therefore mean different things, which is why they are stored
        // joined and only ever compared alongside `source_adapter`.
        let cases = [
            (
                AdapterKind::Codex,
                json!({"type": "response_item", "payload": {"type": "message"}}),
                "response_item/message",
            ),
            (
                AdapterKind::Pi,
                json!({"type": "message", "message": {"role": "user"}}),
                "message/user",
            ),
            (
                AdapterKind::Claude,
                json!({"type": "system", "subtype": "local_command"}),
                "system/local_command",
            ),
            (
                AdapterKind::Claude,
                json!({"type": "attachment", "attachment": {"type": "file"}}),
                "attachment/file",
            ),
            (
                AdapterKind::Claude,
                json!({"type": "user", "message": {"content": [{"type": "tool_result"}]}}),
                "user/tool_result",
            ),
        ];
        for (adapter, value, expected) in cases {
            assert_eq!(record_facts(adapter, &value).native_type, expected);
        }

        // A record the runtime does not refine keeps the bare type, so a
        // prefix comparison covers both shapes of the same kind.
        assert_eq!(
            record_facts(AdapterKind::Codex, &json!({"type": "turn_context"})).native_type,
            "turn_context"
        );
    }
}
