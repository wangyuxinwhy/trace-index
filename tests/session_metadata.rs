//! External-process contracts for caller-owned JSON and rebuild-safe associations.

use serde_json::{Value, json};
use std::fs;
use std::path::Path;
use std::process::{Command, Output};
use tempfile::tempdir;

fn command(root: &Path) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_trace-index"));
    c.env_remove("TRACE_INDEX_CONFIG")
        .env_remove("TRACE_INDEX_DB")
        .env_remove("TRACE_INDEX_METADATA_DB")
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_DATA_HOME", root.join("data"))
        .arg("--db")
        .arg(root.join("index.sqlite"))
        .arg("--metadata-db")
        .arg(root.join("metadata.sqlite"));
    c
}
fn run(root: &Path, args: &[&str]) -> Output {
    command(root).args(args).output().expect("CLI")
}
fn data(output: Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str::<Value>(&String::from_utf8(output.stdout).expect("UTF-8")).expect("JSON")
        ["data"]
        .clone()
}
fn document(root: &Path, verb: &str, value: Option<&str>, version: Option<&str>) -> Output {
    let mut c = command(root);
    c.args([
        "session",
        "metadata",
        verb,
        "--runtime",
        "pi",
        "--native-id",
        "target",
    ]);
    if let Some(value) = value {
        c.args(["--json", value]);
    }
    if let Some(version) = version {
        c.args(["--if-version", version]);
    }
    c.output().expect("CLI")
}
fn seed(root: &Path) {
    let trace = root.join("pi.jsonl");
    fs::write(
        &trace,
        format!(
            "{}\n{}\n",
            json!({"type":"session","version":3,"id":"target","cwd":"/workspace"}),
            json!({"type":"message","message":{"role":"user","content":"Hello"}})
        ),
    )
    .expect("trace");
    data(
        command(root)
            .args(["index", "sync"])
            .arg(trace)
            .output()
            .expect("sync"),
    );
}

#[test]
fn raw_json_null_absence_replacement_and_tombstone_versions() {
    let d = tempdir().unwrap();
    let r = d.path();
    let absent = data(document(r, "get", None, None));
    assert_eq!(absent["found"], false);
    assert!(!r.join("metadata.sqlite").exists());
    assert!(!document(r, "set", Some("{broken"), None).status.success());
    assert!(!r.join("metadata.sqlite").exists());
    assert_eq!(
        data(document(r, "set", Some("null"), Some("0")))["version"],
        1
    );
    let null = data(document(r, "get", None, None));
    assert_eq!(null["found"], true);
    assert_eq!(null["metadata"], Value::Null);
    assert_eq!(
        data(document(r, "set", Some("null"), Some("1")))["changed"],
        false
    );
    assert!(
        !document(r, "set", Some("[1,true]"), Some("0"))
            .status
            .success()
    );
    assert_eq!(data(document(r, "delete", None, Some("1")))["version"], 2);
    let deleted = data(document(r, "get", None, None));
    assert_eq!(deleted["found"], false);
    assert_eq!(deleted["version"], 2);
    assert!(!document(r, "set", Some("{}"), Some("0")).status.success());
    for value in [
        "[]",
        "true",
        "42",
        "\"string\"",
        "{\"arbitrary\":{\"深层\":[null,2]}}",
    ] {
        data(document(r, "set", Some(value), None));
        assert_eq!(
            data(document(r, "get", None, None))["metadata"],
            serde_json::from_str::<Value>(value).unwrap()
        );
    }
    let raw =
        "{\"duplicate\":1,\"duplicate\":2,\"large\":123456789012345678901234567890,\"x\":1e+100}";
    data(document(r, "set", Some(raw), None));
    let output = document(r, "get", None, None);
    assert!(output.status.success());
    assert!(String::from_utf8(output.stdout).unwrap().contains(raw));
    assert!(!r.join("index.sqlite").exists());
}

