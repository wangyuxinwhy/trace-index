//! Stateful Runtime adapter for Claude Code session JSONL.
//!
//! Claude Code publishes the same five domain objects as the other Runtimes.
//! Three native properties shape its Adapter:
//!
//! * Records form a `uuid`/`parentUuid` tree, because a rewind leaves the
//!   abandoned attempt in the file. The pointers are recorded as facts and the
//!   timeline stays a sequence; which branch a record ended on is a query-time
//!   walk. Under two percent of records sit off the active path, so paying a
//!   per-write cost to describe them would be the wrong trade.
//! * A subagent run is a separate file whose records carry the *parent's*
//!   `sessionId` and their own `agentId`. It therefore projects to its own
//!   Session with delegation evidence pointing back to its parent Session.
//! * `promptId` appears on user records only, never on assistant records, so
//!   it helps establish where a Loop begins but not which later Records belong
//!   to it. Membership therefore remains a sequence range.
//!
//! Two classifications are stronger here than their Pi equivalents: authorship
//! can rest on the runtime's own `origin.kind`, and the final answer on the
//! provider's `stop_reason`, rather than on position.

use std::collections::{HashMap, HashSet};

use serde_json::Value;

use crate::adapters::projection::{
    self, BoundedText, ItemDetail, ItemProjection, LoopProjection, RecordFacts, SessionProjection,
};
use crate::adapters::semantic::{self, basis};
use crate::adapters::timestamp::parse_timestamp_ms;

mod parse;

use self::parse::{
    ContentBlock, TaskNotification, attachment_role, attachment_task_notification, attachment_text,
    content_blocks, is_bash_wrapper, is_rejected_goal_status, is_stop_hook_feedback,
    is_subagent_opening, notification_output_facts, queued_human_prompt,
    semantic_role_requires_loop, string, tool_output_facts, turn_position, user_authorship,
    user_task_notification,
};

/// Marks an interrupted turn without depending on the message text, which is
/// localized and has changed wording between versions.
const ABORT_REASON_INTERRUPTED: &str = "interrupted";

pub(crate) fn record_facts(value: &Value) -> RecordFacts {
    let kind = string(value, "type").unwrap_or("unknown");
    // Claude Code spreads its own refinement of the record type over four
    // different fields, so the refinement is read per kind. It keeps a record
    // classifiable as a physical Record without interpreting its payload.
    let refinement = match kind {
        "system" => string(value, "subtype"),
        "attachment" => value
            .get("attachment")
            .and_then(|attachment| string(attachment, "type")),
        "queue-operation" => string(value, "operation"),
        "user" | "assistant" => leading_block_type(value),
        _ => None,
    };
    RecordFacts {
        ts_ms: string(value, "timestamp").and_then(parse_timestamp_ms),
        native_type: projection::native_type(kind, refinement),
        parse_status: "ok",
        parse_error: None,
    }
}

/// A fork header identifies the child Source and its parent without a
/// `sessionId` stamp. The referenced parent context is not present here.
pub(crate) fn fork_context_identity(value: &Value) -> Option<(&str, &str)> {
    if string(value, "type") != Some("fork-context-ref") {
        return None;
    }
    let agent = string(value, "agentId").filter(|id| !id.is_empty())?;
    let parent = string(value, "parentSessionId").filter(|id| !id.is_empty())?;
    Some((agent, parent))
}

/// The type of the first content block, which says what a message record
/// actually carries: a prompt, a tool result, reasoning, or a tool call.
fn leading_block_type(value: &Value) -> Option<&str> {
    let content = value.get("message")?.get("content")?;
    if content.is_string() {
        return Some("text");
    }
    content
        .as_array()?
        .first()
        .and_then(|block| string(block, "type"))
}

/// One item's worth of projection, before it is placed on the timeline.
struct ItemSeed {
    semantic_role: String,
    basis: &'static str,
    preview: Option<BoundedText>,
    detail: ItemDetail,
    linked_session_native_id: Option<String>,
    searchable: bool,
}

#[derive(Debug)]
struct LoopBuilder {
    /// The prompt associated with this Loop. Prompt changes help establish boundaries.
    prompt_id: Option<String>,
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    model: Option<String>,
    effort: Option<String>,
    context_window: Option<u64>,
    usage: Option<crate::domain::Usage>,
    items: Vec<ItemProjection>,
    request_item: Option<usize>,
    final_answer_item: Option<usize>,
    human_count: u32,
    /// The runtime signalled this turn's end: the model stopped of its own
    /// accord, or the harness recorded the turn's duration. Without one of
    /// those the turn is open, not complete — a session can end mid-turn.
    ended: bool,
    end_record_seq: Option<u64>,
    outcome: Option<crate::domain::LoopOutcome>,
    aborted: bool,
    abort_reason: Option<String>,
    /// Whether the model has produced anything in this Loop yet.
    ///
    /// A prompt arriving before it has is a correction to the request already
    /// made, not a new one — the model never got to answer the first — so it
    /// joins this Loop rather than opening another. Claude Code cannot tell
    /// the two apart on its own: `promptId` changes on every submission,
    /// typed-while-idle and typed-while-working alike.
    answered: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct TurnPosition {
    prompt_index: u64,
    turn_index: u64,
}

impl LoopBuilder {
    fn new(start_seq: u64, prompt_id: Option<String>) -> Self {
        Self {
            prompt_id,
            start_seq,
            end_seq: start_seq,
            started_at: None,
            ended_at: None,
            model: None,
            effort: None,
            context_window: None,
            usage: None,
            items: Vec::new(),
            request_item: None,
            final_answer_item: None,
            human_count: 0,
            ended: false,
            end_record_seq: None,
            outcome: None,
            aborted: false,
            abort_reason: None,
            answered: false,
        }
    }

