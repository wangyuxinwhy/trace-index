//! `SQLite` storage and the public query contract.

pub(crate) mod blob;
pub(crate) mod db;
pub(crate) mod persist;
pub(crate) mod persist_diff;
pub(crate) mod public_schema;
pub(crate) mod schema;
mod schema_v5;
pub(crate) mod search;
mod semantic_encode;

pub(crate) mod metadata;