#[test]
fn query_is_live_read_only_and_association_survives_index_deletion() {
    let d = tempdir().unwrap();
    let r = d.path();
    seed(r);
    let empty = data(run(
        r,
        &["query", "run", "SELECT COUNT(*) AS n FROM session_metadata"],
    ));
    assert_eq!(empty["rows"][0]["n"], 0);
    assert!(!r.join("metadata.sqlite").exists());
    data(document(
        r,
        "set",
        Some("{\"anything\":[\"a\",\"b\"]}"),
        None,
    ));
    let sql = "SELECT s.native_id,json_extract(m.metadata,'$.anything[1]') AS value FROM session_metadata m JOIN sessions s USING(session_id)";
    assert_eq!(
        data(run(r, &["query", "run", sql]))["rows"][0]["value"],
        "b"
    );
    let attach = format!("ATTACH '{}' AS other", r.join("other.sqlite").display());
    assert!(!run(r, &["query", "run", &attach]).status.success());
    assert!(!r.join("other.sqlite").exists());
    assert!(
        !run(
            r,
            &[
                "query",
                "run",
                "UPDATE caller_metadata.session_documents SET metadata='null'"
            ]
        )
        .status
        .success()
    );
    data(run(
        r,
        &[
            "session",
            "metadata",
            "set",
            "--runtime",
            "codex",
            "--native-id",
            "target",
            "--json",
            "false",
        ],
    ));
    let rows = data(run(
        r,
        &[
            "query",
            "run",
            "SELECT runtime,session_id FROM session_metadata ORDER BY runtime",
        ],
    ));
    assert_eq!(rows["rows"].as_array().unwrap().len(), 2);
    assert_eq!(rows["rows"][0]["session_id"], Value::Null);
    fs::remove_file(r.join("index.sqlite")).unwrap();
    seed(r);
    assert_eq!(
        data(run(r, &["query", "run", sql]))["rows"][0]["value"],
        "b"
    );
    data(run(
        r,
        &[
            "index",
            "sync",
            "--rebuild",
            r.join("pi.jsonl").to_str().unwrap(),
        ],
    ));
    assert_eq!(
        data(run(r, &["query", "run", sql]))["rows"][0]["value"],
        "b"
    );
    fs::write(r.join("metadata.sqlite"), "not SQLite").unwrap();
    assert!(
        !run(r, &["query", "run", "SELECT * FROM session_metadata"])
            .status
            .success()
    );
}

#[test]
fn concurrent_compare_and_set_has_one_winner() {
    let d = tempdir().unwrap();
    let r = d.path();
    data(document(r, "set", Some("0"), None));
    let spawn = |value: &str| {
        command(r)
            .args([
                "session",
                "metadata",
                "set",
                "--runtime",
                "pi",
                "--native-id",
                "target",
                "--json",
                value,
                "--if-version",
                "1",
            ])
            .spawn()
            .unwrap()
    };
    let mut a = spawn("1");
    let mut b = spawn("2");
    let successes =
        usize::from(a.wait().unwrap().success()) + usize::from(b.wait().unwrap().success());
    assert_eq!(successes, 1);
    assert_eq!(data(document(r, "get", None, None))["version"], 2);
}

#[test]
fn selectors_and_schema_discovery_do_not_create_databases() {
    let d = tempdir().unwrap();
    let r = d.path();
    let schema = data(run(r, &["schema", "get", "session_metadata"]));
    assert_eq!(schema["objects"][0]["columns"].as_array().unwrap().len(), 6);
    assert!(!r.join("index.sqlite").exists());
    assert!(!r.join("metadata.sqlite").exists());
    let show = data(
        command(r)
            .env("TRACE_INDEX_METADATA_DB", "")
            .args(["config", "show"])
            .output()
            .unwrap(),
    );
    assert_eq!(show["metadata_database"]["origin"]["kind"], "cli");
    assert_eq!(
        show["metadata_database"]["path"],
        r.join("metadata.sqlite").to_str().unwrap()
    );
    let shared = Command::new(env!("CARGO_BIN_EXE_trace-index"))
        .env_remove("TRACE_INDEX_CONFIG")
        .env_remove("TRACE_INDEX_METADATA_DB")
        .env("XDG_CONFIG_HOME", r.join("config"))
        .arg("--db")
        .arg(r.join("not-created/../index.sqlite"))
        .arg("--metadata-db")
        .arg(r.join("index.sqlite"))
        .args([
            "session",
            "metadata",
            "set",
            "--runtime",
            "pi",
            "--native-id",
            "target",
            "--json",
            "{}",
        ])
        .output()
        .unwrap();
    assert!(!shared.status.success());
    assert!(
        String::from_utf8_lossy(&shared.stderr)
            .contains("index and metadata database paths must differ")
    );
    assert!(!r.join("index.sqlite").exists());
}

