//! Local Antigravity CLI conversation transcript projection.

use std::path::Path;

use serde_json::Value;

use crate::adapters::projection::{
    BoundedText, ItemDetail, ItemProjection, LoopProjection, RecordFacts, SessionProjection,
    native_type,
};
use crate::adapters::semantic::{self, basis};
use crate::adapters::timestamp::parse_timestamp_ms;
use crate::domain::{Model, ReasoningRepresentation};
use crate::shell::syntax::lang;

/// Returns true when `path` is an Antigravity conversation directory
/// (`.../brain/<conversation-uuid>`).
///
/// Discovery uses this to jump straight to `.system_generated/logs` instead of
/// recursively walking thousands of per-step artifact directories.
pub(crate) fn conversation_dir(path: &Path) -> bool {
    let Some(parent_name) = path.parent().and_then(Path::file_name) else {
        return false;
    };
    if parent_name != "brain" {
        return false;
    }
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(is_uuid)
}

/// The producer places its conversation UUID in the transcript locator, not
/// in each payload. The exact layout also excludes chunk copies and artifacts.
pub(crate) fn transcript(path: &Path) -> Option<(String, bool)> {
    let full = match path.file_name()?.to_str()? {
        "transcript_full.jsonl" => true,
        "transcript.jsonl" => false,
        _ => return None,
    };
    let logs = path.parent()?;
    let generated = logs.parent()?;
    let conversation = generated.parent()?;
    if logs.file_name()? != "logs"
        || generated.file_name()? != ".system_generated"
        || !conversation_dir(conversation)
    {
        return None;
    }
    let id = conversation.file_name()?.to_str()?;
    Some((id.to_ascii_lowercase(), full))
}

fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.bytes().enumerate().all(|(index, byte)| {
            if matches!(index, 8 | 13 | 18 | 23) {
                byte == b'-'
            } else {
                byte.is_ascii_hexdigit()
            }
        })
}

pub(crate) fn is_step(value: &Value) -> bool {
    value.get("step_index").and_then(Value::as_u64).is_some()
        && string(value, "source").is_some()
        && string(value, "type").is_some()
        && string(value, "status").is_some()
        && timestamp(value).is_some()
}

pub(crate) fn record_facts(value: &Value) -> RecordFacts {
    RecordFacts {
        ts_ms: timestamp(value),
        native_type: native_type(
            string(value, "source").unwrap_or("unknown"),
            string(value, "type"),
        ),
        parse_status: "ok",
        parse_error: None,
    }
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn timestamp(value: &Value) -> Option<i64> {
    string(value, "created_at").and_then(parse_timestamp_ms)
}

#[derive(Debug)]
pub(crate) struct AntigravityProjector {
    session: SessionProjection,
    max_text_bytes: usize,
}

impl AntigravityProjector {
    pub(crate) fn new(session_uuid: String, max_text_bytes: usize) -> Self {
        Self {
            session: SessionProjection {
                session_uuid,
                identity_confirmed: false,
                start_seq: 0,
                started_at: None,
                cwd: None,
                forked_from_native_id: None,
                forked_from_locator: None,
                forked_from_record_seq: None,
                delegated_from_native_id: None,
                delegated_from_record_seq: None,
                title: None,
                items: Vec::new(),
                loops: Vec::new(),
            },
            max_text_bytes,
        }
    }

    pub(crate) fn finish(self) -> SessionProjection {
        self.session
    }

    /// Returns true when an already-indexed prefix line provably cannot affect
    /// Antigravity projection, avoiding full JSON DOM allocation on ignored
    /// `GENERIC` / `CHECKPOINT` steps during incremental append scans.
    pub(crate) fn can_skip_prefix_record(&self, bytes: &[u8]) -> bool {
        if !self.session.identity_confirmed {
            return false;
        }
        // Antigravity writes top-level metadata (`step_index`, `source`,
        // `type`, `status`, `created_at`) before any free-text payload keys.
        // Inspect only the metadata prefix before the first payload key so we
        // never scan multi-kilobyte `content` or `thinking` strings and cannot
        // match text inside a payload value.
        let head = &bytes[..bytes.len().min(256)];
        let meta_end = earliest_subslice(
            head,
            &[
                b"\"content\"",
                b"\"thinking\"",
                b"\"tool_calls\"",
                b"\"error\"",
                b"\"media\"",
            ],
        )
        .unwrap_or(head.len());
        let meta = &head[..meta_end];
        !contains_subslice(meta, b"\\u")
            && (contains_subslice(meta, b"\"type\":\"GENERIC\"")
                || contains_subslice(meta, b"\"type\": \"GENERIC\"")
                || contains_subslice(meta, b"\"type\":\"CHECKPOINT\"")
                || contains_subslice(meta, b"\"type\": \"CHECKPOINT\""))
    }

    pub(crate) fn push(&mut self, seq: u64, value: &Value) {
        if !is_step(value) {
            return;
        }
        if !self.session.identity_confirmed {
            self.session.identity_confirmed = true;
            self.session.start_seq = seq;
        }
        let step = Step {
            seq,
            value,
            max_text_bytes: self.max_text_bytes,
        };
        match (string(value, "source"), string(value, "type")) {
            (Some("USER_EXPLICIT"), Some("USER_INPUT")) => {
                // The persisted log gives an explicit input but omits the
                // outer execution id and terminal event. Partition on input
                // for navigation, marking the request classification heuristic.
                self.session.loops.push(LoopProjection {
                    native_id: None,
                    start_seq: seq,
                    end_record_seq: None,
                    outcome: None,
                    model: None,
                    usage: None,
                    items: vec![step.message(semantic::HUMAN_REQUEST, basis::NO_MARKER)],
                });
            }
            (Some("MODEL"), Some("PLANNER_RESPONSE")) => self.planner_response(&step),
            (Some("SYSTEM" | "SYSTEM_SDK"), Some("SYSTEM_MESSAGE"))
                if string(value, "content").is_some() =>
            {
                self.push_item(step.message(semantic::RUNTIME_UNKNOWN, basis::NATIVE_SUBTYPE));
            }
            (Some("SYSTEM"), Some("ERROR_MESSAGE")) => {
                let mut item = step.message(semantic::RUNTIME_NOTICE, basis::NATIVE_SUBTYPE);
                if item.preview.is_none() {
                    item.preview = step.blob("error");
                }
                self.push_item(item);
            }
            // Generic display rows and checkpoints have no reliable call
            // identity or explicit readable-summary contract in this Source.
            _ => {}
        }
    }

    fn planner_response(&mut self, step: &Step<'_>) {
        if let Some(active) = self.session.loops.last_mut()
            && let Some(model) = string(step.value, "model")
        {
            active.model = Some(Model {
                id: model.to_owned(),
                effort: None,
                context_window: None,
            });
        }
        if let Some(thinking) = step.blob("thinking") {
            self.push_item(step.item(
                semantic::AGENT_REASONING,
                basis::NATIVE_SUBTYPE,
                Some(thinking),
                ItemDetail::Reasoning {
                    representation: ReasoningRepresentation::Full,
                },
            ));
        }
        if string(step.value, "content").is_some() {
            self.push_item(
                step.message(semantic::AGENT_COMMENTARY, basis::PHASE_FALLBACK_COMMENTARY),
            );
        }
        if let Some(calls) = step.value.get("tool_calls").and_then(Value::as_array) {
            for (position, call) in calls.iter().enumerate() {
                if let Some(name) = string(call, "name") {
                    self.push_item(step.tool_call(position, call, name));
                }
            }
        }
    }

    fn push_item(&mut self, item: ItemProjection) {
        if let Some(active) = self.session.loops.last_mut() {
            active.items.push(item);
        } else {
            self.session.items.push(item);
        }
    }
}

struct Step<'a> {
    seq: u64,
    value: &'a Value,
    max_text_bytes: usize,
}

