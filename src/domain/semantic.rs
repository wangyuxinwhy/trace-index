//! Runtime-independent meanings published for queryable Items.
//!
//! A Semantic value says what an Item means without copying a Runtime's record
//! shape into the public contract. `SemanticRole` selects exactly one
//! `SemanticValue` shape, and `EvidenceStrength` tells the reader whether that
//! interpretation follows explicit trace structure or a weaker convention.
//!
//! This module owns the domain shapes. The public SQL interface may replace a
//! top-level [`TextContent`] with a Blob reference to avoid repeating large
//! strings; that is a storage and query encoding, not a different Semantic model.

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value as RuntimeValue;

use super::{ItemId, SessionId, ShellFragment, TextContent};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
/// Runtime-independent category used to select an Item's typed Semantic value.
pub(crate) enum SemanticRole {
    #[serde(rename = "human.request")]
    HumanRequest,
    #[serde(rename = "human.steering")]
    HumanSteering,
    #[serde(rename = "agent.commentary")]
    AgentCommentary,
    #[serde(rename = "agent.final_answer")]
    AgentFinalAnswer,
    #[serde(rename = "agent.reasoning")]
    AgentReasoning,
    #[serde(rename = "agent.tool_call")]
    AgentToolCall,
    #[serde(rename = "agent.tool_call.shell")]
    AgentToolCallShell,
    #[serde(rename = "agent.delegation")]
    AgentDelegation,
    #[serde(rename = "tool.output")]
    ToolOutput,
    #[serde(rename = "subagent.activity")]
    SubagentActivity,
    #[serde(rename = "subagent.report")]
    SubagentReport,
    #[serde(rename = "runtime.instructions")]
    RuntimeInstructions,
    #[serde(rename = "runtime.context")]
    RuntimeContext,
    #[serde(rename = "runtime.state")]
    RuntimeState,
    #[serde(rename = "runtime.notice")]
    RuntimeNotice,
    #[serde(rename = "runtime.compaction_summary")]
    RuntimeCompactionSummary,
    #[serde(rename = "runtime.unknown")]
    RuntimeUnknown,
}

pub(crate) const SEMANTIC_ROLES: &[SemanticRole] = &[
    SemanticRole::HumanRequest,
    SemanticRole::HumanSteering,
    SemanticRole::AgentCommentary,
    SemanticRole::AgentFinalAnswer,
    SemanticRole::AgentReasoning,
    SemanticRole::AgentToolCall,
    SemanticRole::AgentToolCallShell,
    SemanticRole::AgentDelegation,
    SemanticRole::ToolOutput,
    SemanticRole::SubagentActivity,
    SemanticRole::SubagentReport,
    SemanticRole::RuntimeInstructions,
    SemanticRole::RuntimeContext,
    SemanticRole::RuntimeState,
    SemanticRole::RuntimeNotice,
    SemanticRole::RuntimeCompactionSummary,
    SemanticRole::RuntimeUnknown,
];

