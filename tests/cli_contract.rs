use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use serde_json::{Value, json};
use tempfile::tempdir;

fn trace_index() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace-index"));
    let isolated =
        std::env::temp_dir().join(format!("trace-index-cli-contract-{}", std::process::id()));
    command
        .env_remove("TRACE_INDEX_CONFIG")
        .env_remove("TRACE_INDEX_DB")
        .env_remove("TRACE_INDEX_METADATA_DB")
        .env("XDG_CONFIG_HOME", isolated.join("config"))
        .env("XDG_DATA_HOME", isolated.join("data"));
    command
}

fn run(arguments: &[&str]) -> Output {
    trace_index()
        .args(arguments)
        .output()
        .expect("trace-index should start")
}

fn run_with_stdin(arguments: &[&str], input: &str) -> Output {
    let mut child = trace_index()
        .args(arguments)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("trace-index should start");
    child
        .stdin
        .take()
        .expect("piped stdin")
        .write_all(input.as_bytes())
        .expect("write stdin");
    child.wait_with_output().expect("wait for trace-index")
}

fn path_text(path: &Path) -> &str {
    path.to_str().expect("test path should be UTF-8")
}

fn compact_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        output.stderr.is_empty(),
        "unexpected stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.stdout.last(), Some(&b'\n'));
    assert!(!output.stdout[..output.stdout.len() - 1].contains(&b'\n'));
    serde_json::from_slice(&output.stdout).expect("one compact JSON value")
}

fn write_codex_trace(path: &Path, request: &str, answer: &str) {
    let records = [
        json!({"type":"session_meta","payload":{"id":"cli-session","cwd":"/workspace"}}),
        json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
        json!({"type":"event_msg","payload":{"type":"user_message","message":request}}),
        json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":request}]}}),
        json!({"type":"event_msg","payload":{"type":"agent_message","phase":"final_answer","message":answer}}),
        json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":answer}}),
    ];
    let mut text = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    fs::write(path, text).expect("write Codex trace");
}

fn index_fixture(directory: &Path, request: &str) -> (PathBuf, PathBuf, Value) {
    let database = directory.join("index.sqlite");
    let trace = directory.join("trace.jsonl");
    write_codex_trace(&trace, request, "finished");
    let report = compact_json(&run(&[
        "--db",
        path_text(&database),
        "index",
        "sync",
        path_text(&trace),
        "--progress",
        "off",
        "--include-sources",
    ]));
    (database, trace, report)
}

#[test]
fn config_init_creates_a_valid_explicit_configuration_without_other_mutation() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config.toml");
    let database = directory.path().join("data/index.sqlite");
    let root = directory.path().join("traces");
    fs::create_dir(&root).expect("create trace root");

    let initialized = compact_json(&run(&[
        "--config",
        path_text(&config),
        "--db",
        path_text(&database),
        "config",
        "init",
        "--root",
        path_text(&root),
    ]));
    assert_eq!(initialized["data"]["created"], true);
    assert_eq!(
        initialized["data"]["config"]["config_file"]["path"],
        path_text(&config)
    );
    assert_eq!(
        initialized["data"]["config"]["config_file"]["origin"]["kind"],
        "cli"
    );
    assert_eq!(
        initialized["data"]["config"]["database"]["origin"]["kind"],
        "cli"
    );
    assert_eq!(
        initialized["data"]["config"]["roots"][0]["path"],
        path_text(&root)
    );
    assert!(config.is_file());
    assert!(
        !database.exists(),
        "config init must not create the database"
    );

    let contents = fs::read_to_string(&config).expect("read initialized configuration");
    assert!(contents.contains("schema_version = 1"));
    assert!(contents.contains("max_indexed_record_bytes = 16777216"));
    assert!(contents.contains("max_published_text_bytes = 65536"));

    let checked = compact_json(&run(&["--config", path_text(&config), "config", "check"]));
    assert_eq!(checked["data"]["valid"], true);
    assert_eq!(checked["data"]["configured_sync_ready"], true);

    let repeated = run(&["--config", path_text(&config), "config", "init"]);
    assert!(!repeated.status.success());
    assert!(repeated.stdout.is_empty());
    assert!(String::from_utf8_lossy(&repeated.stderr).contains("refusing to overwrite"));
}

#[test]
fn config_init_keeps_cli_database_precedence_in_its_report() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config.toml");
    let cli_database = directory.path().join("cli.sqlite");
    let environment_database = directory.path().join("environment.sqlite");
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace-index"));
    command
        .env_remove("TRACE_INDEX_CONFIG")
        .env("TRACE_INDEX_DB", &environment_database)
        .env("XDG_CONFIG_HOME", directory.path().join("config-home"))
        .env("XDG_DATA_HOME", directory.path().join("data-home"))
        .args([
            "--config",
            path_text(&config),
            "--db",
            path_text(&cli_database),
            "config",
            "init",
        ]);

    let initialized = compact_json(&command.output().expect("run config init"));
    assert_eq!(
        initialized["data"]["config"]["database"]["path"],
        path_text(&cli_database)
    );
    assert_eq!(
        initialized["data"]["config"]["database"]["origin"]["kind"],
        "cli"
    );
    let contents = fs::read_to_string(config).expect("read initialized configuration");
    assert!(contents.contains(path_text(&cli_database)));
    assert!(!contents.contains(path_text(&environment_database)));
}

