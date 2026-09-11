//! Shared Trace Index engine used by the CLI and native language bindings.

mod adapters;
mod domain;
mod indexing;
mod ingest;
mod interface;
mod shell;
mod storage;

pub mod api;

/// Runs the command-line interface.
///
/// # Errors
/// Returns argument, I/O, query, or synchronization errors.
pub fn run_cli() -> anyhow::Result<()> {
    interface::cli::run()
}
