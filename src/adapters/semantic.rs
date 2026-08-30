//! Runtime-specific evidence used to produce the public `Semantic` value.
//!
//! Adapters first classify native shapes with the detailed constants in this
//! module. Publication then maps them into the smaller cross-Runtime
//! [`SemanticRole`](crate::domain::SemanticRole) vocabulary and records the
//! weakest evidence strength used. The detailed basis terms are private
//! implementation evidence, not additional public domain concepts.

/// Author prefixes. Every role below starts with one of these.
pub(crate) const AUTHOR_HUMAN: &str = "human";
pub(crate) const AUTHOR_AGENT: &str = "agent";

pub(crate) const HUMAN_REQUEST: &str = "human.request";
pub(crate) const HUMAN_STEERING: &str = "human.steering";
pub(crate) const AGENT_COMMENTARY: &str = "agent.commentary";
pub(crate) const AGENT_FINAL_ANSWER: &str = "agent.final_answer";
pub(crate) const AGENT_REASONING: &str = "agent.reasoning";
pub(crate) const AGENT_TOOL_CALL: &str = "agent.tool_call";
pub(crate) const AGENT_DELEGATION: &str = "agent.delegation";
pub(crate) const TOOL_OUTPUT: &str = "tool.output";
pub(crate) const SUBAGENT_ACTIVITY: &str = "subagent.activity";
pub(crate) const SUBAGENT_REPORT: &str = "subagent.report";

pub(crate) const RUNTIME_UNKNOWN: &str = "runtime.unknown";
pub(crate) const RUNTIME_COMPACTION: &str = "runtime.compaction_summary";
pub(crate) const RUNTIME_STATE: &str = "runtime.state";
pub(crate) const RUNTIME_LIFECYCLE: &str = "runtime.lifecycle";
pub(crate) const RUNTIME_NOTICE: &str = "runtime.notice";

// Detailed Runtime injection shapes. Persistence folds these into the smaller
// public Runtime roles; the supporting Record remains the original evidence.
pub(crate) const RUNTIME_SESSION_REFERENCE: &str = "runtime.session_reference";
pub(crate) const RUNTIME_PROJECT_INSTRUCTIONS: &str = "runtime.project_instructions";
pub(crate) const RUNTIME_MEMORY: &str = "runtime.memory";
pub(crate) const RUNTIME_ENV_CONTEXT: &str = "runtime.env_context";
pub(crate) const RUNTIME_USER_INSTRUCTIONS: &str = "runtime.user_instructions";
pub(crate) const RUNTIME_SKILL_INSTRUCTIONS: &str = "runtime.skill_instructions";
pub(crate) const RUNTIME_PERMISSIONS: &str = "runtime.permissions";
pub(crate) const RUNTIME_COLLAB_MODE: &str = "runtime.collab_mode";
pub(crate) const RUNTIME_PLUGINS: &str = "runtime.plugins";
pub(crate) const RUNTIME_APPS: &str = "runtime.apps";
pub(crate) const RUNTIME_PERSONALITY: &str = "runtime.personality";
pub(crate) const RUNTIME_INTERNAL_CONTEXT: &str = "runtime.internal_context";
pub(crate) const RUNTIME_IMAGE_ATTACHMENT: &str = "runtime.image_attachment";
pub(crate) const RUNTIME_ABORT_NOTICE: &str = "runtime.abort_notice";
pub(crate) const RUNTIME_SLASH_COMMAND: &str = "runtime.slash_command";
pub(crate) const RUNTIME_BASH_COMMAND: &str = "runtime.bash_command";
pub(crate) const RUNTIME_HOOK_OUTPUT: &str = "runtime.hook_output";
pub(crate) const RUNTIME_IDE_CONTEXT: &str = "runtime.ide_context";
pub(crate) const RUNTIME_FILE_CHANGE: &str = "runtime.file_change";
pub(crate) const RUNTIME_FILE_CONTEXT: &str = "runtime.file_context";
pub(crate) const RUNTIME_TOOL_CATALOG: &str = "runtime.tool_catalog";
pub(crate) const RUNTIME_BUDGET: &str = "runtime.budget";

