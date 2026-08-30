//! Stable Source, Record, Session, Loop, and Item domain values.
//!
//! Adapters may use private builders and `SQLite` may normalize nested fields,
//! but neither may change this public fact contract.

mod common;
mod semantic;
mod shell;
mod text;

pub(crate) use common::*;
pub(crate) use semantic::*;
pub(crate) use shell::*;
pub(crate) use text::*;
