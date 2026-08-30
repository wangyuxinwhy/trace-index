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
    fs::create_dir_all(&codex).expect("create existing Codex root");
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
    assert_eq!(roots.len(), 1);
    assert_eq!(roots[0]["label"], "codex");
    assert_eq!(roots[0]["path"], path_text(&codex));
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
    assert!(sync.contains("This is the command that writes the index"));
    assert!(sync.contains("Without PATHS, it uses the configured [[roots]]"));
    assert!(sync.contains("database-wide policy"));
    assert!(sync.contains("Changing policy requires a new"));
    assert!(sync.contains("progress is written to stderr"));

    let status = run(&["index", "status", "--help"]);
    assert!(status.status.success());
    let status = String::from_utf8(status.stdout).expect("UTF-8 status help");
    assert!(status.contains("existing database read-only"));
    assert!(status.contains("does not discover or"));
    assert!(status.contains("stored indexing policy"));

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
            "item_search"
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
    let (database, _, _) = index_fixture(directory.path(), "needle phrase for search");
    let result = compact_json(&run(&[
        "--db",
        path_text(&database),
        "query",
        "run",
        "SELECT json_extract(i.semantic, '$.role') AS role FROM item_search JOIN items AS i ON i.item_id = item_search.rowid JOIN blobs AS b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id') WHERE item_search MATCH trigram_query('needle phrase') AND instr(b.text, 'needle phrase') > 0",
    ]));
    assert_eq!(result["data"]["returned"], 1);
    assert_eq!(result["data"]["rows"][0]["role"], "human.request");
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
