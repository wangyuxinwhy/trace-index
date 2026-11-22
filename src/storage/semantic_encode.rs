//! Pure domain-to-SQL `Semantic<BlobRef>` encoding.
//!
//! The public domain model carries inline `TextContent`; persistence stores
//! bounded text in `content_blobs` and serializes `{ "blob_id": ... }` directly
//! inside `Semantic<BlobRef>` without constructing an intermediate JSON DOM.

use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;

use crate::adapters::projection::{BoundedText, ItemDetail, ItemProjection, SyntaxProjection};
use crate::adapters::semantic;
use crate::domain::{
    ByteRange, Compaction, Completeness, Context as SemanticContext, ContextCategory, Delegation,
    EvidenceStrength, Instruction, InstructionCategory, Invocation, ItemId, PipelinePosition,
    Reasoning, Redirect, SemanticRole, SemanticValue, SessionId, ShellFragment, ShellStatement,
    ShellToolCall, SubagentActivity, SubagentReport, Text, ToolCall, ToolOutput,
};

/// Public SQL access encoding for top-level bounded Semantic text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub(super) struct BlobRef {
    pub(super) blob_id: i64,
}

pub(super) fn session_link_member(role: SemanticRole) -> Result<&'static str> {
    match role {
        SemanticRole::AgentDelegation => Ok("child_session_id"),
        SemanticRole::SubagentActivity => Ok("subagent_session_id"),
        SemanticRole::SubagentReport => Ok("source_session_id"),
        _ => {
            bail!("only delegation, subagent activity, and subagent report may reference a Session")
        }
    }
}

/// Selects the Adapter text that backs the public Semantic `TextContent` and
/// optional full-text search entry without cloning the bounded string.
///
/// A Tool Output keeps its text in the typed detail rather than the general
/// Item preview and is not indexed into conversation search. Every other
/// text-bearing Semantic value is built from `preview`. Tool arguments and
/// parsed shell structure remain ordinary typed Semantic data; Blob is not a
/// generic JSON externalization mechanism.
pub(super) fn item_text_sources(
    item: &ItemProjection,
    role: SemanticRole,
) -> (Option<&BoundedText>, Option<&str>) {
    match role {
        SemanticRole::AgentToolCall | SemanticRole::AgentToolCallShell => (None, None),
        SemanticRole::ToolOutput => {
            let ItemDetail::ToolOutput { output, .. } = &item.detail else {
                unreachable!("tool output role must carry tool output detail")
            };
            let blob = output.as_ref().filter(|content| content.text.is_some());
            (blob, None)
        }
        _ => {
            let blob = item
                .preview
                .as_ref()
                .filter(|content| content.text.is_some());
            let searchable_text = blob.and_then(|content| content.text.as_deref());
            (blob, searchable_text)
        }
    }
}

