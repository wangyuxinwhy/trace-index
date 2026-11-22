//! Stateful Runtime adapter for Codex rollout JSONL.
//!
//! Codex traces cannot be projected one record at a time. Four facts are only
//! decidable with surrounding records:
//!
//! * **Loop membership.** `response_item` Records carry no Loop identity;
//!   native `turn_context`, `task_started`, `task_complete`, and
//!   `turn_aborted` Records establish its sequence range.
//! * **Authorship.** `role=user` covers both real input and runtime
//!   injections. The authoritative signal is a matching UI event: either
//!   `event_msg/user_message` or `event_msg/item_completed` carrying a
//!   `UserMessage`. Establishing that match requires pairing across records.
//! * **Tool results.** `call_id` links a call to an output that arrives later,
//!   possibly interleaved with other calls.
//! * **Request vs steering.** A human message opens a Loop as a request or,
//!   when delivered inside the open Loop, redirects it as steering.
//!
//! The projector consumes an ordered record stream and emits sessions.

use std::collections::{HashMap, VecDeque};

use serde_json::Value;

use crate::adapters::projection::{
    self, BoundedText, ItemDetail, ItemProjection, LoopProjection, RecordFacts, SessionProjection,
    ToolOutputFacts,
};
use crate::adapters::semantic::{self, basis};
use crate::adapters::timestamp::parse_timestamp_ms;
use crate::shell::syntax::lang;

mod parse;
#[cfg(test)]
mod tests;

use self::parse::{
    agent_message_text, awaits_output, classify_message, codex_usage, collect_text,
    command_execution_facts, execution_shell_text, execution_workdir, guardian_message,
    linked_session_from_tagged_text, mcp_end_parts, merge_spawn_end, merge_tool_end,
    message_has_images, message_text, message_type, provider_tool_name, runtime_arguments,
    shell_command, spawn_output_native_id, string_at, structured_tool_end_facts, texts_match,
    tool_output_facts,
};

/// How far apart a `response_item` and its `event_msg` twin may sit while
/// still being considered the same logical message. Codex writes them
/// adjacently; the window only tolerates interleaved bookkeeping records.
const DUAL_TRACK_WINDOW: usize = 16;