#[test]
fn config_init_discover_adds_only_standard_roots_that_exist() {
    let directory = tempdir().expect("temporary directory");
    let home = directory.path().join("home");
    let codex = home.join(".codex/sessions");
    let antigravity = home.join(".gemini/antigravity-cli/brain");
    fs::create_dir_all(&codex).expect("create existing Codex root");
    fs::create_dir_all(&antigravity).expect("create existing Antigravity root");
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace-index"));
    command
        .env_remove("TRACE_INDEX_CONFIG")
        .env_remove("TRACE_INDEX_DB")
        .env("HOME", &home)
        .env("XDG_CONFIG_HOME", directory.path().join("config"))
        .env("XDG_DATA_HOME", directory.path().join("data"))
        .args(["config", "init", "--discover"]);

    let initialized = compact_json(&command.output().expect("run config init --discover"));
    let roots = initialized["data"]["config"]["roots"]
        .as_array()
        .expect("effective roots");
    assert_eq!(roots.len(), 2);
    assert_eq!(roots[0]["label"], "codex");
    assert_eq!(roots[0]["path"], path_text(&codex));
    assert_eq!(roots[1]["label"], "antigravity");
    assert_eq!(roots[1]["path"], path_text(&antigravity));
}

#[test]
fn config_check_warning_only_report_exits_successfully() {
    let directory = tempdir().expect("temporary directory");
    let config = directory.path().join("config.toml");
    compact_json(&run(&["--config", path_text(&config), "config", "init"]));

    let checked = compact_json(&run(&["--config", path_text(&config), "config", "check"]));
    assert_eq!(checked["data"]["valid"], true);
    assert_eq!(checked["data"]["configured_sync_ready"], false);
    assert_eq!(checked["data"]["issues"][0]["level"], "warning");
    assert_eq!(checked["data"]["issues"][0]["code"], "no_configured_roots");
}

#[test]
fn docs_ignore_global_storage_selectors_without_opening_them() {
    let directory = tempdir().expect("temporary directory");
    let missing_config = directory.path().join("missing.toml");
    let missing_database = directory.path().join("missing.sqlite");
    let listed = compact_json(&run(&[
        "--config",
        path_text(&missing_config),
        "--db",
        path_text(&missing_database),
        "docs",
        "list",
    ]));
    assert!(
        listed["data"]["topics"]
            .as_array()
            .is_some_and(|topics| !topics.is_empty())
    );
    assert!(!missing_database.exists());
}

#[test]
fn docs_get_supports_section_listing_and_targeted_extraction() {
    let outline_md = run(&["docs", "get", "reference/public-schema", "--list-sections"]);
    assert!(outline_md.status.success());
    let outline_text = String::from_utf8(outline_md.stdout).expect("UTF-8 outline");
    assert!(outline_text.contains("Semantic SQL encoding"));

    let outline_json = compact_json(&run(&[
        "docs",
        "get",
        "reference/public-schema",
        "--list-sections",
        "--output",
        "json",
    ]));
    assert!(
        outline_json["data"]["sections"]
            .as_array()
            .is_some_and(|sections| sections
                .iter()
                .any(|s| s["heading"] == "Semantic SQL encoding"))
    );

    let section_md = run(&[
        "docs",
        "get",
        "reference/public-schema",
        "--section",
        "Semantic SQL encoding",
    ]);
    assert!(section_md.status.success());
    let section_text = String::from_utf8(section_md.stdout).expect("UTF-8 section");
    assert!(section_text.starts_with("## Semantic SQL encoding"));
    assert!(section_text.contains("call_item_id"));
    assert!(!section_text.contains("## JSON values are typed domain values"));

    let section_json = compact_json(&run(&[
        "docs",
        "get",
        "reference/public-schema",
        "--section",
        "Semantic SQL encoding",
        "--output",
        "json",
    ]));
    assert!(
        section_json["data"]["content"]
            .as_str()
            .is_some_and(|c| c.starts_with("## Semantic SQL encoding"))
    );

    let missing = run(&[
        "docs",
        "get",
        "reference/public-schema",
        "--section",
        "does-not-exist",
    ]);
    assert!(!missing.status.success());
    let stderr = String::from_utf8(missing.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("Available sections:"));
    assert!(stderr.contains("--list-sections"));

    let empty_section = run(&["docs", "get", "reference/public-schema", "--section", ""]);
    assert!(!empty_section.status.success());

    let conflict = run(&[
        "docs",
        "get",
        "reference/public-schema",
        "--list-sections",
        "--section",
        "Relations",
    ]);
    assert_eq!(conflict.status.code(), Some(2));

    let sectionless_outline = run(&["docs", "get", "reference/cli", "--list-sections"]);
    assert!(sectionless_outline.status.success());
    assert!(
        String::from_utf8(sectionless_outline.stdout)
            .expect("UTF-8")
            .contains("no subsections")
    );
    let sectionless_get = run(&["docs", "get", "reference/cli", "--section", "any"]);
    assert!(!sectionless_get.status.success());
    assert!(
        String::from_utf8(sectionless_get.stderr)
            .expect("UTF-8")
            .contains("has no subsections")
    );
}

