use std::io::{Read as _, Write as _};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::Parser as _;

use super::{
    AssetCommand, Cli, Command, ConfigCommand, DocsCommand, DocsOutput, IndexCommand,
    MAX_SQL_INPUT_BYTES, QueryCommand, QueryRunArgs, RecordCommand, SchemaCommand, StderrProgress,
};
use crate::indexing::indexer::IndexOptions;
use crate::indexing::status::status;
use crate::indexing::sync::{self, SyncRoot};
use crate::ingest::record::{export_record, extract_asset, inspect_record, verify_record};
use crate::interface::config::{self, LoadedConfig};
use crate::interface::docs;
use crate::interface::output::ResponseEnvelope;
use crate::interface::query::{public_schema, query_sql};
use crate::storage::db::Store;

pub(crate) fn run() -> Result<()> {
    let Cli {
        config: config_file,
        db: database,
        command,
    } = Cli::parse();
    match command {
        Command::Docs { command } => run_docs(command),
        Command::Config { command } => {
            run_config(command, config_file.as_deref(), database.as_deref())
        }
        command => {
            let loaded = config::load(config_file.as_deref(), database.as_deref())?;
            run_operational(command, &loaded)
        }
    }
}

fn run_operational(command: Command, loaded: &LoadedConfig) -> Result<()> {
    let database = &loaded.effective.database;
    let needs_write = matches!(
        &command,
        Command::Index {
            command: IndexCommand::Sync(_)
        }
    );
    let mut store = if needs_write {
        Store::open(database).with_context(|| {
            format!(
                "failed to open index {} for index sync; sync is a write operation. \
                 If the existing indexed facts are sufficient, skip sync and use a read command",
                database.display()
            )
        })
    } else {
        Store::open_read_only(database)
            .with_context(|| format!("failed to open index {}", database.display()))
    }?;

    match command {
        Command::Index { command } => run_index(&mut store, loaded, command),
        Command::Schema { command } => run_schema(&store, command),
        Command::Query { command } => run_query(&store, command),
        Command::Record { command } => run_record(&store, command),
        Command::Asset { command } => run_asset(&store, command),
        Command::Docs { .. } | Command::Config { .. } => {
            unreachable!("non-operational commands are dispatched before opening the database")
        }
    }
}

fn configured_roots(loaded: &LoadedConfig) -> Vec<SyncRoot> {
    loaded
        .effective
        .roots
        .iter()
        .map(|root| SyncRoot {
            name: root.name.clone(),
            path: root.path.clone(),
        })
        .collect()
}

fn explicit_roots(paths: Vec<PathBuf>, loaded: &LoadedConfig) -> Result<Vec<SyncRoot>> {
    if paths.is_empty() {
        let roots = configured_roots(loaded);
        if roots.is_empty() {
            bail!("index sync requires paths or at least one configured [[roots]] entry");
        }
        return Ok(roots);
    }
    Ok(paths
        .into_iter()
        .enumerate()
        .map(|(index, path)| SyncRoot {
            name: format!("argument-{}", index + 1),
            path,
        })
        .collect())
}

fn run_docs(command: DocsCommand) -> Result<()> {
    match command {
        DocsCommand::List => print_envelope(&docs::list()),
        DocsCommand::Search { query } => print_envelope(&docs::search(&query)?),
        DocsCommand::Get(args) => {
            let document = docs::get(&args.topic)?;
            match args.output {
                DocsOutput::Markdown => print_markdown(&document.content),
                DocsOutput::Json => print_envelope(&document),
            }
        }
    }
}

fn run_config(
    command: ConfigCommand,
    config_file: Option<&std::path::Path>,
    database: Option<&std::path::Path>,
) -> Result<()> {
    match command {
        ConfigCommand::Init => print_envelope(&config::initialize(config_file)?),
        ConfigCommand::Show => {
            let loaded = config::load(config_file, database)?;
            print_envelope(&loaded.effective)
        }
    }
}

