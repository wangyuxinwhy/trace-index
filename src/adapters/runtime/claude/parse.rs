//! Stateless Claude Code wire-format parsers, authorship classifiers, and block extractors.

use serde_json::Value;

use super::TurnPosition;
use crate::adapters::projection::ToolOutputFacts;
use crate::adapters::semantic::{self, basis};
use crate::shell::syntax::lang;

pub(super) struct TaskNotification {
    pub(super) text: String,
    pub(super) task_id: Option<String>,
    pub(super) tool_use_id: Option<String>,
    pub(super) basis: &'static str,
}

pub(super) fn user_task_notification(value: &Value) -> Option<TaskNotification> {
    if value.pointer("/origin/kind").and_then(Value::as_str) != Some("task-notification") {
        return None;
    }
    let text = value.pointer("/message/content")?.as_str()?.to_owned();
    Some(parse_task_notification(text, basis::ORIGIN_KIND))
}

pub(super) fn is_stop_hook_feedback(value: &Value) -> bool {
    value.get("isMeta").and_then(Value::as_bool) == Some(true)
        && value
            .pointer("/message/content")
            .and_then(Value::as_str)
            .is_some_and(|text| text.starts_with("Stop hook feedback:"))
}

pub(super) fn is_rejected_goal_status(attachment: &Value) -> bool {
    string(attachment, "type") == Some("goal_status")
        && attachment.get("met").and_then(Value::as_bool) == Some(false)
}

pub(super) fn is_bash_wrapper(text: &str) -> bool {
    let text = text.trim_start();
    text.starts_with("<bash-input>")
        || text.starts_with("<bash-stdout>")
        || text.starts_with("<bash-stderr>")
}

pub(super) fn is_subagent_opening(value: &Value) -> bool {
    value.get("isSidechain").and_then(Value::as_bool) == Some(true)
        && string(value, "agentId").is_some()
        && value.get("parentUuid").is_some_and(Value::is_null)
}

pub(super) fn attachment_task_notification(attachment: &Value) -> Option<TaskNotification> {
    if string(attachment, "type") != Some("queued_command")
        || string(attachment, "commandMode") != Some("task-notification")
    {
        return None;
    }
    let text = string(attachment, "prompt")?.to_owned();
    Some(parse_task_notification(text, basis::NATIVE_SUBTYPE))
}

fn parse_task_notification(text: String, basis: &'static str) -> TaskNotification {
    TaskNotification {
        task_id: tagged_value(&text, "task-id").map(str::to_owned),
        tool_use_id: tagged_value(&text, "tool-use-id").map(str::to_owned),
        text,
        basis,
    }
}

fn tagged_value<'a>(text: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let value = text.split_once(&open)?.1.split_once(&close)?.0.trim();
    (!value.is_empty()).then_some(value)
}

pub(super) fn notification_output_facts(text: &str) -> ToolOutputFacts {
    let nonzero_exit = match tagged_value(text, "status") {
        Some("completed") => Some(false),
        Some("failed") => Some(true),
        _ => None,
    };
    ToolOutputFacts {
        nonzero_exit,
        ..ToolOutputFacts::default()
    }
}

/// How a user record's authorship was established, decided once per record
/// rather than per content block.
pub(super) enum Authorship {
    /// The runtime named the author outright.
    Declared {
        role: &'static str,
        basis: &'static str,
    },
    /// No declaration available: fall back to recognizing the harness's own
    /// injection markers, and treat unmarked text as typed by a person.
    Inferred,
}

impl Authorship {
    pub(super) fn classify(&self, text: &str) -> (String, &'static str) {
        match self {
            Self::Declared { role, basis } => ((*role).to_owned(), *basis),
            Self::Inferred => {
                let trimmed = text.trim_start();
                if trimmed.starts_with("[Request interrupted by user") {
                    return (
                        semantic::RUNTIME_ABORT_NOTICE.to_owned(),
                        basis::TEXT_PREFIX,
                    );
                }
                // The preamble the runtime writes when a compacted session
                // resumes. It wears the user role but summarizes prior work.
                if trimmed
                    .starts_with("This session is being continued from a previous conversation")
                {
                    return (semantic::RUNTIME_COMPACTION.to_owned(), basis::TEXT_PREFIX);
                }
                let (injected, why) = semantic::runtime_injection(text);
                if injected == semantic::RUNTIME_UNKNOWN && why == basis::NO_MARKER {
                    (semantic::HUMAN_REQUEST.to_owned(), basis::WIRE_ROLE_USER)
                } else {
                    (injected.to_owned(), why)
                }
            }
        }
    }
}

