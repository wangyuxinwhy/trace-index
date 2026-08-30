use std::path::PathBuf;

use anyhow::Result;
use clap::{ArgGroup, Args, Parser, Subcommand, ValueEnum};

use crate::indexing::telemetry::{ProgressMode, StderrProgress};

mod run;

pub(crate) use run::run;

const DEFAULT_MAX_TEXT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_OUTPUT_BYTES: usize = 64 * 1024;
const DEFAULT_MAX_RECORD_READ_BYTES: u64 = 16 * 1024 * 1024;
const DEFAULT_MAX_VALUE_BYTES: usize = 16 * 1024;
const DEFAULT_MAX_ASSET_BYTES: usize = 32 * 1024 * 1024;
const MAX_SQL_INPUT_BYTES: usize = 1024 * 1024;
const ROOT_AFTER_HELP: &str = r"Start here (for Agents):
  trace-index docs get start-here
  trace-index schema list
  trace-index schema get items --compact

  Help, bundled docs, and Schema discovery match this binary. Discover exact
  names and shapes before writing SQL; do not guess relations, columns, or flags.

Conceptual model:
  Source ──contains──> Record
  one or more Sources ──support──> Session ──contains──> Loop ──contains──> Item
  Item ──cites──> one or more physical Records

  Source   One physical Runtime trace input.
  Record   One complete Runtime record with a byte range and fingerprint.
  Session  One continuing logical Agent context, supported by one or more Sources.
  Loop     One outer Agent execution lifecycle inside a Session.
  Item     One independently queryable fact from Agent execution and the default query root.

  Loops inside a Session are ordered by session_position;
  Items inside a Loop are ordered by loop_position. A Session-level Item can have
  no Loop. Every Item cites a non-empty record_ids array as its physical evidence.

Semantic model and SQL encoding:
  Each Item has Semantic = {role, value, evidence_strength}. The role selects the
  typed value; evidence_strength is structural or heuristic. items.semantic is
  the stable SQL encoding. Top-level domain TextContent is encoded as {blob_id}
  and resolved through blobs only when a query needs text.

Semantic role vocabulary:
  human.request
    Human input that opens a new Loop.
  human.steering
    Human input delivered while the current Loop is still running.
  agent.commentary
    Agent progress or intermediate communication emitted during a Loop.
  agent.final_answer
    The Agent response marked as final for a Loop.
  agent.reasoning
    Reasoning exposed by the Runtime as full text, a summary, or unavailable.
  agent.tool_call
    A Runtime tool invocation with Agent-authored arguments.
  agent.tool_call.shell
    A tool invocation whose command-bearing arguments also have parsed shell structure.
  agent.delegation
    Work the Agent sends to a child Agent, optionally linked to its Session.
  tool.output
    The result returned by a tool, optionally linked to the calling Item.
  subagent.activity
    Intermediate activity associated with a child Session.
  subagent.report
    A result returned from a child Session to its parent Agent.
  runtime.instructions
    Instructions injected by the Runtime or harness, not Human input.
  runtime.context
    Environment, memory, file, or Session context supplied by the Runtime, not Human input.
  runtime.state
    Runtime-owned state or a state change placed on the Session timeline.
  runtime.notice
    A Runtime-owned informational or control event placed on the Session timeline.
  runtime.compaction_summary
    A Runtime-produced summary that replaces earlier context.
  runtime.unknown
    Meaningful bounded Runtime content whose more specific stable role is not yet known.

Public SQL surface:
  Domain Relations: sources, records, sessions, loops, items
  Access Relations: blobs, item_search

  Access Relations provide text lookup and candidate retrieval; they are not
  additional domain objects. Facts and provenance still come from Items and Records.

Session identity:
  sessions.session_id is a Trace Index-local join key. sessions.native_id is the
  Runtime-native identity. When a user asks for a Codex, Pi, or Claude Code
  Session id, report native_id together with runtime; keep session_id for SQL joins.