pub(super) fn stored_semantic_value(
    item: &ItemProjection,
    role: SemanticRole,
    blob: Option<BlobRef>,
    target_session_id: Option<SessionId>,
    call_item_id: Option<ItemId>,
) -> Result<SemanticValue<BlobRef>> {
    let has_images = matches!(
        &item.detail,
        ItemDetail::Message {
            has_images: true,
            ..
        }
    );
    let value = match role {
        SemanticRole::AgentReasoning => {
            let ItemDetail::Reasoning { representation } = &item.detail else {
                unreachable!("reasoning role must carry reasoning detail")
            };
            SemanticValue::Reasoning(Reasoning {
                representation: *representation,
                text: blob,
            })
        }
        SemanticRole::AgentToolCall | SemanticRole::AgentToolCallShell => {
            let ItemDetail::ToolCall {
                name,
                workdir,
                args,
                syntax,
                ..
            } = &item.detail
            else {
                unreachable!()
            };
            let tool_call = ToolCall {
                tool_name: name.clone(),
                arguments: args.as_deref().map(semantic_arguments),
                working_directory: workdir.clone(),
            };
            if role == SemanticRole::AgentToolCallShell {
                SemanticValue::ShellToolCall(ShellToolCall {
                    tool_call,
                    shell_fragments: shell_fragments(syntax.as_ref())?,
                })
            } else {
                SemanticValue::ToolCall(tool_call)
            }
        }
        SemanticRole::ToolOutput => {
            let ItemDetail::ToolOutput { facts, .. } = &item.detail else {
                unreachable!()
            };
            SemanticValue::ToolOutput(ToolOutput {
                call_item_id,
                text: blob,
                exit_code: facts.exit_code,
                failed: facts.nonzero_exit,
                duration_ms: facts.duration_ms,
                runtime_truncated: facts.truncated,
                runtime_output_tokens: facts.output_tokens,
            })
        }
        SemanticRole::AgentDelegation => SemanticValue::Delegation(Delegation {
            text: blob,
            has_images,
            child_session_id: target_session_id,
        }),
        SemanticRole::SubagentActivity => SemanticValue::SubagentActivity(SubagentActivity {
            text: blob,
            has_images,
            subagent_session_id: target_session_id,
        }),
        SemanticRole::SubagentReport => SemanticValue::SubagentReport(SubagentReport {
            text: blob,
            has_images,
            source_session_id: target_session_id,
        }),
        SemanticRole::RuntimeCompactionSummary => {
            SemanticValue::Compaction(Compaction { summary: blob })
        }
        SemanticRole::RuntimeInstructions => SemanticValue::Instruction(Instruction {
            text: blob,
            category: instruction_category(&item.semantic_role),
        }),
        SemanticRole::RuntimeContext => SemanticValue::Context(SemanticContext {
            text: blob,
            category: context_category(&item.semantic_role),
            has_images,
        }),
        _ => SemanticValue::Text(Text {
            text: blob,
            has_images,
        }),
    };
    Ok(value)
}

/// Publishes complete structured arguments as JSON and complete scalar tool
/// input as a JSON string. Some Runtime tools, notably Codex web search, record
/// a plain string rather than a JSON object. Oversized arguments are already
/// omitted before reaching persistence; the supporting Record remains the
/// recovery path in that case.
fn semantic_arguments(text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|_| serde_json::Value::String(text.to_owned()))
}

pub(super) fn normalized_role(role: &str, detail: &ItemDetail) -> Result<SemanticRole> {
    let role = match role {
        semantic::HUMAN_REQUEST => SemanticRole::HumanRequest,
        semantic::HUMAN_STEERING => SemanticRole::HumanSteering,
        semantic::AGENT_COMMENTARY => SemanticRole::AgentCommentary,
        semantic::AGENT_FINAL_ANSWER => SemanticRole::AgentFinalAnswer,
        semantic::AGENT_REASONING => SemanticRole::AgentReasoning,
        semantic::AGENT_TOOL_CALL
            if matches!(
                detail,
                ItemDetail::ToolCall {
                    syntax: Some(_),
                    ..
                }
            ) =>
        {
            SemanticRole::AgentToolCallShell
        }
        semantic::AGENT_TOOL_CALL => SemanticRole::AgentToolCall,
        semantic::AGENT_DELEGATION => SemanticRole::AgentDelegation,
        semantic::TOOL_OUTPUT => SemanticRole::ToolOutput,
        semantic::SUBAGENT_ACTIVITY => SemanticRole::SubagentActivity,
        semantic::SUBAGENT_REPORT => SemanticRole::SubagentReport,
        semantic::RUNTIME_COMPACTION => SemanticRole::RuntimeCompactionSummary,
        semantic::RUNTIME_NOTICE
        | semantic::RUNTIME_ABORT_NOTICE
        | semantic::RUNTIME_HOOK_OUTPUT => SemanticRole::RuntimeNotice,
        semantic::RUNTIME_STATE | semantic::RUNTIME_BUDGET | semantic::RUNTIME_FILE_CHANGE => {
            SemanticRole::RuntimeState
        }
        semantic::RUNTIME_INSTRUCTIONS
        | semantic::RUNTIME_PROJECT_INSTRUCTIONS
        | semantic::RUNTIME_USER_INSTRUCTIONS
        | semantic::RUNTIME_SKILL_INSTRUCTIONS
        | semantic::RUNTIME_PERMISSIONS
        | semantic::RUNTIME_COLLAB_MODE
        | semantic::RUNTIME_PLUGINS
        | semantic::RUNTIME_APPS
        | semantic::RUNTIME_PERSONALITY
        | semantic::RUNTIME_TOOL_CATALOG => SemanticRole::RuntimeInstructions,
        semantic::RUNTIME_SESSION_REFERENCE
        | semantic::RUNTIME_MEMORY
        | semantic::RUNTIME_ENV_CONTEXT
        | semantic::RUNTIME_INTERNAL_CONTEXT
        | semantic::RUNTIME_SLASH_COMMAND
        | semantic::RUNTIME_BASH_COMMAND
        | semantic::RUNTIME_IMAGE_ATTACHMENT
        | semantic::RUNTIME_IDE_CONTEXT
        | semantic::RUNTIME_FILE_CONTEXT => SemanticRole::RuntimeContext,
        semantic::RUNTIME_UNKNOWN => SemanticRole::RuntimeUnknown,
        _ => bail!("Adapter emitted unmapped semantic role {role:?}"),
    };
    Ok(role)
}