pub(super) fn user_authorship(
    value: &Value,
    is_subagent: bool,
    is_skill_injection: bool,
    has_images: bool,
) -> Authorship {
    // `isMeta` is the runtime saying this record is not user input.
    if value
        .get("isMeta")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return Authorship::Declared {
            role: if is_skill_injection {
                semantic::RUNTIME_SKILL_INSTRUCTIONS
            } else if has_images {
                semantic::RUNTIME_FILE_CONTEXT
            } else {
                semantic::RUNTIME_NOTICE
            },
            basis: basis::META_FLAG,
        };
    }
    // A subagent trace has no human channel at all: the opening message is the
    // task its launcher wrote, and everything after it is a tool result or a
    // runtime injection. Attributing any of it to a person would let templated
    // delegation prompts read as things a user kept repeating.
    if is_subagent {
        return Authorship::Declared {
            role: semantic::AGENT_DELEGATION,
            basis: basis::SUBAGENT_SOURCE,
        };
    }
    match value
        .get("origin")
        .and_then(|origin| string(origin, "kind"))
    {
        Some("human") => Authorship::Declared {
            role: semantic::HUMAN_REQUEST,
            basis: basis::ORIGIN_KIND,
        },
        // A well-formed task notification was handled before this function.
        // Malformed Runtime traffic is still not human or a child report.
        Some(_) => Authorship::Declared {
            role: semantic::RUNTIME_NOTICE,
            basis: basis::ORIGIN_KIND,
        },
        // `origin` rides on only some prompt records. `promptSource` covers
        // more of them, and the runtime groups its values itself: typed,
        // queued and accepted suggestions are user-originated, while `sdk`
        // and `system` are not.
        None => match string(value, "promptSource") {
            Some("typed" | "queued" | "suggestion_accepted") => Authorship::Declared {
                role: semantic::HUMAN_REQUEST,
                basis: basis::PROMPT_SOURCE,
            },
            // The channel is known and the author is not: a person running the
            // CLI non-interactively and a program driving the SDK write the
            // same record. Reading the SDK would not settle it either — the
            // caller is outside anything the runtime can see.
            Some("sdk") => Authorship::Declared {
                role: semantic::HUMAN_REQUEST,
                basis: basis::PROMPT_SOURCE_SDK,
            },
            Some("system") => Authorship::Declared {
                role: semantic::RUNTIME_NOTICE,
                basis: basis::PROMPT_SOURCE,
            },
            // Neither field: the markers are the only evidence left.
            _ => Authorship::Inferred,
        },
    }
}

/// The language a Claude Code tool's `command` parameter is written in.
///
/// The name is the gate, not the presence of a `command` field. Three tools in
/// the distribution declare one and they do not agree on what it means:
/// `BashTool` (`tools/BashTool/BashTool.tsx:228`, named by
/// `BASH_TOOL_NAME = 'Bash'`) carries a shell command; `PowerShellTool`
/// (`PowerShellTool.tsx:229`, `POWERSHELL_TOOL_NAME = 'PowerShell'`) carries a
/// PowerShell one, which is a different language entirely; and `TaskStopTool`
/// declares `command` on its *output* schema as "the command or description of
/// the stopped task", which is not a command to run at all.
///
/// A blind `input.command` lookup therefore records PowerShell as shell. No
/// PowerShell call appears in the corpus this was built against, but that
/// corpus is macOS and the tool is Windows-only: an unexercised branch is not
/// a dead one.
///
/// `Bash` resolves to `shell` rather than `bash` because that is what the
/// distribution does. `utils/Shell.ts:85-120` honours `CLAUDE_CODE_SHELL`, then
/// `process.env.SHELL` when it names bash or zsh, and otherwise searches in the
/// order `['zsh', 'bash']` — zsh first unless the environment prefers bash. The
/// tool is named Bash; the shell it runs is not necessarily bash.
///
/// PowerShell is declared and left unparsed: naming a language this build has
/// no grammar for is a fact, and silently filing it as shell is not.
fn command_language(tool_name: &str) -> Option<&'static str> {
    match tool_name {
        "Bash" => Some(lang::SHELL),
        "PowerShell" => Some(lang::POWERSHELL),
        _ => None,
    }
}