Query correctly:
  1. Start from items and select facts with semantic.role.
  2. Join sessions or loops when Runtime, context, time, or order defines scope.
  3. Read non-text semantic.value members directly; join blobs only for text.
  4. Follow record_ids only for exact Runtime evidence or index-integrity debugging.
  5. Use schema get before extracting an unfamiliar JSON shape.

Operational boundary:
  Reads never synchronize or mutate the index. Run index sync explicitly only
  when newly written traces are required. Commands return compact JSON on stdout;
  synchronization progress and diagnostics use stderr.

Use '<noun> --help' for capabilities and '<noun> <verb> --help' for exact
arguments, defaults, limits, side effects, and command-specific examples.";

const INDEX_SYNC_AFTER_HELP: &str = r"Mutation and input behavior:
  This is the command that writes the index. With PATHS, it synchronizes those
  files or directories. Without PATHS, it uses the configured [[roots]].
  The first sync stores the effective Record and text byte limits as one
  database-wide policy. An indexed database owns and reuses that policy; an
  explicit conflicting limit is rejected. Changing policy requires a new
  database and a complete reindex.
  The final JSON envelope is written to stdout; progress is written to stderr.

Examples:
  trace-index index sync
  trace-index index sync ~/.codex/sessions ~/.claude/projects --progress ndjson";

const INDEX_STATUS_AFTER_HELP: &str = r"Behavior:
  Opens the selected existing database read-only. It does not discover or
  synchronize traces. The result includes Source coverage and freshness plus
  the stored indexing policy when one has been established.

Example:
  trace-index index status";

const DOCS_AFTER_HELP: &str = r"Behavior:
  Reads documentation bundled with this binary without loading configuration or
  opening the database. The global --config and --db selectors are accepted for
  command-tree consistency but have no effect on docs commands.";

const QUERY_RUN_AFTER_HELP: &str = r#"Behavior:
  Runs one read-only statement against the existing index. It never refreshes
  traces. Inspect relation names and JSON shapes with schema list/get first.

Examples:
  trace-index query run \
    "SELECT json_extract(semantic, '$.role') AS role, COUNT(*) AS item_count FROM items GROUP BY role"

  Use --stdin for multiline SQL so the shell does not turn newlines into a
  literal \n sequence:

  trace-index query run --stdin <<'SQL'
  SELECT loop_position, semantic
  FROM items
  WHERE loop_id = 1
  ORDER BY loop_position;
  SQL

Conditional workflows:
  trace-index docs get how-to/query-evidence
  trace-index docs get how-to/search-literals"#;

const RECORD_EXPORT_AFTER_HELP: &str = r"Behavior:
  Verifies the Record against its Source before writing exact bytes. The
  destination must not exist unless --force is supplied.

Example:
  trace-index record export 12345 --output record.jsonl";

const RECORD_INSPECT_AFTER_HELP: &str = r"Behavior:
  Verifies the indexed byte range against its Source, then returns a bounded,
  display-safe representation. Inline media is represented by an Asset reference.

Example:
  trace-index record inspect 12345";

const RECORD_VERIFY_AFTER_HELP: &str = r"Behavior:
  Checks the original Source bytes without returning the Record body. Integrity
  findings such as verified, source_missing, source_short, or hash_mismatch are returned as data.

Example:
  trace-index record verify 12345";

const ASSET_EXTRACT_AFTER_HELP: &str = r"Behavior:
  Verifies the supporting Record, decodes the referenced inline payload, and
  refuses to replace an existing destination unless --force is supplied.

Example:
  trace-index asset extract '12345#/payload/content/1/image_url' --output image.png";

const CONFIG_INIT_AFTER_HELP: &str = r"Behavior:
  Creates and validates one configuration file without replacing an existing
  file. It does not create the database or synchronize traces. --root is an
  explicit opt-in; --discover adds only standard Runtime roots that exist.
  --db records the initial database path, and [indexing] initializes only a new
  or empty database.