/// Physical classification of one parsed Record.
pub(crate) fn record_facts(value: &Value) -> RecordFacts {
    let kind = value
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let payload_type = value
        .get("payload")
        .and_then(|payload| payload.get("type"))
        .and_then(Value::as_str);
    RecordFacts {
        ts_ms: value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms),
        native_type: projection::native_type(kind, payload_type),
        parse_status: "ok",
        parse_error: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UiKind {
    User,
    Agent,
}

/// A UI-track item awaiting its model-input twin.
///
/// The item already exists: a user-interface record is evidence in its own
/// right, so it is never held back waiting for a twin that may never arrive.
#[derive(Debug)]
struct PendingUi {
    seq: u64,
    ui_kind: UiKind,
    text: String,
    item_index: usize,
}

#[derive(Debug, Clone)]
struct PendingToolEnd {
    seq: u64,
    ts_ms: Option<i64>,
    facts: ToolOutputFacts,
    mcp_call_name: Option<String>,
    mcp_arguments: Option<String>,
    mcp_output: Option<BoundedText>,
}

#[derive(Debug)]
struct LoopBuilder {
    turn_uuid: Option<String>,
    /// Opened by `task_started`, which always precedes the `turn_context`
    /// carrying this turn's settings. Without this the context record read as
    /// a new turn and left the started one permanently `open`.
    awaiting_context: bool,
    start_seq: u64,
    end_seq: u64,
    status: &'static str,
    end_record_seq: Option<u64>,
    abort_reason: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    context_window: Option<u64>,
    usage: Option<crate::domain::Usage>,
    /// A turn-scoped cumulative counter supersedes legacy per-response events.
    structured_usage: bool,
    items: Vec<ItemProjection>,
    /// `call_id` to the item index of its call, so an output can backfill it.
    calls: HashMap<String, usize>,
    /// `call_id` to the output Item that a later structured end event refines.
    outputs: HashMap<String, usize>,
    /// Structured end events can arrive before their response-item output.
    pending_tool_ends: HashMap<String, PendingToolEnd>,
    human_count: u32,
    request_item: Option<usize>,
    /// Assistant items with no native `phase`, resolved when the turn closes.
    unphased: Vec<usize>,
    /// `role=user` items not yet paired with a UI event.
    unpaired_user: Vec<usize>,
    last_agent_message_hash: Option<crate::adapters::projection::ContentHash>,
    /// UI-track items not yet merged with a model-input twin.
    pending_ui: VecDeque<PendingUi>,
}

impl LoopBuilder {
    fn new(start_seq: u64, turn_uuid: Option<String>) -> Self {
        Self {
            turn_uuid,
            awaiting_context: false,
            start_seq,
            end_seq: start_seq,
            status: "open",
            end_record_seq: None,
            abort_reason: None,
            model: None,
            effort: None,
            context_window: None,
            usage: None,
            structured_usage: false,
            items: Vec::new(),
            calls: HashMap::new(),
            outputs: HashMap::new(),
            pending_tool_ends: HashMap::new(),
            human_count: 0,
            request_item: None,
            unphased: Vec::new(),
            unpaired_user: Vec::new(),
            last_agent_message_hash: None,
            pending_ui: VecDeque::new(),
        }
    }

    fn push_item(&mut self, item: ItemProjection) -> usize {
        self.end_seq = self.end_seq.max(item.seq);
        self.items.push(item);
        self.items.len() - 1
    }

    /// Rollouts older than the native `phase` field: the turn's final answer
    /// is the assistant message whose text matches the runtime's own record of
    /// the last agent message.
    fn resolve_final_answer(&mut self) {
        let Some(target) = self.last_agent_message_hash else {
            return;
        };
        for index in std::mem::take(&mut self.unphased) {
            let matches = self.items[index]
                .preview
                .as_ref()
                .is_some_and(|blob| blob.hash == target);
            if matches {
                let item = &mut self.items[index];
                semantic::AGENT_FINAL_ANSWER.clone_into(&mut item.semantic_role);
                basis::PHASE_FALLBACK_TASK_COMPLETE.clone_into(&mut item.basis);
            }
        }
    }

    /// In older rollouts there is no `phase` or `task_complete`. When the
    /// first human request for the next interaction arrives, the last
    /// unphased assistant message immediately before it is the completed
    /// answer. Earlier unphased messages remain commentary.
    fn resolve_final_before_next_human(&mut self) {
        let Some(index) = self.unphased.pop() else {
            return;
        };
        let item = &mut self.items[index];
        semantic::AGENT_FINAL_ANSWER.clone_into(&mut item.semantic_role);
        "next_human_boundary".clone_into(&mut item.basis);
    }

    /// Older Codex writes the next request immediately before its next
    /// `turn_context`. Until that context arrives, the request is physically
    /// adjacent to the previous Loop and is provisionally attached there.
    /// The new context is the structural boundary: move only the trailing
    /// human Item, and promote the assistant Item immediately before it.
    fn take_request_for_next_loop(&mut self) -> Option<(ItemProjection, Option<PendingUi>)> {
        let final_index = *self.unphased.last()?;
        let request_index = self.items.len().checked_sub(1)?;
        if final_index >= request_index
            || !matches!(
                self.items[request_index].semantic_role.as_str(),
                semantic::HUMAN_REQUEST | semantic::HUMAN_STEERING
            )
        {
            return None;
        }

        self.resolve_final_before_next_human();
        let mut request = self.items.pop()?;
        self.unpaired_user.retain(|index| *index != request_index);
        let pending = self
            .pending_ui
            .iter()
            .position(|entry| entry.item_index == request_index)
            .and_then(|position| self.pending_ui.remove(position))
            .map(|mut pending| {
                pending.item_index = 0;
                pending
            });
        self.human_count = self.human_count.saturating_sub(1);
        semantic::HUMAN_REQUEST.clone_into(&mut request.semantic_role);
        request.basis = semantic::compose_basis(basis::PAIRED_USER_EVENT, basis::FIRST_IN_LOOP);
        self.end_seq = self.items.last().map_or(self.start_seq, |item| item.seq);
        Some((request, pending))
    }

    /// Converts to the persisted shape.
    fn finish(mut self) -> LoopProjection {
        self.resolve_final_answer();
        for (call_id, ended) in std::mem::take(&mut self.pending_tool_ends) {
            let Some(name) = ended.mcp_call_name else {
                continue;
            };
            let call_index = self.push_item(ItemProjection {
                seq: ended.seq,
                record_seq: ended.seq,
                ui_seq: None,
                ts_ms: ended.ts_ms,
                semantic_role: semantic::AGENT_TOOL_CALL.to_owned(),
                basis: basis::RECORD_KIND.to_owned(),
                preview: None,
                detail: ItemDetail::ToolCall {
                    call_id: call_id.clone(),
                    name: Some(name),
                    cmd: None,
                    cmd_lang: None,
                    workdir: None,
                    args: ended.mcp_arguments,
                    syntax: None,
                },
                linked_session_native_id: None,
                searchable: false,
            });
            self.calls.insert(call_id.clone(), call_index);
            let output_index = self.push_item(ItemProjection {
                seq: ended.seq,
                record_seq: ended.seq,
                ui_seq: None,
                ts_ms: ended.ts_ms,
                semantic_role: semantic::TOOL_OUTPUT.to_owned(),
                basis: basis::RECORD_KIND.to_owned(),
                preview: None,
                detail: ItemDetail::ToolOutput {
                    call_id: call_id.clone(),
                    output: ended.mcp_output,
                    facts: ended.facts,
                },
                linked_session_native_id: None,
                searchable: false,
            });
            self.outputs.insert(call_id, output_index);
        }
        self.items.sort_by_key(|item| item.seq);

        let outcome = match self.status {
            "completed" => Some(crate::domain::LoopOutcome::Completed),
            "aborted" if self.abort_reason.as_deref() == Some("error") => {
                Some(crate::domain::LoopOutcome::Failed)
            }
            "aborted" => Some(crate::domain::LoopOutcome::Interrupted),
            _ => None,
        };
        LoopProjection {
            native_id: self.turn_uuid,
            start_seq: self.start_seq,
            end_record_seq: self.end_record_seq,
            outcome,
            model: self.model.map(|id| crate::domain::Model {
                id,
                effort: self.effort,
                context_window: self.context_window,
            }),
            usage: self.usage,
            items: self.items,
        }
    }
}

#[derive(Debug)]
struct ClosedLoopIndex {
    position: usize,
    calls: HashMap<String, usize>,
    outputs: HashMap<String, usize>,
}

#[derive(Debug)]
struct SessionBuilder {
    session_uuid: String,
    identity_confirmed: bool,
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    cwd: Option<String>,
    forked_from_native_id: Option<String>,
    delegated_from_native_id: Option<String>,
    items: Vec<ItemProjection>,
    loops: Vec<LoopProjection>,
    closed_loops: HashMap<String, ClosedLoopIndex>,
    seen_token_totals: std::collections::HashSet<crate::domain::Usage>,
    active_loop: Option<LoopBuilder>,
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
            forked_from_native_id: None,
            delegated_from_native_id: None,
            items: Vec::new(),
            loops: Vec::new(),
            closed_loops: HashMap::new(),
            seen_token_totals: std::collections::HashSet::new(),
            active_loop: None,
        }
    }

    /// Returns the open Loop, opening one when an Item arrives before any
    /// native `turn_context` Record.
    fn loop_mut(&mut self, seq: u64) -> &mut LoopBuilder {
        if self.active_loop.is_none() {
            self.active_loop = Some(LoopBuilder::new(seq, None));
        }
        self.active_loop.as_mut().expect("Loop was just ensured")
    }

    /// Ends the current turn's record range.
    ///
    /// Only `turn_context` and the end of a session do this. `task_complete`
    /// and `turn_aborted` set the outcome but do not close the range, because
    /// bookkeeping records keep arriving after them; treating those as a new
    /// turn produced thousands of one-item phantom loops.
    fn flush_loop(&mut self) {
        let Some(turn) = self.active_loop.take() else {
            return;
        };
        self.end_seq = self.end_seq.max(turn.end_seq);
        let finished = turn.finish();
        if let Some(id) = finished.native_id.as_ref() {
            // finish sorts timeline Items, so build indices over that final
            // order once, not by rescanning on every late completion.
            let mut index = ClosedLoopIndex {
                position: self.loops.len(),
                calls: HashMap::new(),
                outputs: HashMap::new(),
            };
            for (position, item) in finished.items.iter().enumerate() {
                match &item.detail {
                    ItemDetail::ToolCall { call_id, .. } => {
                        index.calls.insert(call_id.clone(), position);
                    }
                    ItemDetail::ToolOutput { call_id, .. } => {
                        index.outputs.insert(call_id.clone(), position);
                    }
                    _ => {}
                }
            }
            self.closed_loops.insert(id.clone(), index);
        }
        self.loops.push(finished);
    }

    fn finish(mut self) -> SessionProjection {
        self.flush_loop();
        SessionProjection {
            session_uuid: self.session_uuid,
            identity_confirmed: self.identity_confirmed,
            start_seq: self.start_seq,
            started_at: self.started_at,
            cwd: self.cwd,
            forked_from_record_seq: self.forked_from_native_id.as_ref().map(|_| self.start_seq),
            forked_from_native_id: self.forked_from_native_id,
            forked_from_locator: None,
            delegated_from_record_seq: self
                .delegated_from_native_id
                .as_ref()
                .map(|_| self.start_seq),
            delegated_from_native_id: self.delegated_from_native_id,
            title: None,
            items: self.items,
            loops: self.loops,
        }
    }
}

#[derive(Debug)]
pub(crate) struct CodexProjector {
    max_text_bytes: usize,
    sessions: Vec<SessionProjection>,
    session: Option<SessionBuilder>,
    next_session_ordinal: u32,
    /// Whether this source's authoritative session header has been seen.
    ///
    /// A forked or resumed rollout repeats its ancestors' `session_meta`
    /// after its own. Those are historical references, not further sessions:
    /// treating them as session boundaries hands the entire conversation to
    /// the ancestor and leaves the file's real session empty.
    session_established: bool,
    /// Producer-declared first ordinal belonging to a subagent's own history.
    /// Earlier records are inherited parent history and remain Records only.
    subagent_history_start_ordinal: Option<u64>,
    /// Native identity of the agent whose Source is being projected.
    current_agent_path: Option<String>,
    /// Agent path to native Session id, learned from structured activity.
    agent_thread_ids: HashMap<String, String>,
    /// Guardian Sources receive a Runtime-authored approval transcript rather
    /// than a human request, even though the wire item uses `role=user`.
    guardian_session: bool,
}