    /// Records an observation about whether the turn has ended.
    ///
    /// An ending already observed is never overwritten, because `ended` is
    /// monotonic and the evidence has to be too: a turn the provider closed,
    /// followed by a reply that merely continued, would otherwise report
    /// `completed` on evidence saying it was still going.
    fn observed_end(&mut self, seq: u64, ends: bool) {
        if self.ended {
            return;
        }
        self.ended = ends;
        if ends {
            self.end_record_seq = Some(seq);
        }
    }

    fn push(&mut self, item: ItemProjection) -> usize {
        self.end_seq = self.end_seq.max(item.seq);
        self.items.push(item);
        self.items.len() - 1
    }

    fn finish(self) -> LoopProjection {
        LoopProjection {
            // `promptId` names a submission rather than the outer lifecycle,
            // so it is not a Runtime-native Loop identity.
            native_id: None,
            start_seq: self.start_seq,
            end_record_seq: self.end_record_seq,
            outcome: self.outcome,
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
    /// The `sessionId` stamped on records. For a subagent source this names
    /// the parent session, not this one.
    session_uuid: Option<String>,
    /// Present only for a subagent source, where it is this session's identity.
    agent_id: Option<String>,
    /// Native parent identity and the Record declaring inherited context.
    fork_context_reference: Option<(String, u64)>,
    ordinal: u32,
    start_seq: u64,
    end_seq: u64,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    cwd: Option<String>,
    cli_version: Option<String>,
    git_branch: Option<String>,
    /// Every Loop of the Session, kept open until the Source ends. A prompt id
    /// can reappear after another one intervened, so a Loop has to stay
    /// writable rather than being closed when the next prompt arrives.
    loops: Vec<LoopBuilder>,
    items: Vec<ItemProjection>,
    /// Index into `loops` that new items are appended to.
    current: Option<usize>,
    by_prompt: HashMap<String, usize>,
    /// Claude fork sources can rewrite several inherited prompts to one
    /// `promptId` while preserving their distinct native turn positions.
    /// Exact positions disambiguate those ids; traces without this newer
    /// field retain the prompt-only fallback.
    by_turn_position: HashMap<TurnPosition, usize>,
    usage: projection::UsageLedger,
    /// Tool calls whose output may identify later Runtime-generated records.
    /// Claude repeats the call id in `sourceToolUseID`, which lets injected
    /// Skill bodies retain their purpose instead of becoming generic notices.
    skill_call_ids: HashSet<String>,
    /// Runtime task ids resolve delayed notifications back to their origin.
    /// The notification tag alone is ambiguous: both child Agents and
    /// background tools use `<task-notification>`.
    async_tasks: HashMap<String, AsyncTask>,
    /// Projected Agent-delegation location. Claude reveals the child Session
    /// only in the later launch result, so these few Items need backfilling;
    /// ordinary tool calls never do.
    delegation_locations: HashMap<String, (usize, usize)>,
}

#[derive(Debug, Clone)]
enum AsyncTask {
    Agent,
    Tool { call_id: String },
}

impl SessionBuilder {
    fn new(ordinal: u32, start_seq: u64) -> Self {
        Self {
            session_uuid: None,
            agent_id: None,
            fork_context_reference: None,
            ordinal,
            start_seq,
            end_seq: start_seq,
            started_at: None,
            ended_at: None,
            cwd: None,
            cli_version: None,
            git_branch: None,
            loops: Vec::new(),
            items: Vec::new(),
            current: None,
            by_prompt: HashMap::new(),
            by_turn_position: HashMap::new(),
            usage: projection::UsageLedger::default(),
            skill_call_ids: HashSet::new(),
            async_tasks: HashMap::new(),
            delegation_locations: HashMap::new(),
        }
    }

    fn loop_mut(&mut self, seq: u64) -> &mut LoopBuilder {
        let index = match self.current {
            Some(index) => index,
            None => self.push_loop(seq, None, None),
        };
        &mut self.loops[index]
    }

    fn push_loop(
        &mut self,
        seq: u64,
        prompt_id: Option<String>,
        turn_position: Option<TurnPosition>,
    ) -> usize {
        let index = self.loops.len();
        if let Some(prompt_id) = prompt_id.clone() {
            self.by_prompt.insert(prompt_id, index);
        }
        if let Some(turn_position) = turn_position {
            self.by_turn_position.insert(turn_position, index);
        }
        self.loops.push(LoopBuilder::new(seq, prompt_id));
        self.switch_to(index);
        index
    }

    fn switch_to(&mut self, index: usize) {
        if self.current == Some(index) {
            return;
        }
        self.current = Some(index);
    }

    fn finish(mut self) -> SessionProjection {
        self.end_seq = self
            .loops
            .iter()
            .fold(self.end_seq, |end, loop_| end.max(loop_.end_seq));
        let mut usage = self.usage.finish();
        for turn in &mut self.loops {
            turn.usage = usage.remove(&turn.start_seq);
        }
        let loops = self
            .loops
            .drain(..)
            .map(LoopBuilder::finish)
            .collect::<Vec<_>>();
        let ordinal = self.ordinal;
        let is_subagent = self.agent_id.is_some();
        let session_uuid = self
            .agent_id
            .clone()
            .or_else(|| self.session_uuid.clone())
            .unwrap_or_else(|| format!("unlabeled-{ordinal}"));
        let identity_confirmed = self.agent_id.is_some() || self.session_uuid.is_some();
        let (forked_from_native_id, forked_from_record_seq) = self
            .fork_context_reference
            .map_or((None, None), |(parent, seq)| (Some(parent), Some(seq)));
        SessionProjection {
            session_uuid,
            identity_confirmed,
            start_seq: self.start_seq,
            started_at: self.started_at,
            cwd: self.cwd,
            // Only a subagent source knows its parent: its records carry the
            // launching session's id while naming themselves by agent id.
            forked_from_native_id,
            forked_from_locator: None,
            forked_from_record_seq,
            delegated_from_record_seq: is_subagent.then_some(self.start_seq),
            delegated_from_native_id: if is_subagent { self.session_uuid } else { None },
            title: None,
            items: self.items,
            loops,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ClaudeProjector {
    max_text_bytes: usize,
    sessions: Vec<SessionProjection>,
    session: Option<SessionBuilder>,
    next_session_ordinal: u32,
}

impl ClaudeProjector {
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
            self.session = Some(SessionBuilder::new(ordinal, seq));
        }
        self.session.as_mut().expect("session was just ensured")
    }

    pub(crate) fn push(&mut self, seq: u64, value: &Value) {
        let ts_ms = string(value, "timestamp").and_then(parse_timestamp_ms);
        self.absorb_session_facts(seq, value, ts_ms);

        let record_kind = string(value, "type").unwrap_or("unknown");
        match record_kind {
            "user" => self.user(seq, value, ts_ms),
            "assistant" => self.assistant(seq, value, ts_ms),
            "attachment" => self.attachment(seq, value, ts_ms),
            "system" => self.system(seq, value, ts_ms),
            other => self.control(seq, value, ts_ms, other),
        }
    }

    /// Session-level facts repeat on nearly every record; the first sighting
    /// wins so a later relocation cannot rewrite where the work happened.
    fn absorb_session_facts(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let fork_identity = fork_context_identity(value);
        let session_id = string(value, "sessionId")
            .or_else(|| fork_identity.map(|(_, parent)| parent))
            .map(str::to_owned);
        let cwd = string(value, "cwd").map(str::to_owned);
        let version = string(value, "version").map(str::to_owned);
        let branch = string(value, "gitBranch")
            .filter(|branch| !branch.is_empty())
            .map(str::to_owned);
        // A sidechain record's `agentId` is the identity of the whole source.
        let agent_id = fork_identity
            .map(|(agent, _)| agent.to_owned())
            .or_else(|| {
                value
                    .get("isSidechain")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                    .then(|| string(value, "agentId").map(str::to_owned))
                    .flatten()
            });

        let session = self.session_mut(seq);
        if session.session_uuid.is_none() {
            session.session_uuid = session_id;
        }
        if session.agent_id.is_none() {
            session.agent_id = agent_id;
        }
        if let Some((agent, parent)) = fork_identity
            && session.agent_id.as_deref() == Some(agent)
            && session.session_uuid.as_deref() == Some(parent)
            && session.fork_context_reference.is_none()
        {
            session.fork_context_reference = Some((parent.to_owned(), seq));
        }
        if session.cwd.is_none() {
            session.cwd = cwd;
        }
        if session.cli_version.is_none() {
            session.cli_version = version;
        }
        if session.git_branch.is_none() {
            session.git_branch = branch;
        }
        session.started_at = session.started_at.or(ts_ms);
        session.ended_at = ts_ms.or(session.ended_at);
        session.end_seq = session.end_seq.max(seq);
    }

    fn user(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        // An interruption ends the turn that was running, whatever the next
        // record loops out to open.
        if value.get("interruptedMessageId").is_some() {
            let turn = self.session_mut(seq).loop_mut(seq);
            turn.aborted = true;
            turn.ended = true;
            turn.end_record_seq = Some(seq);
            turn.outcome = Some(crate::domain::LoopOutcome::Interrupted);
            turn.abort_reason = Some(ABORT_REASON_INTERRUPTED.to_owned());
        }

        // Claude may reject an `end_turn` through a Stop hook and immediately
        // continue the same execution. The provider's earlier answer was then
        // only commentary, not the Loop's final answer.
        if is_stop_hook_feedback(value) {
            self.reject_provisional_end(seq);
        }

        // Claude uses the same tagged envelope for child-Agent reports,
        // background command completions, and Monitor events. Resolve the
        // task id against the structured launch result before ordinary
        // user-message handling; otherwise Runtime output opens a fake human
        // Loop and every notification becomes a subagent report.
        if let Some(notification) = user_task_notification(value) {
            self.emit_task_notification(seq, value, ts_ms, notification);
            return;
        }

        // The first sidechain user Record repeats the prompt already carried
        // by the parent's Agent tool call. It still anchors the child Session
        // and `delegated_from`, but publishing it again would create a second
        // delegation Item with identical text. Match only Claude's observed
        // opening shape; later sidechain user Records remain eligible Items.
        if is_subagent_opening(value) {
            return;
        }

        let seeds = self.user_seeds(seq, value);
        for seed in seeds {
            let index = self.emit(seq, ts_ms, value, seed);
            self.mark_human(seq, index);
        }
    }

    fn user_seeds(&mut self, seq: u64, value: &Value) -> Vec<ItemSeed> {
        let is_subagent = self.session_mut(seq).agent_id.is_some()
            || value.get("isSidechain").and_then(Value::as_bool) == Some(true);
        let blocks = content_blocks(value);
        let has_images = blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::Image));
        let has_tool_result = blocks
            .iter()
            .any(|block| matches!(block, ContentBlock::ToolResult { .. }));
        let source_tool_use_id = string(value, "sourceToolUseID");
        let is_skill_injection = source_tool_use_id
            .is_some_and(|call_id| self.session_mut(seq).skill_call_ids.contains(call_id));
        let authorship = user_authorship(value, is_subagent, is_skill_injection, has_images);
        let mut seeds = Vec::new();

        // Text and images are content blocks of one Runtime message, not
        // separate Agent-program facts. Keeping them together makes
        // `has_images` describe the same Item that carries the text.
        let text = blocks
            .iter()
            .filter_map(|block| match block {
                ContentBlock::Text(text) if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if has_tool_result || is_bash_wrapper(&text) {
            // Claude reuses the active human prompt id for `!` commands even
            // when they run after that prompt's turn ended. Tool results also
            // carry the prompt they serve. Both are concrete Runtime triggers,
            // not returns to a closed Loop with the same prompt id.
            self.open_loop_after_end(seq);
        } else {
            let prompt_id = string(value, "promptId").map(str::to_owned);
            self.open_turn_for_prompt(seq, prompt_id, turn_position(value));
        }
        if !text.is_empty() || has_images {
            let (role, why) = authorship.classify(&text);
            let blob = self.optional_blob(&text);
            let linked_session_native_id = (role == semantic::AGENT_DELEGATION)
                .then(|| self.session_mut(seq).agent_id.clone())
                .flatten();
            seeds.push(ItemSeed {
                searchable: blob.is_some() && semantic::is_conversation(&role),
                semantic_role: role,
                basis: why,
                preview: blob,
                detail: ItemDetail::Message { has_images },
                linked_session_native_id,
            });
        }

        for block in &blocks {
            if let ContentBlock::ToolResult {
                call_id,
                text,
                is_error,
            } = block
            {
                self.remember_async_task(seq, value, call_id);
                let blob = text.as_deref().map(|text| self.blob(text));
                seeds.push(ItemSeed {
                    semantic_role: semantic::TOOL_OUTPUT.to_owned(),
                    basis: basis::BLOCK_TYPE,
                    preview: None,
                    detail: ItemDetail::ToolOutput {
                        call_id: call_id.clone(),
                        output: blob,
                        facts: tool_output_facts(value, *is_error),
                    },
                    linked_session_native_id: None,
                    searchable: false,
                });
            }
        }
        seeds
    }

    /// Promotes the first human message of a turn to its request pointer, and
    /// later ones to steering. Returns whether this Item was counted.
    fn mark_human(&mut self, seq: u64, index: usize) -> bool {
        // Runtime and delegation Items may deliberately live at Session scope.
        // `emit` reports that placement with this sentinel, so there is no
        // Loop-local Item to classify as request or steering.
        if index == usize::MAX {
            return false;
        }
        let turn = self.session_mut(seq).loop_mut(seq);
        if !turn.items[index].semantic_role.starts_with("human.") {
            return false;
        }
        turn.human_count += 1;
        let (role, position) = if turn.human_count > 1 {
            (semantic::HUMAN_STEERING, basis::SUBSEQUENT_IN_LOOP)
        } else {
            (semantic::HUMAN_REQUEST, basis::FIRST_IN_LOOP)
        };
        let item = &mut turn.items[index];
        // The authorship term already on the Item — `origin_kind` when the
        // runtime named the author, `wire_role_user` when it did not — is what
        // a caller weighs, so the position rule appends to it instead of
        // replacing it.
        item.basis = semantic::compose_basis(&item.basis.clone(), position);
        role.clone_into(&mut item.semantic_role);
        if role == semantic::HUMAN_REQUEST && turn.request_item.is_none() {
            turn.request_item = Some(index);
        }
        true
    }

    fn assistant(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let is_api_error = value.get("isApiErrorMessage").and_then(Value::as_bool) == Some(true);
        let message = value.get("message").unwrap_or(&Value::Null);
        let model = string(message, "model").map(str::to_owned);
        let effort = string(value, "effort")
            .or_else(|| string(message, "effort"))
            .map(str::to_owned);
        let usage = message.get("usage").and_then(|usage| {
            projection::normalized_usage(
                usage.get("input_tokens").and_then(Value::as_u64),
                usage.get("cache_read_input_tokens").and_then(Value::as_u64),
                usage
                    .get("cache_creation_input_tokens")
                    .and_then(Value::as_u64),
                usage.get("output_tokens").and_then(Value::as_u64),
                usage
                    .get("output_tokens_details")
                    .and_then(|details| details.get("thinking_tokens"))
                    .and_then(Value::as_u64),
                false,
            )
        });
        // The model itself declares the turn finished; the last text block of
        // that record is the answer.
        let stop_reason = string(message, "stop_reason");
        let blocks = content_blocks(value);
        // Virtual tool observations have synthetic stop reasons, not a model's
        // decision to end the surrounding execution.
        let model_response = !(string(message, "model") == Some("<synthetic>")
            && value.get("isVirtual").and_then(Value::as_bool) == Some(true)
            && blocks
                .iter()
                .any(|block| matches!(block, ContentBlock::ToolUse { .. })));
        let ends_turn = model_response && stop_reason == Some("end_turn");
        let signals_end =
            model_response && matches!(stop_reason, Some("end_turn" | "stop_sequence"));

        // The answer is the closing text of the record that ended the turn.
        // A withheld-text block cannot be it.
        let last_text = blocks
            .iter()
            .rposition(|block| matches!(block, ContentBlock::Text(text) if !text.is_empty()));
        let mut seeds = Vec::new();
        for (index, block) in blocks.iter().enumerate() {
            let is_answer = ends_turn && last_text == Some(index);
            if let Some(seed) = self.assistant_block_seed(seq, block, is_api_error, is_answer) {
                seeds.push(seed);
            }
        }

        let mut answer_index = None;
        for seed in seeds {
            let is_answer = seed.semantic_role == semantic::AGENT_FINAL_ANSWER;
            let index = self.emit(seq, ts_ms, value, seed);
            if is_answer {
                answer_index = Some(index);
            }
        }

        let turn = self.session_mut(seq).loop_mut(seq);
        if signals_end {
            turn.observed_end(seq, true);
        } else if stop_reason.is_some() {
            // The provider said why it stopped and it was not an ending — it
            // went to call a tool, or ran out of output. If nothing follows,
            // the turn is open because it was still going, which is not the
            // same as no ending having been recorded.
            turn.observed_end(seq, false);
        }
        turn.model = turn.model.take().or(model);
        turn.effort = turn.effort.take().or(effort);
        let usage_loop = turn.start_seq;
        if string(message, "model") != Some("<synthetic>") {
            self.session_mut(seq)
                .usage
                .observe(string(message, "id"), usage_loop, usage);
        }
        let turn = self.session_mut(seq).loop_mut(seq);
        if let Some(index) = answer_index {
            turn.final_answer_item = Some(index);
        }
    }

    /// Remembers the Runtime task identity returned by an asynchronous tool.
    /// Later notifications contain the task id, while ordinary `ToolOutput`
    /// linking uses the original call id.
    fn remember_async_task(&mut self, seq: u64, value: &Value, call_id: &str) {
        let Some(result) = value.get("toolUseResult").and_then(Value::as_object) else {
            return;
        };
        let session = self.session_mut(seq);
        if let Some(agent_id) = result.get("agentId").and_then(Value::as_str) {
            if let Some(&(loop_index, item_index)) = session.delegation_locations.get(call_id) {
                session.loops[loop_index].items[item_index].linked_session_native_id =
                    Some(agent_id.to_owned());
            }
            session
                .async_tasks
                .insert(agent_id.to_owned(), AsyncTask::Agent);
        }
        for key in ["backgroundTaskId", "taskId"] {
            if let Some(task_id) = result.get(key).and_then(Value::as_str) {
                session.async_tasks.insert(
                    task_id.to_owned(),
                    AsyncTask::Tool {
                        call_id: call_id.to_owned(),
                    },
                );
            }
        }
    }

    fn emit_task_notification(
        &mut self,
        seq: u64,
        value: &Value,
        ts_ms: Option<i64>,
        notification: TaskNotification,
    ) {
        // A delayed result arriving after `turn_duration` starts another
        // execution. Keeping the trigger in the closed Loop is what caused a
        // later response to appear after that Loop's final answer.
        self.open_loop_after_end(seq);
        let known_task = notification
            .task_id
            .as_deref()
            .and_then(|task_id| self.session_mut(seq).async_tasks.get(task_id).cloned());
        let linked_session_native_id = notification.task_id.clone();
        let preview = self.blob(&notification.text);
        let seed = match known_task {
            Some(AsyncTask::Agent) => ItemSeed {
                semantic_role: semantic::SUBAGENT_REPORT.to_owned(),
                basis: notification.basis,
                preview: Some(preview),
                detail: ItemDetail::Message { has_images: false },
                linked_session_native_id,
                searchable: false,
            },
            Some(AsyncTask::Tool { call_id }) => ItemSeed {
                semantic_role: semantic::TOOL_OUTPUT.to_owned(),
                basis: notification.basis,
                preview: None,
                detail: ItemDetail::ToolOutput {
                    call_id,
                    output: Some(preview),
                    facts: notification_output_facts(&notification.text),
                },
                linked_session_native_id: None,
                searchable: false,
            },
            None if notification.tool_use_id.is_some() => ItemSeed {
                semantic_role: semantic::TOOL_OUTPUT.to_owned(),
                basis: notification.basis,
                preview: None,
                detail: ItemDetail::ToolOutput {
                    call_id: notification.tool_use_id.unwrap_or_default(),
                    output: Some(preview),
                    facts: notification_output_facts(&notification.text),
                },
                linked_session_native_id: None,
                searchable: false,
            },
            None => ItemSeed {
                semantic_role: semantic::RUNTIME_NOTICE.to_owned(),
                basis: notification.basis,
                preview: Some(preview),
                detail: ItemDetail::Message { has_images: false },
                linked_session_native_id: None,
                searchable: false,
            },
        };
        self.emit(seq, ts_ms, value, seed);
    }

    fn attachment(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let attachment = value.get("attachment").unwrap_or(&Value::Null);
        let attachment_kind = string(attachment, "type").unwrap_or("unknown");
        if is_rejected_goal_status(attachment) {
            self.reject_provisional_end(seq);
        }
        if let Some(notification) = attachment_task_notification(attachment) {
            self.emit_task_notification(seq, value, ts_ms, notification);
            return;
        }
        if attachment_kind == "queued_command"
            && string(attachment, "commandMode") == Some("prompt")
            && attachment.pointer("/origin/kind").and_then(Value::as_str) == Some("human")
        {
            let Some((text, has_images)) = queued_human_prompt(attachment) else {
                return;
            };
            // A queued prompt delivered while work is active is steering. If
            // the Runtime has already ended that work, the same shape opens a
            // new Loop and is its request.
            self.open_loop_after_end(seq);
            let preview = self.optional_blob(&text);
            let searchable = preview.is_some();
            let index = self.emit(
                seq,
                ts_ms,
                value,
                ItemSeed {
                    semantic_role: semantic::HUMAN_REQUEST.to_owned(),
                    basis: basis::ORIGIN_KIND,
                    preview,
                    detail: ItemDetail::Message { has_images },
                    linked_session_native_id: None,
                    searchable,
                },
            );
            self.mark_human(seq, index);
            return;
        }
        let role = attachment_role(attachment_kind);
        let Some(text) = attachment_text(attachment, attachment_kind, role) else {
            // Structured attachment fields remain available in the Record.
            // Do not serialize a Runtime-specific object into Semantic merely
            // to preserve data already present at its physical source.
            return;
        };
        let preview = self.blob(&text);
        self.emit(
            seq,
            ts_ms,
            value,
            ItemSeed {
                semantic_role: role.to_owned(),
                basis: basis::NATIVE_SUBTYPE,
                preview: Some(preview),
                detail: ItemDetail::Misc,
                linked_session_native_id: None,
                searchable: false,
            },
        );
    }

    fn system(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>) {
        let subtype = string(value, "subtype").unwrap_or("unknown");
        // A compact boundary says compaction happened. Its content is a notice
        // such as "Conversation compacted", not the replacement summary.
        let role = if subtype == "turn_duration" {
            semantic::RUNTIME_LIFECYCLE
        } else {
            semantic::RUNTIME_NOTICE
        };
        let text = string(value, "content").map(str::to_owned);
        let preview = self.blob(text.as_deref().unwrap_or(subtype));
        if subtype == "turn_duration" {
            // The harness recorded how long the turn took, so it ended.
            let turn = self.session_mut(seq).loop_mut(seq);
            turn.ended_at = ts_ms.or(turn.ended_at);
            turn.observed_end(seq, true);
            return;
        }
        self.emit(
            seq,
            ts_ms,
            value,
            ItemSeed {
                semantic_role: role.to_owned(),
                basis: basis::NATIVE_SUBTYPE,
                preview: Some(preview),
                detail: ItemDetail::Misc,
                linked_session_native_id: None,
                searchable: false,
            },
        );
    }

    /// Selects control-plane records that carry an independent Agent-program
    /// fact. Other control records remain available as Records; publishing
    /// mode bookkeeping, cached titles, or snapshots as Items would duplicate
    /// state already represented by Session or Loop.
    fn control(&mut self, seq: u64, value: &Value, ts_ms: Option<i64>, record_kind: &str) {
        if fork_context_identity(value).is_some() {
            // Publish the reference, not reconstructed parent messages. A
            // context header alone is not an execution trigger.
            let preview = self.blob(&value.to_string());
            self.emit(
                seq,
                ts_ms,
                value,
                ItemSeed {
                    semantic_role: semantic::RUNTIME_SESSION_REFERENCE.to_owned(),
                    basis: basis::RECORD_KIND,
                    preview: Some(preview),
                    detail: ItemDetail::Message { has_images: false },
                    linked_session_native_id: None,
                    searchable: false,
                },
            );
            return;
        }
        let role = match record_kind {
            // A launch and its result name the subagent Source that ran.
            "started" => semantic::SUBAGENT_ACTIVITY,
            "result" => semantic::SUBAGENT_REPORT,
            _ => return,
        };
        self.open_loop_after_end(seq);
        let preview = self.blob(record_kind);
        let linked_session_native_id = string(value, "agentId")
            .or_else(|| string(value, "taskId"))
            .map(str::to_owned);
        self.emit(
            seq,
            ts_ms,
            value,
            ItemSeed {
                semantic_role: role.to_owned(),
                basis: basis::RECORD_KIND,
                preview: Some(preview),
                detail: ItemDetail::Misc,
                linked_session_native_id,
                searchable: false,
            },
        );
    }

    /// Starts a new turn when the prompt changes. A tool result carries the
    /// prompt id of the request it serves, so it never opens one.
    fn open_turn_for_prompt(
        &mut self,
        seq: u64,
        prompt_id: Option<String>,
        turn_position: Option<TurnPosition>,
    ) {
        let Some(prompt_id) = prompt_id else {
            return;
        };
        let session = self.session_mut(seq);
        // An exact native position is stronger than prompt identity. This
        // also routes a late record back to its original Loop after another
        // prompt has intervened.
        if let Some(&index) = turn_position
            .as_ref()
            .and_then(|position| session.by_turn_position.get(position))
        {
            session.switch_to(index);
            return;
        }
        // A prompt id already seen names a turn that is still being written to:
        // when a prompt is queued while tools are running, the results of the
        // earlier prompt keep arriving afterwards and carry its id again.
        // Reading each reappearance as a boundary would cut one turn into
        // several and orphan the tool outputs that land in the pieces.
        if let Some(&index) = session.by_prompt.get(&prompt_id) {
            if turn_position.is_none()
                || (!session.loops[index].ended && !session.loops[index].aborted)
            {
                if let Some(turn_position) = turn_position {
                    session.by_turn_position.insert(turn_position, index);
                }
                session.switch_to(index);
                return;
            }
            // A closed Loop plus a previously unseen native position is a
            // distinct turn. Claude fork sources exhibit exactly this shape
            // after rewriting inherited prompt ids.
            session.push_loop(seq, Some(prompt_id), turn_position);
            return;
        }
        match session.current {
            // Records that preceded the first prompt already opened a turn;
            // adopt the prompt rather than leaving it unlabelled. The turn
            // keeps `implicit_open`: the prompt supplies a name, not the
            // boundary, which still sits where the trace happened to resume.
            Some(index)
                if session.loops[index].prompt_id.is_none()
                    && !session.loops[index].ended
                    && !session.loops[index].aborted =>
            {
                session.loops[index].prompt_id = Some(prompt_id.clone());
                session.by_prompt.insert(prompt_id, index);
                if let Some(turn_position) = turn_position {
                    session.by_turn_position.insert(turn_position, index);
                }
            }
            // A new prompt delivered while the outer Loop is still running is
            // steering. `promptId` identifies a submission, not the outer
            // lifecycle, so it maps to the current Loop instead of creating a
            // second one.
            Some(index) if !session.loops[index].ended && !session.loops[index].aborted => {
                session.by_prompt.insert(prompt_id, index);
                if let Some(turn_position) = turn_position {
                    session.by_turn_position.insert(turn_position, index);
                }
            }
            _ => {
                session.push_loop(seq, Some(prompt_id), turn_position);
            }
        }
    }

    /// Opens an implicit Loop only when the Runtime has already closed the
    /// current one and then delivers a new concrete trigger.
    fn open_loop_after_end(&mut self, seq: u64) {
        let session = self.session_mut(seq);
        if session
            .current
            .is_some_and(|index| session.loops[index].ended)
        {
            session.push_loop(seq, None, None);
        }
    }

    /// Reverses only the most recent provider-declared ending after Claude
    /// explicitly says a Stop hook rejected it.
    fn reject_provisional_end(&mut self, seq: u64) {
        let session = self.session_mut(seq);
        let Some(index) = session.current else {
            return;
        };
        let turn = &mut session.loops[index];
        if turn.aborted || turn.outcome.is_some() {
            return;
        }
        let Some(final_index) = turn.final_answer_item.take() else {
            return;
        };
        let item = &mut turn.items[final_index];
        if item.semantic_role != semantic::AGENT_FINAL_ANSWER {
            return;
        }
        semantic::AGENT_COMMENTARY.clone_into(&mut item.semantic_role);
        basis::BLOCK_TYPE.clone_into(&mut item.basis);
        turn.ended = false;
        turn.end_record_seq = None;
    }

    fn emit(
        &mut self,
        seq: u64,
        ts_ms: Option<i64>,
        _record_value: &Value,
        seed: ItemSeed,
    ) -> usize {
        let role = seed.semantic_role.clone();
        let delegation_call_id = if role == semantic::AGENT_DELEGATION {
            match &seed.detail {
                ItemDetail::ToolCall { call_id, .. } => Some(call_id.clone()),
                _ => None,
            }
        } else {
            None
        };
        let answered = seed
            .semantic_role
            .split_once('.')
            .is_some_and(|(author, _)| author == semantic::AUTHOR_AGENT);
        let item = ItemProjection {
            seq,
            record_seq: seq,
            ui_seq: None,
            ts_ms,
            semantic_role: seed.semantic_role,
            basis: seed.basis.to_owned(),
            preview: seed.preview,
            detail: seed.detail,
            linked_session_native_id: seed.linked_session_native_id,
            searchable: seed.searchable,
        };
        let session = self.session_mut(seq);
        if session.current.is_none() && !semantic_role_requires_loop(&role) {
            session.items.push(item);
            return usize::MAX;
        }
        let loop_index = match session.current {
            Some(index) => index,
            None => session.push_loop(seq, None, None),
        };
        let item_index = {
            let turn = &mut session.loops[loop_index];
            turn.started_at = turn.started_at.or(ts_ms);
            turn.ended_at = ts_ms.or(turn.ended_at);
            turn.answered |= answered;
            turn.push(item)
        };
        if let Some(call_id) = delegation_call_id {
            session
                .delegation_locations
                .insert(call_id, (loop_index, item_index));
        }
        item_index
    }

    /// Interns text, or reports its absence. Withheld content becomes a null
    /// blob rather than an empty one, so "no text" and "empty text" do not
    /// collapse into the same row.
    fn assistant_block_seed(
        &mut self,
        seq: u64,
        block: &ContentBlock,
        is_api_error: bool,
        is_answer: bool,
    ) -> Option<ItemSeed> {
        match block {
            ContentBlock::Text(text) => {
                let blob = self.optional_blob(text);
                Some(ItemSeed {
                    semantic_role: if is_api_error {
                        semantic::RUNTIME_NOTICE.to_owned()
                    } else if is_answer {
                        semantic::AGENT_FINAL_ANSWER.to_owned()
                    } else {
                        semantic::AGENT_COMMENTARY.to_owned()
                    },
                    basis: if is_api_error {
                        basis::API_ERROR_FLAG
                    } else if is_answer {
                        basis::STOP_REASON_END_LOOP
                    } else {
                        basis::BLOCK_TYPE
                    },
                    linked_session_native_id: None,
                    searchable: blob.is_some() && !is_api_error,
                    preview: blob,
                    detail: ItemDetail::Message { has_images: false },
                })
            }
            ContentBlock::Thinking(text) => {
                // Reasoning whose text was withheld still becomes an item:
                // the step happened, and the record stays on the timeline.
                let blob = self.optional_blob(text);
                Some(ItemSeed {
                    semantic_role: semantic::AGENT_REASONING.to_owned(),
                    basis: basis::BLOCK_TYPE,
                    preview: blob,
                    detail: ItemDetail::Reasoning {
                        representation: if text.is_empty() {
                            crate::domain::ReasoningRepresentation::Unavailable
                        } else {
                            crate::domain::ReasoningRepresentation::Full
                        },
                    },
                    linked_session_native_id: None,
                    searchable: false,
                })
            }
            ContentBlock::ToolUse {
                call_id,
                name,
                cmd,
                cmd_lang,
                arguments,
                delegation_text,
            } => {
                if name == "Skill" {
                    self.session_mut(seq).skill_call_ids.insert(call_id.clone());
                }
                let args = projection::complete_arguments(arguments, self.max_text_bytes);
                let is_delegation = name == "Agent";
                let preview = is_delegation.then(|| {
                    delegation_text
                        .as_deref()
                        .map_or_else(|| self.blob(name), |text| self.blob(text))
                });
                Some(ItemSeed {
                    semantic_role: if is_delegation {
                        semantic::AGENT_DELEGATION.to_owned()
                    } else {
                        semantic::AGENT_TOOL_CALL.to_owned()
                    },
                    basis: basis::BLOCK_TYPE,
                    preview,
                    detail: ItemDetail::ToolCall {
                        call_id: call_id.clone(),
                        name: Some(name.clone()),
                        cmd: cmd.clone(),
                        cmd_lang: *cmd_lang,
                        workdir: None,
                        args,
                        syntax: None,
                    },
                    linked_session_native_id: None,
                    searchable: is_delegation,
                })
            }
            // The exact block remains in its Record until its content has
            // a typed Semantic meaning. Publishing only its type name
            // would create an Item with no independently useful fact.
            ContentBlock::Image | ContentBlock::ToolResult { .. } | ContentBlock::Other => None,
        }
    }

    fn optional_blob(&self, text: &str) -> Option<BoundedText> {
        (!text.is_empty()).then(|| self.blob(text))
    }

    fn blob(&self, text: &str) -> BoundedText {
        BoundedText::bounded(text, self.max_text_bytes)
    }
}