fn instruction_category(role: &str) -> Option<InstructionCategory> {
    match role {
        semantic::RUNTIME_PROJECT_INSTRUCTIONS => Some(InstructionCategory::Project),
        semantic::RUNTIME_USER_INSTRUCTIONS => Some(InstructionCategory::User),
        semantic::RUNTIME_SKILL_INSTRUCTIONS => Some(InstructionCategory::Skill),
        semantic::RUNTIME_PERMISSIONS => Some(InstructionCategory::Permission),
        semantic::RUNTIME_COLLAB_MODE => Some(InstructionCategory::Collaboration),
        semantic::RUNTIME_PLUGINS => Some(InstructionCategory::Plugin),
        semantic::RUNTIME_TOOL_CATALOG => Some(InstructionCategory::ToolCatalog),
        _ => None,
    }
}

fn context_category(role: &str) -> Option<ContextCategory> {
    match role {
        semantic::RUNTIME_ENV_CONTEXT => Some(ContextCategory::Environment),
        semantic::RUNTIME_MEMORY => Some(ContextCategory::Memory),
        semantic::RUNTIME_FILE_CONTEXT => Some(ContextCategory::File),
        semantic::RUNTIME_SESSION_REFERENCE => Some(ContextCategory::SessionReference),
        semantic::RUNTIME_INTERNAL_CONTEXT
        | semantic::RUNTIME_SLASH_COMMAND
        | semantic::RUNTIME_BASH_COMMAND => Some(ContextCategory::Internal),
        _ => None,
    }
}

pub(super) fn evidence_strength(basis: &str) -> EvidenceStrength {
    if basis.split('+').any(semantic::basis_is_heuristic) {
        EvidenceStrength::Heuristic
    } else {
        EvidenceStrength::Structural
    }
}