impl CodexProjector {
    pub(crate) fn new(max_text_bytes: usize) -> Self {
        Self {
            max_text_bytes,
            sessions: Vec::new(),
            session: None,
            next_session_ordinal: 0,
            session_established: false,
            subagent_history_start_ordinal: None,
            current_agent_path: None,
            agent_thread_ids: HashMap::new(),
            guardian_session: false,
        }
    }

    pub(crate) fn finish(mut self) -> Vec<SessionProjection> {
        self.close_session();
        self.sessions
    }

    /// Hands over sessions already closed by a later `session_meta`, bounding
    /// peak memory to one in-flight session rather than a whole file.
    pub(crate) fn drain_completed(&mut self) -> Vec<SessionProjection> {
        std::mem::take(&mut self.sessions)
    }

    fn close_session(&mut self) {
        if let Some(session) = self.session.take() {
            self.sessions.push(session.finish());
        }
    }

    /// Returns the open session, opening a synthetic one when records precede
    /// any `session_meta` (a resumed file can start mid-conversation).
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

    fn native_session_for_agent(&self, agent_path: &str) -> Option<String> {
        if self.current_agent_path.as_deref() == Some(agent_path) {
            return self
                .session
                .as_ref()
                .map(|session| session.session_uuid.clone());
        }
        self.agent_thread_ids.get(agent_path).cloned()
    }

    /// Returns true when an already-persisted prefix line provably cannot affect
    /// Codex projection, avoiding `serde_json::Value` DOM allocation on large
    /// ignored `compacted` (without summary) and `world_state` records.
    pub(crate) fn can_skip_prefix_record(&self, bytes: &[u8]) -> bool {
        if !self.session_established {
            return false;
        }
        let head = &bytes[..bytes.len().min(192)];
        if contains_subslice(head, b"\"compacted\"") {
            #[derive(serde::Deserialize)]
            struct CompactedPayload {
                summary: Option<String>,
                message: Option<String>,
            }
            #[derive(serde::Deserialize)]
            struct CompactedProbe {
                #[serde(rename = "type")]
                kind: Option<String>,
                payload: Option<CompactedPayload>,
            }
            if let Ok(probe) = serde_json::from_slice::<CompactedProbe>(bytes)
                && probe.kind.as_deref() == Some("compacted")
            {
                let has_summary = probe.payload.as_ref().is_some_and(|payload| {
                    payload
                        .summary
                        .as_deref()
                        .is_some_and(|text| !text.is_empty())
                        || payload
                            .message
                            .as_deref()
                            .is_some_and(|text| !text.is_empty())
                });
                return !has_summary;
            }
        } else if contains_subslice(head, b"\"world_state\"") {
            #[derive(serde::Deserialize)]
            struct KindProbe {
                #[serde(rename = "type")]
                kind: Option<String>,
            }
            if let Ok(probe) = serde_json::from_slice::<KindProbe>(bytes)
                && probe.kind.as_deref() == Some("world_state")
            {
                return true;
            }
        }
        false
    }

    pub(crate) fn push(&mut self, seq: u64, value: &Value) {
        let record_kind = value.get("type").and_then(Value::as_str).unwrap_or("");
        let payload = value.get("payload").unwrap_or(&Value::Null);
        let payload_kind = payload.get("type").and_then(Value::as_str);
        let ts_ms = value
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_timestamp_ms);

        // The authoritative child header itself precedes the history boundary
        // and must be read so the boundary can be established. Everything
        // else before that producer-declared ordinal is inherited parent
        // history: keep its physical Record, but do not project child Items.
        if record_kind == "session_meta" && !self.session_established {
            self.begin_session(seq, payload, ts_ms);
            return;
        }
        let ordinal = value.get("ordinal").and_then(Value::as_u64).unwrap_or(seq);
        if self
            .subagent_history_start_ordinal
            .is_some_and(|start| ordinal < start)
        {
            return;
        }