#[test]
fn ordinary_sync_reuses_the_policy_owned_by_a_non_empty_database() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("index.sqlite");
    let trace = directory.path().join("trace.jsonl");
    let config = directory.path().join("config.toml");
    write_codex_trace(&trace, "stored policy request", "finished");

    let first = compact_json(&run(&[
        "--db",
        path_text(&database),
        "index",
        "sync",
        path_text(&trace),
        "--max-text-bytes",
        "5",
        "--progress",
        "off",
    ]));
    assert_eq!(
        first["data"]["indexing_policy"]["max_published_text_bytes"],
        5
    );

    fs::write(
        &config,
        format!(
            "schema_version = 1\ndatabase = {database:?}\n\n[indexing]\nmax_published_text_bytes = 6\n\n[[roots]]\npath = {trace:?}\n"
        ),
    )
    .expect("write configuration with a different initial policy");
    let second = compact_json(&run(&[
        "--config",
        path_text(&config),
        "index",
        "sync",
        "--progress",
        "off",
    ]));
    assert_eq!(
        second["data"]["indexing_policy"]["max_published_text_bytes"],
        5
    );
}

#[test]
fn index_rejects_a_policy_change_after_sources_exist() {
    let directory = tempdir().expect("temporary directory");
    let (database, trace, report) = index_fixture(directory.path(), "policy request");
    assert_eq!(
        report["data"]["indexing_policy"]["max_published_text_bytes"],
        65536
    );

    let changed = run(&[
        "--db",
        path_text(&database),
        "index",
        "sync",
        path_text(&trace),
        "--max-text-bytes",
        "5",
        "--progress",
        "off",
    ]);
    assert!(!changed.status.success());
    assert!(changed.stdout.is_empty());
    assert!(String::from_utf8_lossy(&changed.stderr).contains("indexing policy mismatch"));

    let status = compact_json(&run(&["--db", path_text(&database), "index", "status"]));
    assert_eq!(
        status["data"]["indexing_policy"]["max_published_text_bytes"],
        65536
    );
}

#[test]
fn root_help_teaches_the_current_control_plane_and_model() {
    let output = run(&["--help"]);
    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let help = String::from_utf8(output.stdout).expect("UTF-8 help");
    for required in [
        "docs get start-here",
        "schema list",
        "Source ──contains──> Record",
        "Session ──contains──> Loop ──contains──> Item",
        "Item ──cites──> one or more physical Records",
        "Item     One independently queryable fact from Agent execution and the default query root",
        "Semantic = {role, value, evidence_strength}",
        "Semantic role vocabulary:",
        "Human input that opens a new Loop.",
        "Human input delivered while the current Loop is still running.",
        "Agent progress or intermediate communication emitted during a Loop.",
        "The Agent response marked as final for a Loop.",
        "Instructions injected by the Runtime or harness, not Human input.",
        "Domain Relations: sources, records, sessions, loops, items",
        "Access Relations: blobs, item_search",
        "Start from items and select facts with semantic.role",
        "Investigation defaults:",
        "Orient with the conversation spine.",
        "human.request, human.steering, agent.commentary, and agent.final_answer",
        "This is an orientation view, not the complete evidence set.",
        "A text-search hit is only a candidate.",
        "Match evidence depth to the claim.",
        "Order Loops by session_position and Items by loop_position.",
        "Report contributing and missing counts",
        "separately; an inner join can silently redefine the population",
        "These are independent boundaries.",
        "Stop when the requested claim is supported.",
        "Reads never synchronize or mutate the index",
    ] {
        assert!(help.contains(required), "help omitted {required:?}");
    }
    for removed in [
        "Turn",
        "items_view",
        "turns_view",
        "tool_calls",
        "native_runtime",
        "native_kind",
        "native_value",
        "semantic_role",
        "semantic_value",
        "end_record_id",
        "content_blobs",
        "trigram candidates",
        "<<'SQL'",
    ] {
        assert!(!help.contains(removed), "help still exposes {removed:?}");
    }
}

