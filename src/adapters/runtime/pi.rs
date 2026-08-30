//! Stateful Runtime adapter for Pi session JSONL.
//!
//! The format is declared exhaustively in the runtime's own
//! `packages/coding-agent/src/core/session-manager.ts`: one `session` header
//! plus nine entry types, every one of them carrying `{id, parentId,
//! timestamp}`. This projector follows that declaration rather than the shapes
//! a corpus happens to contain.
//!
//! Two consequences differ from what reading traces alone suggested:
//!
//! * Pi records form a tree, not a sequence. `parentId` is on every entry, and
//!   `branch_summary` exists to summarize a branch the conversation returned
//!   from, so lineage is kept as a fact here exactly as it is for Claude Code.
//! * Pi's assistant messages carry `stopReason`, so the answer that concludes a
//!   Loop and the Loop's outcome are both structural facts rather than
//!   inferences from position.
//!
//! Pi has no native outer-loop Record. A human message opens a Loop; another
//! human message delivered while that Loop is open is steering.
//!
//! Which makes what does *not* open one part of the same choice. Pi begins
//! every session with entries that declare Session-scoped state — the model,
//! the thinking level, the display name — and reading those as the start of a
//! Loop produced one settings-only Loop per Session, permanently open because
//! no reply could belong to it. They remain Records and configure the next
//! real Loop instead.

use serde_json::Value;

use crate::adapters::projection::{
    self, BoundedText, ItemDetail, ItemProjection, LoopProjection, RecordFacts, SessionProjection,
    ToolOutputFacts,
};
use crate::adapters::semantic::{self, basis};

/// `StopReason` from `packages/ai/src/types.ts`: `stop | length | toolUse |
/// error | aborted`.
///
/// The three Loop outcomes map onto these native reasons directly. `stop` is the
/// model concluding. `toolUse` is the model saying it means to continue, so a
/// Loop ending there is unfinished rather than interrupted — the file simply
/// stopped. The remaining three are the provider reporting the reply did not
/// complete, and each keeps its own name as the abort reason.
const STOP_REASON_STOP: &str = "stop";
const STOP_REASON_TOOL_USE: &str = "toolUse";

const SETTINGS_MODEL: &str = "model_change";
const SETTINGS_EFFORT: &str = "thinking_level_change";

pub(crate) fn record_facts(value: &Value) -> RecordFacts {
    let kind = string(value, "type").unwrap_or("unknown");
    // Every entry type names its own second level. A message's is the wire role
    // it carries; an extension entry's is the `customType` it declared, which
    // is an open set and therefore worth preserving verbatim even when no rule
    // recognizes it.
    let refinement = match kind {
        "message" => value
            .get("message")
            .and_then(|message| string(message, "role")),
        "custom" | "custom_message" => string(value, "customType"),
        _ => None,
    };
    RecordFacts {
        ts_ms: string(value, "timestamp").and_then(parse_timestamp_ms),
        native_type: projection::native_type(kind, refinement),
        parse_status: "ok",
        parse_error: None,
    }
}

#[derive(Debug)]
struct LoopBuilder {
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    model: Option<String>,
    effort: Option<String>,
    usage: Option<crate::domain::Usage>,
    items: Vec<ItemProjection>,
    request_item: Option<usize>,
    final_answer_item: Option<usize>,
    /// The `stopReason` of the most recent assistant message, which is what
    /// says how the Loop ended. Absent while no model reply has landed.
    last_stop_reason: Option<String>,
    last_stop_seq: Option<u64>,
}

impl LoopBuilder {
    fn new(start_seq: u64) -> Self {
        Self {
            start_seq,
            end_seq: start_seq,
            started_at: None,
            ended_at: None,
            model: None,
            effort: None,
            usage: None,
            items: Vec::new(),
            request_item: None,
            final_answer_item: None,
            last_stop_reason: None,
            last_stop_seq: None,
        }
    }

    fn push(&mut self, item: ItemProjection) -> usize {
        self.end_seq = self.end_seq.max(item.seq);
        self.items.push(item);
        self.items.len() - 1
    }

    /// Whether the provider reported the reply did not complete, which makes
    /// the next human message a correction rather than a fresh request.
    fn aborted(&self) -> bool {
        self.last_stop_reason
            .as_deref()
            .is_some_and(|reason| !matches!(reason, STOP_REASON_STOP | STOP_REASON_TOOL_USE))
    }