Examples:
  trace-index config init --discover
  trace-index config init --root ~/.codex/sessions --root /work/traces
  trace-index --db /work/trace-index.sqlite config init --root /work/traces";

const CONFIG_SHOW_AFTER_HELP: &str = r"Behavior:
  Returns absolute configured paths and the origin of each selected or defaulted
  value. Indexing values initialize a new or empty database; use index status
  for an established policy. This command does not inspect roots or open the database.

Example:
  trace-index config show";

const CONFIG_CHECK_AFTER_HELP: &str = r"Behavior:
  Resolves the same configuration file, database path, and roots as operational
  commands, then checks root existence, type, canonical duplication, and database
  path ancestry. It is read-only and does not require the database to exist.
  Any issue with level=error produces valid=false and exit 1 after the JSON
  report. A warning-only report exits 0 even when configured_sync_ready=false,
  because index sync can still receive explicit PATHS.

Example:
  trace-index config check";

#[derive(Debug, Parser)]
#[command(
    name = "trace-index",
    version,
    about,
    after_help = ROOT_AFTER_HELP,
    disable_help_subcommand = true
)]
pub(crate) struct Cli {
    /// Configuration file path.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// `SQLite` index path.
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
#[allow(
    clippy::large_enum_variant,
    reason = "the command is parsed once, so boxing its flat CLI arguments adds indirection without value"
)]
enum Command {
    /// Read documentation bundled with the current CLI.
    #[command(after_help = DOCS_AFTER_HELP)]
    Docs {
        #[command(subcommand)]
        command: DocsCommand,
    },

    /// Initialize and inspect trace-index configuration.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },

    /// Maintain and inspect the current trace index.
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },

    /// Describe the stable public SQL query contract.
    Schema {
        #[command(subcommand)]
        command: SchemaCommand,
    },

    /// Execute one bounded read-only SQL statement against public relations.
    Query {
        #[command(subcommand)]
        command: QueryCommand,
    },

    /// Inspect, verify, or export physical JSONL records.
    Record {
        #[command(subcommand)]
        command: RecordCommand,
    },

    /// Extract inline media referenced by Records.
    Asset {
        #[command(subcommand)]
        command: AssetCommand,
    },
}

#[derive(Debug, Subcommand)]
enum DocsCommand {
    /// List bundled documentation topics.
    #[command(after_help = DOCS_AFTER_HELP)]
    List,

    /// Get one documentation topic.
    #[command(after_help = DOCS_AFTER_HELP)]
    Get(DocsGetArgs),

    /// Search bundled documentation topics.
    #[command(after_help = DOCS_AFTER_HELP)]
    Search {
        /// Case-insensitive text to find.
        query: String,
    },
}

#[derive(Debug, Args)]
struct DocsGetArgs {
    /// Documentation topic name.
    topic: String,