#[test]
fn conditional_guidance_lives_with_the_command_that_needs_it() {
    let init = run(&["config", "init", "--help"]);
    assert!(init.status.success());
    let init = String::from_utf8(init.stdout).expect("UTF-8 init help");
    assert!(init.contains("--root <PATH>"));
    assert!(init.contains("--discover"));
    assert!(init.contains("does not create the database or synchronize traces"));
    assert!(init.contains("--db records the initial database path"));
    assert!(init.contains("initializes only a new"));

    let show = run(&["config", "show", "--help"]);
    assert!(show.status.success());
    let show = String::from_utf8(show.stdout).expect("UTF-8 show help");
    assert!(show.contains("absolute configured paths"));
    assert!(show.contains("origin"));
    assert!(show.contains("use index status"));

    let check = run(&["config", "check", "--help"]);
    assert!(check.status.success());
    let check = String::from_utf8(check.stdout).expect("UTF-8 check help");
    assert!(check.contains("read-only"));
    assert!(check.contains("canonical duplication"));
    assert!(check.contains("database path"));
    assert!(check.contains("level=error produces valid=false"));
    assert!(check.contains("warning-only report exits 0"));

    let query = run(&["query", "run", "--help"]);
    assert!(query.status.success());
    let query = String::from_utf8(query.stdout).expect("UTF-8 query help");
    assert!(query.contains("Use --stdin for multiline SQL"));
    assert!(query.contains("docs get how-to/search-literals"));
    assert!(query.contains("<<'SQL'"));

    let sync = run(&["index", "sync", "--help"]);
    assert!(sync.status.success());
    let sync = String::from_utf8(sync.stdout).expect("UTF-8 sync help");
    assert!(sync.contains("This command writes the index on demand"));
    assert!(sync.contains("Without PATHS, it uses the configured [[roots]]"));
    assert!(sync.contains("database-wide policy"));
    assert!(sync.contains("Changing policy requires a new"));
    assert!(sync.contains("progress is written to stderr"));

    let status = run(&["index", "status", "--help"]);
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).expect("UTF-8 status help");
    assert!(status.contains("existing database read-only"));
    assert!(status.contains("does not discover or"));
    assert!(status.contains("index watch status"));
    assert!(status.contains("stored indexing policy"));

    let watch_run = run(&["index", "watch", "run", "--help"]);
    assert!(watch_run.status.success());
    let watch_run = String::from_utf8(watch_run.stdout).expect("UTF-8 watch run help");
    assert!(watch_run.contains("OS filesystem events"));
    assert!(watch_run.contains("--reconcile-secs"));
    assert!(watch_run.contains("advisory lock"));

    let watch_status = run(&["index", "watch", "status", "--help"]);
    assert!(watch_status.status.success());
    let watch_status = String::from_utf8(watch_status.stdout).expect("UTF-8 watch status help");
    assert!(watch_status.contains("Does not open or create the database"));

    let watch_install = run(&["index", "watch", "install", "--help"]);
    assert!(watch_install.status.success());
    let watch_install = String::from_utf8(watch_install.stdout).expect("UTF-8 watch install help");
    assert!(watch_install.contains("Requires at least one configured [[roots]]"));
    assert!(watch_install.contains("never registers background processes"));

    let watch_uninstall = run(&["index", "watch", "uninstall", "--help"]);
    assert!(watch_uninstall.status.success());
    let watch_uninstall =
        String::from_utf8(watch_uninstall.stdout).expect("UTF-8 watch uninstall help");
    assert!(watch_uninstall.contains("Unloads and removes"));

    let docs = run(&["docs", "list", "--help"]);
    assert!(docs.status.success());
    let docs = String::from_utf8(docs.stdout).expect("UTF-8 docs help");
    assert!(docs.contains("without loading configuration"));
    assert!(docs.contains("have no effect on docs commands"));

    let export = run(&["record", "export", "--help"]);
    assert!(export.status.success());
    let export = String::from_utf8(export.stdout).expect("UTF-8 export help");
    assert!(export.contains("Verifies the Record against its Source"));
    assert!(export.contains("unless --force is supplied"));

    let inspect = run(&["record", "inspect", "--help"]);
    assert!(inspect.status.success());
    let inspect = String::from_utf8(inspect.stdout).expect("UTF-8 inspect help");
    assert!(inspect.contains("display-safe representation"));
    assert!(inspect.contains("Asset reference"));

    let asset = run(&["asset", "extract", "--help"]);
    assert!(asset.status.success());
    let asset = String::from_utf8(asset.stdout).expect("UTF-8 asset help");
    assert!(asset.contains("decodes the referenced inline payload"));
    assert!(asset.contains("unless --force is supplied"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end workflow verifies sync, schema discovery, and querying together"
)]
fn sync_schema_and_query_form_one_machine_readable_workflow() {
    let directory = tempdir().expect("temporary directory");
    let (database, trace, report) = index_fixture(directory.path(), "unique request phrase");
    assert_eq!(report["data"]["failed_files"], 0);
    assert_eq!(report["data"]["indexed_files"], 1);
    assert_eq!(
        Path::new(
            report["data"]["source_results"][0]["path"]
                .as_str()
                .expect("reported Source path")
        )
        .canonicalize()
        .expect("canonical reported Source"),
        trace.canonicalize().expect("canonical fixture Source")
    );

    let schema = compact_json(&run(&["--db", path_text(&database), "schema", "list"]));
    let names = schema["data"]["objects"]
        .as_array()
        .expect("schema objects")
        .iter()
        .map(|object| object["name"].as_str().expect("relation name"))
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        [
            "sources",
            "records",
            "sessions",
            "loops",
            "items",
            "blobs",
            "item_search",
            "session_metadata"
        ]
    );

    for (relation, expected) in [
        ("sources", &["source_id", "locator", "runtime"][..]),
        (
            "records",
            &[
                "record_id",
                "source_id",
                "source_position",
                "content_range",
                "fingerprint",
                "occurred_at",
            ][..],
        ),
        (
            "loops",
            &[
                "loop_id",
                "session_id",
                "session_position",
                "native_id",
                "start_record_id",
                "end",
                "model",
                "usage",
            ][..],
        ),
        (
            "items",
            &[
                "item_id",
                "session_id",
                "loop_id",
                "loop_position",
                "occurred_at",
                "record_ids",
                "semantic",
            ][..],
        ),
        (
            "blobs",
            &["blob_id", "text", "full_bytes", "estimated_tokens"][..],
        ),
    ] {
        let schema = compact_json(&run(&[
            "--db",
            path_text(&database),
            "schema",
            "get",
            relation,
            "--compact",
        ]));
        let columns = schema["data"]["objects"][0]["columns"]
            .as_array()
            .expect("relation columns")
            .iter()
            .map(|column| column["name"].as_str().expect("column name"))
            .collect::<Vec<_>>();
        assert_eq!(columns, expected, "unexpected {relation} contract");
    }

    let result = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT json_extract(i.semantic, '$.role') AS role, b.text FROM items AS i JOIN blobs AS b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id') ORDER BY i.loop_position",
    ]));
    assert_eq!(result["data"]["rows"][0]["role"], "human.request");
    assert_eq!(result["data"]["rows"][0]["text"], "unique request phrase");
    assert_eq!(result["data"]["rows"][1]["role"], "agent.final_answer");
}

