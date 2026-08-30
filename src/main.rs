mod adapters;
mod domain;
mod indexing;
mod ingest;
mod interface;
mod shell;
mod storage;

fn main() -> anyhow::Result<()> {
    interface::cli::run()
}
