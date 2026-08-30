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
use crate::shell::syntax::lang;

/// How far apart a `response_item` and its `event_msg` twin may sit while
/// still being considered the same logical message. Codex writes them
/// adjacently; the window only tolerates interleaved bookkeeping records.
const DUAL_TRACK_WINDOW: usize = 16;

/// Shortest text allowed to pair by containment rather than equality. Short
/// confirmations such as "可以" would otherwise cross-match.
const MIN_CONTAINMENT_BYTES: usize = 8;

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
    mcp_arguments: Option<BoundedText>,
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
    started_at: Option<i64>,
    ended_at: Option<i64>,
    status: &'static str,
    end_record_seq: Option<u64>,
    abort_reason: Option<String>,
    model: Option<String>,
    effort: Option<String>,
    context_window: Option<u64>,
    usage: Option<crate::domain::Usage>,
    personality: Option<String>,
    collab_mode: Option<String>,
    approval_policy: Option<String>,
    sandbox_mode: Option<String>,
    network_access: Option<bool>,
    user_instructions: Option<BoundedText>,
    developer_instructions: Option<BoundedText>,
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
            started_at: None,
            ended_at: None,
            status: "open",
            end_record_seq: None,
            abort_reason: None,
            model: None,
            effort: None,
            context_window: None,
            usage: None,
            personality: None,
            collab_mode: None,
            approval_policy: None,
            sandbox_mode: None,
            network_access: None,
            user_instructions: None,
            developer_instructions: None,
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
                preview: ended.mcp_arguments.clone(),
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
            let output = ended.mcp_output;
            let output_index = self.push_item(ItemProjection {
                seq: ended.seq,
                record_seq: ended.seq,
                ui_seq: None,
                ts_ms: ended.ts_ms,
                semantic_role: semantic::TOOL_OUTPUT.to_owned(),
                basis: basis::RECORD_KIND.to_owned(),
                preview: output.clone(),
                detail: ItemDetail::ToolOutput {
                    call_id: call_id.clone(),
                    output,
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
struct SessionBuilder {
    session_uuid: String,
    identity_confirmed: bool,
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    cwd: Option<String>,
    originator: Option<String>,
    cli_version: Option<String>,
    model_provider: Option<String>,
    source_kind: Option<String>,
    git_repo: Option<String>,
    git_branch: Option<String>,
    git_commit: Option<String>,
    forked_from_native_id: Option<String>,
    delegated_from_native_id: Option<String>,
    base_instructions: Option<BoundedText>,
    items: Vec<ItemProjection>,
    loops: Vec<LoopProjection>,
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
            ended_at: None,
            cwd: None,
            originator: None,
            cli_version: None,
            model_provider: None,
            source_kind: None,
            git_repo: None,
            git_branch: None,
            git_commit: None,
            forked_from_native_id: None,
            delegated_from_native_id: None,
            base_instructions: None,
            items: Vec::new(),
            loops: Vec::new(),
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
            }
            ("event_msg", Some("agent_message")) => self.ui_agent_message(seq, payload, ts_ms),
            // Usage belongs to the containing Loop, not the Item timeline.
            ("event_msg", Some("token_count")) => self.token_count(seq, payload),
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
        session.originator = string_at(payload, "originator").map(str::to_owned);
        session.cli_version = string_at(payload, "cli_version").map(str::to_owned);
        session.model_provider = string_at(payload, "model_provider").map(str::to_owned);
        session.source_kind = session_source_kind(payload);
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
        if let Some(git) = payload.get("git") {
            session.git_repo = string_at(git, "repository_url").map(str::to_owned);
            session.git_branch = string_at(git, "branch").map(str::to_owned);
            session.git_commit = string_at(git, "commit_hash").map(str::to_owned);
        }
        session.base_instructions = payload
            .get("base_instructions")
            .and_then(|value| {
                value
                    .as_str()
                    .or_else(|| value.get("text").and_then(Value::as_str))
            })
            .map(|text| self.blob(text));
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
        let max_text_bytes = self.max_text_bytes;
        let user_instructions =
            string_at(payload, "user_instructions").map(|text| blob_of(text, max_text_bytes));
        let developer_instructions = payload
            .get("collaboration_mode")
            .and_then(|mode| mode.get("settings"))
            .and_then(|settings| settings.get("developer_instructions"))
            .and_then(Value::as_str)
            .map(|text| blob_of(text, max_text_bytes));

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
        turn.personality = string_at(payload, "personality").map(str::to_owned);
        turn.collab_mode = payload
            .get("collaboration_mode")
            .and_then(|mode| string_at(mode, "mode"))
            .map(str::to_owned);
        turn.approval_policy = string_at(payload, "approval_policy").map(str::to_owned);
        if let Some(sandbox) = payload.get("sandbox_policy") {
            turn.sandbox_mode = string_at(sandbox, "type").map(str::to_owned);
            turn.network_access = sandbox.get("network_access").and_then(Value::as_bool);
        }
        turn.user_instructions = user_instructions;
        turn.developer_instructions = developer_instructions;
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

        let turn = self.session_mut(seq).loop_mut(seq);
        turn.context_window = context_window.or(turn.context_window);
        if let Some(sample) = sample {
            turn.usage = Some(projection::add_usage(turn.usage.take(), sample));
        }
        turn.end_seq = turn.end_seq.max(seq);
    }

    fn task_started(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
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
        turn.started_at = turn.started_at.or(ts_ms);
        turn.end_seq = turn.end_seq.max(seq);
    }

    fn task_complete(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let last_message_hash = string_at(payload, "last_agent_message")
            .map(|text| *blake3::hash(text.as_bytes()).as_bytes());
        let session = self.session_mut(seq);
        {
            let turn = session.loop_mut(seq);
            turn.status = "completed";
            turn.end_record_seq = Some(seq);
            // A turn can abort and then complete: the runtime interrupts one
            // attempt and finishes the next inside the same turn_context
            // window. The last outcome wins, and its reason and its evidence
            // must go with it.
            turn.abort_reason = None;
            turn.ended_at = ts_ms;
            turn.end_seq = turn.end_seq.max(seq);
            turn.last_agent_message_hash = last_message_hash;
            // Resolve the answer now: later records must not extend the
            // window in which a fallback match could be found.
            turn.resolve_final_answer();
        }
        session.ended_at = ts_ms.or(session.ended_at);
    }

    fn turn_aborted(&mut self, seq: u64, payload: &Value, ts_ms: Option<i64>) {
        let reason = string_at(payload, "reason").map(str::to_owned);
        let session = self.session_mut(seq);
        {
            let turn = session.loop_mut(seq);
            turn.status = "aborted";
            turn.end_record_seq = Some(seq);
            turn.abort_reason = reason;
            turn.ended_at = ts_ms;
            turn.end_seq = turn.end_seq.max(seq);
        }
        session.ended_at = ts_ms.or(session.ended_at);
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
        let args_blob = raw_args.as_deref().map(|text| self.blob(text));
        let preview_text = cmd.clone().or_else(|| raw_args.clone());
        let preview = preview_text.as_deref().map(|text| self.blob(text));
        let is_delegation = name.as_deref() == Some("spawn_agent");

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
                args: args_blob,
                syntax: None,
            },
            linked_session_native_id: None,
            searchable: is_delegation,
        };
        let turn = self.session_mut(seq).loop_mut(seq);
        let index = turn.push_item(item);
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
            preview: blob.clone(),
            detail: ItemDetail::ToolOutput {
                call_id: call_id.clone(),
                output: blob,
                facts,
            },
            linked_session_native_id: None,
            searchable: false,
        };
        let turn = self.session_mut(seq).loop_mut(seq);
        let index = turn.push_item(item);
        turn.outputs.insert(call_id.clone(), index);
        if let Some(ended) = turn.pending_tool_ends.remove(&call_id) {
            merge_tool_end(&mut turn.items[index], &ended);
        }
        if let Some(native_id) = spawn_native_id
            && let Some(call_index) = turn.calls.get(&call_id).copied()
            && turn.items[call_index].semantic_role == semantic::AGENT_DELEGATION
            && turn.items[call_index].linked_session_native_id.is_none()
        {
            merge_spawn_end(&mut turn.items[call_index], seq, native_id);
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
            mcp_arguments: mcp_arguments.as_deref().map(|text| self.blob(text)),
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
        let turn = self.session_mut(seq).loop_mut(seq);
        if let Some(index) = turn.calls.get(call_id).copied() {
            merge_spawn_end(&mut turn.items[index], seq, native_id.to_owned());
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
            let turn = self.session_mut(seq).loop_mut(seq);
            if let Some(call_index) = turn.calls.get(event_id).copied()
                && turn.items[call_index].semantic_role == semantic::AGENT_DELEGATION
                && turn.items[call_index].linked_session_native_id.is_none()
            {
                merge_spawn_end(&mut turn.items[call_index], seq, thread_id.clone());
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
        blob_of(text, self.max_text_bytes)
    }
}

/// Decides a message's semantic role.
///
/// Authorship is structural: `paired` means a matching UI-track user event
/// exists, which is the only proof a human supplied the text. Tag prefixes
/// only subdivide runtime injections; they never decide authorship, because
/// they change between Codex versions while the dual-track structure does not.
///
/// Both findings are recorded. Codex is the only adapter that establishes
/// authorship without reading the text, so it is the only one with two things
/// to say, and saying only the first left `basis = 'tag_prefix'` selecting
/// every Claude and Pi injection and no Codex one — the marker had matched
/// there too, and the record of it was thrown away.
fn classify_message(
    wire_role: &str,
    text: Option<&str>,
    phase: Option<&str>,
    paired: bool,
) -> (String, String) {
    let injection = |authorship: &'static str, fallback_role: &'static str| {
        if let Some(text) = text {
            if text.trim_start().starts_with("<user_action>") {
                return (
                    semantic::RUNTIME_INTERNAL_CONTEXT.to_owned(),
                    semantic::compose_basis(authorship, basis::TAG_PREFIX),
                );
            }
            let (role, marker) = semantic::runtime_injection(text);
            if role != semantic::RUNTIME_UNKNOWN {
                // A `user_shell_command` is text injected by the Runtime after
                // it has executed a local command. It is neither an agent tool
                // call nor human speech, so keep it in the existing Runtime
                // context category instead of publishing an unknown Item.
                let role = if role == semantic::RUNTIME_BASH_COMMAND {
                    semantic::RUNTIME_INTERNAL_CONTEXT
                } else {
                    role
                };
                return (role.to_owned(), semantic::compose_basis(authorship, marker));
            }
        }
        (fallback_role.to_owned(), authorship.to_owned())
    };
    match wire_role {
        "assistant" => match phase {
            Some("final_answer") => (
                semantic::AGENT_FINAL_ANSWER.to_owned(),
                basis::PHASE_FIELD.to_owned(),
            ),
            Some(_) => (
                semantic::AGENT_COMMENTARY.to_owned(),
                basis::PHASE_FIELD.to_owned(),
            ),
            None => (
                semantic::AGENT_COMMENTARY.to_owned(),
                basis::PHASE_FALLBACK_COMMENTARY.to_owned(),
            ),
        },
        "user" if paired => (
            // Refined to request or steering once turn position is known.
            semantic::HUMAN_REQUEST.to_owned(),
            basis::PAIRED_USER_EVENT.to_owned(),
        ),
        "user" => injection(basis::UNPAIRED_USER, semantic::RUNTIME_UNKNOWN),
        "developer" => injection(
            basis::WIRE_ROLE_DEVELOPER,
            semantic::RUNTIME_PROJECT_INSTRUCTIONS,
        ),
        _ => injection(
            basis::WIRE_ROLE_SYSTEM,
            semantic::RUNTIME_PROJECT_INSTRUCTIONS,
        ),
    }
}

fn guardian_message(
    role: String,
    evidence_basis: String,
    guardian_session: bool,
    wire_role: &str,
) -> (String, String) {
    if guardian_session && wire_role == "user" && role == semantic::RUNTIME_UNKNOWN {
        (
            semantic::RUNTIME_INTERNAL_CONTEXT.to_owned(),
            semantic::compose_basis(&evidence_basis, basis::SUBAGENT_SOURCE),
        )
    } else {
        (role, evidence_basis)
    }
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

/// Codex writes the same message twice with small differences: the UI text
/// omits the image wrapper blocks the context text carries. Equality is tried
/// first; containment is allowed only for text long enough not to collide.
fn texts_match(ui_text: &str, context_text: &str) -> bool {
    if ui_text == context_text {
        return true;
    }
    let (shorter, longer) = if ui_text.len() <= context_text.len() {
        (ui_text, context_text)
    } else {
        (context_text, ui_text)
    };
    shorter.len() >= MIN_CONTAINMENT_BYTES && longer.contains(shorter)
}

fn message_text(payload: &Value) -> Option<String> {
    let blocks = payload.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

fn message_has_images(payload: &Value) -> bool {
    payload
        .get("content")
        .and_then(Value::as_array)
        .is_some_and(|blocks| {
            blocks.iter().any(|block| {
                block
                    .get("type")
                    .and_then(Value::as_str)
                    .is_some_and(|kind| kind.contains("image"))
            })
        })
}

fn provider_tool_name(call_kind: &str) -> Option<&'static str> {
    match call_kind {
        "web_search_call" => Some("web_search"),
        "image_generation_call" => Some("image_generation"),
        "tool_search_call" => Some("tool_search"),
        _ => None,
    }
}

/// Keeps the Runtime-authored argument shape instead of assuming every call
/// uses the Responses API's JSON-encoded string form.
fn runtime_arguments(payload: &Value, call_kind: &str) -> Option<String> {
    if let Some(arguments) = payload.get("arguments") {
        return argument_text(arguments);
    }
    if let Some(input) = payload.get("input") {
        return argument_text(input);
    }
    match call_kind {
        "web_search_call" => payload.get("action").and_then(argument_text),
        "image_generation_call" => {
            let mut arguments = serde_json::Map::new();
            for key in ["prompt", "revised_prompt"] {
                if let Some(value) = payload.get(key) {
                    arguments.insert(key.to_owned(), value.clone());
                }
            }
            (!arguments.is_empty())
                .then(|| serde_json::to_string(&arguments).expect("JSON object serializes"))
        }
        _ => None,
    }
}

fn argument_text(value: &Value) -> Option<String> {
    match value {
        Value::String(text) => Some(text.clone()),
        Value::Null => None,
        _ => serde_json::to_string(value).ok(),
    }
}

fn session_source_kind(payload: &Value) -> Option<String> {
    match payload.get("source")? {
        Value::String(value) => Some(value.clone()),
        Value::Object(value) if value.contains_key("subagent") => Some("subagent".to_owned()),
        Value::Object(value) if value.len() == 1 => value.keys().next().cloned(),
        Value::Object(_) => Some("object".to_owned()),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::Array(_) => None,
    }
}

fn blob_of(text: &str, max_bytes: usize) -> BoundedText {
    BoundedText::bounded(text, max_bytes)
}

fn collect_text<'a>(value: &'a Value, fragments: &mut Vec<&'a str>) {
    match value {
        Value::String(text) => fragments.push(text),
        Value::Array(values) => {
            for value in values {
                collect_text(value, fragments);
            }
        }
        Value::Object(object) => {
            if let Some(text) = object.get("text").and_then(Value::as_str) {
                fragments.push(text);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

/// Parses the structured envelope Codex puts in front of exec output
/// (`Chunk ID`, `Wall time`, `Process exited with code`, `Original token
/// count`) into columns, so failures and durations are queryable without
/// scanning text.
fn tool_output_facts(fragments: &[&str]) -> ToolOutputFacts {
    let mut facts = ToolOutputFacts::default();
    let headers = fragments
        .iter()
        .filter_map(|text| text.split_once("\nOutput:\n").map(|(header, _)| header))
        .filter(|header| {
            header.starts_with("Chunk ID:")
                || header.starts_with("Script completed")
                || header.starts_with("Command")
                || header.starts_with("Exit code:")
        })
        .collect::<Vec<_>>();
    facts.duration_ms = headers
        .iter()
        .find_map(|header| reported_duration_ms(header));

    // Two envelope shapes carry an exit code. `exec_command` prints a header
    // line; the JS `exec` tool returns a JSON result object per chunk, and it
    // is by far the more common tool, so parsing only the header form leaves
    // most calls with no outcome at all.
    let nested = nested_tool_results(fragments);
    let exit_codes = headers
        .iter()
        .flat_map(|header| header.lines())
        .filter_map(|line| {
            line.strip_prefix("Process exited with code ")
                .or_else(|| line.strip_prefix("Exit code: "))
                .and_then(|value| value.parse::<i64>().ok())
        })
        .chain(
            nested
                .iter()
                .filter_map(|result| result.get("exit_code").and_then(Value::as_i64)),
        )
        .collect::<Vec<_>>();
    // A single code identifies one process; a composed output reporting
    // several keeps only the aggregate outcome.
    if exit_codes.len() == 1 {
        facts.exit_code = exit_codes.first().copied();
    }
    let reported_failures = nested
        .iter()
        .filter_map(|result| {
            result.get("isError").and_then(Value::as_bool).or_else(|| {
                result
                    .get("success")
                    .and_then(Value::as_bool)
                    .map(|value| !value)
            })
        })
        .collect::<Vec<_>>();
    facts.nonzero_exit = (!exit_codes.is_empty())
        .then(|| exit_codes.iter().any(|code| *code != 0))
        .or_else(|| {
            (!reported_failures.is_empty()).then(|| reported_failures.iter().any(|failed| *failed))
        });

    let explicitly_truncated = fragments
        .iter()
        .any(|text| text.contains("Warning: truncated output"));
    if explicitly_truncated
        || headers
            .iter()
            .any(|header| header.contains("Original token count: "))
        || nested
            .iter()
            .any(|result| result.contains_key("original_token_count"))
    {
        facts.truncated = Some(explicitly_truncated);
    }
    facts.output_tokens = fragments
        .iter()
        .find_map(|text| reported_original_tokens(text))
        .or_else(|| {
            let reported = nested
                .iter()
                .filter_map(|result| result.get("original_token_count").and_then(Value::as_u64))
                .collect::<Vec<_>>();
            (!reported.is_empty())
                .then(|| reported.into_iter().try_fold(0_u64, u64::checked_add))
                .flatten()
        });
    if facts.duration_ms.is_none() {
        let durations = nested
            .iter()
            .filter_map(|result| {
                result
                    .get("wall_time_seconds")
                    .or_else(|| result.get("duration_seconds"))
                    .and_then(Value::as_f64)
            })
            .collect::<Vec<_>>();
        facts.duration_ms = (!durations.is_empty())
            .then(|| durations.into_iter().sum::<f64>())
            .and_then(|seconds| {
                std::time::Duration::try_from_secs_f64(seconds)
                    .ok()?
                    .as_millis()
                    .try_into()
                    .ok()
            });
    }
    facts
}

/// Facts published by Codex's structured completion event for a tool call.
/// These events are authoritative for duration and outcome and are merged
/// into the corresponding `tool.output` rather than becoming another Item.
fn structured_tool_end_facts(payload: &Value) -> ToolOutputFacts {
    let kind = string_at(payload, "type").unwrap_or("");
    let exit_code = payload.get("exit_code").and_then(Value::as_i64);
    let nonzero_exit = match kind {
        "exec_command_end" => exit_code.map(|code| code != 0).or_else(|| {
            string_at(payload, "status").and_then(|status| match status {
                "failed" | "cancelled" => Some(true),
                "completed" => Some(false),
                _ => None,
            })
        }),
        "mcp_tool_call_end" => payload.get("result").and_then(|result| {
            if result.get("Err").is_some() {
                Some(true)
            } else {
                result
                    .get("Ok")
                    .map(|ok| ok.get("isError").and_then(Value::as_bool).unwrap_or(false))
            }
        }),
        "patch_apply_end" => payload
            .get("success")
            .and_then(Value::as_bool)
            .map(|success| !success),
        _ => None,
    };
    ToolOutputFacts {
        exit_code,
        nonzero_exit,
        duration_ms: structured_duration_ms(payload.get("duration")),
        ..ToolOutputFacts::default()
    }
}

fn mcp_end_parts(payload: &Value) -> (Option<String>, Option<String>, Option<String>) {
    if string_at(payload, "type") != Some("mcp_tool_call_end") {
        return (None, None, None);
    }
    let invocation = payload.get("invocation");
    let name = invocation.and_then(|invocation| {
        let tool = string_at(invocation, "tool")?;
        Some(match string_at(invocation, "server") {
            Some(server) if !server.is_empty() => format!("{server}.{tool}"),
            _ => tool.to_owned(),
        })
    });
    let arguments = invocation
        .and_then(|invocation| invocation.get("arguments"))
        .and_then(argument_text);
    let output = payload.get("result").and_then(|result| {
        if let Some(ok) = result.get("Ok") {
            let mut fragments = Vec::new();
            if let Some(content) = ok.get("content") {
                collect_text(content, &mut fragments);
            }
            if !fragments.is_empty() {
                return Some(fragments.join("\n"));
            }
            return argument_text(ok);
        }
        result.get("Err").and_then(argument_text)
    });
    (name, arguments, output)
}

fn structured_duration_ms(duration: Option<&Value>) -> Option<u64> {
    let duration = duration?;
    let secs = duration.get("secs").and_then(Value::as_u64).unwrap_or(0);
    let nanos = duration.get("nanos").and_then(Value::as_u64).unwrap_or(0);
    secs.checked_mul(1_000)?.checked_add(nanos / 1_000_000)
}

fn merge_tool_end(item: &mut ItemProjection, ended: &PendingToolEnd) {
    item.seq = item.seq.min(ended.seq);
    item.ui_seq = Some(ended.seq);
    let ItemDetail::ToolOutput { facts, .. } = &mut item.detail else {
        return;
    };
    if ended.facts.exit_code.is_some() {
        facts.exit_code = ended.facts.exit_code;
    }
    if ended.facts.nonzero_exit.is_some() {
        facts.nonzero_exit = ended.facts.nonzero_exit;
    }
    if ended.facts.duration_ms.is_some() {
        facts.duration_ms = ended.facts.duration_ms;
    }
}

fn merge_spawn_end(item: &mut ItemProjection, end_seq: u64, native_id: String) {
    item.seq = item.seq.min(end_seq);
    item.ui_seq = Some(end_seq);
    semantic::AGENT_DELEGATION.clone_into(&mut item.semantic_role);
    item.linked_session_native_id = Some(native_id);
    item.searchable = true;
}

fn spawn_output_native_id(output: &str, current_session_native_id: Option<&str>) -> Option<String> {
    serde_json::from_str::<Value>(output)
        .ok()
        .and_then(|value| string_at(&value, "agent_id").map(str::to_owned))
        .or_else(|| {
            output
                .starts_with("You are the newly spawned agent.")
                .then(|| current_session_native_id.map(str::to_owned))
                .flatten()
        })
}

/// Collects JSON result objects embedded in a tool output.
///
/// The JS `exec` tool emits one object per chunk, carrying the exit code,
/// wall time and pre-truncation token count that the plain-text envelope of
/// `exec_command` puts in header lines instead.
fn nested_tool_results(fragments: &[&str]) -> Vec<serde_json::Map<String, Value>> {
    let mut results = Vec::new();
    for text in fragments {
        for line in text.lines() {
            let trimmed = line.trim();
            if !trimmed.starts_with('{') && !trimmed.starts_with('[') {
                continue;
            }
            if let Ok(value) = serde_json::from_str::<Value>(trimmed) {
                collect_result_objects(value, &mut results);
            }
        }
    }
    results
}

fn collect_result_objects(value: Value, results: &mut Vec<serde_json::Map<String, Value>>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_result_objects(value, results);
            }
        }
        Value::Object(object) => {
            if [
                "exit_code",
                "original_token_count",
                "wall_time_seconds",
                "duration_seconds",
                "isError",
                "success",
            ]
            .iter()
            .any(|key| object.contains_key(*key))
            {
                results.push(object.clone());
            }
            for value in object.into_values() {
                collect_result_objects(value, results);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) => {}
    }
}

fn reported_duration_ms(header: &str) -> Option<u64> {
    header.lines().find_map(|line| {
        let seconds = line
            .strip_prefix("Wall time: ")
            .or_else(|| line.strip_prefix("Wall time "))?
            .strip_suffix(" seconds")?
            .parse::<f64>()
            .ok()?;
        std::time::Duration::try_from_secs_f64(seconds)
            .ok()?
            .as_millis()
            .try_into()
            .ok()
    })
}

fn reported_original_tokens(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        line.strip_prefix("Original token count: ")
            .or_else(|| {
                line.strip_prefix("Warning: truncated output (original token count: ")
                    .and_then(|value| value.strip_suffix(')'))
            })
            .and_then(|value| value.parse().ok())
    })
}

fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Shared with the Pi adapter, which uses the same timestamp encoding.
pub(crate) fn parse_timestamp_ms_public(text: &str) -> Option<i64> {
    parse_timestamp_ms(text)
}

/// Parses RFC 3339 timestamps to epoch milliseconds.
///
/// Hand-rolled to avoid a date dependency: the input shape is fixed by the
/// rollout format, and only the offset needs real handling.
fn parse_timestamp_ms(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() < 19 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: i64 = text.get(0..4)?.parse().ok()?;
    let month: i64 = text.get(5..7)?.parse().ok()?;
    let day: i64 = text.get(8..10)?.parse().ok()?;
    let hour: i64 = text.get(11..13)?.parse().ok()?;
    let minute: i64 = text.get(14..16)?.parse().ok()?;
    let second: i64 = text.get(17..19)?.parse().ok()?;

    let rest = text.get(19..).unwrap_or("");
    let (fraction, zone) = rest.strip_prefix('.').map_or(("", rest), |digits| {
        let end = digits
            .find(|character: char| !character.is_ascii_digit())
            .unwrap_or(digits.len());
        (&digits[..end], &digits[end..])
    });
    let millis = match fraction.len() {
        0 => 0,
        length @ 1..=3 => {
            fraction.parse::<i64>().ok()? * 10_i64.pow(3 - u32::try_from(length).ok()?)
        }
        _ => fraction.get(0..3)?.parse().ok()?,
    };

    let offset_seconds = match zone.as_bytes().first() {
        None | Some(b'Z' | b'z') => 0,
        Some(sign @ (b'+' | b'-')) => {
            let hours: i64 = zone.get(1..3)?.parse().ok()?;
            let minutes: i64 = zone.get(4..6).unwrap_or("0").parse().unwrap_or(0);
            let magnitude = hours * 3600 + minutes * 60;
            if *sign == b'-' { -magnitude } else { magnitude }
        }
        Some(_) => return None,
    };

    let days = days_from_civil(year, month, day);
    Some((days * 86_400 + hour * 3600 + minute * 60 + second - offset_seconds) * 1000 + millis)
}

/// Days since the Unix epoch, using Howard Hinnant's civil-date algorithm.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Whether a tool call can receive a separate output record.
///
/// The Responses API defines output items only for function, MCP, custom and
/// tool-search calls. A web search or an image generation is executed by the
/// provider and carries its own status and result on the call item, so waiting
/// for an output that the protocol never emits reported 22 of 26 apparently
/// unpaired calls in a month of rollouts.
fn awaits_output(call_kind: &str) -> bool {
    !matches!(call_kind, "web_search_call" | "image_generation_call")
}

/// The readable part of an agent message. The payload itself may be encrypted,
/// in which case only the header survives.
fn agent_message_text(payload: &Value) -> String {
    match payload.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|block| block.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(""),
        _ => String::new(),
    }
}