    fn finish(self) -> LoopProjection {
        let outcome = match self.last_stop_reason.as_deref() {
            Some(STOP_REASON_STOP) => Some(crate::domain::LoopOutcome::Completed),
            Some("aborted") => Some(crate::domain::LoopOutcome::Interrupted),
            Some("error" | "length") => Some(crate::domain::LoopOutcome::Failed),
            _ => None,
        };
        LoopProjection {
            native_id: None,
            start_seq: self.start_seq,
            end_record_seq: outcome.and(self.last_stop_seq),
            outcome,
            model: self.model.map(|id| crate::domain::Model {
                id,
                effort: self.effort,
                context_window: None,
            }),
            usage: self.usage,
            items: self.items,
        }
    }
}

#[derive(Debug)]
struct SessionBuilder {
    session_uuid: String,
    identity_confirmed: bool,
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    cwd: Option<String>,
    forked_from_locator: Option<String>,
    title: Option<String>,
    model_provider: Option<String>,
    loops: Vec<LoopProjection>,
    items: Vec<ItemProjection>,
    active_loop: Option<LoopBuilder>,
    /// Settings declarations that arrived before any Loop existed.
    ///
    /// Pi opens every session with `model_change` and `thinking_level_change`,
    /// ahead of the first human message. They declare state rather than report
    /// that anything happened — Pi's own `sessionEntryToContextMessages`
    /// returns nothing for them, and only `getSessionContextSettings` reads
    /// them — so they must not open a Loop. Letting them did: one phantom Loop
    /// per session, holding nothing but settings and permanently `open`,
    /// because no reply could ever belong to it.
    /// The model those settings named, waiting for the same Loop.
    pending_model: Option<String>,
    pending_effort: Option<String>,
}

impl SessionBuilder {
    fn new(session_uuid: String, start_seq: u64, identity_confirmed: bool) -> Self {
        Self {
            session_uuid,
            identity_confirmed,
            start_seq,
            end_seq: start_seq,
            started_at: None,
            cwd: None,
            forked_from_locator: None,
            title: None,
            model_provider: None,
            loops: Vec::new(),
            items: Vec::new(),
            active_loop: None,
            pending_model: None,
            pending_effort: None,
        }
    }

    fn loop_mut(&mut self, seq: u64) -> &mut LoopBuilder {
        if self.active_loop.is_none() {
            self.active_loop = Some(LoopBuilder::new(seq));
            self.adopt_pending_settings();
        }
        self.active_loop.as_mut().expect("Loop was just ensured")
    }

    /// Hands settings that preceded a Loop to the Loop they configure.
    ///
    /// They come first in the file, so the Loop's range extends back to cover
    /// them and they lead its item list, which is where the reader looking for
    /// "what model answered this" expects to find them.
    fn adopt_pending_settings(&mut self) {
        if self.pending_model.is_none() && self.pending_effort.is_none() {
            return;
        }
        let model = self.pending_model.take();
        let effort = self.pending_effort.take();
        let Some(turn) = self.active_loop.as_mut() else {
            self.pending_model = model;
            self.pending_effort = effort;
            return;
        };
        turn.model = turn.model.take().or(model);
        turn.effort = turn.effort.take().or(effort);
    }

    /// Opens the Loop a human message begins.
    ///
    /// Separate from `loop_mut` because only the caller knows which Record is
    /// opening the Loop: by the time `loop_mut` runs, a message and a tool
    /// result look alike, and it would file every boundary as implicit.
    fn open_loop_for_human(&mut self, seq: u64) {
        match self.active_loop.as_mut() {
            // Opened for an earlier record but never written to, so this
            // message is still what the Loop begins with.
            Some(turn) if turn.items.is_empty() => {}
            Some(_) => {}
            None => {
                self.active_loop = Some(LoopBuilder::new(seq));
            }
        }
        // The settings that opened the file configure this Loop, and the
        // boundary stays the human message: a settings record is not why the
        // Loop exists.
        self.adopt_pending_settings();
    }

    fn close_loop(&mut self) {
        if let Some(turn) = self.active_loop.take() {
            self.end_seq = self.end_seq.max(turn.end_seq);
            let finished = turn.finish();
            self.loops.push(finished);
        }
    }