fn shell_fragments(syntax: Option<&SyntaxProjection>) -> Result<Vec<ShellFragment>> {
    let Some(syntax) = syntax else {
        return Ok(Vec::new());
    };
    let mut fragments = Vec::new();
    for (fragment_index, fragment) in syntax.fragments.iter().enumerate() {
        let mut statements = Vec::new();
        let mut fragment_base = None;
        for (statement_index, statement) in syntax.statements.iter().enumerate() {
            if statement.fragment != fragment_index {
                continue;
            }
            let base = *fragment_base.get_or_insert(statement_index);
            let invocations = syntax
                .invocations
                .iter()
                .filter(|value| value.statement == statement_index)
                .map(|value| Invocation {
                    program: value.program.clone(),
                    argv: value.argv.clone(),
                })
                .collect();
            let redirects = syntax
                .redirects
                .iter()
                .filter(|value| value.statement == statement_index)
                .map(|value| Redirect {
                    source_fd: value.source_fd_raw.clone(),
                    operator: value.operator.clone(),
                    target: value.target_raw.clone(),
                    range: ByteRange {
                        start: u64::from(value.start_byte),
                        end: u64::from(value.end_byte),
                    },
                })
                .collect();
            statements.push(ShellStatement {
                range: ByteRange {
                    start: u64::from(statement.start_byte),
                    end: u64::from(statement.end_byte),
                },
                parent_position: statement
                    .parent
                    .and_then(|value| value.checked_sub(base))
                    .map(|value| u64::try_from(value).unwrap_or(u64::MAX)),
                connector: statement.shell.as_ref().and_then(|value| {
                    (value.connector != "first").then(|| value.connector.to_owned())
                }),
                pipeline: statement
                    .shell
                    .as_ref()
                    .and_then(|value| value.pipeline_id.zip(value.pipeline_pos))
                    .map(|(id, position)| PipelinePosition {
                        id: u64::from(id),
                        position: u64::from(position),
                    }),
                invocations,
                redirects,
            });
        }
        fragments.push(ShellFragment {
            text: fragment
                .content
                .content()
                .context("Shell Fragment must retain bounded text")?,
            completeness: if fragment.parse_status == "parsed" {
                Completeness::Complete
            } else {
                Completeness::Partial
            },
            statements,
        });
    }
    ensure!(
        !fragments.is_empty(),
        "declared Shell call produced no root fragment"
    );
    Ok(fragments)
}

#[cfg(test)]
mod tests {
    use anyhow::{Result, bail, ensure};

    use super::BlobRef;
    use crate::domain::{
        Compaction, Completeness, Context as SemanticContext, Delegation, EvidenceStrength,
        Instruction, Reasoning, Semantic, SemanticRole, SemanticValue, ShellFragment,
        ShellToolCall, SubagentActivity, SubagentReport, Text, TextContent, ToolCall, ToolOutput,
    };