/// Whether a role belongs to the conversation rather than to the harness.
///
/// Drives what enters the full-text index: runtime injections are templated
/// and repeat every turn, so indexing them costs more than the raw data and
/// buries real matches.
pub(crate) fn is_conversation(semantic_role: &str) -> bool {
    matches!(
        semantic_role.split_once('.').map(|(author, _)| author),
        Some(AUTHOR_HUMAN | AUTHOR_AGENT)
    )
}

/// Joins the evidence for authorship with the evidence for position.
///
/// A `semantic_role` is a pair, so its evidence is a pair. Deciding that a
/// human wrote something and deciding whether it opens a Loop or steers one
/// are separate findings resting on separate observations, and a caller asking
/// "did a person really type this" needs the first half — which two adapters
/// used to discard by overwriting it with the second.
///
/// The authorship term is passed in rather than read back, because pairing can
/// itself be the evidence: a Codex model-input record provisionally filed as
/// `unpaired_user` becomes `paired_user_event` once its twin arrives.
pub(crate) fn compose_basis(authorship: &str, position: &str) -> String {
    format!("{authorship}+{position}")
}

/// Whether a stored `basis` carries a given term.
///
/// Terms are joined by `+`, so equality is wrong on any composed value and a
/// plain substring test is too loose: it would match a term that merely spells
/// another one's prefix. Splitting on the separator is the only reading that
/// stays correct as terms are added.
pub(crate) fn has_basis_term(stored: &str, term: &str) -> bool {
    stored.split('+').any(|part| part == term)
}

/// Private evidence terms used by the Runtime adapters. Several terms can be
/// composed with `+`; only their weakest strength is published.
pub(crate) mod basis {
    pub(crate) const PAIRED_USER_EVENT: &str = "paired_user_event";
    pub(crate) const UNPAIRED_USER: &str = "unpaired_user";
    pub(crate) const ORIGIN_KIND: &str = "origin_kind";
    pub(crate) const PROMPT_SOURCE: &str = "prompt_source";
    pub(crate) const PROMPT_SOURCE_SDK: &str = "prompt_source_sdk";
    pub(crate) const WIRE_ROLE_USER: &str = "wire_role_user";
    pub(crate) const WIRE_ROLE_DEVELOPER: &str = "wire_role_developer";
    pub(crate) const WIRE_ROLE_SYSTEM: &str = "wire_role_system";
    pub(crate) const PHASE_FIELD: &str = "phase_field";
    pub(crate) const PHASE_FALLBACK_TASK_COMPLETE: &str = "phase_fallback_task_complete";
    pub(crate) const PHASE_FALLBACK_COMMENTARY: &str = "phase_fallback_commentary";
    pub(crate) const STOP_REASON_END_LOOP: &str = "stop_reason_end_loop";
    pub(crate) const STOP_REASON_CONTINUES: &str = "stop_reason_continues";
    pub(crate) const RECORD_KIND: &str = "record_kind";
    pub(crate) const NATIVE_SUBTYPE: &str = "native_subtype";
    pub(crate) const BLOCK_TYPE: &str = "block_type";
    pub(crate) const META_FLAG: &str = "meta_flag";
    pub(crate) const API_ERROR_FLAG: &str = "api_error_flag";
    pub(crate) const SUBAGENT_SOURCE: &str = "subagent_source";
    pub(crate) const AGENT_PATH: &str = "agent_path";
    pub(crate) const FIRST_IN_LOOP: &str = "first_in_loop";
    pub(crate) const SUBSEQUENT_IN_LOOP: &str = "subsequent_in_loop";
    pub(crate) const TAG_PREFIX: &str = "tag_prefix";
    pub(crate) const MARKDOWN_HEADER: &str = "markdown_header";
    pub(crate) const TEXT_PREFIX: &str = "text_prefix";
    pub(crate) const NO_MARKER: &str = "no_marker";
}