#[test]
fn text_search_returns_candidates_that_can_be_exact_checked() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(
        directory.path(),
        "needle phrase for search 和索引 CamelCaseId a1b2c3d4e5",
    );
    let result = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT json_extract(i.semantic, '$.role') AS role FROM item_search JOIN items AS i ON i.item_id = item_search.rowid JOIN blobs AS b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id') WHERE item_search MATCH search_query('needle phrase') AND instr(b.text, 'needle phrase') > 0",
    ]));
    assert_eq!(result["data"]["returned"], 1);
    assert_eq!(result["data"]["rows"][0]["role"], "human.request");

    let term_result = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT json_extract(i.semantic, '$.role') AS role, bm25(item_search) AS score FROM item_search(search_query('索引 camel a1b2c3*')) JOIN items AS i ON i.item_id = item_search.rowid JOIN blobs AS b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id') WHERE instr(b.text, '索引') > 0 ORDER BY score",
    ]));
    assert_eq!(term_result["data"]["returned"], 1);
    assert_eq!(term_result["data"]["rows"][0]["role"], "human.request");
}

#[test]
fn multiline_sql_works_from_stdin_and_output_is_bounded() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "multiline request");
    let sql = "SELECT\n  json_extract(semantic, '$.role') AS role,\n  COUNT(*) AS item_count\nFROM items\nGROUP BY role\nORDER BY role\n";
    let result = compact_json(&run_with_stdin(
        &["--db", path_text(&database), "query", "run", "--stdin"],
        sql,
    ));
    assert_eq!(result["data"]["returned"], 2);
    assert!(result["data"].get("hint").is_none());

    let bounded = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT printf('%02000d', 1) AS value",
        "--max-output-bytes",
        "1024",
    ]));
    assert_eq!(bounded["data"]["complete"], false);
    assert_eq!(bounded["data"]["incomplete_reason"], "output_budget");
    assert!(
        bounded["data"]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("shorter snippets"))
    );
    assert_eq!(bounded["data"]["returned"], 0);

    let row_limited = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT 1 AS value UNION ALL SELECT 2",
        "--limit",
        "1",
    ]));
    assert_eq!(row_limited["data"]["complete"], false);
    assert_eq!(row_limited["data"]["incomplete_reason"], "row_limit");
    assert!(
        row_limited["data"]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("stable, unique ORDER BY key"))
    );

    let cell_limited = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT printf('%0100d', 1) AS value",
        "--max-cell-bytes",
        "8",
    ]));
    assert_eq!(cell_limited["data"]["complete"], true);
    assert_eq!(cell_limited["data"]["cells_truncated"], 1);
    assert!(
        cell_limited["data"]["hint"]
            .as_str()
            .is_some_and(|hint| hint.contains("prefixes only"))
    );
}

#[test]
fn query_run_is_read_only_and_does_not_escape_the_database() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "safe query");
    for sql in [
        "DELETE FROM domain_items",
        "ATTACH DATABASE '/tmp/other.sqlite' AS other",
    ] {
        let output = run(&["--db", path_text(&database), "query", "run", sql]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        assert!(!output.stderr.is_empty());
    }
}

#[test]
fn record_inspection_verifies_the_original_source_bytes() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "inspect me");
    let selected = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT record_id FROM records WHERE source_position = 0",
    ]));
    let record_id = selected["data"]["rows"][0]["record_id"]
        .as_i64()
        .expect("Record id")
        .to_string();

    let inspected = compact_json(&run(&[
        "--db",
        path_text(&database),
        "record",
        "inspect",
        &record_id,
    ]));
    assert_eq!(inspected["data"]["raw_verified"], true);
    assert_eq!(inspected["data"]["representation"]["type"], "session_meta");

    let verified = compact_json(&run(&[
        "--db",
        path_text(&database),
        "record",
        "verify",
        &record_id,
    ]));
    assert_eq!(verified["data"]["status"], "verified");
}

