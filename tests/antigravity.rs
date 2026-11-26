use std::fs::{self, OpenOptions};
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};
use tempfile::TempDir;

const ID: &str = "12345678-1234-4321-abcd-1234567890ab";

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_trace-index"));
    command
        .arg("--db")
        .arg(root.join("index.sqlite"))
        .arg("--metadata-db")
        .arg(root.join("metadata.sqlite"));
    command
}

fn data(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
}

fn query(root: &Path, sql: &str) -> Value {
    data(&command(root).args(["query", "run", sql]).output().unwrap())["rows"].clone()
}

fn sync(root: &Path, source: &Path) -> Value {
    data(
        &command(root)
            .args(["index", "sync", "--include-sources"])
            .arg(source)
            .output()
            .unwrap(),
    )
}

fn record(step: u64, source: &str, kind: &str, payload: &Value) -> String {
    let mut value = json!({
        "step_index": step, "source": source, "type": kind, "status": "DONE",
        "created_at": "2026-01-02T00:00:00.123456Z"
    });
    value
        .as_object_mut()
        .unwrap()
        .extend(payload.as_object().unwrap().clone());
    format!("{value}\n")
}

#[test]
fn indexes_an_antigravity_session_and_promotes_a_short_transcript_atomically() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let brain = root.join("brain");
    let logs = brain.join(ID).join(".system_generated/logs");
    fs::create_dir_all(logs.join("chunks/full")).unwrap();
    let short = logs.join("transcript.jsonl");
    let full = logs.join("transcript_full.jsonl");
    let request = record(
        0,
        "USER_EXPLICIT",
        "USER_INPUT",
        &json!({"content":"show results"}),
    );
    let planner = record(
        2,
        "MODEL",
        "PLANNER_RESPONSE",
        &json!({
            "content":"Here are the results", "model":"test-model",
            "thinking":"Check the file",
            "tool_calls":[{"name":"run_command","args":{"CommandLine":"printf ok | cat; printf err 2>errors.log","Cwd":"/repo"}}]
        }),
    );
    fs::write(&short, &request).unwrap();
    fs::write(logs.join("chunks/full/0.jsonl"), &request).unwrap();
    assert_eq!(sync(root, &brain)["indexed_records"], 1);
    assert_eq!(
        query(
            root,
            "SELECT runtime,native_id,working_directory FROM sessions"
        ),
        json!([{"runtime":"antigravity","native_id":ID,"working_directory":null}])
    );

    fs::write(&full, "invalid\n").unwrap();
    assert_eq!(sync(root, &brain)["skipped_files"], 1);
    assert_eq!(
        query(root, "SELECT COUNT(*) AS n FROM items"),
        json!([{"n":1}])
    );

    fs::write(&full, format!("{request}{planner}")).unwrap();
    let report = sync(root, &brain);
    assert_eq!(report["discovered_files"], 1);
    assert_eq!(report["indexed_records"], 2);
    assert_eq!(
        query(root, "SELECT COUNT(*) AS n FROM records"),
        json!([{"n":2}])
    );
    let sources = query(root, "SELECT locator FROM sources");
    assert_eq!(sources.as_array().unwrap().len(), 1);
    assert!(
        sources[0]["locator"]
            .as_str()
            .unwrap()
            .ends_with("/transcript_full.jsonl")
    );
    assert_eq!(sync(root, &short)["skipped_files"], 1);

    assert_projection(root);
    assert_incremental(root, &brain, &full);
}

fn assert_projection(root: &Path) {
    let projected = query(
        root,
        "SELECT json_extract(semantic,'$.value.working_directory') AS cwd,
                json_extract(semantic,'$.value.shell_fragments[0].statements[1].invocations[0].program') AS program,
                json_extract(semantic,'$.value.shell_fragments[0].statements[1].pipeline.position') AS pipeline_position,
                json_extract(semantic,'$.value.shell_fragments[0].statements[2].redirects[0].target') AS redirect
           FROM items WHERE json_extract(semantic,'$.role')='agent.tool_call.shell'",
    );
    assert_eq!(
        projected,
        json!([{"cwd":"/repo","program":"cat","pipeline_position":1,"redirect":"errors.log"}])
    );
    let loops = query(
        root,
        "SELECT session_position,end,usage,json_extract(model,'$.id') AS model FROM loops",
    );
    assert_eq!(
        loops,
        json!([{"session_position":0,"end":null,"usage":null,"model":"test-model"}])
    );
    assert_eq!(
        query(
            root,
            "SELECT COUNT(*) AS n FROM item_search(search_query('results'))"
        ),
        json!([{"n":2}])
    );
}

fn assert_incremental(root: &Path, brain: &Path, full: &Path) {
    assert_eq!(sync(root, brain)["metrics"]["writes"]["records"], 0);
    let before = query(root, "SELECT record_id FROM records ORDER BY record_id");
    let lookup = query(
        root,
        "SELECT record_ids FROM items WHERE json_extract(semantic,'$.role')='agent.tool_call.shell'",
    );
    let records: Vec<i64> =
        serde_json::from_str(lookup[0]["record_ids"].as_str().unwrap()).unwrap();
    let verified = data(
        &command(root)
            .args(["record", "verify", &records[0].to_string()])
            .output()
            .unwrap(),
    );
    assert_eq!(verified["status"], "verified");

    OpenOptions::new()
        .append(true)
        .open(full)
        .unwrap()
        .write_all(
            record(
                2,
                "MODEL",
                "GENERIC",
                &json!({"content":"opaque display row"}),
            )
            .as_bytes(),
        )
        .unwrap();
    assert_eq!(sync(root, brain)["indexed_records"], 1);
    let after = query(root, "SELECT record_id FROM records ORDER BY record_id");
    assert!(
        after
            .as_array()
            .unwrap()
            .starts_with(before.as_array().unwrap())
    );
    assert_eq!(
        query(root, "SELECT COUNT(*) AS n FROM items"),
        json!([{"n":4}])
    );
}