/// Reads the `Message Type:` header the runtime writes above an agent message.
fn message_type(text: &str) -> Option<&str> {
    text.strip_prefix("Message Type: ")
        .map(|rest| rest.split('\n').next().unwrap_or(rest).trim())
        .filter(|value| !value.is_empty())
}

/// Reads the native child Session id from the older structured notification
/// wrapper. Only a role already identified as a report is eligible; arbitrary
/// conversation JSON must never create a Session link.
fn linked_session_from_tagged_text(role: &str, text: Option<&str>) -> Option<String> {
    if role != semantic::SUBAGENT_REPORT {
        return None;
    }
    let text = text?;
    let payload = text
        .strip_prefix("<subagent_notification>")?
        .trim_start()
        .strip_suffix("</subagent_notification>")?
        .trim();
    serde_json::from_str::<Value>(payload)
        .ok()
        .and_then(|value| string_at(&value, "agent_path").map(str::to_owned))
}

/// Which argument field of a Codex tool carries a shell command.
///
/// Current tools declare one under different names:
/// `exec_command` uses `cmd` (`core/src/tools/handlers/shell_spec.rs:33`, named
/// at `:92`) and `shell_command` uses `command` (`:160`, named at `:214`). No
/// other handler under `core/src/tools/handlers` declares either field, so the
/// tool name is the gate rather than the presence of a likely-looking key.
/// Older rollouts used `shell` with argv such as `["/bin/zsh", "-lc", "..."]`;
/// the third element is the script in that producer shape.
///
/// Both run a shell, not bash. `exec_command` takes an optional `shell`
/// parameter documented as "Shell binary to launch. Defaults to the user's
/// default shell" (`:62-65`), and `shell_command` describes its argument as a
/// "Shell script to run in the user's default shell" (`:158-161`). On the
/// machine this was built against the default resolves to zsh: the runtime
/// echoes `/bin/zsh` in 2,355 tool outputs against 273 for `/bin/bash`.
///
/// `exec` is deliberately absent. Its argument is JavaScript run in a V8
/// sandbox (`code-mode-runtime/src/runtime/globals.rs`), and the shell command
/// is a string inside a `tools.exec_command({cmd})` call within it. Reaching it
/// needs a JavaScript parse, not a field lookup.
fn shell_command(tool_name: Option<&str>, arguments: Option<&Value>) -> Option<String> {
    let arguments = arguments?;
    match tool_name {
        Some("exec_command") => string_at(arguments, "cmd").map(str::to_owned),
        Some("shell_command") => string_at(arguments, "command").map(str::to_owned),
        Some("shell") => {
            let command = arguments.get("command")?.as_array()?;
            let flag = command.get(1)?.as_str()?;
            matches!(flag, "-c" | "-lc")
                .then(|| command.get(2)?.as_str().map(str::to_owned))
                .flatten()
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        CodexProjector, classify_message, linked_session_from_tagged_text, runtime_arguments,
        shell_command, structured_tool_end_facts, tool_output_facts,
    };
    use crate::adapters::{projection::ItemDetail, semantic};

    #[test]
    fn nested_tool_metadata_does_not_invent_zero_duration() {
        let output = "Script completed\nOutput:\n{\"exit_code\":0,\"original_token_count\":4}";
        let facts = tool_output_facts(&[output]);
        assert_eq!(facts.duration_ms, None);
        assert_eq!(facts.output_tokens, Some(4));
        assert_eq!(facts.nonzero_exit, Some(false));
        assert_eq!(facts.truncated, Some(false));
    }

    #[test]
    fn nested_legacy_metadata_preserves_exit_and_duration() {
        let output = r#"{"output":"","metadata":{"exit_code":1,"duration_seconds":0.1}}"#;
        let facts = tool_output_facts(&[output]);
        assert_eq!(facts.exit_code, Some(1));
        assert_eq!(facts.nonzero_exit, Some(true));
        assert_eq!(facts.duration_ms, Some(100));
    }

    #[test]
    fn legacy_and_provider_tool_arguments_remain_queryable() {
        let shell_payload = json!({
            "arguments": {"command": ["/bin/zsh", "-lc", "printf hello"]}
        });
        let shell_args = runtime_arguments(&shell_payload, "function_call").unwrap();
        let shell_args = serde_json::from_str(&shell_args).unwrap();
        assert_eq!(
            shell_command(Some("shell"), Some(&shell_args)).as_deref(),
            Some("printf hello")
        );

        let web_payload = json!({
            "action": {"type": "open_page", "url": "https://example.com"}
        });
        assert_eq!(
            runtime_arguments(&web_payload, "web_search_call").as_deref(),
            Some(r#"{"type":"open_page","url":"https://example.com"}"#)
        );

        let image_payload = json!({"prompt": "cat", "revised_prompt": "orange cat"});
        assert_eq!(
            runtime_arguments(&image_payload, "image_generation_call").as_deref(),
            Some(r#"{"prompt":"cat","revised_prompt":"orange cat"}"#)
        );
    }

    #[test]
    fn structured_end_is_authoritative_for_duration_and_outcome() {
        let facts = structured_tool_end_facts(&json!({
            "type": "exec_command_end",
            "duration": {"secs": 600, "nanos": 191_955_000},
            "exit_code": 1,
            "status": "failed"
        }));
        assert_eq!(facts.duration_ms, Some(600_191));
        assert_eq!(facts.exit_code, Some(1));
        assert_eq!(facts.nonzero_exit, Some(true));

        let mcp = structured_tool_end_facts(&json!({
            "type": "mcp_tool_call_end",
            "duration": {"secs": 0, "nanos": 339_252_833},
            "result": {"Ok": {"isError": true}}
        }));
        assert_eq!(mcp.duration_ms, Some(339));
        assert_eq!(mcp.nonzero_exit, Some(true));
    }

    #[test]
    fn structured_end_merges_into_existing_tool_output() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "call",
                    "output": "Chunk ID: x\nWall time: 1 seconds\nProcess exited with code 0\nOutput:\nrunning"
                }
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "exec_command_end",
                    "call_id": "call",
                    "duration": {"secs": 60, "nanos": 1_000_000},
                    "exit_code": 1,
                    "status": "failed"
                }
            }),
        );
        let sessions = projector.finish();
        let item = &sessions[0].loops[0].items[0];
        let ItemDetail::ToolOutput { facts, .. } = &item.detail else {
            panic!("expected tool output")
        };
        assert_eq!(facts.duration_ms, Some(60_001));
        assert_eq!(facts.exit_code, Some(1));
        assert_eq!(facts.nonzero_exit, Some(true));
        assert_eq!(item.ui_seq, Some(3));
    }

    #[test]
    fn unpaired_inner_mcp_end_synthesizes_existing_tool_roles() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "timestamp": "1970-01-01T00:00:02Z",
                "type": "event_msg",
                "payload": {
                    "type": "mcp_tool_call_end",
                    "call_id": "inner-call",
                    "invocation": {
                        "server": "codex",
                        "tool": "list_mcp_resources",
                        "arguments": {"cursor": "next"}
                    },
                    "duration": {"secs": 2, "nanos": 0},
                    "result": {"Ok": {"content": [{"type": "text", "text": "done"}], "isError": false}}
                }
            }),
        );
        let sessions = projector.finish();
        let items = &sessions[0].loops[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].seq, 2);
        assert_eq!(items[1].seq, 2);
        assert_eq!(items[0].ts_ms, Some(2_000));
        assert_eq!(items[1].ts_ms, Some(2_000));
        assert_eq!(items[0].semantic_role, semantic::AGENT_TOOL_CALL);
        assert_eq!(items[1].semantic_role, semantic::TOOL_OUTPUT);
        let ItemDetail::ToolCall { name, args, .. } = &items[0].detail else {
            panic!("expected tool call")
        };
        assert_eq!(name.as_deref(), Some("codex.list_mcp_resources"));
        assert!(args.as_ref().is_some_and(|args| args.text.is_some()));
        let ItemDetail::ToolOutput { facts, .. } = &items[1].detail else {
            panic!("expected tool output")
        };
        assert_eq!(facts.duration_ms, Some(2_000));
        assert_eq!(facts.nonzero_exit, Some(false));
    }

    #[test]
    fn child_history_before_declared_boundary_does_not_project_items() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({
                "ordinal": 0,
                "type": "session_meta",
                "payload": {
                    "id": "child-session",
                    "thread_source": "subagent",
                    "agent_path": "/root/child",
                    "subagent_history_start_ordinal": 3
                }
            }),
        );
        projector.push(
            1,
            &json!({
                "ordinal": 1,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "parent copy"}]
                }
            }),
        );
        projector.push(
            3,
            &json!({"ordinal": 3, "type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            4,
            &json!({
                "ordinal": 4,
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": "child answer"}]
                }
            }),
        );

        let sessions = projector.finish();
        let item_seqs = sessions[0]
            .loops
            .iter()
            .flat_map(|turn| turn.items.iter().map(|item| item.record_seq))
            .collect::<Vec<_>>();
        assert_eq!(item_seqs, vec![4]);
    }

    #[test]
    fn structured_subagent_items_keep_native_session_links() {
        let text = concat!(
            "<subagent_notification>\n",
            r#"{"agent_path":"child-session","status":{"completed":"done"}}"#,
            "\n</subagent_notification>"
        );
        assert_eq!(
            linked_session_from_tagged_text(semantic::SUBAGENT_REPORT, Some(text)).as_deref(),
            Some("child-session")
        );

        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({
                "type": "session_meta",
                "payload": {"id": "child-session", "agent_path": "/root/child"}
            }),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "inter_agent_communication",
                "payload": {
                    "author": "/root",
                    "recipient": "/root/child",
                    "content": "Message Type: NEW_TASK\nPayload:\nwork"
                }
            }),
        );
        let sessions = projector.finish();
        let item = &sessions[0].loops[0].items[0];
        assert_eq!(item.semantic_role, semantic::AGENT_DELEGATION);
        assert_eq!(
            item.linked_session_native_id.as_deref(),
            Some("child-session")
        );
        assert!(item.basis.contains("agent_path"));
    }

    #[test]
    fn spawn_end_and_prior_activity_link_existing_items() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "spawn-call",
                    "name": "spawn_agent",
                    "arguments": "{\"message\":\"work\"}"
                }
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "collab_agent_spawn_end",
                    "call_id": "spawn-call",
                    "new_thread_id": "child-session"
                }
            }),
        );
        projector.push(
            4,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "sub_agent_activity",
                    "kind": "interacted",
                    "agent_path": "/root/child",
                    "agent_thread_id": "child-session"
                }
            }),
        );
        projector.push(
            5,
            &json!({
                "type": "inter_agent_communication",
                "payload": {
                    "author": "/root/child",
                    "recipient": "/root",
                    "content": "Message Type: FINAL_ANSWER\nPayload:\ndone"
                }
            }),
        );

        let sessions = projector.finish();
        let items = &sessions[0].loops[0].items;
        assert_eq!(items[0].semantic_role, semantic::AGENT_DELEGATION);
        assert_eq!(
            items[0].linked_session_native_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(items[0].ui_seq, Some(3));
        assert_eq!(items[1].semantic_role, semantic::SUBAGENT_ACTIVITY);
        assert_eq!(items[2].semantic_role, semantic::SUBAGENT_REPORT);
        assert_eq!(
            items[2].linked_session_native_id.as_deref(),
            Some("child-session")
        );
    }

    #[test]
    fn legacy_spawn_output_links_the_delegation() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "spawn-call",
                    "name": "spawn_agent",
                    "arguments": "{\"message\":\"work\"}"
                }
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call_output",
                    "call_id": "spawn-call",
                    "output": "{\"agent_id\":\"child-session\",\"nickname\":\"Euler\"}"
                }
            }),
        );
        let sessions = projector.finish();
        let delegation = &sessions[0].loops[0].items[0];
        assert_eq!(delegation.semantic_role, semantic::AGENT_DELEGATION);
        assert_eq!(
            delegation.linked_session_native_id.as_deref(),
            Some("child-session")
        );
        assert_eq!(delegation.ui_seq, Some(3));
        assert_eq!(
            super::spawn_output_native_id(
                "You are the newly spawned agent. The prior conversation history was forked.",
                Some("current-child")
            )
            .as_deref(),
            Some("current-child")
        );

        let mut activity_projector = CodexProjector::new(1024);
        activity_projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
        );
        activity_projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        activity_projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "function_call",
                    "call_id": "spawn-call",
                    "name": "spawn_agent",
                    "arguments": "{\"message\":\"work\"}"
                }
            }),
        );
        activity_projector.push(
            3,
            &json!({
                "type": "event_msg",
                "payload": {
                    "type": "sub_agent_activity",
                    "kind": "started",
                    "event_id": "spawn-call",
                    "agent_path": "/root/child",
                    "agent_thread_id": "child-session"
                }
            }),
        );
        let sessions = activity_projector.finish();
        assert_eq!(
            sessions[0].loops[0].items[0]
                .linked_session_native_id
                .as_deref(),
            Some("child-session")
        );
    }

    #[test]
    fn guardian_transcript_and_user_action_are_runtime_context() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({
                "type": "session_meta",
                "payload": {
                    "id": "guardian",
                    "source": {"subagent": {"other": "guardian"}}
                }
            }),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "The following is the Codex agent history whose request action you are assessing."}]
                }
            }),
        );
        let sessions = projector.finish();
        assert_eq!(
            sessions[0].loops[0].items[0].semantic_role,
            semantic::RUNTIME_INTERNAL_CONTEXT
        );

        let (role, _) = classify_message(
            "user",
            Some("<user_action>\n<context>review</context>\n</user_action>"),
            None,
            false,
        );
        assert_eq!(role, semantic::RUNTIME_INTERNAL_CONTEXT);
    }

    #[test]
    fn first_human_message_after_agent_output_is_steering() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "commentary",
                    "content": [{"type": "output_text", "text": "working"}]
                }
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "change direction"}
            }),
        );
        projector.push(
            4,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "change direction"}]
                }
            }),
        );

        let sessions = projector.finish();
        let items = &sessions[0].loops[0].items;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].semantic_role, semantic::AGENT_COMMENTARY);
        assert_eq!(items[1].semantic_role, semantic::HUMAN_STEERING);
        assert_eq!(items[1].record_seq, 4);
        assert_eq!(items[1].ui_seq, Some(3));
    }

    #[test]
    fn empty_agent_conversation_records_do_not_project_items() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "event_msg",
                "payload": {"type": "agent_message", "phase": "final_answer", "message": ""}
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "final_answer",
                    "content": [{"type": "output_text", "text": ""}]
                }
            }),
        );
        projector.push(
            4,
            &json!({
                "type": "event_msg",
                "payload": {"type": "agent_message", "phase": "commentary", "message": ""}
            }),
        );
        projector.push(
            5,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "phase": "commentary",
                    "content": [{"type": "output_text", "text": ""}]
                }
            }),
        );

        let sessions = projector.finish();
        assert!(sessions[0].loops[0].items.is_empty());
    }

    #[test]
    fn old_unphased_answer_resolves_at_next_human_boundary() {
        let mut projector = CodexProjector::new(1024);
        projector.push(
            0,
            &json!({"type": "session_meta", "payload": {"id": "session"}}),
        );
        projector.push(
            1,
            &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
        );
        projector.push(
            2,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "first"}]
                }
            }),
        );
        projector.push(
            3,
            &json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "first"}
            }),
        );
        projector.push(
            4,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}]
                }
            }),
        );
        projector.push(
            5,
            &json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "next"}]
                }
            }),
        );
        projector.push(
            6,
            &json!({
                "type": "event_msg",
                "payload": {"type": "user_message", "message": "next"}
            }),
        );
        projector.push(
            7,
            &json!({"type": "turn_context", "payload": {"turn_id": "next-turn"}}),
        );
        let sessions = projector.finish();
        assert_eq!(sessions[0].loops.len(), 2);
        assert_eq!(
            sessions[0].loops[0].items[1].semantic_role,
            semantic::AGENT_FINAL_ANSWER
        );
        assert_eq!(
            sessions[0].loops[1].items[0].semantic_role,
            semantic::HUMAN_REQUEST
        );
        assert_eq!(sessions[0].loops[1].items[0].record_seq, 5);
    }

    #[test]
    fn user_shell_command_is_runtime_context_not_unknown() {
        let (role, _) = classify_message(
            "user",
            Some("<user_shell_command>\necho hello\n</user_shell_command>"),
            None,
            false,
        );
        assert_eq!(role, semantic::RUNTIME_INTERNAL_CONTEXT);
    }
}

// tested at the public relation boundary in `indexer`.