        match (record_kind, payload_kind) {
            ("session_meta", _) => self.begin_session(seq, payload, ts_ms),
            ("turn_context", _) => self.begin_loop(seq, payload),
            ("response_item", Some("message")) => self.context_message(seq, payload, ts_ms),
            ("response_item", Some("agent_message")) => {
                self.agent_message(seq, payload, ts_ms, basis::NATIVE_SUBTYPE);
            }
            // The same delivery under its own record type rather than a
            // response-item payload. Author and recipient carry the direction
            // either way, so the routing is shared.
            ("inter_agent_communication", _) => {
                self.agent_message(seq, payload, ts_ms, basis::RECORD_KIND);
            }
            ("response_item", Some("reasoning")) => self.reasoning(seq, payload, ts_ms),
            (
                "response_item",
                Some(
                    call_kind @ ("function_call"
                    | "custom_tool_call"
                    | "web_search_call"
                    | "image_generation_call"
                    | "tool_search_call"),
                ),
            ) => self.tool_call(seq, payload, ts_ms, call_kind),
            (
                "response_item",
                Some("function_call_output" | "custom_tool_call_output" | "tool_search_output"),
            ) => self.tool_output(seq, payload, ts_ms),
            ("event_msg", Some("user_message")) => self.ui_user_message(seq, payload, ts_ms),
            ("event_msg", Some("item_completed")) => {
                self.completed_user_message(seq, payload, ts_ms);
                self.completed_command_execution(seq, payload, ts_ms);
            }
            ("event_msg", Some("agent_message")) => self.ui_agent_message(seq, payload, ts_ms),
            // Usage belongs to the containing Loop, not the Item timeline.
            ("event_msg", Some("token_count")) => self.token_count(seq, payload),
            ("token_usage_record", _) => self.token_usage_record(seq, payload),
            ("event_msg", Some("task_started")) => self.task_started(seq, payload, ts_ms),
            ("event_msg", Some("task_complete")) => self.task_complete(seq, payload, ts_ms),
            ("event_msg", Some("turn_aborted")) => self.turn_aborted(seq, payload, ts_ms),
            ("event_msg", Some("sub_agent_activity")) => {
                self.sub_agent_activity(seq, payload, ts_ms);
            }
            ("event_msg", Some("collab_agent_spawn_end")) => {
                self.collab_agent_spawn_end(seq, payload);
            }
            ("event_msg", Some("exec_command_end" | "mcp_tool_call_end" | "patch_apply_end")) => {
                self.tool_end(seq, payload, ts_ms);
            }
            ("compacted", _) | ("event_msg", Some("context_compacted")) => {
                self.compaction(seq, payload, ts_ms, record_kind);
            }
            _ => self.misc(seq, payload, ts_ms, record_kind, payload_kind),
        }
    }

    // ── session and turn boundaries ────────────────────────────────────────

    fn begin_session(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        if self.session_established {
            // An ancestor's replayed header. Keep it as a fact, but do not
            // let it claim the records that follow.
            self.session_reference(seq, payload, ts_ms);
            return;
        }
        self.session_established = true;
        self.close_session();
        let ordinal = self.next_session_ordinal;
        self.next_session_ordinal += 1;
        let uuid =
            string_at(payload, "id").map_or_else(|| format!("unlabeled-{ordinal}"), str::to_owned);

        let mut session = SessionBuilder::new(uuid, seq, true);
        session.started_at = string_at(payload, "timestamp")
            .and_then(parse_timestamp_ms)
            .or(ts_ms);
        session.cwd = string_at(payload, "cwd").map(str::to_owned);
        session.forked_from_native_id = string_at(payload, "forked_from_id").map(str::to_owned);
        let is_subagent = string_at(payload, "thread_source") == Some("subagent")
            || payload
                .get("source")
                .and_then(|source| source.get("subagent"))
                .is_some();
        if is_subagent {
            self.subagent_history_start_ordinal = payload
                .get("subagent_history_start_ordinal")
                .and_then(Value::as_u64);
            session.delegated_from_native_id = string_at(payload, "parent_thread_id")
                .or_else(|| string_at(payload, "parent_session_id"))
                .map(str::to_owned);
        }
        self.current_agent_path = string_at(payload, "agent_path")
            .or_else(|| {
                payload
                    .get("source")
                    .and_then(|source| source.get("subagent"))
                    .and_then(|subagent| subagent.get("thread_spawn"))
                    .and_then(|spawn| string_at(spawn, "agent_path"))
            })
            .map(str::to_owned);
        self.guardian_session = payload
            .get("source")
            .and_then(|source| source.get("subagent"))
            .and_then(|subagent| string_at(subagent, "other"))
            == Some("guardian");
        self.session = Some(session);
    }

    /// Records a replayed ancestor header without starting a session.
    fn session_reference(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let label = string_at(payload, "id")
            .unwrap_or("session_meta")
            .to_owned();
        let preview = self.blob(&label);
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::RUNTIME_SESSION_REFERENCE.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: Some(preview),
            detail: ItemDetail::Misc,
            linked_session_native_id: None,
            searchable: false,
        };
        self.session_mut(seq).items.push(item);
    }

    fn begin_loop(&mut self, seq: u64, payload: &Value) {
        let turn_uuid = string_at(payload, "turn_id").map(str::to_owned);
        let session = self.session_mut(seq);

        // A runtime may re-emit turn_context for a turn already in progress,
        // for instance after compaction or a settings refresh. The same turn
        // id means the same turn: refresh its settings and keep the range,
        // rather than cutting it in two. Splitting left the first half
        // permanently `open`, because task_complete only arrives once.
        // The turn `task_started` opened is this one. Matching on the turn id
        // alone would not do: rollouts predating `turn_id` carry none, and
        // adopting by absence there would merge consecutive loops.
        let continues_current = session
            .active_loop
            .as_ref()
            .is_some_and(|turn| turn.awaiting_context)
            || (turn_uuid.is_some()
                && session
                    .active_loop
                    .as_ref()
                    .is_some_and(|turn| turn.turn_uuid == turn_uuid));
        let carried_request = (!continues_current)
            .then(|| {
                session
                    .active_loop
                    .as_mut()
                    .and_then(LoopBuilder::take_request_for_next_loop)
            })
            .flatten();
        if !continues_current {
            session.flush_loop();
        }
        let adopted = turn_uuid.clone();
        let mut turn = session.active_loop.take().unwrap_or_else(|| {
            LoopBuilder::new(
                carried_request
                    .as_ref()
                    .map_or(seq, |(request, _)| request.seq),
                turn_uuid,
            )
        });
        if let Some((request, pending)) = carried_request {
            let request_index = turn.push_item(request);
            turn.human_count = 1;
            turn.request_item = Some(request_index);
            if let Some(pending) = pending {
                turn.pending_ui.push_back(pending);
            }
        }
        turn.end_seq = turn.end_seq.max(seq);
        if turn.awaiting_context {
            turn.awaiting_context = false;
            turn.turn_uuid = turn.turn_uuid.take().or(adopted);
        }

        turn.model = string_at(payload, "model").map(str::to_owned);
        turn.effort = string_at(payload, "effort").map(str::to_owned);
        turn.context_window = payload
            .get("model_context_window")
            .and_then(Value::as_u64)
            .or(turn.context_window);
        session.active_loop = Some(turn);
    }

    /// Folds per-request usage reports into the Loop they belong to.
    ///
    /// Codex's `input_tokens` already includes cache reads. Its
    /// `total_token_usage` is Session-cumulative and therefore deliberately
    /// excluded from the Loop value.
    fn token_count(&mut self, seq: u64, payload: &Value) {
        let info = payload.get("info").filter(|value| !value.is_null());
        let context_window = info
            .and_then(|value| value.get("model_context_window"))
            .and_then(Value::as_u64);
        let sample = info
            .and_then(|value| value.get("last_token_usage"))
            .and_then(|usage| {
                projection::normalized_usage(
                    usage.get("input_tokens").and_then(Value::as_u64),
                    usage.get("cached_input_tokens").and_then(Value::as_u64),
                    usage
                        .get("cache_write_input_tokens")
                        .and_then(Value::as_u64),
                    usage.get("output_tokens").and_then(Value::as_u64),
                    usage.get("reasoning_output_tokens").and_then(Value::as_u64),
                    true,
                )
            });

        let session = self.session_mut(seq);
        let total = info
            .and_then(|info| info.get("total_token_usage"))
            .and_then(codex_usage);
        let repeated =
            sample.is_some() && total.is_some_and(|total| !session.seen_token_totals.insert(total));
        let turn = session.loop_mut(seq);
        turn.context_window = context_window.or(turn.context_window);
        if !turn.structured_usage
            && !repeated
            && let Some(sample) = sample
        {
            turn.usage = Some(projection::add_usage(turn.usage.take(), sample));
        }
        turn.end_seq = turn.end_seq.max(seq);
    }

    /// `TokenUsageRecord` publishes both response and cumulative counters.
    /// Only `turn_token_usage` belongs to a Loop; adding `usage` or
    /// `thread_token_usage` would double count or import other Loops' work.
    fn token_usage_record(&mut self, seq: u64, payload: &Value) {
        let Some(turn_id) = string_at(payload, "turn_id") else {
            return;
        };
        let Some(usage) = payload.get("turn_token_usage").and_then(codex_usage) else {
            return;
        };
        let Some(session) = self.session.as_mut() else {
            return;
        };
        if string_at(payload, "thread_id").is_some_and(|id| id != session.session_uuid) {
            return;
        }
        if let Some(turn) = session.active_loop.as_mut()
            && turn.turn_uuid.as_deref() == Some(turn_id)
        {
            turn.usage = Some(usage);
            turn.structured_usage = true;
            turn.end_seq = turn.end_seq.max(seq);
        } else if let Some(index) = session.closed_loops.get(turn_id) {
            // Usage can land after the next Loop has started. It still owns
            // the explicitly named turn, never whichever turn is current.
            session.loops[index.position].usage = Some(usage);
        }
    }

    fn task_started(&mut self, seq: u64, payload: &Value, _ts_ms: Option<i64>) {
        let native_id = string_at(payload, "turn_id").map(str::to_owned);
        let session = self.session_mut(seq);
        let starts_new = session.active_loop.as_ref().is_some_and(|current| {
            current.status != "open"
                || native_id
                    .as_ref()
                    .zip(current.turn_uuid.as_ref())
                    .is_some_and(|(next, current)| next != current)
        });
        if starts_new {
            if let Some(current) = session.active_loop.as_mut()
                && current.end_record_seq.is_none()
            {
                current.end_record_seq = Some(seq);
            }
            session.flush_loop();
        }
        // A Loop begins here. The following `turn_context` describes the same
        // Loop and must not become its start evidence.
        let opened_here = session.active_loop.is_none();
        let turn = session.loop_mut(seq);
        turn.awaiting_context |= opened_here;
        if opened_here {
            turn.turn_uuid = native_id;
        }
        turn.end_seq = turn.end_seq.max(seq);
    }

    fn task_complete(&mut self, seq: u64, payload: &Value, _ts_ms: Option<i64>) {
        let last_message_hash = string_at(payload, "last_agent_message")
            .map(|text| *blake3::hash(text.as_bytes()).as_bytes());
        let session = self.session_mut(seq);
        let turn = session.loop_mut(seq);
        turn.status = "completed";
        turn.end_record_seq = Some(seq);
        // A turn can abort and then complete: the runtime interrupts one
        // attempt and finishes the next inside the same turn_context
        // window. The last outcome wins, and its reason and its evidence
        // must go with it.
        turn.abort_reason = None;
        turn.end_seq = turn.end_seq.max(seq);
        turn.last_agent_message_hash = last_message_hash;
        // Resolve the answer now: later records must not extend the
        // window in which a fallback match could be found.
        turn.resolve_final_answer();
    }

    fn turn_aborted(&mut self, seq: u64, payload: &Value, _ts_ms: Option<i64>) {
        let reason = string_at(payload, "reason").map(str::to_owned);
        let session = self.session_mut(seq);
        let turn = session.loop_mut(seq);
        turn.status = "aborted";
        turn.end_record_seq = Some(seq);
        turn.abort_reason = reason;
        turn.end_seq = turn.end_seq.max(seq);
    }

    // ── messages and the dual-track pairing ────────────────────────────────

    /// Projects a `response_item/message`: the context track, i.e. what the
    /// model actually saw.
    /// Projects a message one agent addressed to another.
    ///
    /// Codex writes multi-agent delivery as a pair: an `agent_message`
    /// response item carrying `author` and `recipient` agent paths, and an
    /// `inter_agent_communication_metadata` record the protocol describes as
    /// local delivery metadata outside the Responses API item. Both were
    /// falling through to `runtime.unknown`, which made 7,554 agent-authored
    /// messages invisible to `author IN ('human','agent')`.
    ///
    /// Two producers write the body, and they differ. A completion is built by
    /// the `InterAgentCompletionMessage` fragment with its payload inline; an
    /// ordinary delivery goes through `to_model_input_item`, which encrypts the
    /// payload and prefixes it with the message type. So the leading line names
    /// the type when present, and the paths always give the direction.
    fn agent_message(
        &mut self,
        seq: u64,
        payload: &Value,
        ts_ms: Option<i64>,
        structural: &'static str,
    ) {
        let author = payload.get("author").and_then(Value::as_str).unwrap_or("");
        let recipient = payload
            .get("recipient")
            .and_then(Value::as_str)
            .unwrap_or("");
        let text = agent_message_text(payload);
        let downward = !recipient.is_empty() && recipient.starts_with(&format!("{author}/"));

        let (role, why) = match (downward, message_type(&text)) {
            // Direction is structural and already proves that work is being
            // sent to a child. A text header must not weaken that evidence.
            (true, _) => (semantic::AGENT_DELEGATION, basis::AGENT_PATH),
            (false, Some("FINAL_ANSWER")) => (semantic::SUBAGENT_REPORT, basis::TEXT_PREFIX),
            (false, Some(_)) => (semantic::SUBAGENT_ACTIVITY, basis::TEXT_PREFIX),
            (false, None) => (semantic::SUBAGENT_ACTIVITY, basis::AGENT_PATH),
        };
        let linked_session_native_id = match role {
            semantic::AGENT_DELEGATION => self.native_session_for_agent(recipient),
            semantic::SUBAGENT_REPORT | semantic::SUBAGENT_ACTIVITY => {
                self.native_session_for_agent(author)
            }
            _ => None,
        };
        let blob = (!text.is_empty()).then(|| self.blob(&text));
        let turn = self.session_mut(seq).loop_mut(seq);
        turn.push_item(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: role.to_owned(),
            basis: semantic::compose_basis(structural, why),
            preview: blob,
            detail: ItemDetail::Message { has_images: false },
            linked_session_native_id,
            searchable: semantic::is_conversation(role),
        });
    }

    fn context_message(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let wire_role = string_at(payload, "role").unwrap_or("unknown").to_owned();
        let text = message_text(payload);
        let has_images = message_has_images(payload);
        let phase = string_at(payload, "phase").map(str::to_owned);
        // Some Codex runs emit paired final-answer records whose text is
        // explicitly empty. They carry no conversation fact, so neither twin
        // should become an Item.
        if wire_role == "assistant" && text.is_none() && !has_images {
            return;
        }
        let blob = text.as_deref().map(|text| self.blob(text));
        let guardian_session = self.guardian_session;

        let turn = self.session_mut(seq).loop_mut(seq);

        // The UI twin, if any, was already projected as an item. Merge into it
        // rather than emitting a second row for the same logical message.
        if let Some(pending) = text
            .as_deref()
            .and_then(|text| take_matching_ui(turn, text, ui_kind(&wire_role)))
        {
            let index = pending.item_index;
            {
                let item = &mut turn.items[index];
                // The model-input record is the primary witness: it is what
                // actually entered the context. The UI record stays attached.
                item.record_seq = seq;
                item.ui_seq = Some(pending.seq);
                // Occurrence time follows the primary witness. Keep the UI
                // time only when the model-input record omitted its own.
                item.ts_ms = ts_ms.or(item.ts_ms);
                // Model-input text carries the full structure; UI text omits
                // wrappers such as image placeholders.
                if blob.is_some() {
                    item.preview.clone_from(&blob);
                    item.detail = ItemDetail::Message { has_images };
                }
            }
            // A `user` twin keeps the classification the UI record already
            // earned; re-running it would count the same human message twice
            // and demote the turn's opening request to steering.
            if wire_role != "user" {
                let (role, why) =
                    classify_message(&wire_role, text.as_deref(), phase.as_deref(), true);
                let (role, why) = guardian_message(role, why, guardian_session, &wire_role);
                let item = &mut turn.items[index];
                item.semantic_role.clone_from(&role);
                why.clone_into(&mut item.basis);
                item.linked_session_native_id =
                    linked_session_from_tagged_text(&role, text.as_deref());
                item.searchable = semantic::is_conversation(&role);
                if role == semantic::AGENT_COMMENTARY && phase.is_none() {
                    turn.unphased.push(index);
                }
            }
            turn.end_seq = turn.end_seq.max(seq);
            return;
        }

        let (role, why) = classify_message(&wire_role, text.as_deref(), phase.as_deref(), false);
        let (role, why) = guardian_message(role, why, guardian_session, &wire_role);
        let searchable = semantic::is_conversation(&role);
        let index = turn.push_item(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: role.clone(),
            basis: why.clone(),
            preview: blob,
            detail: ItemDetail::Message { has_images },
            linked_session_native_id: linked_session_from_tagged_text(&role, text.as_deref()),
            searchable,
        });

        if role == semantic::AGENT_COMMENTARY && phase.is_none() {
            turn.unphased.push(index);
        }
        // Term-wise, not equality: `basis` is now composed, and `==` here went
        // silently false the moment the marker term was appended.
        if semantic::has_basis_term(&why, basis::UNPAIRED_USER) {
            // A model-input record that arrived before its UI twin can still be
            // reclassified as human input when the twin shows up.
            turn.unpaired_user.push(index);
        }
    }

    fn register_human(turn: &mut LoopBuilder, index: usize) {
        turn.human_count += 1;
        let agent_already_spoke = turn.items[..index].iter().any(|item| {
            matches!(
                item.semantic_role.as_str(),
                semantic::AGENT_COMMENTARY | semantic::AGENT_FINAL_ANSWER
            )
        });
        let (role, why) = if turn.human_count > 1 || agent_already_spoke {
            (semantic::HUMAN_STEERING, basis::SUBSEQUENT_IN_LOOP)
        } else {
            (semantic::HUMAN_REQUEST, basis::FIRST_IN_LOOP)
        };
        let item = &mut turn.items[index];
        role.clone_into(&mut item.semantic_role);
        item.basis = semantic::compose_basis(basis::PAIRED_USER_EVENT, why);
        item.searchable = true;
        if role == semantic::HUMAN_REQUEST && turn.request_item.is_none() {
            turn.request_item = Some(index);
        }
    }

    /// A real user input, recorded by the UI track.
    ///
    /// This is the authoritative evidence that a human typed something, so it
    /// becomes an item immediately. If the model-input twin arrives later it
    /// merges into this item; if it never arrives, the input is still indexed.
    fn ui_user_message(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let text = string_at(payload, "message").unwrap_or_default().to_owned();
        let has_images = payload
            .get("images")
            .and_then(Value::as_array)
            .is_some_and(|images| !images.is_empty());
        self.register_ui_user_message(seq, text, has_images, ts_ms);
    }

    /// Current Codex emits completed user input as a typed UI item rather
    /// than the older `event_msg/user_message` payload. Other completed Item
    /// types stay unmodelled until a query needs their distinct semantics.
    fn completed_user_message(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let Some(item) = payload.get("item") else {
            return;
        };
        if string_at(item, "type") != Some("UserMessage") {
            return;
        }
        let text = message_text(item).unwrap_or_default();
        let has_images = message_has_images(item);
        self.register_ui_user_message(seq, text, has_images, ts_ms);
    }

    fn register_ui_user_message(
        &mut self,
        seq: u64,
        text: String,
        has_images: bool,
        ts_ms: Option<i64>,
    ) {
        let blob = (!text.is_empty()).then(|| self.blob(&text));
        let session = self.session_mut(seq);
        let turn = session.loop_mut(seq);

        // Reverse pairing: the model-input twin may already have been projected
        // and provisionally classified as a runtime injection.
        if let Some(index) = take_matching_context(turn, &text) {
            turn.items[index].ui_seq = Some(seq);
            Self::register_human(turn, index);
            return;
        }

        let index = turn.push_item(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::HUMAN_REQUEST.to_owned(),
            basis: basis::PAIRED_USER_EVENT.to_owned(),
            preview: blob,
            detail: ItemDetail::Message { has_images },
            linked_session_native_id: None,
            searchable: true,
        });
        Self::register_human(turn, index);
        push_pending(
            turn,
            PendingUi {
                seq,
                ui_kind: UiKind::User,
                text,
                item_index: index,
            },
        );
    }

    fn ui_agent_message(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let text = string_at(payload, "message").unwrap_or_default().to_owned();
        if text.is_empty() {
            return;
        }
        let phase = string_at(payload, "phase").map(str::to_owned);
        let blob = (!text.is_empty()).then(|| self.blob(&text));
        let (role, why) = match phase.as_deref() {
            Some("final_answer") => (semantic::AGENT_FINAL_ANSWER, basis::PHASE_FIELD),
            Some(_) => (semantic::AGENT_COMMENTARY, basis::PHASE_FIELD),
            None => (semantic::AGENT_COMMENTARY, basis::PHASE_FALLBACK_COMMENTARY),
        };
        let turn = self.session_mut(seq).loop_mut(seq);
        let index = turn.push_item(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: role.to_owned(),
            basis: why.to_owned(),
            preview: blob,
            detail: ItemDetail::Message { has_images: false },
            linked_session_native_id: None,
            searchable: true,
        });
        if phase.is_none() {
            turn.unphased.push(index);
        }
        push_pending(
            turn,
            PendingUi {
                seq,
                ui_kind: UiKind::Agent,
                text,
                item_index: index,
            },
        );
    }

    fn reasoning(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let summary = payload
            .get("summary")
            .and_then(Value::as_array)
            .map(|blocks| {
                blocks
                    .iter()
                    .filter_map(|block| block.get("text").and_then(Value::as_str))
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .filter(|text| !text.is_empty());
        let blob = summary.as_deref().map(|text| self.blob(text));

        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::AGENT_REASONING.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: blob,
            detail: ItemDetail::Reasoning {
                representation: if summary.is_some() {
                    crate::domain::ReasoningRepresentation::Summary
                } else {
                    crate::domain::ReasoningRepresentation::Unavailable
                },
            },
            linked_session_native_id: None,
            searchable: false,
        };
        self.session_mut(seq).loop_mut(seq).push_item(item);
    }

    // ── tool calls ─────────────────────────────────────────────────────────

    /// Native execution Items witness commands actually submitted to a tool,
    /// including commands hidden behind a JavaScript orchestration call. Do
    /// not infer child invocations from the script itself.
    fn completed_command_execution(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let Some(item) = payload.get("item") else {
            return;
        };
        if string_at(item, "type") != Some("CommandExecution")
            || !matches!(
                string_at(item, "source"),
                Some("agent" | "unified_exec_startup")
            )
            || string_at(item, "id").is_none()
        {
            return;
        }
        let max_text_bytes = self.max_text_bytes;
        let turn_id = string_at(payload, "turn_id");
        let session = self.session_mut(seq);
        if string_at(payload, "thread_id").is_some_and(|id| id != session.session_uuid) {
            return;
        }
        let call_id = string_at(item, "id")
            .expect("checked command identity")
            .to_owned();
        if turn_id.is_some_and(|id| {
            session
                .active_loop
                .as_ref()
                .and_then(|turn| turn.turn_uuid.as_deref())
                != Some(id)
        }) {
            if let Some(index) = turn_id.and_then(|id| session.closed_loops.get_mut(id)) {
                let positions = (
                    index.calls.get(&call_id).copied(),
                    index.outputs.get(&call_id).copied(),
                );
                let (call, output) = upsert_command_execution(
                    &mut session.loops[index.position].items,
                    positions,
                    seq,
                    ts_ms,
                    item,
                    max_text_bytes,
                );
                index.calls.insert(call_id.clone(), call);
                index.outputs.insert(call_id, output);
            }
            return;
        }
        let turn = session.loop_mut(seq);
        let positions = (
            turn.calls.get(&call_id).copied(),
            turn.outputs.get(&call_id).copied(),
        );
        let (call_index, output_index) =
            upsert_command_execution(&mut turn.items, positions, seq, ts_ms, item, max_text_bytes);
        turn.calls.insert(call_id.clone(), call_index);
        turn.outputs.insert(call_id.clone(), output_index);
        if let Some(ended) = turn.pending_tool_ends.remove(&call_id) {
            merge_tool_end(&mut turn.items[output_index], &ended);
        }
        turn.end_seq = turn.end_seq.max(seq);
    }

    fn tool_call(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>, call_kind: &str) {
        let call_id = string_at(payload, "call_id")
            .or_else(|| string_at(payload, "id"))
            .map_or_else(|| format!("seq-{seq}"), str::to_owned);
        let name = string_at(payload, "name")
            .map(str::to_owned)
            .or_else(|| provider_tool_name(call_kind).map(str::to_owned));
        let raw_args = runtime_arguments(payload, call_kind);
        let parsed = raw_args
            .as_deref()
            .and_then(|args| serde_json::from_str::<Value>(args).ok());
        let (cmd, cmd_lang) = shell_command(name.as_deref(), parsed.as_ref())
            .map_or((None, None), |text| (Some(text), Some(lang::SHELL)));
        let workdir = parsed
            .as_ref()
            .and_then(|args| string_at(args, "workdir").map(str::to_owned));
        let args = raw_args
            .as_deref()
            .and_then(|text| projection::complete_arguments(text, self.max_text_bytes));
        let is_delegation = name.as_deref() == Some("spawn_agent");
        let preview = is_delegation
            .then(|| {
                cmd.as_deref()
                    .or(raw_args.as_deref())
                    .map(|text| self.blob(text))
            })
            .flatten();

        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: if is_delegation {
                semantic::AGENT_DELEGATION
            } else {
                semantic::AGENT_TOOL_CALL
            }
            .to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview,
            detail: ItemDetail::ToolCall {
                call_id: call_id.clone(),
                name,
                cmd,
                cmd_lang,
                workdir,
                args,
                syntax: None,
            },
            linked_session_native_id: None,
            searchable: is_delegation,
        };
        let turn = self.session_mut(seq).loop_mut(seq);
        let index = if let Some(&index) = turn.calls.get(&call_id) {
            // Completion may precede its model-input twin. Keep the native
            // execution evidence and adopt Agent-authored arguments when they
            // arrive, rather than publishing a second invocation.
            let existing = &mut turn.items[index];
            adopt_response_witness(existing, seq);
            if let (
                ItemDetail::ToolCall {
                    name,
                    args,
                    cmd,
                    cmd_lang,
                    workdir,
                    ..
                },
                ItemDetail::ToolCall {
                    name: new_name,
                    args: new_args,
                    cmd: new_cmd,
                    cmd_lang: new_lang,
                    workdir: new_workdir,
                    ..
                },
            ) = (&mut existing.detail, item.detail)
            {
                *name = new_name;
                *args = new_args;
                if cmd.is_none() {
                    *cmd = new_cmd;
                    *cmd_lang = new_lang;
                }
                if workdir.is_none() {
                    *workdir = new_workdir;
                }
            }
            index
        } else {
            turn.push_item(item)
        };
        if awaits_output(call_kind) {
            turn.calls.insert(call_id.clone(), index);
        }
    }

    fn tool_output(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let call_id =
            string_at(payload, "call_id").map_or_else(|| format!("seq-{seq}"), str::to_owned);
        let mut fragments = Vec::new();
        if let Some(output) = payload.get("output") {
            collect_text(output, &mut fragments);
        }
        let facts = tool_output_facts(&fragments);
        let joined = (!fragments.is_empty()).then(|| fragments.join("\n"));
        let blob = joined.as_deref().map(|text| self.blob(text));
        let current_session_native_id = self
            .session
            .as_ref()
            .map(|session| session.session_uuid.as_str());
        let spawn_native_id = joined
            .as_deref()
            .and_then(|output| spawn_output_native_id(output, current_session_native_id));

        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::TOOL_OUTPUT.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: ItemDetail::ToolOutput {
                call_id: call_id.clone(),
                output: blob,
                facts,
            },
            linked_session_native_id: None,
            searchable: false,
        };
        let max_text_bytes = self.max_text_bytes;
        let turn = self.session_mut(seq).loop_mut(seq);
        let index = if let Some(&index) = turn.outputs.get(&call_id) {
            let existing = &mut turn.items[index];
            adopt_response_witness(existing, seq);
            // Native outcome/duration win, while the response owns its earlier
            // Runtime truncation and token boundary. Preserve both witnesses.
            if let (
                ItemDetail::ToolOutput { output, facts, .. },
                ItemDetail::ToolOutput {
                    output: new_output,
                    facts: new_facts,
                    ..
                },
            ) = (&mut existing.detail, item.detail)
            {
                if output.is_none() {
                    *output = new_output;
                }
                facts.exit_code = facts.exit_code.or(new_facts.exit_code);
                facts.nonzero_exit = facts.nonzero_exit.or(new_facts.nonzero_exit);
                facts.duration_ms = facts.duration_ms.or(new_facts.duration_ms);
                facts.truncated = new_facts.truncated.or(facts.truncated);
                facts.output_tokens = new_facts.output_tokens.or(facts.output_tokens);
            }
            index
        } else {
            turn.push_item(item)
        };
        turn.outputs.insert(call_id.clone(), index);
        if let Some(ended) = turn.pending_tool_ends.remove(&call_id) {
            merge_tool_end(&mut turn.items[index], &ended);
        }
        if let Some(native_id) = spawn_native_id
            && let Some(call_index) = turn.calls.get(&call_id).copied()
            && turn.items[call_index].semantic_role == semantic::AGENT_DELEGATION
            && turn.items[call_index].linked_session_native_id.is_none()
        {
            merge_spawn_end(&mut turn.items[call_index], seq, native_id, max_text_bytes);
        }
        // The call keeps the correlation; the indexer resolves it to row ids.
        let _ = turn.calls.get(&call_id);
    }

    fn tool_end(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let Some(call_id) = string_at(payload, "call_id") else {
            return;
        };
        let facts = structured_tool_end_facts(payload);
        let (mcp_call_name, mcp_arguments, mcp_output) = mcp_end_parts(payload);
        let ended = PendingToolEnd {
            seq,
            ts_ms,
            facts,
            mcp_call_name,
            mcp_arguments: mcp_arguments
                .as_deref()
                .and_then(|text| projection::complete_arguments(text, self.max_text_bytes)),
            mcp_output: mcp_output.as_deref().map(|text| self.blob(text)),
        };
        let turn = self.session_mut(seq).loop_mut(seq);
        if let Some(index) = turn.outputs.get(call_id).copied() {
            merge_tool_end(&mut turn.items[index], &ended);
        } else {
            turn.pending_tool_ends.insert(call_id.to_owned(), ended);
        }
    }

    fn collab_agent_spawn_end(&mut self, seq: u64, payload: &Value) {
        let (Some(call_id), Some(native_id)) = (
            string_at(payload, "call_id"),
            string_at(payload, "new_thread_id"),
        ) else {
            return;
        };
        let max_text_bytes = self.max_text_bytes;
        let turn = self.session_mut(seq).loop_mut(seq);
        if let Some(index) = turn.calls.get(call_id).copied() {
            merge_spawn_end(
                &mut turn.items[index],
                seq,
                native_id.to_owned(),
                max_text_bytes,
            );
        }
    }

    fn sub_agent_activity(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let activity = string_at(payload, "kind").unwrap_or("unknown").to_owned();
        let linked_session_native_id = string_at(payload, "agent_thread_id").map(str::to_owned);
        if let (Some(agent_path), Some(thread_id)) = (
            string_at(payload, "agent_path"),
            linked_session_native_id.as_ref(),
        ) {
            self.agent_thread_ids
                .insert(agent_path.to_owned(), thread_id.clone());
        }
        if let (Some(event_id), Some(thread_id)) = (
            string_at(payload, "event_id"),
            linked_session_native_id.as_ref(),
        ) {
            let max_text_bytes = self.max_text_bytes;
            let turn = self.session_mut(seq).loop_mut(seq);
            if let Some(call_index) = turn.calls.get(event_id).copied()
                && turn.items[call_index].semantic_role == semantic::AGENT_DELEGATION
                && turn.items[call_index].linked_session_native_id.is_none()
            {
                merge_spawn_end(
                    &mut turn.items[call_index],
                    seq,
                    thread_id.clone(),
                    max_text_bytes,
                );
            }
        }
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::SUBAGENT_ACTIVITY.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: Some(self.blob(&activity)),
            detail: ItemDetail::Misc,
            linked_session_native_id,
            searchable: false,
        };
        self.session_mut(seq).loop_mut(seq).push_item(item);
    }

    fn compaction(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>, _record_kind: &str) {
        // The replacement history is deliberately not expanded: those records
        // are copies of messages already indexed at their original positions.
        // A compaction becomes a Semantic Item only when the Runtime publishes
        // an actual summary. Bookkeeping-only compaction remains a Record.
        let Some(summary) = string_at(payload, "summary")
            .or_else(|| string_at(payload, "message"))
            .filter(|summary| !summary.is_empty())
        else {
            return;
        };
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::RUNTIME_COMPACTION.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: Some(self.blob(summary)),
            detail: ItemDetail::Misc,
            linked_session_native_id: None,
            searchable: false,
        };
        self.session_mut(seq).loop_mut(seq).push_item(item);
    }

    /// Publishes the one long-tail Runtime event with a stable meaning today.
    /// Unmodelled payloads remain physical Records until a real query earns a
    /// Semantic field; they are not copied into an open-ended Item value.
    fn misc(
        &mut self,
        seq: u64,
        payload: &Value,
        ts_ms: Option<i64>,
        record_kind: &str,
        payload_kind: Option<&str>,
    ) {
        if (record_kind, payload_kind) != ("event_msg", Some("error")) {
            return;
        }
        let text = string_at(payload, "message")
            .or_else(|| string_at(payload, "error"))
            .unwrap_or("error");
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::RUNTIME_NOTICE.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: Some(self.blob(text)),
            detail: ItemDetail::Misc,
            linked_session_native_id: None,
            searchable: false,
        };
        let session = self.session_mut(seq);
        if let Some(turn) = session.active_loop.as_mut() {
            turn.push_item(item);
        } else {
            session.items.push(item);
        }
    }

    fn blob(&self, text: &str) -> BoundedText {
        BoundedText::bounded(text, self.max_text_bytes)
    }
}