#[test]
fn record_verification_reports_a_missing_source_as_data() {
    let directory = tempdir().expect("temporary directory");
    let (database, trace, _) = index_fixture(directory.path(), "inspect me");
    let selected = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT record_id FROM records WHERE source_position = 0",
    ]));
    let record_id = selected["data"]["rows"][0]["record_id"]
        .as_i64()
        .expect("Record id")
        .to_string();
    fs::remove_file(trace).expect("remove Source");

    let verified = compact_json(&run(&[
        "--db",
        path_text(&database),
        "record",
        "verify",
        &record_id,
    ]));
    assert_eq!(verified["data"]["status"], "source_missing");
    assert_eq!(verified["data"]["actual_bytes"], 0);
    assert!(verified["data"].get("actual_hash").is_none());

    let inspected = compact_json(&run(&[
        "--db",
        path_text(&database),
        "record",
        "inspect",
        &record_id,
    ]));
    assert_eq!(inspected["data"]["raw_verified"], false);
    assert_eq!(
        inspected["data"]["representation"]["$trace_ref"]["reason"],
        "source_missing"
    );
}

#[test]
fn a_trace_without_confirmed_session_identity_is_skipped_atomically() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("index.sqlite");
    let trace = directory.path().join("invalid.jsonl");
    fs::write(
        &trace,
        "{\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"orphan\"}}\n",
    )
    .expect("write invalid Source");
    let report = compact_json(&run(&[
        "--db",
        path_text(&database),
        "index",
        "sync",
        path_text(&trace),
        "--progress",
        "off",
        "--include-sources",
    ]));
    assert_eq!(report["data"]["skipped_files"], 1);
    assert_eq!(report["data"]["failed_files"], 0);

    let state = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT (SELECT COUNT(*) FROM sources) AS sources,
                (SELECT COUNT(*) FROM records) AS records,
                (SELECT COUNT(*) FROM sessions) AS sessions",
    ]));
    assert_eq!(state["data"]["rows"][0]["sources"], 0);
    assert_eq!(state["data"]["rows"][0]["records"], 0);
    assert_eq!(state["data"]["rows"][0]["sessions"], 0);
}

#[test]
fn ndjson_progress_stays_on_stderr_and_final_json_stays_on_stdout() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("index.sqlite");
    let trace = directory.path().join("trace.jsonl");
    write_codex_trace(&trace, "progress request", "progress answer");
    let output = run(&[
        "--db",
        path_text(&database),
        "index",
        "sync",
        path_text(&trace),
        "--progress",
        "ndjson",
    ]);
    assert!(output.status.success());
    serde_json::from_slice::<Value>(&output.stdout).expect("final stdout JSON");
    let progress = String::from_utf8(output.stderr).expect("progress UTF-8");
    assert!(!progress.is_empty());
    for line in progress.lines() {
        let event: Value = serde_json::from_str(line).expect("progress NDJSON");
        assert_eq!(event["type"], "progress");
    }
}

#[test]
fn bundled_docs_are_discoverable_without_an_index() {
    let listed = compact_json(&run(&["docs", "list"]));
    assert!(
        listed["data"]["topics"]
            .as_array()
            .expect("topics")
            .iter()
            .any(|topic| topic["name"] == "start-here")
    );
    let topic = run(&["docs", "get", "start-here"]);
    assert!(topic.status.success());
    assert!(topic.stderr.is_empty());
    let topic = String::from_utf8_lossy(&topic.stdout);
    assert!(topic.contains("`semantic = {role, value, evidence_strength}`"));
}

#[test]
fn removed_flat_commands_and_legacy_relations_are_not_resurrected() {
    for arguments in [
        vec!["query", "SELECT 1"],
        vec!["help"],
        vec!["asset", "inspect", "1#/payload/image"],
        vec!["index", "--help"],
        vec!["event", "get", "1"],
    ] {
        let output = run(&arguments);
        if arguments == ["index", "--help"] {
            assert!(output.status.success());
            continue;
        }
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
    }

    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "legacy check");
    for relation in ["turns_view", "items_view", "tool_calls", "content_blobs"] {
        let output = run(&["--db", path_text(&database), "schema", "get", relation]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn error_recovery_hints_point_to_public_contract_discovery() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "hello world");

    let unknown_flag = run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "--sql",
        "SELECT 1",
    ]);
    assert_eq!(unknown_flag.status.code(), Some(2));
    assert!(unknown_flag.stdout.is_empty());
    let stderr = String::from_utf8_lossy(&unknown_flag.stderr);
    assert!(stderr.contains("positional argument, '--stdin', or '--file <PATH>'"));
    assert!(stderr.contains("trace-index query run --help"));

    for bad_cli in [
        vec!["docs", "list", "--bad"],
        vec!["--badflag", "query", "run", "SELECT 1"],
    ] {
        let output = run(&bad_cli);
        assert_eq!(output.status.code(), Some(2));
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("trace-index <noun> --help"));
        assert!(!stderr.contains("pass SQL as a positional argument"));
    }

    for missing_sql in [
        "SELECT nonexistent_col FROM sessions",
        "SELECT * FROM nonexistent_relation",
    ] {
        let output = run(&["--db", path_text(&database), "query", "run", missing_sql]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains("trace-index schema list"));
        assert!(stderr.contains("trace-index schema get <RELATION> --compact"));
    }

    for fts_sql in [
        "SELECT rowid FROM item_search WHERE text MATCH '\"hello world\"'",
        "SELECT rowid FROM item_search WHERE text MATCH 'text:hello'",
        "SELECT rowid FROM item_search WHERE text MATCH 'hello-world'",
    ] {
        let output = run(&["--db", path_text(&database), "query", "run", fts_sql]);
        assert!(!output.status.success());
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("item_search(search_query('...'))"),
            "missing search_query hint for `{fts_sql}`: {stderr}"
        );
        assert!(stderr.contains("how-to/search-literals"));
    }
}

