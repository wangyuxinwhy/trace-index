//! Agent-facing CLI contract: configuration, documentation, queries, and output.

pub(crate) mod cli;
pub(crate) mod config;
pub(crate) mod docs;
#[cfg(test)]
pub(crate) mod docs_generated;
pub(crate) mod output;
pub(crate) mod query;