/// Keep Agent-authored arguments and response-only output boundaries in the
/// primary witness slot. Repeated native completions may refresh the other
/// slot, but cannot discard the source of those retained fields.
fn adopt_response_witness(item: &mut ItemProjection, seq: u64) {
    let native_seq = item.ui_seq.unwrap_or(item.record_seq);
    item.record_seq = seq;
    item.ui_seq = (native_seq != seq).then_some(native_seq);
}

/// Refreshes native command evidence by its id, shared by modern Items and
/// legacy exec events. Runtime-resolved argv is not an Agent argument object.
fn upsert_command_execution(
    items: &mut Vec<ItemProjection>,
    positions: (Option<usize>, Option<usize>),
    seq: u64,
    ts_ms: Option<i64>,
    item: &Value,
    max_text_bytes: usize,
) -> (usize, usize) {
    let id = string_at(item, "id").expect("checked command identity");
    let blob = |text: &str| BoundedText::bounded(text, max_text_bytes);
    let call_index = positions.0.unwrap_or_else(|| {
        let cmd = execution_shell_text(item);
        let index = items.len();
        items.push(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::AGENT_TOOL_CALL.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: ItemDetail::ToolCall {
                call_id: id.to_owned(),
                name: (string_at(item, "source") == Some("unified_exec_startup"))
                    .then(|| "exec_command".to_owned()),
                cmd_lang: cmd.as_ref().map(|_| lang::SHELL),
                cmd,
                workdir: string_at(item, "cwd").and_then(execution_workdir),
                args: None,
                syntax: None,
            },
            linked_session_native_id: None,
            searchable: false,
        });
        index
    });
    let call = &mut items[call_index];
    if call.record_seq != seq {
        call.ui_seq = Some(seq);
    }
    if let ItemDetail::ToolCall {
        workdir,
        cmd,
        cmd_lang,
        ..
    } = &mut call.detail
    {
        if let Some(cwd) = string_at(item, "cwd").and_then(execution_workdir) {
            *workdir = Some(cwd);
        }
        if cmd.is_none() {
            *cmd = execution_shell_text(item);
            *cmd_lang = cmd.as_ref().map(|_| lang::SHELL);
        }
    }
    let text = string_at(item, "aggregated_output")
        .or_else(|| string_at(item, "formatted_output"))
        .map(str::to_owned)
        .or_else(|| {
            let parts = [string_at(item, "stdout"), string_at(item, "stderr")];
            parts
                .iter()
                .any(Option::is_some)
                .then(|| parts.into_iter().flatten().collect::<String>())
        });
    let output = text.as_deref().map(blob);
    let facts = command_execution_facts(item);
    let output_index = if let Some(index) = positions.1 {
        if items[index].record_seq != seq {
            items[index].ui_seq = Some(seq);
        }
        let existing_item = &mut items[index];
        if let ItemDetail::ToolOutput {
            output: existing,
            facts: existing_facts,
            ..
        } = &mut existing_item.detail
        {
            if output.is_some() {
                *existing = output;
            }
            existing_facts.exit_code = facts.exit_code.or(existing_facts.exit_code);
            existing_facts.nonzero_exit = facts.nonzero_exit.or(existing_facts.nonzero_exit);
            existing_facts.duration_ms = facts.duration_ms.or(existing_facts.duration_ms);
        }
        index
    } else {
        let index = items.len();
        items.push(ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: semantic::TOOL_OUTPUT.to_owned(),
            basis: basis::RECORD_KIND.to_owned(),
            preview: None,
            detail: ItemDetail::ToolOutput {
                call_id: id.to_owned(),
                output,
                facts,
            },
            linked_session_native_id: None,
            searchable: false,
        });
        index
    };
    (call_index, output_index)
}