    fn finish(mut self) -> SessionProjection {
        self.close_loop();
        SessionProjection {
            session_uuid: self.session_uuid,
            identity_confirmed: self.identity_confirmed,
            start_seq: self.start_seq,
            started_at: self.started_at,
            cwd: self.cwd,
            forked_from_record_seq: self.forked_from_locator.as_ref().map(|_| self.start_seq),
            forked_from_native_id: None,
            forked_from_locator: self.forked_from_locator,
            delegated_from_native_id: None,
            delegated_from_record_seq: None,
            title: self.title,
            items: self.items,
            loops: self.loops,
        }
    }
}

#[derive(Debug)]
pub(crate) struct PiProjector {
    max_text_bytes: usize,
    sessions: Vec<SessionProjection>,
    session: Option<SessionBuilder>,
    next_session_ordinal: u32,
}

impl PiProjector {
    pub(crate) fn new(max_text_bytes: usize) -> Self {
        Self {
            max_text_bytes,
            sessions: Vec::new(),
            session: None,
            next_session_ordinal: 0,
        }
    }

    pub(crate) fn drain_completed(&mut self) -> Vec<SessionProjection> {
        std::mem::take(&mut self.sessions)
    }

    pub(crate) fn finish(mut self) -> Vec<SessionProjection> {
        if let Some(session) = self.session.take() {
            self.sessions.push(session.finish());
        }
        self.sessions
    }

    fn session_mut(&mut self, seq: u64) -> &mut SessionBuilder {
        if self.session.is_none() {
            let ordinal = self.next_session_ordinal;
            self.next_session_ordinal += 1;
            self.session = Some(SessionBuilder::new(
                format!("unlabeled-{ordinal}"),
                seq,
                false,
            ));
        }
        self.session.as_mut().expect("session was just ensured")
    }

    pub(crate) fn push(&mut self, seq: u64, value: &Value) {
        let record_kind = string(value, "type").unwrap_or("unknown");
        let ts_ms = string(value, "timestamp").and_then(parse_timestamp_ms);
        match record_kind {
            "session" => self.begin_session(seq, value, ts_ms),
            "message" => self.message(seq, value, ts_ms),
            // Both carry a `summary` standing in for context the model can no
            // longer see: one for history that was compacted away, one for a
            // branch the conversation came back from. Same purpose, so the same
            // role, with `native_type` keeping them apart.
            "compaction" | "branch_summary" => self.summary_entry(seq, value, ts_ms),
            "custom_message" => self.custom_message(seq, value, ts_ms),
            "session_info" => self.session_info(seq, value),
            SETTINGS_MODEL => self.model_change(seq, value),
            SETTINGS_EFFORT => self.effort_change(seq, value),
            // Settings, labels, extension state, and unknown entry types remain
            // Records. They do not become Items unless Pi puts them into the
            // Agent program through one of the explicit shapes above.
            _ => {}
        }
    }

    fn begin_session(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        if let Some(session) = self.session.take() {
            self.sessions.push(session.finish());
        }
        let ordinal = self.next_session_ordinal;
        self.next_session_ordinal += 1;
        let native_id = string(value, "id");
        let uuid = native_id.map_or_else(|| format!("unlabeled-{ordinal}"), str::to_owned);
        let mut session = SessionBuilder::new(uuid, seq, native_id.is_some());
        session.started_at = ts_ms;
        session.cwd = string(value, "cwd").map(str::to_owned);
        session.forked_from_locator = string(value, "parentSession").map(str::to_owned);
        self.session = Some(session);
    }