pub(crate) fn describe_semantic_roles() -> String {
    format!(
        "One of: {}.",
        SEMANTIC_ROLES
            .iter()
            .map(|role| role.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    )
}

impl SemanticRole {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::HumanRequest => "human.request",
            Self::HumanSteering => "human.steering",
            Self::AgentCommentary => "agent.commentary",
            Self::AgentFinalAnswer => "agent.final_answer",
            Self::AgentReasoning => "agent.reasoning",
            Self::AgentToolCall => "agent.tool_call",
            Self::AgentToolCallShell => "agent.tool_call.shell",
            Self::AgentDelegation => "agent.delegation",
            Self::ToolOutput => "tool.output",
            Self::SubagentActivity => "subagent.activity",
            Self::SubagentReport => "subagent.report",
            Self::RuntimeInstructions => "runtime.instructions",
            Self::RuntimeContext => "runtime.context",
            Self::RuntimeState => "runtime.state",
            Self::RuntimeNotice => "runtime.notice",
            Self::RuntimeCompactionSummary => "runtime.compaction_summary",
            Self::RuntimeUnknown => "runtime.unknown",
        }
    }

    /// Short stable meaning used by every Agent-facing discovery surface.
    #[cfg(test)]
    pub(crate) const fn meaning(self) -> &'static str {
        match self {
            Self::HumanRequest => "Human input that opens a new Loop.",
            Self::HumanSteering => "Human input delivered while the current Loop is still running.",
            Self::AgentCommentary => {
                "Agent progress or intermediate communication emitted during a Loop."
            }
            Self::AgentFinalAnswer => "The Agent response marked as final for a Loop.",
            Self::AgentReasoning => {
                "Reasoning exposed by the Runtime as full text, a summary, or unavailable."
            }
            Self::AgentToolCall => "A Runtime tool invocation with Agent-authored arguments.",
            Self::AgentToolCallShell => {
                "A tool invocation whose command-bearing arguments also have parsed shell structure."
            }
            Self::AgentDelegation => {
                "Work the Agent sends to a child Agent, optionally linked to its Session."
            }
            Self::ToolOutput => {
                "The result returned by a tool, optionally linked to the calling Item."
            }
            Self::SubagentActivity => "Intermediate activity associated with a child Session.",
            Self::SubagentReport => "A result returned from a child Session to its parent Agent.",
            Self::RuntimeInstructions => {
                "Instructions injected by the Runtime or harness, not Human input."
            }
            Self::RuntimeContext => {
                "Environment, memory, file, or Session context supplied by the Runtime, not Human input."
            }
            Self::RuntimeState => {
                "Runtime-owned state or a state change placed on the Session timeline."
            }
            Self::RuntimeNotice => {
                "A Runtime-owned informational or control event placed on the Session timeline."
            }
            Self::RuntimeCompactionSummary => {
                "A Runtime-produced summary that replaces earlier context."
            }
            Self::RuntimeUnknown => {
                "Meaningful bounded Runtime content whose more specific stable role is not yet known."
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// How strongly the physical trace supports the published interpretation.
pub(crate) enum EvidenceStrength {
    /// The role follows an explicit Runtime event type or structural boundary.
    Structural,
    /// The role depends on a weaker Runtime convention and may need qualification.
    Heuristic,
}

impl EvidenceStrength {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Structural => "structural",
            Self::Heuristic => "heuristic",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Text {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    pub(crate) has_images: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ReasoningRepresentation {
    Full,
    Summary,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Reasoning {
    pub(crate) representation: ReasoningRepresentation,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// A generic tool invocation whose arguments remain in their Runtime-authored shape.
pub(crate) struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) tool_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) arguments: Option<RuntimeValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) working_directory: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// A tool call with additional shell structure when the arguments contain shell text.
///
/// This is a more specific `ToolCall` value, not a separate Item or a derived
/// relation. `shell_fragments` preserves the command text and the structure that
/// Trace Index can establish from it.
pub(crate) struct ShellToolCall {
    #[serde(flatten)]
    pub(crate) tool_call: ToolCall,
    pub(crate) shell_fragments: Vec<ShellFragment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Observable result returned for a tool call.
pub(crate) struct ToolOutput {
    /// Item that issued the call, when the trace exposes a reliable pairing key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) call_item_id: Option<ItemId>,
    /// Bounded textual output published by Trace Index.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    /// Process-style exit code when the Runtime reports one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) exit_code: Option<i64>,
    /// Runtime-reported failure state when it is distinct from an exit code.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) failed: Option<bool>,
    /// Elapsed tool execution time reported or structurally measured, in milliseconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) duration_ms: Option<u64>,
    /// Whether the Runtime says the original output was truncated before indexing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_truncated: Option<bool>,
    /// Runtime-reported output-token count, whose tokenizer and scope remain Runtime-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Delegation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    pub(crate) has_images: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) child_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentActivity {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    pub(crate) has_images: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) subagent_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SubagentReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    pub(crate) has_images: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Compaction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<TextContent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum InstructionCategory {
    Project,
    User,
    Skill,
    Permission,
    Collaboration,
    Plugin,
    ToolCatalog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Instruction {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<InstructionCategory>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ContextCategory {
    Environment,
    Memory,
    File,
    SessionReference,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Context {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) text: Option<TextContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) category: Option<ContextCategory>,
    pub(crate) has_images: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
/// Closed set of value shapes selected by [`SemanticRole`].
pub(crate) enum SemanticValue {
    Text(Text),
    Reasoning(Reasoning),
    ToolCall(ToolCall),
    ShellToolCall(ShellToolCall),
    ToolOutput(ToolOutput),
    Delegation(Delegation),
    SubagentActivity(SubagentActivity),
    SubagentReport(SubagentReport),
    Compaction(Compaction),
    Instruction(Instruction),
    Context(Context),
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
/// Trace Index's queryable interpretation of one Item.
pub(crate) struct Semantic {
    /// Selects the contract of `value`.
    pub(crate) role: SemanticRole,
    /// Typed value whose variant must agree with `role`.
    pub(crate) value: SemanticValue,
    /// Strength of the trace evidence supporting this interpretation.
    pub(crate) evidence_strength: EvidenceStrength,
}

pub(crate) fn validate_semantic(semantic: &Semantic) -> Result<()> {
    use SemanticRole as R;
    use SemanticValue as V;
    let valid = matches!(
        (semantic.role, &semantic.value),
        (R::AgentReasoning, V::Reasoning(_))
            | (R::AgentToolCall, V::ToolCall(_))
            | (R::AgentToolCallShell, V::ShellToolCall(_))
            | (R::ToolOutput, V::ToolOutput(_))
            | (R::AgentDelegation, V::Delegation(_))
            | (R::SubagentActivity, V::SubagentActivity(_))
            | (R::SubagentReport, V::SubagentReport(_))
            | (R::RuntimeCompactionSummary, V::Compaction(_))
            | (R::RuntimeInstructions, V::Instruction(_))
            | (R::RuntimeContext, V::Context(_))
            | (
                R::HumanRequest
                    | R::HumanSteering
                    | R::AgentCommentary
                    | R::AgentFinalAnswer
                    | R::RuntimeState
                    | R::RuntimeNotice
                    | R::RuntimeUnknown,
                V::Text(_)
            )
    );
    ensure!(valid, "semantic role and value type do not match");

    if let V::Reasoning(reasoning) = &semantic.value {
        match reasoning.representation {
            ReasoningRepresentation::Full | ReasoningRepresentation::Summary => ensure!(
                reasoning
                    .text
                    .as_ref()
                    .is_some_and(|text| !text.value.is_empty()),
                "full or summary reasoning requires text"
            ),
            ReasoningRepresentation::Unavailable => ensure!(
                reasoning.text.is_none(),
                "unavailable reasoning cannot contain text"
            ),
        }
    }
    if let V::ShellToolCall(call) = &semantic.value {
        ensure!(
            !call.shell_fragments.is_empty(),
            "ShellToolCall requires at least one fragment"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_a_role_value_mismatch() {
        let semantic = Semantic {
            role: SemanticRole::HumanRequest,
            value: SemanticValue::ToolCall(ToolCall {
                tool_name: None,
                arguments: None,
                working_directory: None,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };
        assert!(validate_semantic(&semantic).is_err());
    }

    #[test]
    fn omits_absent_optional_semantic_members() {
        let semantic = Semantic {
            role: SemanticRole::ToolOutput,
            value: SemanticValue::ToolOutput(ToolOutput {
                call_item_id: None,
                text: None,
                exit_code: None,
                failed: None,
                duration_ms: None,
                runtime_truncated: None,
                runtime_output_tokens: None,
            }),
            evidence_strength: EvidenceStrength::Structural,
        };
        assert_eq!(
            serde_json::to_value(semantic).expect("serialize Semantic"),
            serde_json::json!({
                "role": "tool.output",
                "value": {},
                "evidence_strength": "structural"
            })
        );
    }
}
