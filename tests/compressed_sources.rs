use std::fs;
use std::io::Write as _;
use std::path::Path;
use std::process::Command;

use serde_json::{Value, json};
use tempfile::TempDir;

const HEADER: &str = "{\"timestamp\":\"2026-01-02T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{\"id\":\"encoded-session\",\"cwd\":\"/repo\"}}\n";
const FIRST: &str = "{\"timestamp\":\"2026-01-02T00:00:01Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"first request\"}}\n";
const SECOND: &str = "{\"timestamp\":\"2026-01-02T00:00:02Z\",\"type\":\"event_msg\",\"payload\":{\"type\":\"user_message\",\"message\":\"second request\"}}\n";

fn invoke(root: &Path, arguments: &[&str], success: bool) -> Value {
    let output = Command::new(env!("CARGO_BIN_EXE_trace-index"))
        .arg("--db")
        .arg(root.join("index.sqlite"))
        .arg("--metadata-db")
        .arg(root.join("metadata.sqlite"))
        .args(arguments)
        .output()
        .unwrap();
    assert_eq!(
        output.status.success(),
        success,
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice::<Value>(&output.stdout).unwrap()["data"].clone()
}

fn command(root: &Path, arguments: &[&str]) -> Value {
    invoke(root, arguments, true)
}

fn sync(root: &Path, source: &Path) -> Value {
    command(
        root,
        &[
            "index",
            "sync",
            "--include-sources",
            source.to_str().unwrap(),
        ],
    )
}

fn rows(root: &Path, sql: &str) -> Value {
    command(root, &["query", "run", sql])["rows"].clone()
}

fn compress(text: &str) -> Vec<u8> {
    zstd::stream::encode_all(text.as_bytes(), 1).unwrap()
}

#[test]
fn follows_plain_to_compressed_rotation_and_verifies_original_record_bytes() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let sources = root.join("sources");
    fs::create_dir(&sources).unwrap();
    let plain = sources.join("rollout.jsonl");
    let archive = sources.join("rollout.jsonl.zst");
    let initial = format!("{HEADER}{FIRST}");
    fs::write(&plain, &initial).unwrap();
    fs::write(&archive, compress(&initial)).unwrap();
    let synced = sync(root, &sources);
    assert_eq!(synced["indexed_records"], 2);
    assert_eq!(synced["discovered_files"], 1);
    assert_eq!(
        rows(root, "SELECT source_id,locator FROM sources"),
        json!([{"source_id":1,"locator":plain.canonicalize().unwrap()}])
    );
    fs::remove_file(&plain).unwrap();

    let rotated = sync(root, &sources);
    assert_eq!(rotated["unchanged_files"], 1);
    assert_eq!(rotated["metrics"]["writes"]["items"], 0);
    assert_eq!(
        rows(root, "SELECT source_id,locator FROM sources"),
        json!([{"source_id":1,"locator":archive.canonicalize().unwrap()}])
    );
    assert_eq!(
        command(root, &["record", "verify", "2"])["status"],
        "verified"
    );

    fs::OpenOptions::new()
        .append(true)
        .open(&archive)
        .unwrap()
        .write_all(&compress(SECOND))
        .unwrap();
    assert_eq!(sync(root, &sources)["indexed_records"], 1);
    assert_eq!(
        rows(root, "SELECT record_id FROM records ORDER BY record_id"),
        json!([{"record_id":1},{"record_id":2},{"record_id":3}])
    );
    let inspected = command(root, &["record", "inspect", "3"]);
    assert_eq!(inspected["raw_verified"], true);
    let exported = root.join("out.jsonl");
    command(
        root,
        &[
            "record",
            "export",
            "3",
            "--output",
            exported.to_str().unwrap(),
        ],
    );
    assert_eq!(fs::read_to_string(exported).unwrap(), SECOND.trim_end());
    assert_eq!(sync(root, &sources)["unchanged_files"], 1);

    fs::write(&plain, initial).unwrap();
    assert_eq!(sync(root, &sources)["skipped_files"], 1);
    assert_eq!(
        command(root, &["record", "verify", "3"])["status"],
        "verified"
    );
}

#[test]
fn a_damaged_archive_cannot_replace_the_previous_index() {
    let temporary = TempDir::new().unwrap();
    let root = temporary.path();
    let archive = root.join("rollout.jsonl.zst");
    let contents = compress(&format!("{HEADER}{FIRST}"));
    fs::write(&archive, &contents).unwrap();
    let config = root.join("trace-index.toml");
    fs::write(&config, "[[roots]]\npath = 'rollout.jsonl.zst'\n").unwrap();
    let checked = command(
        root,
        &["--config", config.to_str().unwrap(), "config", "check"],
    );
    assert_eq!(checked["configured_sync_ready"], true);
    assert_eq!(sync(root, &archive)["indexed_records"], 2);
    let mut incomplete = contents.clone();
    incomplete.truncate(incomplete.len() - 3);
    fs::write(&archive, incomplete).unwrap();
    let failed = invoke(
        root,
        &[
            "index",
            "sync",
            "--include-sources",
            archive.to_str().unwrap(),
        ],
        false,
    );
    assert_eq!(failed["failed_files"], 1);
    assert_eq!(
        rows(root, "SELECT count(*) AS n FROM records"),
        json!([{"n":2}])
    );
    fs::write(&archive, contents).unwrap();
    assert_eq!(sync(root, &archive)["indexed_records"], 0);
    assert_eq!(
        command(root, &["record", "verify", "2"])["status"],
        "verified"
    );
}

#[test]
fn an_empty_record_beyond_the_end_of_a_source_is_short() {
    for compressed in [false, true] {
        let temporary = TempDir::new().unwrap();
        let root = temporary.path();
        let name = if compressed {
            "rollout.jsonl.zst"
        } else {
            "rollout.jsonl"
        };
        let path = root.join(name);
        let write = |content: &str| {
            let bytes = if compressed {
                compress(content)
            } else {
                content.as_bytes().to_vec()
            };
            fs::write(&path, bytes).unwrap();
        };
        write(&format!("{HEADER}{FIRST}\n"));
        assert_eq!(sync(root, &path)["indexed_records"], 3);
        assert_eq!(
            command(root, &["record", "verify", "3"])["status"],
            "verified"
        );
        write(HEADER);
        assert_eq!(
            command(root, &["record", "verify", "3"])["status"],
            "source_short"
        );
    }
}