#[test]
fn index_watch_status_and_service_dry_run_are_non_mutating() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("index.sqlite");
    let config = directory.path().join("config.toml");
    let traces_dir = directory.path().join("traces");
    fs::create_dir_all(&traces_dir).expect("create traces dir");

    // Watch status works even before the database exists, without creating it.
    let status_before = compact_json(&run(&[
        "--db",
        path_text(&database),
        "index",
        "watch",
        "status",
    ]));
    assert_eq!(status_before["data"]["active"], false);
    assert_eq!(status_before["data"]["service_installed"], false);
    assert!(!database.exists());

    // Initialize explicit config, then preview install/uninstall via --dry-run.
    let init = run(&[
        "--config",
        path_text(&config),
        "--db",
        path_text(&database),
        "config",
        "init",
        "--root",
        path_text(&traces_dir),
    ]);
    assert!(init.status.success());

    let install_preview = compact_json(&run(&[
        "--config",
        path_text(&config),
        "--db",
        path_text(&database),
        "index",
        "watch",
        "install",
        "--dry-run",
    ]));
    assert_eq!(install_preview["data"]["action"], "install");
    assert_eq!(install_preview["data"]["dry_run"], true);
    assert!(install_preview["data"].get("loaded").is_none());
    let definition = install_preview["data"]["definition"]
        .as_str()
        .expect("dry-run definition");
    assert!(definition.contains("watch"));
    assert!(definition.contains("run"));

    let uninstall_preview = compact_json(&run(&[
        "--config",
        path_text(&config),
        "--db",
        path_text(&database),
        "index",
        "watch",
        "uninstall",
        "--dry-run",
    ]));
    assert_eq!(uninstall_preview["data"]["action"], "uninstall");
    assert_eq!(uninstall_preview["data"]["dry_run"], true);

    // Sync a fixture and verify `index status` exposes the live `watch` object.
    let (indexed_db, _, _) = index_fixture(directory.path(), "watch status check");
    let index_status = compact_json(&run(&["--db", path_text(&indexed_db), "index", "status"]));
    assert_eq!(index_status["data"]["watch"]["active"], false);
    assert_eq!(index_status["data"]["watch"]["service_installed"], false);
}

