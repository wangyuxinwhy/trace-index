//! Small value types shared by more than one domain object.
//!
//! These types describe the domain contract. A storage backend may normalize
//! sparse values such as Session relationships into separate tables without
//! changing the model presented here.

use serde::{Deserialize, Serialize};

macro_rules! id_type {
    ($name:ident) => {
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize,
        )]
        #[serde(transparent)]
        pub(crate) struct $name(pub(crate) i64);
    };
}

id_type!(SessionId);
id_type!(ItemId);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct ByteRange {
    /// Inclusive byte offset in the Source.
    pub(crate) start: u64,
    /// Exclusive byte offset in the Source.
    pub(crate) end: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum LoopOutcome {
    /// The Runtime reported normal completion.
    Completed,
    /// Execution stopped before normal completion.
    Interrupted,
    /// The Runtime reported failure.
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Model {
    /// Runtime-reported model identity.
    pub(crate) id: String,
    /// Runtime vocabulary for the configured reasoning intensity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) effort: Option<String>,
    /// Runtime-reported context capacity for this model execution.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) context_window: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Usage {
    /// All input tokens processed by the model, including cached input.
    pub(crate) input: u64,
    /// Cache-read tokens, already included in `input`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cached: Option<u64>,
    /// Cache-write tokens, already included in `input`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) cache_write: Option<u64>,
    /// All output tokens produced by the model, including reasoning tokens.
    pub(crate) output: u64,
    /// Reasoning tokens, already included in `output`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) reasoning: Option<u64>,
}