    #[allow(
        clippy::too_many_lines,
        reason = "one pass keeps Pi content-block ordering intact across message, reasoning and tool-call items"
    )]
    fn message(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let message = value.get("message").unwrap_or(&Value::Null);
        let role = string(message, "role").unwrap_or("unknown").to_owned();

        if role == "toolResult" {
            self.tool_result(seq, message, ts_ms);
            return;
        }

        let model = string(message, "model").map(str::to_owned);
        let provider = string(message, "provider").map(str::to_owned);
        let stop_reason = string(message, "stopReason").map(str::to_owned);
        let usage = message.get("usage").and_then(|usage| {
            projection::normalized_usage(
                usage.get("input").and_then(Value::as_u64),
                usage.get("cacheRead").and_then(Value::as_u64),
                usage.get("cacheWrite").and_then(Value::as_u64),
                usage.get("output").and_then(Value::as_u64),
                usage.get("reasoning").and_then(Value::as_u64),
                false,
            )
        });

        // `/skill name args` expands into a single user message holding the
        // skill body and then the person's own words. The runtime splits them
        // back apart with `parseSkillBlock`, and so does this: leaving the
        // request inside the injection hides it from `messages` and from
        // search, which is the mistake `semantic_role` exists to prevent.
        let (text, trailing_request) = match (role.as_str(), message_text(message)) {
            ("user", Some(whole)) => match split_skill_block(&whole) {
                Some((block, trailing)) => (Some(block.to_owned()), trailing.map(str::to_owned)),
                None => (Some(whole), None),
            },
            (_, text) => (text, None),
        };
        let reasoning = reasoning_text(message);
        let tool_calls = collect_tool_calls(message);
        let has_images = has_images(message);

        // A human message opens a Loop only when no Loop is still running.
        // Pi persists steering with the same user-message shape, so arrival
        // while the current Loop has no terminal stop reason remains in that
        // Loop and is classified by this structural position.
        let mut steers_current = false;
        if role == "user" {
            let session = self.session_mut(seq);
            let current_ended = session.active_loop.as_ref().is_some_and(|turn| {
                turn.aborted() || turn.last_stop_reason.as_deref() == Some(STOP_REASON_STOP)
            });
            if current_ended {
                session.close_loop();
            }
            steers_current = session.active_loop.is_some();
            if !steers_current {
                session.open_loop_for_human(seq);
            }
        }

        let (semantic_role, why) = match role.as_str() {
            "user" => text.as_deref().map_or(
                (semantic::HUMAN_REQUEST.to_owned(), basis::WIRE_ROLE_USER),
                |text| {
                    let (injected, why) = semantic::runtime_injection(text);
                    if injected == semantic::RUNTIME_UNKNOWN && why == basis::NO_MARKER {
                        (semantic::HUMAN_REQUEST.to_owned(), basis::WIRE_ROLE_USER)
                    } else {
                        (injected.to_owned(), why)
                    }
                },
            ),
            // `stopReason` is on every assistant message the runtime writes, so
            // the answer that concludes a Loop is a reported fact here rather
            // than the last reply by position.
            "assistant" => match stop_reason.as_deref() {
                Some(STOP_REASON_STOP) => (
                    semantic::AGENT_FINAL_ANSWER.to_owned(),
                    basis::STOP_REASON_END_LOOP,
                ),
                Some(_) => (
                    semantic::AGENT_COMMENTARY.to_owned(),
                    basis::STOP_REASON_CONTINUES,
                ),
                None => (semantic::AGENT_COMMENTARY.to_owned(), basis::RECORD_KIND),
            },
            _ => (semantic::RUNTIME_UNKNOWN.to_owned(), basis::RECORD_KIND),
        };

        let is_human = semantic_role.starts_with("human.");
        let is_assistant = role == "assistant";
        let is_final_answer = semantic_role == semantic::AGENT_FINAL_ANSWER;
        let session = self.session_mut(seq);
        if let Some(provider) = provider {
            session.model_provider.get_or_insert(provider);
        }
        let turn = session.loop_mut(seq);
        turn.started_at = turn.started_at.or(ts_ms);
        turn.ended_at = ts_ms.or(turn.ended_at);
        turn.model = turn.model.take().or(model);
        if let Some(usage) = usage {
            turn.usage = Some(projection::add_usage(turn.usage.take(), usage));
        }
        if is_assistant {
            turn.last_stop_reason = stop_reason;
            turn.last_stop_seq = Some(seq);
        }

        // Every mixed assistant message in the observed Pi corpus has the same
        // shape: contiguous thinking, then contiguous text, then tool calls.
        // Keep that real order without an ordering layer for shapes Pi has not
        // produced.
        if let Some(reasoning) = reasoning {
            let preview = self.blob(&reasoning);
            self.session_mut(seq).loop_mut(seq).push(ItemProjection {
                seq,
                record_seq: seq,
                ui_seq: None,
                ts_ms,
                semantic_role: semantic::AGENT_REASONING.to_owned(),
                basis: basis::BLOCK_TYPE.to_owned(),
                preview: Some(preview),
                detail: ItemDetail::Reasoning {
                    representation: crate::domain::ReasoningRepresentation::Full,
                },
                searchable: false,
                linked_session_native_id: None,
            });
        }

        let omit_empty_commentary = is_assistant
            && semantic_role == semantic::AGENT_COMMENTARY
            && text.is_none()
            && !has_images;
        let mut human_index = None;
        if !omit_empty_commentary {
            let preview = text.as_deref().map(|text| self.blob(text));
            let turn = self.session_mut(seq).loop_mut(seq);
            let index = turn.push(ItemProjection {
                seq,
                record_seq: seq,
                ui_seq: None,
                ts_ms,
                semantic_role,
                basis: why.to_owned(),
                preview,
                detail: ItemDetail::Message { has_images },
                searchable: is_human || is_assistant,
                linked_session_native_id: None,
            });
            if is_final_answer {
                turn.final_answer_item = Some(index);
            }
            if is_human {
                human_index = Some(index);
            }
        }

        // Whichever Item a person actually wrote: the message itself, or the
        // tail left after a skill body was expanded ahead of it.
        if let Some(request) = trailing_request {
            let preview = self.blob(&request);
            human_index = Some(self.session_mut(seq).loop_mut(seq).push(ItemProjection {
                seq,
                record_seq: seq,
                ui_seq: None,
                ts_ms,
                semantic_role: semantic::HUMAN_REQUEST.to_owned(),
                basis: basis::WIRE_ROLE_USER.to_owned(),
                preview: Some(preview),
                detail: ItemDetail::Message { has_images },
                searchable: true,
                linked_session_native_id: None,
            }));
        }
        if let Some(index) = human_index {
            let position = if steers_current {
                basis::SUBSEQUENT_IN_LOOP
            } else {
                basis::FIRST_IN_LOOP
            };
            let turn = self.session_mut(seq).loop_mut(seq);
            let item = &mut turn.items[index];
            item.basis = semantic::compose_basis(&item.basis, position);
            if steers_current {
                semantic::HUMAN_STEERING.clone_into(&mut item.semantic_role);
            }
            if !steers_current && turn.request_item.is_none() {
                turn.request_item = Some(index);
            }
        }

        for call in tool_calls {
            let args = self.blob(&call.arguments);
            let preview = self.blob(&call.name);
            self.session_mut(seq).loop_mut(seq).push(ItemProjection {
                seq,
                record_seq: seq,
                ui_seq: None,
                ts_ms,
                semantic_role: semantic::AGENT_TOOL_CALL.to_owned(),
                basis: basis::RECORD_KIND.to_owned(),
                preview: Some(preview),
                detail: ItemDetail::ToolCall {
                    call_id: call.id,
                    cmd_lang: (call.name == PI_SHELL_TOOL)
                        .then_some(crate::shell::syntax::lang::SHELL),
                    name: Some(call.name),
                    cmd: call.cmd,
                    // Pi's bash tool takes no working directory: `command` and
                    // `timeout` are its only parameters, and the executor is
                    // handed a `cwd` the model never sees.
                    workdir: None,
                    args: Some(args),
                    syntax: None,
                },
                searchable: false,
                linked_session_native_id: None,
            });
        }
    }

    fn tool_result(&mut self, seq: u64, message: &Value, ts_ms: Option<i64>) {
        let call_id =
            string(message, "toolCallId").map_or_else(|| format!("seq-{seq}"), str::to_owned);
        let text = message_text(message);
        let blob = text.as_deref().map(|text| self.blob(text));
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::TOOL_OUTPUT.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: blob.clone(),
            detail: ItemDetail::ToolOutput {
                call_id,
                output: blob,
                facts: ToolOutputFacts {
                    nonzero_exit: message.get("isError").and_then(Value::as_bool),
                    truncated: message
                        .pointer("/details/truncation/truncated")
                        .and_then(Value::as_bool),
                    ..ToolOutputFacts::default()
                },
            },
            searchable: false,
            linked_session_native_id: None,
        };
        self.session_mut(seq).loop_mut(seq).push(item);
    }

    /// An entry whose payload is a summary standing in for dropped context.
    fn summary_entry(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        // The summary text is the whole point of the entry; the previous
        // projection stored the literal record type instead and dropped it.
        let preview = string(value, "summary").map(|summary| self.blob(summary));
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::RUNTIME_COMPACTION.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview,
            detail: ItemDetail::Message { has_images: false },
            searchable: false,
            linked_session_native_id: None,
        };
        self.session_mut(seq).loop_mut(seq).push(item);
    }

    /// An extension's injected message, typed by the `customType` it declared.
    fn custom_message(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let custom_type = string(value, "customType").unwrap_or("unknown");
        let (semantic_role, why) = semantic::pi_custom_message(custom_type);
        let text = content_text(value.get("content"));
        let preview = text.as_deref().map(|text| self.blob(text));
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic_role.to_owned(),
            basis: why.to_owned(),
            preview,
            detail: ItemDetail::Message {
                // The runtime converts these to a user message before the model
                // sees them, so that is the channel they arrive on.
                has_images: false,
            },
            searchable: false,
            linked_session_native_id: None,
        };
        self.session_mut(seq).loop_mut(seq).push(item);
    }

    /// A name a person gave the Session. Kept as a Session fact rather than a
    /// timeline row, because it describes the Session and not a moment in it.
    fn session_info(&mut self, seq: u64, value: &Value) {
        let name = string(value, "name").map(str::to_owned);
        let session = self.session_mut(seq);
        // A Session can be renamed, so the last name written wins.
        if name.is_some() {
            session.title = name;
        }
    }

    fn model_change(&mut self, seq: u64, value: &Value) {
        let provider = string(value, "provider").map(str::to_owned);
        let model = string(value, "modelId").map(str::to_owned);
        let session = self.session_mut(seq);
        if let Some(provider) = provider {
            session.model_provider = Some(provider);
        }
        if let Some(model) = model {
            match session.active_loop.as_mut() {
                Some(turn) => turn.model = Some(model),
                // No Loop yet, and reaching for one here would open the very
                // Loop this Record must not open. The model is held until the
                // Loop it configures exists.
                None => session.pending_model = Some(model),
            }
        }
    }

    fn effort_change(&mut self, seq: u64, value: &Value) {
        let effort = string(value, "thinkingLevel").map(str::to_owned);
        let session = self.session_mut(seq);
        if let Some(effort) = effort {
            match session.active_loop.as_mut() {
                Some(turn) => turn.effort = Some(effort),
                None => session.pending_effort = Some(effort),
            }
        }
    }

    fn blob(&self, text: &str) -> BoundedText {
        BoundedText::bounded(text, self.max_text_bytes)
    }
}