fn run_index(store: &mut Store, loaded: &LoadedConfig, command: IndexCommand) -> Result<()> {
    match command {
        IndexCommand::Sync(args) => {
            if args.max_record_bytes == 0 {
                bail!("--max-record-bytes must be greater than zero");
            }
            if args.max_text_bytes == 0 {
                bail!("--max-text-bytes must be greater than zero");
            }
            let roots = explicit_roots(args.paths, loaded)?;
            let mut observer = StderrProgress::new(args.progress.into());
            let mut report = sync::execute_observed(
                store,
                &roots,
                &IndexOptions {
                    rebuild: args.rebuild,
                    max_record_bytes: args.max_record_bytes,
                    max_text_bytes: args.max_text_bytes,
                },
                &mut observer,
            )?;
            if !args.include_sources {
                report.source_results.retain(|source| {
                    source.malformed_records > 0
                        || source.oversized_records > 0
                        || source.incomplete_tail
                        || source.error.is_some()
                });
            }
            let failed = report.failed_files;
            let discovered = report.discovered_files;
            print_envelope(&report)?;
            // A skipped file is an ordinary finding and stays in the report
            // only. A failed one means a trace this tool reads went unindexed,
            // which the exit status has to say, because a caller that trusts a
            // zero here would go on to query a corpus that is quietly short.
            if failed > 0 {
                bail!(
                    "{failed} of {discovered} discovered files are traces that could not be \
                     indexed; see source_results entries with action=failed"
                );
            }
        }
        IndexCommand::Status => {
            print_envelope(&status(store, &loaded.effective.database)?)?;
        }
    }
    Ok(())
}

fn run_schema(store: &Store, command: SchemaCommand) -> Result<()> {
    let (object, include_descriptions) = match command {
        SchemaCommand::List => (None, true),
        SchemaCommand::Get { object, compact } => (Some(object), !compact),
    };
    print_envelope(&public_schema(
        store,
        object.as_deref(),
        include_descriptions,
    )?)
}

fn run_query(store: &Store, command: QueryCommand) -> Result<()> {
    let QueryCommand::Run(args) = command;
    let sql = read_query_sql(&args)?;
    print_envelope(&query_sql(
        store,
        &sql,
        args.limit,
        args.max_cell_bytes,
        args.max_output_bytes,
        Duration::from_secs(u64::try_from(args.timeout_seconds)?),
    )?)
}

fn read_query_sql(args: &QueryRunArgs) -> Result<String> {
    if let Some(sql) = &args.sql {
        if sql.len() > MAX_SQL_INPUT_BYTES {
            bail!("inline SQL exceeds the {MAX_SQL_INPUT_BYTES}-byte input limit");
        }
        return Ok(sql.clone());
    }

    if let Some(path) = &args.file {
        let file = std::fs::File::open(path)
            .with_context(|| format!("failed to open SQL file {}", path.display()))?;
        return read_bounded_utf8_input(
            file,
            &format!("SQL file {}", path.display()),
            MAX_SQL_INPUT_BYTES,
        );
    }

    if args.stdin {
        return read_bounded_utf8_input(
            std::io::stdin().lock(),
            "SQL from stdin",
            MAX_SQL_INPUT_BYTES,
        );
    }

    unreachable!("clap requires exactly one SQL input")
}

fn read_bounded_utf8_input(
    reader: impl std::io::Read,
    source: &str,
    max_bytes: usize,
) -> Result<String> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(max_bytes)? + 1)
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read {source}"))?;
    if bytes.len() > max_bytes {
        bail!("{source} exceeds the {max_bytes}-byte input limit");
    }
    String::from_utf8(bytes).with_context(|| format!("{source} is not valid UTF-8"))
}

fn run_record(store: &Store, command: RecordCommand) -> Result<()> {
    match command {
        RecordCommand::Inspect(args) => print_envelope(&inspect_record(
            store,
            args.id,
            args.max_record_bytes,
            args.max_value_bytes,
            args.max_output_bytes,
        )?)?,
        RecordCommand::Verify { id } => print_envelope(&verify_record(store, id)?)?,
        RecordCommand::Export(args) => print_envelope(&export_record(
            store,
            args.id,
            &args.output,
            args.max_bytes,
            args.force,
        )?)?,
    }
    Ok(())
}

fn run_asset(store: &Store, command: AssetCommand) -> Result<()> {
    match command {
        AssetCommand::Extract(args) => print_envelope(&extract_asset(
            store,
            &args.reference,
            &args.output,
            args.max_record_bytes,
            args.max_bytes,
            args.force,
        )?)?,
    }
    Ok(())
}

fn print_envelope(value: &impl serde::Serialize) -> Result<()> {
    print_json(&ResponseEnvelope { data: value })
}

fn print_json(value: &impl serde::Serialize) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    serde_json::to_writer(&mut lock, value).context("failed to serialize JSON output")?;
    writeln!(lock).context("failed to write JSON output")?;
    Ok(())
}

fn print_markdown(markdown: &str) -> Result<()> {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    lock.write_all(markdown.as_bytes())
        .context("failed to write Markdown output")?;
    if !markdown.ends_with('\n') {
        writeln!(lock).context("failed to terminate Markdown output")?;
    }
    Ok(())
}