impl Step<'_> {
    fn blob(&self, key: &str) -> Option<BoundedText> {
        string(self.value, key).map(|text| BoundedText::bounded(text, self.max_text_bytes))
    }

    fn item(
        &self,
        role: &str,
        basis: &str,
        preview: Option<BoundedText>,
        detail: ItemDetail,
    ) -> ItemProjection {
        ItemProjection {
            seq: self.seq,
            record_seq: self.seq,
            ui_seq: None,
            ts_ms: timestamp(self.value),
            semantic_role: role.to_owned(),
            basis: basis.to_owned(),
            preview,
            detail,
            linked_session_native_id: None,
            searchable: semantic::is_conversation(role),
        }
    }

    fn message(&self, role: &str, basis: &str) -> ItemProjection {
        let has_images = self
            .value
            .get("media")
            .and_then(Value::as_array)
            .is_some_and(|media| {
                media.iter().any(|entry| {
                    string(entry, "mime_type").is_some_and(|mime| mime.starts_with("image/"))
                })
            });
        self.item(
            role,
            basis,
            self.blob("content"),
            ItemDetail::Message { has_images },
        )
    }

    fn tool_call(&self, position: usize, call: &Value, name: &str) -> ItemProjection {
        let args = call.get("args").filter(|args| !args.is_null());
        let cmd = args
            .filter(|_| name == "run_command")
            .and_then(|args| string(args, "CommandLine"));
        self.item(
            semantic::AGENT_TOOL_CALL,
            basis::BLOCK_TYPE,
            None,
            ItemDetail::ToolCall {
                // Private storage key; the transcript does not expose a call id.
                call_id: format!("{}:{position}", self.seq),
                name: Some(name.to_owned()),
                cmd: cmd.map(str::to_owned),
                cmd_lang: cmd.map(|_| lang::SHELL),
                workdir: args
                    .filter(|_| name == "run_command")
                    .and_then(|args| string(args, "Cwd"))
                    .map(str::to_owned),
                args: args.and_then(|args| {
                    crate::adapters::projection::complete_arguments(
                        &args.to_string(),
                        self.max_text_bytes,
                    )
                }),
                syntax: None,
            },
        )
    }
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn earliest_subslice(haystack: &[u8], needles: &[&[u8]]) -> Option<usize> {
    needles
        .iter()
        .filter_map(|needle| {
            haystack
                .windows(needle.len())
                .position(|window| window == *needle)
        })
        .min()
}