    /// Output Markdown directly or return a compact JSON document.
    #[arg(long, value_enum, default_value_t = DocsOutput::Markdown)]
    output: DocsOutput,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum DocsOutput {
    Markdown,
    Json,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
enum CliProgress {
    Auto,
    Human,
    Ndjson,
    Off,
}

impl From<CliProgress> for ProgressMode {
    fn from(value: CliProgress) -> Self {
        match value {
            CliProgress::Auto => Self::Auto,
            CliProgress::Human => Self::Human,
            CliProgress::Ndjson => Self::Ndjson,
            CliProgress::Off => Self::Off,
        }
    }
}

#[derive(Debug, Subcommand)]
enum ConfigCommand {
    /// Create a new configuration file without overwriting an existing one.
    #[command(after_help = CONFIG_INIT_AFTER_HELP)]
    Init {
        /// Add one trace file or directory to the new configuration; repeatable.
        #[arg(long, value_name = "PATH")]
        root: Vec<PathBuf>,

        /// Add standard Codex, Pi, and Claude Code roots that currently exist.
        #[arg(long)]
        discover: bool,
    },

    /// Show resolved configuration values and their origins.
    #[command(after_help = CONFIG_SHOW_AFTER_HELP)]
    Show,

    /// Validate configuration and configured root readiness without mutation.
    #[command(after_help = CONFIG_CHECK_AFTER_HELP)]
    Check,
}

#[derive(Debug, Subcommand)]
enum IndexCommand {
    /// Incrementally index supported Agent trace files or directories.
    Sync(IndexSyncArgs),

    /// Show index coverage, parse status, and source freshness.
    #[command(after_help = INDEX_STATUS_AFTER_HELP)]
    Status,
}

#[derive(Debug, Args)]
#[command(after_help = INDEX_SYNC_AFTER_HELP)]
struct IndexSyncArgs {
    /// Trace JSONL files or directories to index recursively.
    paths: Vec<PathBuf>,

    /// Rebuild matching sources even when their indexed prefix is unchanged.
    #[arg(long)]
    rebuild: bool,

    /// Request maximum bytes indexed for one JSONL Record in this sync.
    ///
    /// The built-in value is 16777216.
    #[arg(long, value_name = "BYTES", value_parser = parse_positive_usize)]
    max_record_bytes: Option<usize>,

    /// Request maximum bytes published for one Semantic text value in this sync.
    ///
    /// The built-in value is 65536.
    #[arg(long, value_name = "BYTES", value_parser = parse_positive_usize)]
    max_text_bytes: Option<usize>,

    /// Include one result object per discovered source in the final envelope.
    #[arg(long)]
    include_sources: bool,

    /// Progress rendering for this synchronization.
    #[arg(long, value_enum, default_value_t = CliProgress::Auto)]
    progress: CliProgress,
}

#[derive(Debug, Subcommand)]
enum SchemaCommand {
    /// List public SQL relations and their descriptions.
    List,

    /// Get one public SQL relation's schema.
    Get {
        /// Public relation name.
        object: String,

        /// Omit descriptions and return only names, types, and nullability.
        #[arg(long)]
        compact: bool,
    },
}

#[derive(Debug, Subcommand)]
enum QueryCommand {
    /// Run one bounded read-only SQL statement.
    Run(QueryRunArgs),
}

#[derive(Debug, Args)]
#[command(after_help = QUERY_RUN_AFTER_HELP)]
#[command(group(
    ArgGroup::new("input")
        .required(true)
        .multiple(false)
        .args(["sql", "file", "stdin"])
))]
struct QueryRunArgs {
    /// SQL statement to execute. Prefer --file or --stdin for multiline SQL.
    sql: Option<String>,

    /// Read the SQL statement from a UTF-8 file.
    #[arg(long, value_name = "PATH")]
    file: Option<PathBuf>,

    /// Read the SQL statement from standard input.
    #[arg(long)]
    stdin: bool,

    /// Maximum rows returned.
    #[arg(long, default_value_t = 1_000, value_parser = parse_limit)]
    limit: usize,

    /// Maximum UTF-8 bytes returned for one text or blob cell.
    #[arg(long, default_value_t = DEFAULT_MAX_TEXT_BYTES, value_parser = parse_cell_bytes)]
    max_cell_bytes: usize,

    /// Maximum serialized bytes returned across query rows.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES, value_parser = parse_output_bytes)]
    max_output_bytes: usize,

    /// Abort the query after this many seconds.
    #[arg(long, default_value_t = 30, value_parser = parse_timeout_seconds)]
    timeout_seconds: usize,
}

#[derive(Debug, Subcommand)]
enum RecordCommand {
    /// Verify and inspect one display-safe source-backed Record representation.
    #[command(after_help = RECORD_INSPECT_AFTER_HELP)]
    Inspect(RecordInspectArgs),

    /// Verify one Record against its original source bytes.
    #[command(after_help = RECORD_VERIFY_AFTER_HELP)]
    Verify {
        /// Numeric Record identity, as returned by `records.record_id`.
        id: i64,
    },

    /// Write one byte-exact verified Record to a file.
    Export(RecordExportArgs),
}