    /// Converts a domain `Semantic<TextContent>` into its SQL `Semantic<BlobRef>`
    /// access encoding for contract-equivalence tests.
    fn stored_semantic(semantic: &Semantic, blob_id: Option<i64>) -> Result<serde_json::Value> {
        let blob_ref = |has_text: bool| -> Result<Option<BlobRef>> {
            match (has_text, blob_id) {
                (true, Some(blob_id)) => Ok(Some(BlobRef { blob_id })),
                (false, None) => Ok(None),
                (true, None) => bail!("text-bearing Semantic value was stored without a Blob"),
                (false, Some(_)) => bail!("text-bearing Semantic value was not externalized"),
            }
        };
        let value = match &semantic.value {
            SemanticValue::Text(value) => SemanticValue::Text(Text {
                text: blob_ref(value.text.is_some())?,
                has_images: value.has_images,
            }),
            SemanticValue::Reasoning(value) => SemanticValue::Reasoning(Reasoning {
                representation: value.representation,
                text: blob_ref(value.text.is_some())?,
            }),
            SemanticValue::ToolCall(value) => {
                ensure!(
                    blob_id.is_none(),
                    "text-bearing Semantic value has no text member"
                );
                SemanticValue::ToolCall(value.clone())
            }
            SemanticValue::ShellToolCall(value) => {
                ensure!(
                    blob_id.is_none(),
                    "text-bearing Semantic value has no text member"
                );
                SemanticValue::ShellToolCall(value.clone())
            }
            SemanticValue::ToolOutput(value) => SemanticValue::ToolOutput(ToolOutput {
                call_item_id: value.call_item_id,
                text: blob_ref(value.text.is_some())?,
                exit_code: value.exit_code,
                failed: value.failed,
                duration_ms: value.duration_ms,
                runtime_truncated: value.runtime_truncated,
                runtime_output_tokens: value.runtime_output_tokens,
            }),
            SemanticValue::Delegation(value) => SemanticValue::Delegation(Delegation {
                text: blob_ref(value.text.is_some())?,
                has_images: value.has_images,
                child_session_id: value.child_session_id,
            }),
            SemanticValue::SubagentActivity(value) => {
                SemanticValue::SubagentActivity(SubagentActivity {
                    text: blob_ref(value.text.is_some())?,
                    has_images: value.has_images,
                    subagent_session_id: value.subagent_session_id,
                })
            }
            SemanticValue::SubagentReport(value) => SemanticValue::SubagentReport(SubagentReport {
                text: blob_ref(value.text.is_some())?,
                has_images: value.has_images,
                source_session_id: value.source_session_id,
            }),
            SemanticValue::Compaction(value) => SemanticValue::Compaction(Compaction {
                summary: blob_ref(value.summary.is_some())?,
            }),
            SemanticValue::Instruction(value) => SemanticValue::Instruction(Instruction {
                text: blob_ref(value.text.is_some())?,
                category: value.category,
            }),
            SemanticValue::Context(value) => SemanticValue::Context(SemanticContext {
                text: blob_ref(value.text.is_some())?,
                category: value.category,
                has_images: value.has_images,
            }),
        };
        Ok(serde_json::to_value(Semantic {
            role: semantic.role,
            value,
            evidence_strength: semantic.evidence_strength,
        })?)
    }

    #[test]
    fn text_capable_value_without_text_does_not_require_a_blob() {
        let semantic = Semantic {
            role: SemanticRole::RuntimeNotice,
            value: SemanticValue::Text(Text {
                text: None,
                has_images: true,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, None).expect("store image-only Semantic");

        assert_eq!(stored["role"], "runtime.notice");
        assert_eq!(stored["value"]["has_images"], true);
        assert!(stored["value"].get("text").is_none());
    }

    #[test]
    fn tool_output_text_is_replaced_by_one_blob_reference() {
        let semantic = Semantic {
            role: SemanticRole::ToolOutput,
            value: SemanticValue::ToolOutput(ToolOutput {
                call_item_id: None,
                text: Some(TextContent {
                    value: "large output".to_owned(),
                    full_bytes: 12,
                    estimated_tokens: 3,
                }),
                exit_code: None,
                failed: None,
                duration_ms: None,
                runtime_truncated: None,
                runtime_output_tokens: None,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, Some(42)).expect("store ToolOutput Semantic");

        assert_eq!(
            stored["value"]["text"],
            serde_json::json!({ "blob_id": 42 })
        );
        assert!(!stored.to_string().contains("large output"));
    }

    #[test]
    fn shell_fragment_text_remains_inline() {
        let semantic = Semantic {
            role: SemanticRole::AgentToolCallShell,
            value: SemanticValue::ShellToolCall(ShellToolCall {
                tool_call: ToolCall {
                    tool_name: Some("exec_command".to_owned()),
                    arguments: None,
                    working_directory: None,
                },
                shell_fragments: vec![ShellFragment {
                    text: TextContent {
                        value: "echo hello".to_owned(),
                        full_bytes: 10,
                        estimated_tokens: 3,
                    },
                    completeness: Completeness::Complete,
                    statements: Vec::new(),
                }],
            }),
            evidence_strength: EvidenceStrength::Structural,
        };

        let stored = stored_semantic(&semantic, None).expect("store Shell Semantic");

        assert_eq!(
            stored["value"]["shell_fragments"][0]["text"]["value"],
            "echo hello"
        );
        assert!(
            stored["value"]["shell_fragments"][0]["text"]
                .get("blob_id")
                .is_none()
        );
    }
}