pub(crate) fn basis_is_heuristic(term: &str) -> bool {
    matches!(
        term,
        basis::PROMPT_SOURCE_SDK
            | basis::WIRE_ROLE_USER
            | basis::PHASE_FALLBACK_TASK_COMPLETE
            | basis::PHASE_FALLBACK_COMMENTARY
            | basis::TAG_PREFIX
            | basis::MARKDOWN_HEADER
            | basis::TEXT_PREFIX
            | basis::NO_MARKER
    )
}

/// Maps a runtime-injected text block to its purpose.
///
/// Only reached once the author is already known to be `runtime` (decided
/// structurally, by the dual-track difference). Tag prefixes are a secondary
/// signal used to subdivide, never to decide authorship — they change between
/// Codex versions, the dual-track structure does not.
/// The marker each runtime writes, and the role it names.
///
/// A table rather than a `match` so a test can walk every marker and check the
/// role it produces is declared, which is what keeps the vocabulary and the
/// rules that reach it from drifting apart.
///
/// **Every entry must be verified against the code that writes it** — the Codex
/// and Pi sources, the Claude Code distribution — and not inferred from traces
/// alone. A trace is the output; only the producer says what markers exist.
/// Inferring them cost two invented entries here, `<ide-selection>` and
/// `<app-context>`, neither of which appears in any runtime.
///
/// Two failure modes make trace-only inference unsafe in opposite directions.
/// A marker absent from a corpus may be perfectly real and simply untriggered:
/// `<user-prompt-submit-hook>` and `<ide_selection>` are in the Claude Code
/// distribution but appear in none of 307 sampled sessions. And a marker
/// present in a corpus may be gone from the current release, because a corpus
/// spans versions while a distribution is one — this corpus covers twelve Codex
/// versions and nineteen of Claude Code.
pub(crate) static INJECTION_TAGS: &[(&str, &str)] = &[
    // Codex. Taken from `codex-rs/protocol/src/protocol.rs`, whose `*_OPEN_TAG`
    // constants each fragment in `codex-rs/core/src/context/` returns from
    // `ContextualUserFragment::type_markers`.
    ("environment_context", RUNTIME_ENV_CONTEXT),
    ("environments_instructions", RUNTIME_ENV_CONTEXT),
    ("user_instructions", RUNTIME_USER_INSTRUCTIONS),
    ("skills_instructions", RUNTIME_SKILL_INSTRUCTIONS),
    ("skill", RUNTIME_SKILL_INSTRUCTIONS),
    ("permissions", RUNTIME_PERMISSIONS),
    ("collaboration_mode", RUNTIME_COLLAB_MODE),
    ("multi_agent_mode", RUNTIME_COLLAB_MODE),
    ("realtime_conversation", RUNTIME_COLLAB_MODE),
    ("realtime_delegation", RUNTIME_COLLAB_MODE),
    ("plugins_instructions", RUNTIME_PLUGINS),
    ("recommended_plugins", RUNTIME_PLUGINS),
    ("apps_instructions", RUNTIME_APPS),
    ("personality_spec", RUNTIME_PERSONALITY),
    ("turn_aborted", RUNTIME_ABORT_NOTICE),
    ("model_switch", RUNTIME_NOTICE),
    ("codex_internal_context", RUNTIME_INTERNAL_CONTEXT),
    // The pre-`codex_internal_context` spelling, still matched by the runtime.
    ("goal_context", RUNTIME_INTERNAL_CONTEXT),
    ("tools", RUNTIME_TOOL_CATALOG),
    ("context_window", RUNTIME_BUDGET),
    ("context_window_guidance", RUNTIME_BUDGET),
    ("rollout_budget", RUNTIME_BUDGET),
    ("user_shell_command", RUNTIME_BASH_COMMAND),
    ("image", RUNTIME_IMAGE_ATTACHMENT),
    ("/image", RUNTIME_IMAGE_ATTACHMENT),
    ("subagent_notification", SUBAGENT_REPORT),
    // Claude Code wraps its own harness traffic in the same position, as
    // user-role text the model reads but no human typed.
    ("system-reminder", RUNTIME_NOTICE),
    ("command-name", RUNTIME_SLASH_COMMAND),
    ("command-message", RUNTIME_SLASH_COMMAND),
    ("command-args", RUNTIME_SLASH_COMMAND),
    ("local-command-stdout", RUNTIME_SLASH_COMMAND),
    ("local-command-stderr", RUNTIME_SLASH_COMMAND),
    ("local-command-caveat", RUNTIME_SLASH_COMMAND),
    ("bash-input", RUNTIME_BASH_COMMAND),
    ("bash-stdout", RUNTIME_BASH_COMMAND),
    ("bash-stderr", RUNTIME_BASH_COMMAND),
    ("user-prompt-submit-hook", RUNTIME_HOOK_OUTPUT),
    ("ide_selection", RUNTIME_IDE_CONTEXT),
    ("task-notification", SUBAGENT_REPORT),
];

