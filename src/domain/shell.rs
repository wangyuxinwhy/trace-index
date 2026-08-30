//! Shell structure nested inside `Semantic(role = agent.tool_call.shell)`.
//!
//! A Shell Fragment remains part of one Item's Semantic value. Statements,
//! invocations, redirects, and pipelines make common command questions easier
//! to answer; they are not independent domain objects or public Relations.

use serde::{Deserialize, Serialize};

use super::{ByteRange, TextContent};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// One executable name and the arguments passed to it.
pub(crate) struct Invocation {
    pub(crate) program: String,
    pub(crate) argv: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// One syntactic redirection within a shell statement.
pub(crate) struct Redirect {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) source_fd: Option<String>,
    pub(crate) operator: String,
    pub(crate) target: String,
    pub(crate) range: ByteRange,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct PipelinePosition {
    pub(crate) id: u64,
    pub(crate) position: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// One parsed statement within the fragment's original text.
pub(crate) struct ShellStatement {
    pub(crate) range: ByteRange,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) parent_position: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) connector: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) pipeline: Option<PipelinePosition>,
    pub(crate) invocations: Vec<Invocation>,
    pub(crate) redirects: Vec<Redirect>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Whether the fragment contains a whole command or only a recoverable portion.
pub(crate) enum Completeness {
    Complete,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
/// Shell text plus the structure that can be established from that text.
pub(crate) struct ShellFragment {
    pub(crate) text: TextContent,
    pub(crate) completeness: Completeness,
    pub(crate) statements: Vec<ShellStatement>,
}
