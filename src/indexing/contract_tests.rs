use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use serde_json::{Value, json};
use tempfile::tempdir;

use super::{IndexOptions, index_paths};
use crate::storage::db::Store;

fn write_jsonl(path: &Path, records: &[Value]) {
    let mut text = records
        .iter()
        .map(Value::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    text.push('\n');
    fs::write(path, text).expect("write trace fixture");
}

fn options(rebuild: bool) -> IndexOptions {
    IndexOptions {
        rebuild,
        max_record_bytes: 1024 * 1024,
        max_text_bytes: 64 * 1024,
    }
}

fn index_fixture(path: &Path) -> (tempfile::TempDir, Store) {
    let directory = tempdir().expect("temporary index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, &[PathBuf::from(path)], &options(false)).expect("index fixture");
    (directory, store)
}

#[test]
fn a_new_source_does_not_clear_a_projection_that_cannot_exist() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("codex.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"new-source","cwd":"/workspace"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"hello"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}),
        ],
    );

    let directory = tempdir().expect("temporary index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    store
        .connection()
        .authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Delete {
                table_name: "domain_items" | "domain_loops",
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .expect("install delete guard");

    index_paths(&mut store, &[trace], &options(false))
        .expect("a new Source must not attempt to delete an old projection");
}

fn count(connection: &Connection, relation: &str) -> i64 {
    connection
        .query_row(&format!("SELECT COUNT(*) FROM {relation}"), [], |row| {
            row.get(0)
        })
        .expect("count relation")
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture verifies the complete nested Item contract"
)]
fn codex_publishes_the_five_objects_semantics_and_shell_details() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("codex.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-session","cwd":"/workspace"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            json!({"type":"turn_context","payload":{"turn_id":"loop-1","model":"gpt-test","effort":"high","model_context_window":128_000}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"run it"}}),
            json!({"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"run it"}]}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"exec_command","arguments":"{\"cmd\":\"echo hello >out.txt\"}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"hello"}}),
            json!({"type":"event_msg","payload":{"type":"token_count","info":{"model_context_window":128_000,"last_token_usage":{"input_tokens":100,"cached_input_tokens":60,"output_tokens":20,"reasoning_output_tokens":8},"total_token_usage":{"total_tokens":999_999}}}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","phase":"final_answer","message":"done"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "sources"), 1);
    assert_eq!(count(connection, "records"), 10);
    assert_eq!(count(connection, "sessions"), 1);
    assert_eq!(count(connection, "loops"), 1);
    assert_eq!(count(connection, "items"), 4);

    let source_columns = connection
        .prepare("SELECT name FROM pragma_table_info('sources') ORDER BY cid")
        .expect("prepare Source columns")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query Source columns")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect Source columns");
    assert_eq!(source_columns, ["source_id", "locator", "runtime"]);

    let (runtime, range_start, range_end): (String, i64, i64) = connection
        .query_row(
            "SELECT s.runtime,
                    json_extract(r.content_range, '$.start'),
                    json_extract(r.content_range, '$.end')
               FROM sources s
               JOIN records r USING(source_id)
              WHERE r.source_position = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read Source and Record contract");
    assert_eq!(runtime, "codex");
    assert_eq!(range_start, 0);
    assert!(range_end > range_start);

    let (end_record_id, outcome): (i64, String) = connection
        .query_row(
            "SELECT json_extract(end, '$.record_id'),
                    json_extract(end, '$.outcome')
               FROM loops",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read Loop end");
    assert!(end_record_id > 0);
    assert_eq!(outcome, "completed");

    let (model, effort, context_window, input, cached, output, reasoning, total): (
        String,
        String,
        i64,
        i64,
        i64,
        i64,
        i64,
        i64,
    ) = connection
        .query_row(
            "SELECT json_extract(model, '$.id'),
                    json_extract(model, '$.effort'),
                    json_extract(model, '$.context_window'),
                    json_extract(usage, '$.input'),
                    json_extract(usage, '$.cached'),
                    json_extract(usage, '$.output'),
                    json_extract(usage, '$.reasoning'),
                    json_extract(usage, '$.input') + json_extract(usage, '$.output')
               FROM loops",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .expect("read Loop model and usage");
    assert_eq!(model, "gpt-test");
    assert_eq!(effort, "high");
    assert_eq!(context_window, 128_000);
    assert_eq!(
        (input, cached, output, reasoning, total),
        (100, 60, 20, 8, 120)
    );

    let roles = connection
        .prepare("SELECT json_extract(semantic, '$.role') FROM items ORDER BY loop_position")
        .expect("prepare roles")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query roles")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect roles");
    assert_eq!(
        roles,
        [
            "human.request",
            "agent.tool_call.shell",
            "tool.output",
            "agent.final_answer",
        ]
    );

    let (evidence_strength, fragment_count, invocation_program, redirect_target):
        (String, i64, String, String) = connection
        .query_row(
            "SELECT json_extract(semantic, '$.evidence_strength'),
                    json_array_length(semantic, '$.value.shell_fragments'),
                    json_extract(semantic, '$.value.shell_fragments[0].statements[0].invocations[0].program'),
                    json_extract(semantic, '$.value.shell_fragments[0].statements[0].redirects[0].target')
               FROM items
              WHERE json_extract(semantic, '$.role') = 'agent.tool_call.shell'",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                ))
            },
        )
        .expect("read shell semantic value");
    assert_eq!(evidence_strength, "structural");
    assert_eq!(fragment_count, 1);
    assert_eq!(invocation_program, "echo");
    assert_eq!(redirect_target, "out.txt");

    let human_record_count: i64 = connection
        .query_row(
            "SELECT json_array_length(record_ids)
               FROM items
              WHERE json_extract(semantic, '$.role') = 'human.request'",
            [],
            |row| row.get(0),
        )
        .expect("human evidence");
    assert_eq!(human_record_count, 2);
    assert_eq!(count(connection, "item_search"), 2);
}

#[test]
fn codex_message_pairing_uses_the_primary_response_timestamp() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("codex-message-times.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"timestamp":"2026-08-20T00:00:00.000Z","type":"session_meta","payload":{"id":"message-times","cwd":"/workspace"}}),
            json!({"timestamp":"2026-08-20T00:00:00.010Z","type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            // Response-first user message.
            json!({"timestamp":"2026-08-20T00:00:00.100Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"request"}]}}),
            json!({"timestamp":"2026-08-20T00:00:00.110Z","type":"event_msg","payload":{"type":"user_message","message":"request"}}),
            // UI-first user message.
            json!({"timestamp":"2026-08-20T00:00:00.200Z","type":"event_msg","payload":{"type":"user_message","message":"steer"}}),
            json!({"timestamp":"2026-08-20T00:00:00.210Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"steer"}]}}),
            // UI-first agent message.
            json!({"timestamp":"2026-08-20T00:00:00.300Z","type":"event_msg","payload":{"type":"agent_message","phase":"final_answer","message":"done"}}),
            json!({"timestamp":"2026-08-20T00:00:00.310Z","type":"response_item","payload":{"type":"message","role":"assistant","phase":"final_answer","content":[{"type":"output_text","text":"done"}]}}),
            json!({"timestamp":"2026-08-20T00:00:00.320Z","type":"event_msg","payload":{"type":"task_complete","last_agent_message":"done"}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let rows = store
        .connection()
        .prepare(
            "SELECT json_extract(semantic, '$.role'),
                    occurred_at,
                    json_array_length(record_ids)
               FROM items
              ORDER BY loop_position",
        )
        .expect("prepare paired message query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })
        .expect("query paired messages")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect paired messages");
    let timestamp = crate::adapters::runtime::codex::parse_timestamp_ms_public;
    assert_eq!(
        rows,
        [
            (
                "human.request".to_owned(),
                timestamp("2026-08-20T00:00:00.100Z").expect("request timestamp"),
                2,
            ),
            (
                "human.steering".to_owned(),
                timestamp("2026-08-20T00:00:00.210Z").expect("steering timestamp"),
                2,
            ),
            (
                "agent.final_answer".to_owned(),
                timestamp("2026-08-20T00:00:00.310Z").expect("answer timestamp"),
                2,
            ),
        ]
    );

    let searchable = store
        .connection()
        .prepare(
            "SELECT json_extract(i.semantic, '$.role'), b.text
               FROM item_search search
               JOIN items i ON i.item_id = search.rowid
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              ORDER BY i.loop_position",
        )
        .expect("prepare paired-message search query")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query paired-message search rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect paired-message search rows");
    assert_eq!(
        searchable,
        [
            ("human.request".to_owned(), "request".to_owned()),
            ("human.steering".to_owned(), "steer".to_owned()),
            ("agent.final_answer".to_owned(), "done".to_owned()),
        ]
    );
}

#[test]
fn codex_item_completed_user_message_is_human_input() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("codex-item-completed-user.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"timestamp":"2026-08-25T08:43:03.000Z","type":"session_meta","payload":{"id":"item-completed-user","cwd":"/workspace"}}),
            json!({"timestamp":"2026-08-25T08:43:03.100Z","type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            json!({"timestamp":"2026-08-25T08:43:05.429Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"design the CLI"}]}}),
            json!({"timestamp":"2026-08-25T08:43:05.433Z","type":"event_msg","payload":{"type":"item_completed","item":{"type":"UserMessage","content":[{"type":"text","text":"design the CLI"}]}}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let (role, occurred_at, evidence_records): (String, i64, i64) = store
        .connection()
        .query_row(
            "SELECT json_extract(semantic, '$.role'),
                    occurred_at,
                    json_array_length(record_ids)
               FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read completed user Item");

    assert_eq!(role, "human.request");
    assert_eq!(
        occurred_at,
        crate::adapters::runtime::codex::parse_timestamp_ms_public("2026-08-25T08:43:05.429Z")
            .expect("response timestamp")
    );
    assert_eq!(evidence_records, 2);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one cross-Runtime fixture verifies both shared Item semantics and normalized Loop values"
)]
fn pi_and_claude_publish_the_same_request_and_answer_semantics() {
    let traces = tempdir().expect("trace directory");
    let pi = traces.path().join("pi.jsonl");
    write_jsonl(
        &pi,
        &[
            json!({"type":"session","id":"pi-session","cwd":"/workspace"}),
            json!({"type":"model_change","id":"pm","parentId":null,"modelId":"pi-model","provider":"pi-provider"}),
            json!({"type":"thinking_level_change","id":"pe","parentId":"pm","thinkingLevel":"high"}),
            json!({"type":"message","id":"p1","message":{"role":"user","content":[{"type":"text","text":"hello from pi"}]}}),
            json!({"type":"message","id":"p2","parentId":"p1","message":{"role":"assistant","model":"pi-model","content":[{"type":"text","text":"pi answer"}],"stopReason":"stop","usage":{"input":10,"cacheRead":20,"cacheWrite":3,"output":7,"reasoning":2}}}),
        ],
    );
    let claude = traces.path().join("claude.jsonl");
    write_jsonl(
        &claude,
        &[
            json!({"type":"user","sessionId":"claude-session","uuid":"c1","parentUuid":null,"promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"text","text":"hello from claude"}]}}),
            json!({"type":"assistant","sessionId":"claude-session","uuid":"c2","parentUuid":"c1","promptId":"prompt-1","effort":"high","message":{"role":"assistant","model":"claude-model","content":[{"type":"text","text":"claude answer"}],"stop_reason":"end_turn","usage":{"input_tokens":10,"cache_read_input_tokens":20,"cache_creation_input_tokens":3,"output_tokens":7,"output_tokens_details":{"thinking_tokens":2}}}}),
        ],
    );

    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, &[pi, claude], &options(false)).expect("index both runtimes");
    let connection = store.connection();

    let rows = connection
        .prepare(
            "SELECT s.runtime, json_extract(i.semantic, '$.role')
               FROM items i JOIN sessions s USING(session_id)
              WHERE json_extract(i.semantic, '$.role') IN ('human.request', 'agent.final_answer')
              ORDER BY s.runtime, i.loop_position",
        )
        .expect("prepare semantic query")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query semantics")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect semantics");
    assert_eq!(
        rows,
        [
            ("claude".to_owned(), "human.request".to_owned()),
            ("claude".to_owned(), "agent.final_answer".to_owned()),
            ("pi".to_owned(), "human.request".to_owned()),
            ("pi".to_owned(), "agent.final_answer".to_owned()),
        ]
    );
    assert_eq!(count(connection, "sessions"), 2);
    assert_eq!(count(connection, "loops"), 2);

    let loop_values = connection
        .prepare(
            "SELECT s.runtime,
                    json_extract(l.model, '$.id'),
                    json_extract(l.model, '$.effort'),
                    json_extract(l.usage, '$.input'),
                    json_extract(l.usage, '$.cached'),
                    json_extract(l.usage, '$.cache_write'),
                    json_extract(l.usage, '$.output'),
                    json_extract(l.usage, '$.reasoning')
               FROM loops l JOIN sessions s USING(session_id)
              ORDER BY s.runtime",
        )
        .expect("prepare cross-Runtime Loop values")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
                row.get::<_, i64>(7)?,
            ))
        })
        .expect("query cross-Runtime Loop values")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect cross-Runtime Loop values");
    assert_eq!(
        loop_values,
        [
            (
                "claude".to_owned(),
                "claude-model".to_owned(),
                Some("high".to_owned()),
                33,
                20,
                3,
                7,
                2,
            ),
            (
                "pi".to_owned(),
                "pi-model".to_owned(),
                Some("high".to_owned()),
                33,
                20,
                3,
                7,
                2,
            ),
        ]
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one policy contract test compares the same bounded text under three database-wide policies"
)]
fn semantic_text_states_when_only_a_bounded_prefix_is_published() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("bounded-text.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"bounded-session","cwd":"/workspace"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            json!({"type":"response_item","payload":{"type":"function_call","call_id":"call-1","name":"read_file","arguments":"{}"}}),
            json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"call-1","output":"abcdef你好"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ],
    );

    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(
        &mut store,
        std::slice::from_ref(&trace),
        &IndexOptions {
            rebuild: false,
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 5,
        },
    )
    .expect("index bounded text fixture");

    let (value, visible_bytes, full_bytes, estimated_tokens): (String, i64, i64, i64) = store
        .connection()
        .query_row(
            "SELECT b.text,
                    length(CAST(b.text AS BLOB)),
                    b.full_bytes,
                    b.estimated_tokens
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'tool.output'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .expect("read bounded TextContent");
    assert_eq!(value, "abcde");
    assert_eq!(visible_bytes, 5);
    assert_eq!(full_bytes, 12);
    assert_eq!(estimated_tokens, 4);

    let policy_error = index_paths(
        &mut store,
        std::slice::from_ref(&trace),
        &IndexOptions {
            rebuild: true,
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 10,
        },
    )
    .expect_err("a non-empty index must reject a different publication policy");
    assert!(
        policy_error
            .to_string()
            .contains("indexing policy mismatch")
    );

    let expanded_directory = tempdir().expect("expanded index directory");
    let mut expanded_store =
        Store::open(&expanded_directory.path().join("index.sqlite")).expect("open expanded index");
    index_paths(
        &mut expanded_store,
        std::slice::from_ref(&trace),
        &IndexOptions {
            rebuild: false,
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 10,
        },
    )
    .expect("index with a larger text bound in a new database");
    let expanded: String = expanded_store
        .connection()
        .query_row(
            "SELECT b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'tool.output'",
            [],
            |row| row.get(0),
        )
        .expect("read expanded TextContent");
    assert_eq!(expanded, "abcdef你");

    let reduced_directory = tempdir().expect("reduced index directory");
    let mut reduced_store =
        Store::open(&reduced_directory.path().join("index.sqlite")).expect("open reduced index");
    index_paths(
        &mut reduced_store,
        &[trace],
        &IndexOptions {
            rebuild: true,
            max_record_bytes: 1024 * 1024,
            max_text_bytes: 4,
        },
    )
    .expect("rebuild with a smaller text bound");
    let reduced: String = reduced_store
        .connection()
        .query_row(
            "SELECT b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'tool.output'",
            [],
            |row| row.get(0),
        )
        .expect("read reduced TextContent");
    assert_eq!(reduced, "abcd");
    assert_eq!(count(store.connection(), "content_blobs"), 1);
    assert_eq!(count(expanded_store.connection(), "content_blobs"), 1);
    assert_eq!(count(reduced_store.connection(), "content_blobs"), 1);
}

#[test]
fn semantic_text_blobs_deduplicate_without_changing_public_items() {
    let traces = tempdir().expect("trace directory");
    let first = traces.path().join("first.jsonl");
    let second = traces.path().join("second.jsonl");
    for (path, session_id) in [(&first, "first-session"), (&second, "second-session")] {
        write_jsonl(
            path,
            &[
                json!({"type":"session_meta","payload":{"id":session_id,"cwd":"/workspace"}}),
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
                json!({"type":"event_msg","payload":{"type":"user_message","message":"same request"}}),
                json!({"type":"event_msg","payload":{"type":"agent_message","phase":"final_answer","message":"same answer"}}),
                json!({"type":"event_msg","payload":{"type":"task_complete","last_agent_message":"same answer"}}),
            ],
        );
    }

    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, &[first, second], &options(false)).expect("index repeated text");
    let connection = store.connection();

    assert_eq!(count(connection, "items"), 4);
    assert_eq!(count(connection, "content_blobs"), 2);
    let (blob_references, distinct_blobs, inline_texts): (i64, i64, i64) = connection
        .query_row(
            "SELECT COUNT(json_extract(semantic, '$.value.text.blob_id')),
                    COUNT(DISTINCT json_extract(semantic, '$.value.text.blob_id')),
                    SUM(json_type(semantic, '$.value.text.value') IS NOT NULL)
               FROM domain_items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("inspect private Blob storage");
    assert_eq!((blob_references, distinct_blobs, inline_texts), (4, 2, 0));

    let public_values = connection
        .prepare(
            "SELECT json_extract(i.semantic, '$.role'), b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              ORDER BY i.session_id, i.loop_position",
        )
        .expect("prepare public Semantic query")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query public Semantic values")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect public Semantic values");
    assert_eq!(
        public_values,
        [
            ("human.request".to_owned(), "same request".to_owned()),
            ("agent.final_answer".to_owned(), "same answer".to_owned()),
            ("human.request".to_owned(), "same request".to_owned()),
            ("agent.final_answer".to_owned(), "same answer".to_owned()),
        ]
    );
}

#[test]
fn codex_web_search_keeps_runtime_authored_action_in_semantic() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("web-search.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"web-session","cwd":"/workspace"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"loop-1"}}),
            json!({"type":"response_item","payload":{"type":"web_search_call","id":"search-1","action":{"query":"trace index design"}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let (role, tool_name, query): (String, String, String) = store
        .connection()
        .query_row(
            "SELECT json_extract(semantic, '$.role'),
                    json_extract(semantic, '$.value.tool_name'),
                    json_extract(semantic, '$.value.arguments.query')
               FROM items",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read web-search Semantic value");
    assert_eq!(role, "agent.tool_call");
    assert_eq!(tool_name, "web_search");
    assert_eq!(query, "trace index design");
}

#[test]
fn claude_attachment_needs_meaningful_text_to_become_an_item() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-attachments.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({
                "type":"attachment",
                "sessionId":"attachment-session",
                "attachment":{
                    "type":"file",
                    "displayPath":"src/lib.rs",
                    "content":{"file":{"content":"actual file content"}}
                }
            }),
            json!({
                "type":"attachment",
                "sessionId":"attachment-session",
                "attachment":{
                    "type":"deferred_tools_delta",
                    "addedNames":["some_tool"]
                }
            }),
            json!({
                "type":"attachment",
                "sessionId":"attachment-session",
                "attachment":{
                    "type":"invoked_skills",
                    "skills":[{"name":"example","content":"actual skill body"}]
                }
            }),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "records"), 3);
    assert_eq!(count(connection, "items"), 2);
    let (role, category, text): (String, String, String) = connection
        .query_row(
            "SELECT json_extract(i.semantic, '$.role'),
                    json_extract(i.semantic, '$.value.category'),
                    b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'runtime.context'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read attachment Semantic value");
    assert_eq!(role, "runtime.context");
    assert_eq!(category, "file");
    assert_eq!(text, "actual file content");

    let (category, text): (String, String) = connection
        .query_row(
            "SELECT json_extract(i.semantic, '$.value.category'), b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'runtime.instructions'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read invoked-skill Semantic value");
    assert_eq!(category, "skill");
    assert_eq!(text, "actual skill body");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one fixture covers active steering, image steering, and a post-end request"
)]
fn claude_queued_human_prompts_follow_loop_position() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-queued.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({
                "type":"user",
                "sessionId":"queued-session",
                "uuid":"request",
                "promptId":"prompt-1",
                "origin":{"kind":"human"},
                "message":{"role":"user","content":[{"type":"text","text":"start"}]}
            }),
            json!({
                "type":"attachment",
                "sessionId":"queued-session",
                "attachment":{
                    "type":"queued_command",
                    "commandMode":"prompt",
                    "origin":{"kind":"human"},
                    "prompt":"change direction"
                }
            }),
            json!({
                "type":"attachment",
                "sessionId":"queued-session",
                "attachment":{
                    "type":"queued_command",
                    "commandMode":"prompt",
                    "origin":{"kind":"human"},
                    "prompt":[
                        {"type":"text","text":"inspect this image"},
                        {"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}
                    ]
                }
            }),
            json!({
                "type":"assistant",
                "sessionId":"queued-session",
                "message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"first answer"}]}
            }),
            json!({
                "type":"system",
                "sessionId":"queued-session",
                "subtype":"turn_duration",
                "durationMs":100
            }),
            json!({
                "type":"attachment",
                "sessionId":"queued-session",
                "attachment":{
                    "type":"queued_command",
                    "commandMode":"prompt",
                    "origin":{"kind":"human"},
                    "prompt":"start another loop"
                }
            }),
            json!({
                "type":"assistant",
                "sessionId":"queued-session",
                "message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"second answer"}]}
            }),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    let human = connection
        .prepare(
            "SELECT json_extract(i.semantic, '$.role'),
                    b.text,
                    json_extract(i.semantic, '$.value.has_images'),
                    json_extract(i.semantic, '$.evidence_strength'),
                    json_array_length(i.record_ids)
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') LIKE 'human.%'
              ORDER BY CAST(json_extract(i.record_ids, '$[0]') AS INTEGER)",
        )
        .expect("prepare queued human query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .expect("query queued human semantics")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect queued human semantics");
    assert_eq!(
        human,
        [
            (
                "human.request".to_owned(),
                "start".to_owned(),
                false,
                "structural".to_owned(),
                1,
            ),
            (
                "human.steering".to_owned(),
                "change direction".to_owned(),
                false,
                "structural".to_owned(),
                1,
            ),
            (
                "human.steering".to_owned(),
                "inspect this image".to_owned(),
                true,
                "structural".to_owned(),
                1,
            ),
            (
                "human.request".to_owned(),
                "start another loop".to_owned(),
                false,
                "structural".to_owned(),
                1,
            ),
        ]
    );

    assert_eq!(count(connection, "loops"), 2);
    assert_eq!(count(connection, "item_search"), 6);
    assert_eq!(count(connection, "items"), 6);
}

#[test]
fn claude_delayed_tool_output_after_turn_duration_opens_a_loop() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-delayed-output.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"delayed-session","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"run it"}}),
            json!({"type":"assistant","sessionId":"delayed-session","message":{"role":"assistant","stop_reason":"tool_use","content":[{"type":"tool_use","id":"call-bg","name":"Bash","input":{"command":"sleep 1","run_in_background":true}}]}}),
            json!({"type":"user","sessionId":"delayed-session","promptId":"prompt-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-bg","content":"Background task started"}]},"toolUseResult":{"backgroundTaskId":"bg-1"}}),
            json!({"type":"assistant","sessionId":"delayed-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"waiting for it"}]}}),
            json!({"type":"system","sessionId":"delayed-session","subtype":"turn_duration","durationMs":100}),
            json!({"type":"user","sessionId":"delayed-session","origin":{"kind":"task-notification"},"message":{"role":"user","content":"<task-notification><task-id>bg-1</task-id><tool-use-id>call-bg</tool-use-id><status>completed</status><summary>done</summary></task-notification>"}}),
            json!({"type":"assistant","sessionId":"delayed-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"finished"}]}}),
            json!({"type":"system","sessionId":"delayed-session","subtype":"turn_duration","durationMs":50}),
            json!({"type":"user","sessionId":"delayed-session","promptId":"prompt-2","origin":{"kind":"human"},"message":{"role":"user","content":"what next?"}}),
            json!({"type":"assistant","sessionId":"delayed-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"next answer"}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "loops"), 3);

    let loops = connection
        .prepare(
            "SELECT l.loop_id,
                    SUM(json_extract(i.semantic, '$.role') = 'human.request'),
                    SUM(json_extract(i.semantic, '$.role') = 'tool.output'),
                    SUM(json_extract(i.semantic, '$.role') = 'agent.final_answer')
               FROM loops l
               JOIN items i USING (loop_id)
              GROUP BY l.loop_id
              ORDER BY MIN(CAST(json_extract(i.record_ids, '$[0]') AS INTEGER))",
        )
        .expect("prepare delayed-output Loops")
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .expect("query delayed-output Loops")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect delayed-output Loops");
    assert_eq!(loops, [(1, 1, 1), (0, 1, 1), (1, 0, 1)]);
}

#[test]
fn claude_repeated_tool_results_after_turn_duration_share_the_new_loop() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-repeated-results.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"result-session","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"start both"}}),
            json!({"type":"assistant","sessionId":"result-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"waiting"}]}}),
            json!({"type":"system","sessionId":"result-session","subtype":"turn_duration","durationMs":100}),
            json!({"type":"user","sessionId":"result-session","promptId":"prompt-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-1","content":"first result"}]}}),
            json!({"type":"user","sessionId":"result-session","promptId":"prompt-1","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-2","content":"second result"}]}}),
            json!({"type":"assistant","sessionId":"result-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"both finished"}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "loops"), 2);
    let loops = connection
        .prepare(
            "SELECT SUM(json_extract(i.semantic, '$.role') = 'tool.output'),
                    SUM(json_extract(i.semantic, '$.role') = 'agent.final_answer')
               FROM loops l
               JOIN items i USING (loop_id)
              GROUP BY l.loop_id
              ORDER BY l.session_position",
        )
        .expect("prepare repeated-result Loops")
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .expect("query repeated-result Loops")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect repeated-result Loops");
    assert_eq!(loops, [(0, 1), (2, 1)]);
}

#[test]
fn claude_bash_wrapper_after_turn_duration_does_not_return_to_old_prompt() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-bash-wrapper.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"bash-session","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"prepare it"}}),
            json!({"type":"assistant","sessionId":"bash-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"prepared"}]}}),
            json!({"type":"system","sessionId":"bash-session","subtype":"turn_duration","durationMs":100}),
            json!({"type":"user","sessionId":"bash-session","promptId":"prompt-1","message":{"role":"user","content":"<bash-input>pwd</bash-input>"}}),
            json!({"type":"user","sessionId":"bash-session","promptId":"prompt-1","message":{"role":"user","content":"<bash-stdout>/tmp</bash-stdout><bash-stderr></bash-stderr>"}}),
            json!({"type":"assistant","sessionId":"bash-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"shell finished"}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "loops"), 2);
    let loops = connection
        .prepare(
            "SELECT SUM(json_extract(i.semantic, '$.role') = 'human.request'),
                    SUM(json_extract(i.semantic, '$.role') = 'runtime.context'),
                    COALESCE(SUM(json_extract(i.semantic, '$.value.category') = 'internal'), 0),
                    SUM(json_extract(i.semantic, '$.role') = 'agent.final_answer')
               FROM loops l
               JOIN items i USING (loop_id)
              GROUP BY l.loop_id
              ORDER BY l.session_position",
        )
        .expect("prepare Bash-wrapper Loops")
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .expect("query Bash-wrapper Loops")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect Bash-wrapper Loops");
    assert_eq!(loops, [(1, 0, 0, 1), (0, 2, 2, 1)]);
}

#[test]
fn claude_stop_hook_rejection_demotes_the_provisional_final() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-stop-hook.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"hook-session","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"finish the work"}}),
            json!({"type":"assistant","sessionId":"hook-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"premature answer"}]}}),
            json!({"type":"user","sessionId":"hook-session","isMeta":true,"message":{"role":"user","content":"Stop hook feedback:\nThe goal is not complete."}}),
            json!({"type":"attachment","sessionId":"hook-session","attachment":{"type":"goal_status","met":false,"reason":"not complete"}}),
            json!({"type":"assistant","sessionId":"hook-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"actual final"}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    assert_eq!(count(connection, "loops"), 1);
    let answers = connection
        .prepare(
            "SELECT json_extract(i.semantic, '$.role'), b.text
               FROM items i
               JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE b.text IN ('premature answer', 'actual final')
              ORDER BY i.loop_position",
        )
        .expect("prepare Stop-hook answers")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query Stop-hook answers")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect Stop-hook answers");
    assert_eq!(
        answers,
        [
            ("agent.commentary".to_owned(), "premature answer".to_owned()),
            ("agent.final_answer".to_owned(), "actual final".to_owned()),
        ]
    );
}

#[test]
fn claude_synthetic_api_error_is_a_runtime_notice() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-api-error.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"error-session","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"finish the work"}}),
            json!({"type":"assistant","sessionId":"error-session","message":{"role":"assistant","stop_reason":"end_turn","content":[{"type":"text","text":"partial answer"}]}}),
            json!({"type":"assistant","sessionId":"error-session","error":"server_error","isApiErrorMessage":true,"message":{"role":"assistant","model":"<synthetic>","stop_reason":"stop_sequence","content":[{"type":"text","text":"API Error: Connection closed mid-response."}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let items = store
        .connection()
        .prepare(
            "SELECT json_extract(semantic, '$.role'),
                    json_extract(semantic, '$.evidence_strength')
               FROM items
              ORDER BY loop_position",
        )
        .expect("prepare API-error Items")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("query API-error Items")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect API-error Items");
    assert_eq!(
        items,
        [
            ("human.request".to_owned(), "structural".to_owned()),
            ("agent.final_answer".to_owned(), "structural".to_owned()),
            ("runtime.notice".to_owned(), "structural".to_owned()),
        ]
    );
}

#[test]
fn claude_catalog_deltas_publish_runtime_provided_instruction_text() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-catalog-deltas.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({
                "type":"attachment",
                "sessionId":"catalog-session",
                "attachment":{
                    "type":"agent_listing_delta",
                    "addedTypes":["Explore","Plan"],
                    "addedLines":["- Explore: inspect files","- Plan: design changes"]
                }
            }),
            json!({
                "type":"attachment",
                "sessionId":"catalog-session",
                "attachment":{
                    "type":"deferred_tools_delta",
                    "addedNames":["Read","Write"],
                    "addedLines":["Read","Write"]
                }
            }),
            json!({
                "type":"attachment",
                "sessionId":"catalog-session",
                "attachment":{
                    "type":"mcp_instructions_delta",
                    "addedNames":["browser","mail"],
                    "addedBlocks":["## browser\nUse one search.","## mail\nConfirm before send."]
                }
            }),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    let instructions = connection
        .prepare(
            "SELECT json_extract(i.semantic, '$.value.category'),
                    b.text,
                    json_extract(i.semantic, '$.evidence_strength'),
                    json_array_length(i.record_ids)
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'runtime.instructions'
              ORDER BY i.loop_position",
        )
        .expect("prepare catalog instruction query")
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
            ))
        })
        .expect("query catalog instructions")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect catalog instructions");
    assert_eq!(
        instructions,
        [
            (
                "tool_catalog".to_owned(),
                "- Explore: inspect files\n- Plan: design changes".to_owned(),
                "structural".to_owned(),
                1,
            ),
            (
                "tool_catalog".to_owned(),
                "Read\nWrite".to_owned(),
                "structural".to_owned(),
                1,
            ),
            (
                "tool_catalog".to_owned(),
                "## browser\nUse one search.\n\n## mail\nConfirm before send.".to_owned(),
                "structural".to_owned(),
                1,
            ),
        ]
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM items WHERE json_extract(semantic, '$.role') = 'runtime.unknown'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count unknown semantics"),
        0
    );
}

#[test]
fn claude_compaction_boundary_is_a_notice_not_a_summary() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-compaction.jsonl");
    write_jsonl(
        &trace,
        &[json!({
            "type":"system",
            "sessionId":"compaction-session",
            "subtype":"compact_boundary",
            "content":"Conversation compacted"
        })],
    );

    let (_directory, store) = index_fixture(&trace);
    let (role, text): (String, String) = store
        .connection()
        .query_row(
            "SELECT json_extract(i.semantic, '$.role'), b.text
               FROM items i
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read compaction boundary Semantic value");
    assert_eq!(role, "runtime.notice");
    assert_eq!(text, "Conversation compacted");
}

#[test]
fn a_trace_without_a_confirmed_session_is_skipped_atomically() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("missing-session.jsonl");
    write_jsonl(
        &trace,
        &[json!({"type":"event_msg","payload":{"type":"user_message","message":"orphan"}})],
    );
    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");

    let report = index_paths(&mut store, &[trace], &options(false)).expect("index Source");
    assert_eq!(report.skipped_files, 1);
    assert_eq!(report.failed_files, 0);
    assert_eq!(count(store.connection(), "sources"), 0);
    assert_eq!(count(store.connection(), "records"), 0);
    assert_eq!(count(store.connection(), "sessions"), 0);
    assert_eq!(count(store.connection(), "loops"), 0);
    assert_eq!(count(store.connection(), "items"), 0);
}

#[test]
fn rebuilding_a_source_replaces_session_level_items_without_duplicates() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("session-item.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"current","cwd":"/workspace"}}),
            json!({"type":"session_meta","payload":{"id":"ancestor","cwd":"/workspace"}}),
        ],
    );
    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, std::slice::from_ref(&trace), &options(false)).expect("first index");
    index_paths(&mut store, &[trace], &options(true)).expect("rebuild index");

    let session_items: i64 = store
        .connection()
        .query_row(
            "SELECT COUNT(*)
               FROM items
              WHERE loop_id IS NULL
                AND json_extract(semantic, '$.role') = 'runtime.context'",
            [],
            |row| row.get(0),
        )
        .expect("count session items");
    assert_eq!(session_items, 1);
}

#[test]
fn one_logical_session_can_be_supported_by_two_sources() {
    let traces = tempdir().expect("trace directory");
    // The filenames deliberately sort in the opposite order from the Runtime
    // timestamps. Source discovery order must not define Session chronology.
    let first = traces.path().join("z-first.jsonl");
    let second = traces.path().join("a-second.jsonl");
    write_jsonl(
        &first,
        &[
            json!({"timestamp":"2026-08-01T00:00:00Z","type":"session_meta","payload":{"id":"continued","cwd":"/workspace"}}),
            json!({"timestamp":"2026-08-01T00:01:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"timestamp":"2026-08-01T00:01:01Z","type":"event_msg","payload":{"type":"user_message","message":"first"}}),
            json!({"timestamp":"2026-08-01T00:01:02Z","type":"event_msg","payload":{"type":"task_complete"}}),
            json!({"timestamp":"2026-08-01T00:02:00Z","type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
            json!({"timestamp":"2026-08-01T00:02:01Z","type":"event_msg","payload":{"type":"user_message","message":"second"}}),
        ],
    );
    write_jsonl(
        &second,
        &[
            json!({"timestamp":"2026-08-01T00:03:00Z","type":"session_meta","payload":{"id":"continued","cwd":"/workspace","history_base":{"thread_id":"old-rollout","end_ordinal_exclusive":6,"end_byte_offset":1234}}}),
            json!({"timestamp":"2026-08-01T00:03:01Z","type":"event_msg","payload":{"type":"task_started","turn_id":"three"}}),
            json!({"timestamp":"2026-08-01T00:03:02Z","type":"event_msg","payload":{"type":"user_message","message":"continued"}}),
        ],
    );
    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, &[first, second], &options(false)).expect("index lineage Sources");

    let (sessions, source_count): (i64, i64) = store
        .connection()
        .query_row(
            "SELECT COUNT(*), json_array_length(MAX(source_ids)) FROM sessions",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read logical Session");
    assert_eq!(sessions, 1);
    assert_eq!(source_count, 2);
    let positions = store
        .connection()
        .prepare(
            "SELECT l.session_position, private.ordinal
               FROM loops l
               JOIN domain_loops private ON private.id = l.loop_id
              ORDER BY l.session_position",
        )
        .expect("prepare Loop positions")
        .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))
        .expect("query Loop positions")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect Loop positions");
    assert_eq!(positions, [(0, 0), (1, 1), (2, 0)]);
}

#[test]
fn session_conversation_spine_orders_loops_then_items() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("conversation.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"conversation","cwd":"/workspace"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"request one"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"working","phase":"commentary"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"steer it"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"answer one","phase":"final_answer"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
            json!({"type":"event_msg","payload":{"type":"user_message","message":"request two"}}),
            json!({"type":"event_msg","payload":{"type":"agent_message","message":"answer two","phase":"final_answer"}}),
        ],
    );
    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    index_paths(&mut store, &[trace], &options(false)).expect("index conversation");

    let spine = store
        .connection()
        .prepare(
            "SELECT l.session_position,
                    i.loop_position,
                    json_extract(i.semantic, '$.role'),
                    b.text
               FROM loops l
               JOIN items i USING (loop_id)
               JOIN blobs b
                 ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') IN (
                    'human.request', 'human.steering',
                    'agent.commentary', 'agent.final_answer')
              ORDER BY l.session_position, i.loop_position",
        )
        .expect("prepare conversation query")
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .expect("query conversation")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect conversation");
    assert_eq!(
        spine,
        [
            (0, 0, "human.request".to_owned(), "request one".to_owned()),
            (0, 1, "agent.commentary".to_owned(), "working".to_owned()),
            (0, 2, "human.steering".to_owned(), "steer it".to_owned()),
            (
                0,
                3,
                "agent.final_answer".to_owned(),
                "answer one".to_owned()
            ),
            (1, 0, "human.request".to_owned(), "request two".to_owned()),
            (
                1,
                1,
                "agent.final_answer".to_owned(),
                "answer two".to_owned()
            ),
        ]
    );
}

#[test]
#[allow(clippy::too_many_lines)] // One end-to-end fixture exercises all three notification origins.
fn claude_async_notifications_keep_their_real_origin_and_links() {
    let traces = tempdir().expect("trace directory");
    let parent = traces.path().join("claude-parent.jsonl");
    let child = traces.path().join("claude-child.jsonl");
    write_jsonl(
        &parent,
        &[
            json!({"type":"user","sessionId":"parent-session","uuid":"request","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"start"}}),
            json!({"type":"assistant","sessionId":"parent-session","uuid":"agent-call","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-agent","name":"Agent","input":{"prompt":"audit it","description":"audit"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","sessionId":"parent-session","uuid":"agent-started","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-agent","content":"Agent started"}]},"toolUseResult":{"agentId":"child-1"}}),
            json!({"type":"assistant","sessionId":"parent-session","uuid":"bash-call","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-bg","name":"Bash","input":{"command":"sleep 1","run_in_background":true}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","sessionId":"parent-session","uuid":"bash-started","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-bg","content":"Background task started"}]},"toolUseResult":{"backgroundTaskId":"bg-1"}}),
            json!({"type":"user","sessionId":"parent-session","uuid":"agent-report","origin":{"kind":"task-notification"},"promptSource":"system","message":{"role":"user","content":"<task-notification><task-id>child-1</task-id><tool-use-id>call-agent</tool-use-id><status>completed</status><summary>Agent finished</summary></task-notification>"}}),
            json!({"type":"attachment","sessionId":"parent-session","uuid":"bash-report","attachment":{"type":"queued_command","commandMode":"task-notification","prompt":"<task-notification><task-id>bg-1</task-id><tool-use-id>call-bg</tool-use-id><status>completed</status><summary>Background command completed</summary></task-notification>"}}),
            json!({"type":"assistant","sessionId":"parent-session","uuid":"monitor-call","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-monitor","name":"Monitor","input":{"command":"watch"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","sessionId":"parent-session","uuid":"monitor-started","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-monitor","content":"Monitor started"}]},"toolUseResult":{"taskId":"monitor-1"}}),
            json!({"type":"user","sessionId":"parent-session","uuid":"monitor-event","origin":{"kind":"task-notification"},"promptSource":"system","message":{"role":"user","content":"<task-notification><task-id>monitor-1</task-id><summary>Monitor event</summary><event>done</event></task-notification>"}}),
        ],
    );
    write_jsonl(
        &child,
        &[
            json!({"type":"user","sessionId":"parent-session","agentId":"child-1","isSidechain":true,"parentUuid":null,"uuid":"child-request","message":{"role":"user","content":"audit it"}}),
            json!({"type":"user","sessionId":"parent-session","agentId":"child-1","isSidechain":true,"parentUuid":"child-request","uuid":"child-follow-up","message":{"role":"user","content":"sidechain follow-up"}}),
        ],
    );

    let directory = tempdir().expect("index directory");
    let mut store = Store::open(&directory.path().join("index.sqlite")).expect("open index");
    // Discovery is lexicographic, so `claude-child.jsonl` is indexed before
    // the parent. This exercises the direct-resolution path; the schema test
    // separately covers a target Session that appears after its Item.
    index_paths(&mut store, &[parent, child], &options(false)).expect("index Claude tasks");
    let connection = store.connection();

    let notification_roles = connection
        .prepare(
            "SELECT json_extract(i.semantic, '$.role')
               FROM items i
               JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE b.text LIKE '<task-notification>%'
              ORDER BY i.item_id",
        )
        .expect("prepare notification roles")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query notification roles")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect notification roles");
    assert_eq!(
        notification_roles,
        ["subagent.report", "tool.output", "tool.output"]
    );

    let (delegation_child, report_source): (i64, i64) = connection
        .query_row(
            "SELECT
                 (SELECT json_extract(semantic, '$.value.child_session_id')
                    FROM items
                   WHERE json_extract(semantic, '$.role') = 'agent.delegation'
                     AND json_extract(semantic, '$.value.text.blob_id') IS NOT NULL
                   ORDER BY item_id LIMIT 1),
                 (SELECT json_extract(semantic, '$.value.source_session_id')
                    FROM items
                   WHERE json_extract(semantic, '$.role') = 'subagent.report'
                   ORDER BY item_id LIMIT 1)",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read Claude Session links");
    assert_eq!(delegation_child, report_source);

    let (projected_openings, retained_follow_ups, searchable_parent_delegations): (i64, i64, i64) =
        connection
            .query_row(
                "SELECT
                 (SELECT COUNT(*)
                    FROM items i
                    JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
                   WHERE json_extract(i.semantic, '$.role') = 'agent.delegation'
                     AND b.text = 'audit it'),
                 (SELECT COUNT(*)
                    FROM items i
                    JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
                   WHERE b.text = 'sidechain follow-up'),
                 (SELECT COUNT(*)
                    FROM item_search s
                    JOIN items i ON i.item_id = s.rowid
                    JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
                   WHERE json_extract(i.semantic, '$.role') = 'agent.delegation'
                     AND b.text = 'audit it')",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("read deduplicated Claude delegation");
    assert_eq!(projected_openings, 1);
    assert_eq!(retained_follow_ups, 1);
    assert_eq!(searchable_parent_delegations, 1);

    let linked_delayed_outputs: i64 = connection
        .query_row(
            "SELECT COUNT(*)
               FROM items i
               JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE b.text LIKE '<task-notification>%'
                AND json_extract(i.semantic, '$.role') = 'tool.output'
                AND json_extract(i.semantic, '$.value.call_item_id') IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count linked delayed outputs");
    assert_eq!(linked_delayed_outputs, 2);
}

#[test]
fn claude_keeps_message_images_and_runtime_injections_together() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-images-and-skill.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"image-session","uuid":"request","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":[{"type":"text","text":"inspect this"},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]}}),
            json!({"type":"assistant","sessionId":"image-session","uuid":"skill-call","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-skill","name":"Skill","input":{"skill":"example"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","sessionId":"image-session","uuid":"skill-result","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-skill","content":"Launching skill: example"}]}}),
            json!({"type":"user","sessionId":"image-session","uuid":"skill-body","isMeta":true,"sourceToolUseID":"call-skill","message":{"role":"user","content":[{"type":"text","text":"Base directory for this skill: /skills/example\n\n# Example skill"}]}}),
            json!({"type":"user","sessionId":"image-session","uuid":"file-images","isMeta":true,"message":{"role":"user","content":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}},{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA=="}}]}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    let (human_count, human_images): (i64, bool) = connection
        .query_row(
            "SELECT COUNT(*), MAX(json_extract(semantic, '$.value.has_images'))
               FROM items
              WHERE json_extract(semantic, '$.role') LIKE 'human.%'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read human image message");
    assert_eq!(human_count, 1);
    assert!(human_images);

    let (instruction_category, instruction_text): (String, String) = connection
        .query_row(
            "SELECT json_extract(i.semantic, '$.value.category'), b.text
               FROM items i
               JOIN blobs b ON b.blob_id = json_extract(i.semantic, '$.value.text.blob_id')
              WHERE json_extract(i.semantic, '$.role') = 'runtime.instructions'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read Skill injection");
    assert_eq!(instruction_category, "skill");
    assert!(instruction_text.starts_with("Base directory for this skill:"));

    let (context_category, context_images): (String, bool) = connection
        .query_row(
            "SELECT json_extract(semantic, '$.value.category'),
                    json_extract(semantic, '$.value.has_images')
               FROM items
              WHERE json_extract(semantic, '$.role') = 'runtime.context'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("read Runtime image context");
    assert_eq!(context_category, "file");
    assert!(context_images);
}

#[test]
fn claude_preserves_structured_output_and_runtime_attachment_facts() {
    let traces = tempdir().expect("trace directory");
    let trace = traces.path().join("claude-output-facts.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"user","sessionId":"facts-session","uuid":"request","promptId":"prompt-1","origin":{"kind":"human"},"message":{"role":"user","content":"fetch"}}),
            json!({"type":"assistant","sessionId":"facts-session","uuid":"fetch-call","message":{"role":"assistant","content":[{"type":"tool_use","id":"call-fetch","name":"WebFetch","input":{"url":"https://example.test"}}],"stop_reason":"tool_use"}}),
            json!({"type":"user","sessionId":"facts-session","uuid":"fetch-result","message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call-fetch","content":"HTTP 404"}]},"toolUseResult":{"code":404,"codeText":"Not Found","durationMs":1572,"truncated":true}}),
            json!({"type":"attachment","sessionId":"facts-session","uuid":"tokens","attachment":{"type":"total_tokens_reminder","text":"<total_tokens>1000 tokens left</total_tokens>"}}),
            json!({"type":"attachment","sessionId":"facts-session","uuid":"hook","attachment":{"type":"hook_system_message","content":"Review before pushing","hookEvent":"PostToolUse"}}),
        ],
    );

    let (_directory, store) = index_fixture(&trace);
    let connection = store.connection();
    let (failed, duration_ms, runtime_truncated): (bool, i64, bool) = connection
        .query_row(
            "SELECT json_extract(semantic, '$.value.failed'),
                    json_extract(semantic, '$.value.duration_ms'),
                    json_extract(semantic, '$.value.runtime_truncated')
               FROM items
              WHERE json_extract(semantic, '$.role') = 'tool.output'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("read ToolOutput facts");
    assert!(failed);
    assert_eq!(duration_ms, 1572);
    assert!(runtime_truncated);

    let runtime_roles = connection
        .prepare(
            "SELECT json_extract(semantic, '$.role')
               FROM items
              WHERE json_extract(semantic, '$.role') LIKE 'runtime.%'
              ORDER BY item_id",
        )
        .expect("prepare Runtime roles")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query Runtime roles")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect Runtime roles");
    assert_eq!(runtime_roles, ["runtime.state", "runtime.notice"]);
}