#[derive(Debug, Args)]
struct RecordInspectArgs {
    /// Numeric Record identity, as returned by `records.record_id`.
    id: i64,

    /// Maximum source Record bytes to materialize for representation.
    #[arg(long, default_value_t = DEFAULT_MAX_RECORD_READ_BYTES)]
    max_record_bytes: u64,

    /// Maximum bytes retained for one scalar string value.
    #[arg(long, default_value_t = DEFAULT_MAX_VALUE_BYTES, value_parser = parse_cell_bytes)]
    max_value_bytes: usize,

    /// Maximum bytes retained for the structural representation.
    #[arg(long, default_value_t = DEFAULT_MAX_OUTPUT_BYTES, value_parser = parse_output_bytes)]
    max_output_bytes: usize,
}

#[derive(Debug, Args)]
#[command(after_help = RECORD_EXPORT_AFTER_HELP)]
struct RecordExportArgs {
    /// Numeric Record identity, as returned by `records.record_id`.
    id: i64,

    /// Destination for the byte-exact JSONL Record.
    #[arg(long)]
    output: PathBuf,

    /// Maximum Record bytes written by this command.
    #[arg(long, default_value_t = DEFAULT_MAX_RECORD_READ_BYTES)]
    max_bytes: u64,

    /// Replace an existing destination.
    #[arg(long)]
    force: bool,
}

#[derive(Debug, Subcommand)]
enum AssetCommand {
    /// Decode one inline asset and write it to a file.
    #[command(after_help = ASSET_EXTRACT_AFTER_HELP)]
    Extract(AssetExtractArgs),
}

#[derive(Debug, Args)]
struct AssetExtractArgs {
    /// Reference returned by `record inspect`.
    reference: String,

    /// Destination for the decoded asset.
    #[arg(long)]
    output: PathBuf,

    /// Maximum source Record bytes read while resolving the reference.
    #[arg(long, default_value_t = DEFAULT_MAX_RECORD_READ_BYTES)]
    max_record_bytes: u64,

    /// Maximum decoded asset bytes written by this command.
    #[arg(long, default_value_t = DEFAULT_MAX_ASSET_BYTES, value_parser = parse_asset_bytes)]
    max_bytes: usize,

    /// Replace an existing destination.
    #[arg(long)]
    force: bool,
}

fn parse_limit(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, 1, 10_000, "limit")
}

fn parse_positive_usize(value: &str) -> Result<usize, String> {
    value
        .parse::<usize>()
        .map_err(|error| format!("invalid byte limit {value:?}: {error}"))
        .and_then(|parsed| {
            if parsed == 0 {
                Err("byte limit must be greater than zero".to_owned())
            } else {
                Ok(parsed)
            }
        })
}

fn parse_cell_bytes(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, 1, 1024 * 1024, "max-cell-bytes")
}

fn parse_output_bytes(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, 1024, 16 * 1024 * 1024, "max-output-bytes")
}

fn parse_asset_bytes(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, 1, 1024 * 1024 * 1024, "max-bytes")
}

fn parse_timeout_seconds(value: &str) -> Result<usize, String> {
    parse_bounded_usize(value, 1, 300, "timeout-seconds")
}

fn parse_bounded_usize(
    value: &str,
    minimum: usize,
    maximum: usize,
    name: &str,
) -> Result<usize, String> {
    let parsed = value
        .parse::<usize>()
        .map_err(|error| format!("invalid {name} {value:?}: {error}"))?;
    if !(minimum..=maximum).contains(&parsed) {
        return Err(format!(
            "{name} must be between {minimum} and {maximum}, inclusive"
        ));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::ROOT_AFTER_HELP;
    use crate::domain::SEMANTIC_ROLES;

    #[test]
    fn root_help_explains_every_semantic_role() {
        for role in SEMANTIC_ROLES {
            assert!(
                ROOT_AFTER_HELP.contains(&format!("  {}\n    {}", role.as_str(), role.meaning())),
                "root Help does not explain Semantic role `{}`",
                role.as_str()
            );
        }
    }
}