/// Untagged injections, matched by their opening text.
///
/// Codex fragments whose `type_markers` is `("", "")` carry no tag at all, so a
/// text prefix is the only handle and the classification is heuristic by
/// construction rather than by omission.
///
/// Two of these have a ceiling that no amount of care removes. The multi-agent
/// hints are the defaults of the config fields `root_agent_usage_hint_text` and
/// `subagent_usage_hint_text`, so a customized hint will not match; the image
/// notice interpolates the output paths, so only its opening is fixed. Both are
/// recorded here rather than discovered later as a silent miss.
pub(crate) static INJECTION_PREFIXES: &[(&str, &str)] = &[
    ("Approved command prefix saved:", RUNTIME_PERMISSIONS),
    ("Network rule saved:", RUNTIME_PERMISSIONS),
    // codex-rs/core/src/config/mod.rs, DEFAULT_MULTI_AGENT_V2_*_USAGE_HINT_TEXT.
    (
        "You are an agent in a team of agents collaborating to complete a task.",
        RUNTIME_COLLAB_MODE,
    ),
    (
        "You are `/root`, the primary agent in a team of agents",
        RUNTIME_COLLAB_MODE,
    ),
    // codex-rs/ext/image-generation/src/artifact.rs, image_generation_output_hint.
    ("Generated images are saved to ", RUNTIME_NOTICE),
    // codex-rs/core/src/context/legacy_*.rs, whose `matches_text` overrides
    // these same literals because their markers are empty.
    ("Warning: apply_patch was requested via ", RUNTIME_NOTICE),
    (
        "Warning: The maximum number of unified exec processes you can keep open is",
        RUNTIME_NOTICE,
    ),
    (
        "Warning: Your account was flagged for potentially high-risk cyber activity",
        RUNTIME_NOTICE,
    ),
];

/// Leading markdown headers that name an injection, for markers that are not
/// XML-ish tags.
pub(crate) static INJECTION_HEADERS: &[(&str, &str)] = &[
    ("# AGENTS.md instructions", RUNTIME_PROJECT_INSTRUCTIONS),
    ("## Memory", RUNTIME_MEMORY),
];