/// Splits an expanded `/skill` invocation into the injected body and whatever
/// the person typed after it.
///
/// Mirrors `parseSkillBlock` in `packages/coding-agent/src/core/agent-session.ts`,
/// which anchors on the opening tag and takes the last `</skill>` as the end, so
/// a skill whose own body mentions the tag still splits where the runtime splits
/// it. Returns `None` when the text is not a skill expansion at all.
fn split_skill_block(text: &str) -> Option<(&str, Option<&str>)> {
    const CLOSE: &str = "</skill>";
    if !text.starts_with("<skill name=\"") {
        return None;
    }
    let close = text.rfind(CLOSE)?;
    let end = close + CLOSE.len();
    let trailing = text[end..].trim();
    Some((&text[..end], (!trailing.is_empty()).then_some(trailing)))
}

struct PiToolCall {
    id: String,
    name: String,
    cmd: Option<String>,
    arguments: String,
}

/// The one Pi tool that carries a shell command, and the field it uses.
///
/// `packages/coding-agent/src/core/tools/bash.ts:28` declares `command` as the
/// only string parameter, and `:270` names the tool `bash`. No other tool under
/// `core/tools` declares a `command` field, so the name is the whole gate: a
/// blind `arguments.command` lookup would start inventing shell commands the
/// moment any tool adds a same-named parameter meaning something else.
///
/// The tool's own description says "Bash command to execute", but
/// `createLocalBashOperations` spawns `getShellConfig()`
/// (`src/utils/shell.ts:51-118`), which honours a `shellPath` setting and
/// otherwise resolves `/bin/bash`, then bash on PATH, then `sh`. The command is
/// therefore shell, and only usually bash. Reading the parameter schema alone
/// would have recorded the wrong language.
const PI_SHELL_TOOL: &str = "bash";