#[test]
fn three_runtimes_move_fork_and_changed_join_keys_keep_user_documents() {
    let d = tempdir().unwrap();
    let r = d.path().join("space?#");
    fs::create_dir_all(&r).unwrap();
    let fixtures = [
        (
            "a-codex.jsonl",
            json!({"type":"session_meta","payload":{"id":"target","cwd":"/workspace"}}),
        ),
        (
            "b-claude.jsonl",
            json!({"type":"user","uuid":"request","sessionId":"target","cwd":"/workspace","message":{"role":"user","content":"Hello"}}),
        ),
        (
            "c-pi.jsonl",
            json!({"type":"session","version":3,"id":"target","cwd":"/workspace"}),
        ),
    ];
    for (name, header) in fixtures {
        fs::write(r.join(name), format!("{header}\n")).unwrap();
    }
    let input = r.join("document.json");
    fs::write(&input, "{\"arbitrary\":42}").unwrap();
    for runtime in ["codex", "claude", "pi"] {
        data(
            command(&r)
                .args([
                    "session",
                    "metadata",
                    "set",
                    "--runtime",
                    runtime,
                    "--native-id",
                    "target",
                    "--file",
                ])
                .arg(&input)
                .output()
                .unwrap(),
        );
    }
    let sync = || {
        data(
            command(&r)
                .args(["index", "sync"])
                .arg(&r)
                .output()
                .unwrap(),
        )
    };
    sync();
    let sql = "SELECT s.runtime,s.session_id,json_extract(m.metadata,'$.arbitrary') AS value FROM sessions s JOIN session_metadata m USING(session_id) ORDER BY s.runtime";
    let before = data(run(&r, &["query", "run", sql]));
    assert_eq!(before["returned"], 3);
    fs::rename(r.join("c-pi.jsonl"), r.join("moved-pi.jsonl")).unwrap();
    fs::write(
        r.join("0-extra.jsonl"),
        format!(
            "{}\n",
            json!({"type":"session","version":3,"id":"extra","cwd":"/workspace"})
        ),
    )
    .unwrap();
    fs::write(r.join("z-child.jsonl"),format!("{}\n",json!({"type":"session","version":3,"id":"child","cwd":"/workspace","parentSession":r.join("moved-pi.jsonl")}))).unwrap();
    fs::remove_file(r.join("index.sqlite")).unwrap();
    sync();
    let after = data(run(&r, &["query", "run", sql]));
    assert_eq!(after["returned"], 3);
    assert_pi_fork(&r);
    for (old, new) in before["rows"]
        .as_array()
        .unwrap()
        .iter()
        .zip(after["rows"].as_array().unwrap())
    {
        assert_eq!(new["value"], 42);
        assert_eq!(new["runtime"], old["runtime"]);
        assert_ne!(new["session_id"], old["session_id"]);
    }
    let child = data(run(
        &r,
        &[
            "query",
            "run",
            "SELECT COUNT(*) AS n FROM session_metadata WHERE native_id='child' OR native_id='extra'",
        ],
    ));
    assert_eq!(child["rows"][0]["n"], 0);
    replace_via_stdin(&r);
    assert_eq!(
        data(document(&r, "get", None, None))["metadata"],
        json!([true, null, 3])
    );
}

fn replace_via_stdin(r: &std::path::Path) {
    use std::io::Write as _;
    let mut c = command(r);
    let mut child = c
        .args([
            "session",
            "metadata",
            "set",
            "--runtime",
            "pi",
            "--native-id",
            "target",
            "--stdin",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"[true,null,3]")
        .unwrap();
    data(child.wait_with_output().unwrap());
}

fn assert_pi_fork(r: &Path) {
    let parent = data(run(
        r,
        &[
            "query",
            "run",
            "SELECT p.runtime,p.native_id FROM sessions c JOIN sessions p ON p.session_id=json_extract(c.forked_from,'$.session_id') WHERE c.runtime='pi' AND c.native_id='child'",
        ],
    ));
    assert_eq!(parent["returned"], 1);
    assert_eq!(parent["rows"][0]["native_id"], "target");
    assert_eq!(parent["rows"][0]["runtime"], "pi");
}