pub(super) enum ContentBlock {
    Text(String),
    Thinking(String),
    Image,
    ToolUse {
        call_id: String,
        name: String,
        cmd: Option<String>,
        cmd_lang: Option<&'static str>,
        arguments: String,
        delegation_text: Option<String>,
    },
    ToolResult {
        call_id: String,
        text: Option<String>,
        is_error: Option<bool>,
    },
    Other,
}

pub(super) fn content_blocks(value: &Value) -> Vec<ContentBlock> {
    let Some(content) = value
        .get("message")
        .and_then(|message| message.get("content"))
    else {
        return Vec::new();
    };
    if let Some(text) = content.as_str() {
        return if text.is_empty() {
            Vec::new()
        } else {
            vec![ContentBlock::Text(text.to_owned())]
        };
    }
    let Some(blocks) = content.as_array() else {
        return Vec::new();
    };
    blocks
        .iter()
        .filter_map(|block| match string(block, "type") {
            // Empty text is kept rather than filtered: a `thinking` block whose
            // text was withheld still carries a signature, so reasoning did
            // happen. Dropping it would remove the record from the timeline and
            // break the lineage chain that runs through it.
            Some("text") => Some(ContentBlock::Text(
                string(block, "text").unwrap_or_default().to_owned(),
            )),
            Some("thinking") => Some(ContentBlock::Thinking(
                string(block, "thinking").unwrap_or_default().to_owned(),
            )),
            Some("image") => Some(ContentBlock::Image),
            Some("tool_use" | "server_tool_use" | "mcp_tool_use") => {
                let input = block.get("input");
                let name = string(block, "name").unwrap_or("tool").to_owned();
                Some(ContentBlock::ToolUse {
                    call_id: string(block, "id").unwrap_or_default().to_owned(),
                    cmd: command_language(&name)
                        .and(input)
                        .and_then(|input| string(input, "command"))
                        .map(str::to_owned),
                    cmd_lang: command_language(&name),
                    name,
                    arguments: input.map(ToString::to_string).unwrap_or_default(),
                    delegation_text: input
                        .and_then(|input| string(input, "prompt"))
                        .map(str::to_owned),
                })
            }
            Some("tool_result") => Some(ContentBlock::ToolResult {
                call_id: string(block, "tool_use_id").unwrap_or_default().to_owned(),
                text: block_result_text(block).or_else(|| virtual_tool_output_text(value, blocks)),
                is_error: block.get("is_error").and_then(Value::as_bool),
            }),
            Some(_) => Some(ContentBlock::Other),
            None => None,
        })
        .collect()
}