#[test]
fn schema_get_accepts_multiple_relations_and_query_run_supports_tsv_output() {
    let directory = tempdir().expect("temporary directory");
    let (database, _, _) = index_fixture(directory.path(), "tsv output fixture");

    // Multi-relation `schema get` preserves requested order and deduplicates names.
    let multi_schema = compact_json(&run(&[
        "schema",
        "get",
        "items",
        "sessions",
        "loops",
        "items",
        "--compact",
    ]));
    let objects = multi_schema["data"]["objects"]
        .as_array()
        .expect("schema objects array");
    let names: Vec<&str> = objects
        .iter()
        .map(|object| object["name"].as_str().expect("object name"))
        .collect();
    assert_eq!(names, ["items", "sessions", "loops"]);
    assert!(
        objects
            .iter()
            .all(|object| object.get("description").is_none())
    );
    assert!(
        objects
            .iter()
            .all(|object| !object["columns"].as_array().expect("columns").is_empty())
    );

    // Unknown relation in a multi-relation list fails cleanly.
    let unknown = run(&["schema", "get", "sessions", "does_not_exist"]);
    assert!(!unknown.status.success());
    assert!(String::from_utf8_lossy(&unknown.stderr).contains("trace-index schema list"));

    // `--format tsv` preserves SQL column order, renders NULL as empty, and escapes tabs/newlines/backslashes.
    let tsv_output = run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "--format",
        "tsv",
        "SELECT 1 AS z_num, NULL AS a_null, 'line1' || char(10) || 'col2' || char(9) || 'slash\\' || char(13) AS m_text",
    ]);
    assert!(
        tsv_output.status.success(),
        "tsv query failed: {}",
        String::from_utf8_lossy(&tsv_output.stderr)
    );
    assert!(
        tsv_output.stderr.is_empty(),
        "complete untruncated TSV query should emit no stderr notice: {}",
        String::from_utf8_lossy(&tsv_output.stderr)
    );
    let stdout = String::from_utf8(tsv_output.stdout).expect("utf-8 tsv");
    assert_eq!(
        stdout,
        "z_num\ta_null\tm_text\n1\t\tline1\\ncol2\\tslash\\\\\\r\n"
    );

    // Incomplete or cell-truncated TSV writes a notice to stderr while keeping stdout pure TSV.
    let truncated = run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "--format",
        "tsv",
        "--limit",
        "1",
        "--max-cell-bytes",
        "8",
        "SELECT item_id, json_extract(semantic, '$.role') AS role FROM items ORDER BY item_id",
    ]);
    assert!(truncated.status.success());
    let truncated_stdout = String::from_utf8(truncated.stdout).expect("utf-8 truncated tsv");
    let lines: Vec<&str> = truncated_stdout.lines().collect();
    assert_eq!(lines.len(), 2, "header plus one row: {truncated_stdout:?}");
    assert_eq!(lines[0], "item_id\trole");
    let truncated_stderr = String::from_utf8(truncated.stderr).expect("utf-8 stderr");
    assert!(
        truncated_stderr.contains("notice: TSV output is incomplete"),
        "expected stderr truncation notice, got: {truncated_stderr:?}"
    );
    assert!(truncated_stderr.contains("reason=row_limit"));
    assert!(truncated_stderr.contains("cells_truncated=1"));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "end-to-end external-process contract check for templates, --render forking, and config check"
)]
fn query_templates_and_parameterized_query_run_work_end_to_end() {
    let directory = tempdir().expect("temporary directory");
    let config_path = directory.path().join("config.toml");
    let unused_db = directory.path().join("never-created.sqlite");

    // `query template list` and `show` work offline without creating any database or config file.
    let list = compact_json(&run(&[
        "--db",
        path_text(&unused_db),
        "query",
        "template",
        "list",
    ]));
    assert!(!unused_db.exists());
    let template_names: Vec<&str> = list["data"]["templates"]
        .as_array()
        .expect("templates array")
        .iter()
        .map(|entry| entry["name"].as_str().expect("template name"))
        .collect();
    for expected in [
        "item-context",
        "recent-sessions",
        "search-items",
        "search-sessions",
        "session-transcript",
        "shell-calls",
    ] {
        assert!(
            template_names.contains(&expected),
            "missing built-in template {expected} in {template_names:?}"
        );
    }

    let show = compact_json(&run(&[
        "--db",
        path_text(&unused_db),
        "query",
        "template",
        "show",
        "session-transcript",
    ]));
    assert_eq!(show["data"]["name"], "session-transcript");
    assert_eq!(show["data"]["origin"]["kind"], "builtin");
    assert!(!unused_db.exists());

    // Create a minimal explicit config for template dir + config check tests.
    fs::write(&config_path, "schema_version = 1\n").expect("write config.toml");

    // Index a fixture and exercise `--render` -> lossless `query run --params-json` forking.
    let (database, _, _) = index_fixture(directory.path(), "template e2e probe");
    let rendered = compact_json(&run(&[
        "--config",
        path_text(&config_path),
        "--db",
        path_text(&database),
        "query",
        "template",
        "run",
        "session-transcript",
        "-p",
        "session=1",
        "--render",
    ]));
    assert_eq!(rendered["data"]["template"], "session-transcript");
    assert_eq!(rendered["data"]["resolved"]["session"]["session_id"], 1);
    let rendered_sql = rendered["data"]["sql"].as_str().expect("rendered sql");
    let rendered_params = serde_json::to_string(&rendered["data"]["params"]).expect("params json");

    let forked = compact_json(&run_with_stdin(
        &[
            "--db",
            path_text(&database),
            "query",
            "run",
            "--stdin",
            "--params-json",
            &rendered_params,
        ],
        rendered_sql,
    ));
    assert!(forked["data"]["returned"].as_u64().expect("returned") >= 1);

    // Execute template directly with `--format tsv` and verify clean stdout.
    let tsv = run(&[
        "--config",
        path_text(&config_path),
        "--db",
        path_text(&database),
        "query",
        "template",
        "run",
        "recent-sessions",
        "--format",
        "tsv",
    ]);
    assert!(
        tsv.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&tsv.stderr)
    );
    let tsv_text = String::from_utf8(tsv.stdout).expect("utf-8 tsv");
    assert!(tsv_text.starts_with("session_id\truntime\tnative_id\t"));
    assert_eq!(tsv_text.lines().count(), 2);

    // User template under <config_dir>/templates/ loads cleanly; invalid template fails `config check`.
    let templates_dir = directory.path().join("templates");
    fs::create_dir_all(&templates_dir).expect("mkdir templates");
    fs::write(
        templates_dir.join("count-items.sql"),
        "/* trace-index-template\nname = \"count-items\"\ndescription = \"Count items\"\n*/\nSELECT COUNT(*) AS total FROM items\n",
    )
    .expect("write user template");
    let custom_run = compact_json(&run(&[
        "--config",
        path_text(&config_path),
        "--db",
        path_text(&database),
        "query",
        "template",
        "run",
        "count-items",
    ]));
    assert_eq!(custom_run["data"]["origin"]["kind"], "user");
    assert!(
        custom_run["data"]["rows"][0]["total"]
            .as_u64()
            .expect("total")
            >= 1
    );

    fs::write(
        templates_dir.join("bad-shadow.sql"),
        "/* trace-index-template\nname = \"shell-calls\"\ndescription = \"Attempt shadow\"\n*/\nSELECT 1 AS x\n",
    )
    .expect("write shadowing template");
    let check_out = run(&[
        "--config",
        path_text(&config_path),
        "--db",
        path_text(&database),
        "config",
        "check",
    ]);
    assert!(!check_out.status.success());
    let check_json: Value = serde_json::from_slice(&check_out.stdout).expect("config check json");
    assert_eq!(check_json["data"]["valid"], false);
    assert!(
        check_json["data"]["issues"]
            .as_array()
            .expect("issues")
            .iter()
            .any(|issue| issue["code"] == "invalid_query_template")
    );
}