/// Pi extension message types, and the role each names.
///
/// Pi types its injections at the source instead of wrapping them in a marker:
/// an extension calling `sendMessage` supplies a `customType`, and the entry
/// keeps it. So this table is matched on a declared field rather than on a text
/// convention, which is why it carries a structural basis while
/// `INJECTION_TAGS` carries a heuristic one.
///
/// The set is open by construction — a `customType` is whatever string an
/// extension passes — so an unlisted one is expected rather than exceptional,
/// and lands as `runtime.unknown` with the type preserved in `native_type`.
/// Each entry below names the file that constructs it; two types observed in
/// the corpus, `pi-memory-recall` and `subagent_companion_suggestions`, are
/// deliberately absent because their extension was not found to read.
pub(crate) static PI_CUSTOM_MESSAGE_TYPES: &[(&str, &str)] = &[
    // pi-extensions/extensions/skill-recall.ts: skills matched against the
    // current task, wrapped in `<skill-recall>`.
    ("skill-recall", RUNTIME_SKILL_INSTRUCTIONS),
    // .pi/extensions/space-mem.ts, MEMORY_RECALL_CUSTOM_TYPE: personal,
    // project and team memory, wrapped in `<memory-recall>`.
    ("pi-space-memory-recall", RUNTIME_MEMORY),
    // pi-webui/extensions/lark-auth.ts: whether the Lark credential is usable.
    ("lark-auth-check", RUNTIME_NOTICE),
    // pi-extensions/extensions/onboarding.ts, buildKickoff: the script the
    // model is to run the onboarding walkthrough from.
    ("onboarding", RUNTIME_NOTICE),
    // .pi/extensions/todo.ts, buildContextMessage: the team's todo list,
    // refreshed onto each prompt.
    ("todo-context", RUNTIME_STATE),
    // pi-subagents/src/runs/background/notify.ts: a background subagent
    // finished, delivered on SUBAGENT_ASYNC_COMPLETE_EVENT.
    ("subagent-notify", SUBAGENT_REPORT),
    // pi-subagents/src/extension/control-notices.ts: a control event about a
    // subagent, neither its task nor its result.
    ("subagent_control_notice", SUBAGENT_ACTIVITY),
];

/// The role a Pi extension message type names, and the evidence for it.
pub(crate) fn pi_custom_message(custom_type: &str) -> (&'static str, &'static str) {
    PI_CUSTOM_MESSAGE_TYPES
        .iter()
        .find(|(declared, _)| *declared == custom_type)
        .map_or((RUNTIME_UNKNOWN, basis::RECORD_KIND), |(_, role)| {
            (role, basis::NATIVE_SUBTYPE)
        })
}

pub(crate) fn runtime_injection(text: &str) -> (&'static str, &'static str) {
    let trimmed = text.trim_start();

    if let Some((_, role)) = INJECTION_HEADERS
        .iter()
        .find(|(header, _)| trimmed.starts_with(header))
    {
        return (role, basis::MARKDOWN_HEADER);
    }

    let Some(tag) = leading_xml_tag(trimmed) else {
        return INJECTION_PREFIXES
            .iter()
            .find(|(prefix, _)| trimmed.starts_with(prefix))
            .map_or((RUNTIME_UNKNOWN, basis::NO_MARKER), |(_, role)| {
                (role, basis::TEXT_PREFIX)
            });
    };

    INJECTION_TAGS
        .iter()
        .find(|(marker, _)| *marker == tag)
        .map_or((RUNTIME_UNKNOWN, basis::TAG_PREFIX), |(_, role)| {
            (role, basis::TAG_PREFIX)
        })
}

/// Returns the element name of a leading XML-ish tag.
///
/// Tolerates Codex's non-strict markers such as `<permissions instructions>`
/// and `<image name=[Image #1]>`, where the text after the name is not valid
/// XML attribute syntax.
fn leading_xml_tag(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('<')?;
    let name_end = rest
        .find(|character: char| {
            !(character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | ':' | '/'))
        })
        .unwrap_or(rest.len());
    if name_end == 0 {
        return None;
    }
    let name = &rest[..name_end];
    let delimiter = rest[name_end..].chars().next();
    if !delimiter.is_none_or(|character| character == '>' || character.is_ascii_whitespace()) {
        return None;
    }
    text.find('>')?;
    Some(name)
}