const fn ui_kind(wire_role: &str) -> Option<UiKind> {
    match wire_role.as_bytes() {
        b"user" => Some(UiKind::User),
        b"assistant" => Some(UiKind::Agent),
        _ => None,
    }
}

/// Drops the oldest waiting entry once the window is full. The item it points
/// at stays indexed; only the chance to merge a twin into it is given up.
fn push_pending(turn: &mut LoopBuilder, entry: PendingUi) {
    if turn.pending_ui.len() >= DUAL_TRACK_WINDOW {
        turn.pending_ui.pop_front();
    }
    turn.pending_ui.push_back(entry);
}

/// Finds the UI twin of a model-input message and consumes it.
fn take_matching_ui(
    turn: &mut LoopBuilder,
    text: &str,
    wanted: Option<UiKind>,
) -> Option<PendingUi> {
    let wanted = wanted?;
    let position = turn
        .pending_ui
        .iter()
        .rposition(|entry| entry.ui_kind == wanted && texts_match(&entry.text, text))?;
    turn.pending_ui.remove(position)
}

/// Reverse direction: a UI event arriving after its model-input twin.
fn take_matching_context(turn: &mut LoopBuilder, text: &str) -> Option<usize> {
    let items = &turn.items;
    let position = turn.unpaired_user.iter().rposition(|index| {
        items[*index]
            .preview
            .as_ref()
            .and_then(|blob| blob.text.as_deref())
            .is_some_and(|candidate| texts_match(text, candidate))
    })?;
    Some(turn.unpaired_user.remove(position))
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}