fn collect_tool_calls(message: &Value) -> Vec<PiToolCall> {
    message
        .get("content")
        .and_then(Value::as_array)
        .map(|blocks| {
            blocks
                .iter()
                .filter(|block| string(block, "type") == Some("toolCall"))
                .map(|block| {
                    let name = string(block, "name").unwrap_or("tool").to_owned();
                    let arguments = block.get("arguments");
                    PiToolCall {
                        id: string(block, "id").unwrap_or_default().to_owned(),
                        cmd: (name == PI_SHELL_TOOL)
                            .then(|| arguments.and_then(|args| string(args, "command")))
                            .flatten()
                            .map(str::to_owned),
                        name,
                        arguments: arguments
                            .map(std::string::ToString::to_string)
                            .unwrap_or_default(),
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

fn has_images(message: &Value) -> bool {
    message
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks
                .iter()
                .any(|block| string(block, "type") == Some("image"))
        })
}

/// The text the model addressed to the reader. Reasoning is deliberately left
/// out: it is a separate channel and becomes its own Item.
fn message_text(message: &Value) -> Option<String> {
    content_text(message.get("content"))
}

fn content_text(content: Option<&Value>) -> Option<String> {
    let content = content?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let blocks = content.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| string(block, "type") == Some("text"))
        .filter_map(|block| string(block, "text"))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn reasoning_text(message: &Value) -> Option<String> {
    let blocks = message.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter(|block| string(block, "type") == Some("thinking"))
        .filter_map(|block| string(block, "thinking"))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

fn parse_timestamp_ms(text: &str) -> Option<i64> {
    super::codex::parse_timestamp_ms_public(text)
}

#[cfg(test)]
mod tests {
    use serde_json::{Value, json};

    use super::PiProjector;
    use crate::adapters::projection::ItemDetail;

    fn project(records: &[Value]) -> Vec<crate::adapters::projection::ItemProjection> {
        let mut projector = PiProjector::new(1024 * 1024);
        for (index, record) in records.iter().enumerate() {
            projector.push(u64::try_from(index + 1).expect("test sequence"), record);
        }
        projector
            .finish()
            .into_iter()
            .flat_map(|session| session.loops)
            .flat_map(|turn| turn.items)
            .collect()
    }

    #[test]
    fn omits_empty_assistant_commentary_but_keeps_other_blocks() {
        let items = project(&[
            json!({"type":"session","id":"pi-session"}),
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"inspect"}]}}),
            json!({
                "type":"message",
                "message":{
                    "role":"assistant",
                    "content":[
                        {"type":"thinking","thinking":"I should inspect it"},
                        {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"}}
                    ],
                    "stopReason":"toolUse"
                }
            }),
        ]);

        let roles = items
            .iter()
            .map(|item| item.semantic_role.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            ["human.request", "agent.reasoning", "agent.tool_call"]
        );
    }

    #[test]
    fn preserves_pi_content_block_order_across_item_roles() {
        let items = project(&[
            json!({"type":"session","id":"pi-session"}),
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"inspect"}]}}),
            json!({
                "type":"message",
                "message":{
                    "role":"assistant",
                    "content":[
                        {"type":"thinking","thinking":"First reason"},
                        {"type":"text","text":"Then explain"},
                        {"type":"toolCall","id":"call-1","name":"read","arguments":{"path":"README.md"}}
                    ],
                    "stopReason":"toolUse"
                }
            }),
        ]);

        let roles = items
            .iter()
            .map(|item| item.semantic_role.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            roles,
            [
                "human.request",
                "agent.reasoning",
                "agent.commentary",
                "agent.tool_call"
            ]
        );
    }

    #[test]
    fn maps_pi_tool_result_truncation_into_existing_output_fact() {
        let items = project(&[
            json!({"type":"session","id":"pi-session"}),
            json!({"type":"message","message":{"role":"user","content":[{"type":"text","text":"read it"}]}}),
            json!({
                "type":"message",
                "message":{
                    "role":"toolResult",
                    "toolCallId":"call-1",
                    "content":[{"type":"text","text":"output was truncated"}],
                    "isError":false,
                    "details":{"truncation":{"truncated":true,"truncatedBy":"bytes"}}
                }
            }),
        ]);

        let output = items
            .iter()
            .find(|item| item.semantic_role == "tool.output")
            .expect("tool output Item");
        let ItemDetail::ToolOutput { facts, .. } = &output.detail else {
            panic!("tool output detail");
        };
        assert_eq!(facts.truncated, Some(true));
        assert_eq!(facts.nonzero_exit, Some(false));
    }
}