/// Code Mode's virtual companion result can leave message content empty while
/// retaining the executed tool's output in the enclosing result object. Only a
/// single result block can unambiguously own those fields.
fn virtual_tool_output_text(value: &Value, blocks: &[Value]) -> Option<String> {
    if value.get("isVirtual").and_then(Value::as_bool) != Some(true)
        || value.get("turnCompanion").and_then(Value::as_bool) != Some(true)
        || blocks.len() != 1
    {
        return None;
    }
    let result = value.get("toolUseResult")?;
    let text = ["stdout", "stderr"]
        .iter()
        .filter_map(|key| string(result, key).filter(|text| !text.is_empty()))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// A tool result's payload is either a string or the same block list shape.
fn block_result_text(block: &Value) -> Option<String> {
    let content = block.get("content")?;
    if let Some(text) = content.as_str() {
        return (!text.is_empty()).then(|| text.to_owned());
    }
    let text = content
        .as_array()?
        .iter()
        .filter_map(|inner| match string(inner, "type") {
            Some("text") => string(inner, "text"),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

/// Runtime-native output facts. Claude Code reports no exit code, so the
/// column stays null rather than being inferred from output text.
pub(super) fn tool_output_facts(value: &Value, is_error: Option<bool>) -> ToolOutputFacts {
    let result = value.get("toolUseResult");
    let duration_ms = result.and_then(|result| {
        result
            .get("totalDurationMs")
            .or_else(|| result.get("durationMs"))
            .and_then(Value::as_u64)
    });
    let http_failed = result
        .and_then(|result| result.get("code"))
        .and_then(Value::as_u64)
        .filter(|code| *code >= 400)
        .map(|_| true);
    ToolOutputFacts {
        exit_code: None,
        nonzero_exit: http_failed.or(is_error),
        duration_ms,
        output_tokens: result
            .and_then(|result| result.get("totalTokens"))
            .and_then(Value::as_u64),
        truncated: result
            .and_then(|result| result.get("truncated"))
            .and_then(Value::as_bool),
    }
}

pub(super) fn semantic_role_requires_loop(role: &str) -> bool {
    matches!(
        role,
        semantic::HUMAN_REQUEST
            | semantic::HUMAN_STEERING
            | semantic::AGENT_COMMENTARY
            | semantic::AGENT_FINAL_ANSWER
    )
}

/// Maps an attachment to what the runtime injects it for.
///
/// Each arm is taken from the distribution's own construction site rather than
/// from the name, which is not always a safe guide: `file_history_snapshot`
/// records an edit while `compact_file_reference` carries the same fields to
/// re-supply content that compaction dropped.
pub(super) fn attachment_role(attachment_kind: &str) -> &'static str {
    match attachment_kind {
        // The last two are `{banner, toolUseID}` after a read was cut short,
        // and `{maxTurns, turnCount}` when the loop hit its cap.
        "task_reminder"
        | "silent_turn_reminder"
        | "elapsed_time_reminder"
        | "date_change"
        | "goal_status"
        | "read_truncation_notice"
        | "max_turns_reached" => semantic::RUNTIME_NOTICE,
        "total_tokens_reminder" => semantic::RUNTIME_BUDGET,
        "model" => semantic::RUNTIME_INTERNAL_CONTEXT,
        "queued_command" => semantic::RUNTIME_SLASH_COMMAND,
        "edited_text_file" | "file_history_snapshot" => semantic::RUNTIME_FILE_CHANGE,
        // `{filename, content: {file: {filePath, content, numLines, …}}}`, and
        // the plan and post-compaction variants of the same thing.
        "file" | "compact_file_reference" | "plan_file_reference" => semantic::RUNTIME_FILE_CONTEXT,
        // A hook's own output, or the runtime reporting one did not complete.
        // `hook_blocking_error` is the third member the distribution builds
        // alongside these two; no Source here has produced one.
        "hook_system_message" | "hook_success" | "hook_cancelled" | "hook_blocking_error" => {
            semantic::RUNTIME_HOOK_OUTPUT
        }
        // `{allowedTools, model}`, emitted with a slash command's message set.
        "command_permissions" => semantic::RUNTIME_PERMISSIONS,
        // `{skills: [{name, path, content}]}` — the invoked skill's body, not
        // the catalog, which is `skill_listing`.
        "invoked_skills" => semantic::RUNTIME_SKILL_INSTRUCTIONS,
        // Which operating mode the model is in. These sit alongside the
        // `mode` and `permission-mode` Records, which classify the same way.
        "plan_mode"
        | "plan_mode_exit"
        | "auto_mode"
        | "auto_mode_exit"
        | "workflow_keyword_request"
        | "ultrathink_effort"
        | "ultra_effort_enter" => semantic::RUNTIME_STATE,
        "skill_listing"
        | "agent_listing_delta"
        | "deferred_tools_delta"
        | "mcp_instructions_delta" => semantic::RUNTIME_TOOL_CATALOG,
        _ => semantic::RUNTIME_UNKNOWN,
    }
}

/// Selects actual text carried by an attachment.
///
/// Claude Code uses attachments for both model-visible text and structured
/// bookkeeping. Only the former is a Text-valued Semantic fact. The ordered
/// paths below are all shapes observed in real Sources; everything else stays
/// in its physical Record until a typed query need exists.
pub(super) fn attachment_text(
    attachment: &Value,
    attachment_kind: &str,
    semantic_role: &str,
) -> Option<String> {
    for key in ["text", "prompt", "snippet", "reason", "message"] {
        if let Some(text) = string(attachment, key).filter(|text| !text.is_empty()) {
            return Some(text.to_owned());
        }
    }
    if let Some(text) = attachment
        .get("content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_owned());
    }
    if let Some(text) = attachment
        .pointer("/content/file/content")
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
    {
        return Some(text.to_owned());
    }
    if attachment_kind == "invoked_skills" {
        let text = attachment
            .get("skills")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|skill| string(skill, "content"))
            .filter(|content| !content.is_empty())
            .collect::<Vec<_>>()
            .join("\n\n");
        if !text.is_empty() {
            return Some(text);
        }
    }
    let catalog_text = match attachment_kind {
        "agent_listing_delta" | "deferred_tools_delta" => {
            joined_nonempty_strings(attachment.get("addedLines"), "\n")
        }
        "mcp_instructions_delta" => joined_nonempty_strings(attachment.get("addedBlocks"), "\n\n"),
        _ => None,
    };
    if catalog_text.is_some() {
        return catalog_text;
    }
    for key in ["displayPath", "filename"] {
        if let Some(text) = string(attachment, key).filter(|text| !text.is_empty()) {
            return Some(text.to_owned());
        }
    }
    (semantic_role == semantic::RUNTIME_STATE).then(|| attachment_kind.to_owned())
}

/// Reads the message delivered from Claude Code's input queue. The attachment
/// itself says that the author is human; Loop position decides whether the
/// message is the request or later steering.
pub(super) fn queued_human_prompt(attachment: &Value) -> Option<(String, bool)> {
    match attachment.get("prompt")? {
        Value::String(text) => (!text.is_empty()).then(|| (text.to_owned(), false)),
        Value::Array(blocks) => {
            let text = blocks
                .iter()
                .filter(|block| string(block, "type") == Some("text"))
                .filter_map(|block| string(block, "text"))
                .filter(|text| !text.is_empty())
                .collect::<Vec<_>>()
                .join("\n\n");
            let has_images = blocks
                .iter()
                .any(|block| string(block, "type") == Some("image"));
            (!text.is_empty() || has_images).then_some((text, has_images))
        }
        _ => None,
    }
}

fn joined_nonempty_strings(value: Option<&Value>, separator: &str) -> Option<String> {
    let text = value?
        .as_array()?
        .iter()
        .filter_map(Value::as_str)
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join(separator);
    (!text.is_empty()).then_some(text)
}

/// Runtime-provided identity and parent pointer. A compaction record has no parent of
/// its own and names its logical predecessor instead, so a walk over this
/// column crosses compaction boundaries unaided.
pub(super) fn string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

pub(super) fn turn_position(value: &Value) -> Option<TurnPosition> {
    let position = value.get("turnPosition")?;
    Some(TurnPosition {
        prompt_index: position.get("promptIndex")?.as_u64()?,
        turn_index: position.get("turnIndex")?.as_u64()?,
    })
}

// Lifecycle behavior is tested at the public relation boundary in `indexer`.
#[cfg(test)]
mod virtual_output_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn structured_output_fallback_requires_an_unambiguous_virtual_companion() {
        let block = json!({"type":"tool_result","tool_use_id":"call","content":"explicit"});
        let mut value = json!({"isVirtual":true,"turnCompanion":true,"toolUseResult":{"stdout":"fallback"},"message":{"content":[block.clone()]}});
        assert!(
            matches!(&content_blocks(&value)[0], ContentBlock::ToolResult {text:Some(text), ..} if text == "explicit")
        );
        value["message"]["content"][0]["content"] = json!("");
        assert!(
            matches!(&content_blocks(&value)[0], ContentBlock::ToolResult {text:Some(text), ..} if text == "fallback")
        );
        value["isVirtual"] = json!(false);
        assert!(matches!(
            &content_blocks(&value)[0],
            ContentBlock::ToolResult { text: None, .. }
        ));
        value["isVirtual"] = json!(true);
        value["message"]["content"] = json!([value["message"]["content"][0].clone(), block]);
        assert!(matches!(
            &content_blocks(&value)[0],
            ContentBlock::ToolResult { text: None, .. }
        ));
    }
}
