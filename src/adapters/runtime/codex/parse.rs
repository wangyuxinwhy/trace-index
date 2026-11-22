//! Stateless Codex wire-format parsers, classifiers, and tool-output extractors.

use serde_json::Value;

use super::PendingToolEnd;
use crate::adapters::projection::{self, BoundedText, ItemDetail, ItemProjection, ToolOutputFacts};
use crate::adapters::semantic::{self, basis};

/// Shortest text allowed to pair by containment rather than equality. Short
/// confirmations such as "可以" would otherwise cross-match.
const MIN_CONTAINMENT_BYTES: usize = 8;

/// Normalizes a Codex counter whose input already includes cache tokens.
pub(super) fn codex_usage(usage: &Value) -> Option<crate::domain::Usage> {
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
}

pub(super) fn command_execution_facts(item: &Value) -> ToolOutputFacts {
    let exit_code = item.get("exit_code").and_then(Value::as_i64);
    let persistence_truncated = string_at(item, "aggregated_output").is_some_and(|output| {
        output.contains("\n... command output truncated for persistence ...\n")
    });
    ToolOutputFacts {
        exit_code,
        nonzero_exit: exit_code
            .map(|code| code != 0)
            .or_else(|| match string_at(item, "status") {
                Some("failed" | "declined") => Some(true),
                Some("completed") => Some(false),
                _ => None,
            }),
        duration_ms: structured_duration_ms(item.get("duration")),
        truncated: persistence_truncated.then_some(true),
        ..ToolOutputFacts::default()
    }
}

/// Only recognized shell `-c` argv contains shell source. Other executables'
/// arguments and Codex's parsed-command summaries are not shell scripts.
pub(super) fn execution_shell_text(item: &Value) -> Option<String> {
    let command = item.get("command")?.as_array()?;
    if command.len() < 3 {
        return None;
    }
    let executable = command[0].as_str()?.rsplit('/').next()?;
    if !matches!(executable, "sh" | "bash" | "zsh" | "dash" | "ksh")
        || !matches!(command[1].as_str()?, "-c" | "-lc" | "-cl")
    {
        return None;
    }
    command[2].as_str().map(str::to_owned)
}

/// Native `CommandExecution.cwd` is a `PathUri`. Only local file URIs or legacy
/// absolute paths can establish a working directory on this machine.
pub(super) fn execution_workdir(value: &str) -> Option<String> {
    if !value.starts_with("file:") {
        return value.starts_with('/').then(|| value.to_owned());
    }
    let path = value
        .strip_prefix("file://localhost")
        .or_else(|| value.strip_prefix("file://"))?;
    if !path.starts_with('/') {
        return None;
    }
    let mut bytes = Vec::new();
    let mut input = path.as_bytes().iter().copied();
    while let Some(byte) = input.next() {
        if byte == b'%' {
            let hi = char::from(input.next()?).to_digit(16)?;
            let lo = char::from(input.next()?).to_digit(16)?;
            bytes.push(u8::try_from(hi * 16 + lo).ok()?);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

/// Paired UI evidence determines Human authorship; markers only subdivide
/// Runtime injections. Preserve both authorship and classification evidence.
pub(super) fn classify_message(
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

pub(super) fn guardian_message(
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

/// Codex writes the same message twice with small differences: the UI text
/// omits the image wrapper blocks the context text carries. Equality is tried
/// first; containment is allowed only for text long enough not to collide.
pub(super) fn texts_match(ui_text: &str, context_text: &str) -> bool {
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

pub(super) fn message_text(payload: &Value) -> Option<String> {
    let blocks = payload.get("content")?.as_array()?;
    let text = blocks
        .iter()
        .filter_map(|block| block.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!text.is_empty()).then_some(text)
}

pub(super) fn message_has_images(payload: &Value) -> bool {
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

pub(super) fn provider_tool_name(call_kind: &str) -> Option<&'static str> {
    match call_kind {
        "web_search_call" => Some("web_search"),
        "image_generation_call" => Some("image_generation"),
        "tool_search_call" => Some("tool_search"),
        _ => None,
    }
}

/// Keeps the Runtime-authored argument shape instead of assuming every call
/// uses the Responses API's JSON-encoded string form.
pub(super) fn runtime_arguments(payload: &Value, call_kind: &str) -> Option<String> {
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

pub(super) fn collect_text<'a>(value: &'a Value, fragments: &mut Vec<&'a str>) {
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
pub(super) fn tool_output_facts(fragments: &[&str]) -> ToolOutputFacts {
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
pub(super) fn structured_tool_end_facts(payload: &Value) -> ToolOutputFacts {
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

pub(super) fn mcp_end_parts(payload: &Value) -> (Option<String>, Option<String>, Option<String>) {
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

pub(super) fn merge_tool_end(item: &mut ItemProjection, ended: &PendingToolEnd) {
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

pub(super) fn merge_spawn_end(
    item: &mut ItemProjection,
    end_seq: u64,
    native_id: String,
    max_text_bytes: usize,
) {
    item.seq = item.seq.min(end_seq);
    item.ui_seq = Some(end_seq);
    semantic::AGENT_DELEGATION.clone_into(&mut item.semantic_role);
    if item.preview.is_none()
        && let ItemDetail::ToolCall { cmd, args, .. } = &item.detail
    {
        item.preview = cmd
            .as_deref()
            .or(args.as_deref())
            .map(|text| BoundedText::bounded(text, max_text_bytes));
    }
    item.linked_session_native_id = Some(native_id);
    item.searchable = true;
}

pub(super) fn spawn_output_native_id(
    output: &str,
    current_session_native_id: Option<&str>,
) -> Option<String> {
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

pub(super) fn string_at<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    value.get(key).and_then(Value::as_str)
}

/// Whether a tool call can receive a separate output record.
///
/// The Responses API defines output items only for function, MCP, custom and
/// tool-search calls. A web search or an image generation is executed by the
/// provider and carries its own status and result on the call item, so waiting
/// for an output that the protocol never emits reported 22 of 26 apparently
/// unpaired calls in a month of rollouts.
pub(super) fn awaits_output(call_kind: &str) -> bool {
    !matches!(call_kind, "web_search_call" | "image_generation_call")
}

/// The readable part of an agent message. The payload itself may be encrypted,
/// in which case only the header survives.
pub(super) fn agent_message_text(payload: &Value) -> String {
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
pub(super) fn message_type(text: &str) -> Option<&str> {
    text.strip_prefix("Message Type: ")
        .map(|rest| rest.split('\n').next().unwrap_or(rest).trim())
        .filter(|value| !value.is_empty())
}

/// Reads the native child Session id from the older structured notification
/// wrapper. Only a role already identified as a report is eligible; arbitrary
/// conversation JSON must never create a Session link.
pub(super) fn linked_session_from_tagged_text(role: &str, text: Option<&str>) -> Option<String> {
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
/// is a string inside a `tools.exec_command({cmd})` call within it. Child
/// executions are established from native `CommandExecution` evidence, never
/// by interpreting strings in the orchestration script.
pub(super) fn shell_command(tool_name: Option<&str>, arguments: Option<&Value>) -> Option<String> {
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
