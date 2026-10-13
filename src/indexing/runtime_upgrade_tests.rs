//! End-to-end publication checks for newer Runtime evidence shapes.

use super::{count, index_fixture, index_paths, options, write_jsonl};
use rusqlite::Connection;
use serde_json::{Value, json};
use tempfile::tempdir;

fn semantics(connection: &Connection) -> Vec<Value> {
    connection
        .prepare("SELECT semantic FROM items ORDER BY item_id")
        .expect("prepare Items")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("query Items")
        .map(|row| serde_json::from_str(&row.expect("Item row")).expect("Semantic JSON"))
        .collect()
}

fn text(connection: &Connection, semantic: &Value) -> String {
    let id = semantic["value"]["text"]["blob_id"]
        .as_i64()
        .expect("BlobRef");
    connection
        .query_row("SELECT text FROM blobs WHERE blob_id = ?", [id], |row| {
            row.get(0)
        })
        .expect("Semantic text")
}

#[test]
fn pi_prompt_patches_never_open_a_human_loop_or_rewrite_history() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("pi.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session","version":3,"id":"pi-prompt","cwd":"/workspace"}),
            json!({"type":"model_change","modelId":"virtual-model"}),
            json!({"type":"thinking_level_change","thinkingLevel":"medium"}),
            json!({"type":"message","id":"prompt","message":{"role":"system","content":"","sections":{"preamble":"Initial instructions","skills":"Skill catalog"},"toolsAdded":[{"name":"read","description":"Read a file","parameters":{"type":"object"}}]}}),
            json!({"type":"message","id":"request-1","message":{"role":"user","content":"Inspect a file"}}),
            json!({"type":"message","message":{"role":"assistant","model":"physical-model","thinkingLevel":"high","stopReason":"toolUse","content":[{"type":"toolCall","id":"read-1","name":"read","arguments":{"path":"file.txt"}}]}}),
            json!({"type":"message","message":{"role":"toolResult","toolCallId":"read-1","content":[{"type":"text","text":"File content"}],"isError":false,"durationMs":123}}),
            json!({"type":"message","message":{"role":"assistant","model":"physical-model","thinkingLevel":"high","stopReason":"stop","content":[{"type":"text","text":"Inspected"}]}}),
            json!({"type":"message","message":{"role":"system","content":"Appended instruction","sections":{"preamble":"Updated instructions","skills":null},"toolsRemoved":[{"name":"read"}]}}),
            json!({"type":"message","id":"request-2","message":{"role":"user","content":"Continue"}}),
            json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Done"}]}}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 2);
    let items = semantics(db);
    assert_eq!(
        items
            .iter()
            .filter(|s| s["role"] == "human.request")
            .count(),
        2
    );
    assert!(
        !items
            .iter()
            .any(|s| s["role"] == "human.steering" || s["role"] == "runtime.unknown")
    );
    let instructions: Vec<_> = items
        .iter()
        .filter(|s| s["role"] == "runtime.instructions")
        .map(|s| text(db, s))
        .collect();
    for expected in [
        "Initial instructions",
        "Updated instructions",
        "Skill catalog",
        "Appended instruction",
        "Read a file",
        "Removed tool: read",
    ] {
        assert!(
            instructions.iter().any(|value| value.contains(expected)),
            "missing {expected}"
        );
    }
    assert!(
        items.iter().any(|s| s["role"] == "runtime.state"
            && text(db, s).contains("Removed prompt section: skills"))
    );
    let output = items
        .iter()
        .find(|s| s["role"] == "tool.output")
        .expect("Tool Output");
    assert_eq!(output["value"]["duration_ms"], 123);
    assert!(output["value"]["call_item_id"].is_number());
    let starts: Vec<i64> = db.prepare("SELECT r.source_position FROM loops l JOIN records r ON r.record_id = l.start_record_id ORDER BY l.session_position")
        .expect("prepare starts").query_map([], |row| row.get(0)).expect("starts")
        .collect::<Result<_, _>>().expect("start rows");
    assert_eq!(starts, [4, 9]);
    let model: String = db
        .query_row(
            "SELECT model FROM loops WHERE session_position = 0",
            [],
            |row| row.get(0),
        )
        .expect("model");
    assert_eq!(
        serde_json::from_str::<Value>(&model).expect("model JSON"),
        json!({"id":"physical-model","effort":"high"})
    );
    let session_items: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM items WHERE loop_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("Session Items");
    assert_eq!(session_items, 7);
}

#[test]
fn pi_system_only_source_has_no_loop() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("pi-system.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session","version":3,"id":"pi-system","cwd":"/workspace"}),
            json!({"type":"message","message":{"role":"system","content":"System instruction","sections":{"optional":null}}}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    assert_eq!(count(store.connection(), "loops"), 0);
    assert_eq!(count(store.connection(), "items"), 2);
}

#[test]
fn pi_idle_custom_messages_do_not_extend_completed_loops_or_invent_runs() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("pi-custom.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session","version":3,"id":"pi-custom","cwd":"/workspace"}),
            json!({"type":"message","message":{"role":"user","content":"First request"}}),
            json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"First answer"}]}}),
            json!({"type":"custom_message","customType":"context-update","content":"Idle context"}),
            json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Extension-triggered answer"}]}}),
            json!({"type":"custom_message","customType":"context-update","content":"Notification only"}),
            json!({"type":"message","message":{"role":"user","content":"Next request"}}),
            json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Next answer"}]}}),
            json!({"type":"custom_message","customType":"context-update","content":"Trailing notification"}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 3);
    let items = semantics(db);
    assert_eq!(
        items
            .iter()
            .filter(|s| s["role"] == "human.request")
            .count(),
        2
    );
    assert!(!items.iter().any(|s| s["role"] == "human.steering"));
    assert_eq!(db.query_row::<i64,_,_>("SELECT COUNT(*) FROM items WHERE loop_id IS NULL AND json_extract(semantic,'$.role') = 'runtime.unknown'",[],|r|r.get(0)).expect("idle context"),3);
    assert_eq!(db.query_row::<i64,_,_>("SELECT MAX(n) FROM (SELECT COUNT(*) AS n FROM items WHERE json_extract(semantic,'$.role') = 'agent.final_answer' GROUP BY loop_id)",[],|r|r.get(0)).expect("one answer per run"),1);
}

#[test]
fn pi_context_edits_and_compaction_checkpoints_preserve_original_items() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("pi-context.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session","version":3,"id":"pi-context","cwd":"/workspace"}),
            json!({"type":"message","id":"original","message":{"role":"user","content":"Original request"}}),
            json!({"type":"context_edit","targetId":"original","replacement":{"content":"Replacement for future context"}}),
            json!({"type":"context_edit","targetId":"original","replacement":null}),
            json!({"type":"compaction","summary":"Earlier work summarized","systemMessage":{"role":"system","content":"Checkpoint instructions"}}),
            json!({"type":"message","message":{"role":"assistant","content":[{"type":"text","text":"Done"}],"stopReason":"stop"}}),
            json!({"type":"context_edit","targetId":"original","replacement":null}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 1);
    let items = semantics(db);
    let request = items
        .iter()
        .find(|s| s["role"] == "human.request")
        .expect("original Item");
    assert_eq!(text(db, request), "Original request");
    assert_eq!(
        items
            .iter()
            .filter(|s| s["role"] == "runtime.state")
            .count(),
        3
    );
    assert!(
        items.iter().any(
            |s| s["role"] == "runtime.instructions" && text(db, s) == "Checkpoint instructions"
        )
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT COUNT(*) FROM items WHERE loop_id IS NULL",
            [],
            |row| row.get(0)
        )
        .expect("Session state"),
        1
    );
}

#[test]
fn pi_retry_context_edits_belong_to_the_same_outer_loop() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("pi-retry.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session","version":3,"id":"pi-retry","cwd":"/workspace"}),
            json!({"type":"message","id":"request","message":{"role":"user","content":"Work"}}),
            json!({"type":"message","id":"failed-attempt","message":{"role":"assistant","stopReason":"error","content":[]}}),
            json!({"type":"context_edit","targetId":"failed-attempt","replacement":null}),
            json!({"type":"message","message":{"role":"assistant","stopReason":"stop","content":[{"type":"text","text":"Recovered"}]}}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 1);
    let (state_scope, outcome): (Option<i64>, String) = db
        .query_row(
            "SELECT i.loop_id, json_extract(l.end, '$.outcome') FROM items i
         JOIN loops l ON l.loop_id = i.loop_id
         WHERE json_extract(i.semantic, '$.role') = 'runtime.state'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("recovery scope");
    assert!(state_scope.is_some());
    assert_eq!(outcome, "completed");
}

fn usage(input: u64, output: u64) -> Value {
    json!({"input_tokens":input,"cached_input_tokens":input / 2,"cache_write_input_tokens":0,"output_tokens":output,"reasoning_output_tokens":output / 2})
}

fn usage_record(turn: &str, input: u64, output: u64) -> Value {
    json!({"type":"token_usage_record","payload":{"thread_id":"codex-usage","turn_id":turn,"response_id":"response","usage":usage(2,1),"turn_token_usage":usage(input,output),"thread_token_usage":usage(9999,999)}})
}

fn legacy_usage(input: u64, output: u64) -> Value {
    json!({"type":"event_msg","payload":{"type":"token_count","info":{"last_token_usage":usage(input,output),"total_token_usage":usage(9999,999),"model_context_window":128_000}}})
}

#[test]
fn codex_turn_counters_replace_legacy_usage_without_cross_turn_or_duplicate_totals() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-usage.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-usage"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"turn_context","payload":{"turn_id":"one","model":"model"}}),
            legacy_usage(10, 2),
            usage_record("one", 30, 4),
            legacy_usage(20, 2),
            usage_record("one", 30, 4),
            usage_record("one", 45, 6),
            json!({"type":"token_usage_record","payload":{"turn_id":"one","turn_token_usage":{"input_tokens":-1,"output_tokens":5}}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
            json!({"type":"turn_context","payload":{"turn_id":"two","model":"model"}}),
            legacy_usage(7, 1),
            usage_record("one", 50, 8),
            json!({"type":"token_usage_record","payload":{"thread_id":"other-session","turn_id":"two","turn_token_usage":usage(10000,100)}}),
            usage_record("unknown-turn", 10000, 100),
            usage_record("two", 8, 2),
            legacy_usage(8, 2),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 2);
    let values: Vec<String> = db
        .prepare("SELECT usage FROM loops ORDER BY session_position")
        .expect("prepare Usage")
        .query_map([], |row| row.get(0))
        .expect("query Usage")
        .collect::<Result<_, _>>()
        .expect("Usage rows");
    assert_eq!(
        serde_json::from_str::<Value>(&values[0]).expect("first Usage"),
        json!({"input":50,"cached":25,"cache_write":0,"output":8,"reasoning":4})
    );
    assert_eq!(
        serde_json::from_str::<Value>(&values[1]).expect("second Usage"),
        json!({"input":8,"cached":4,"cache_write":0,"output":2,"reasoning":1})
    );
    assert_eq!(count(db, "items"), 0, "Usage is a Loop fact, not an Item");
}

#[test]
fn codex_structured_usage_alone_updates_an_unfinished_loop() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-structured.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-usage"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"turn_context","payload":{"turn_id":"one","model":"model"}}),
            usage_record("one", 42, 6),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let value: String = store
        .connection()
        .query_row("SELECT usage FROM loops", [], |row| row.get(0))
        .expect("Usage");
    assert_eq!(
        serde_json::from_str::<Value>(&value).expect("Usage JSON")["input"],
        42
    );
}

fn command_completion(source: &str, command: &[&str]) -> Value {
    json!({"type":"event_msg","payload":{"type":"item_completed","turn_id":"one","item":{
        "type":"CommandExecution","id":"command-1","source":source,"command":command,
        "cwd":"file:///workspace/a%20b","status":"failed","aggregated_output":"native output",
        "exit_code":7,"duration":{"secs":2,"nanos":150_000_000}
    }}})
}

#[test]
fn codex_native_executions_publish_shell_and_result_without_guessing_script_calls() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-exec.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-exec"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"turn_context","payload":{"turn_id":"one","model":"model"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"outer","name":"exec","input":"if (false) await tools.exec_command({cmd: 'echo never'}); await tools.exec_command({cmd: 'printf actual >out.txt'});"}}),
            command_completion(
                "unified_exec_startup",
                &["/bin/zsh", "-lc", "printf actual >out.txt"],
            ),
            command_completion(
                "unified_exec_startup",
                &["/bin/zsh", "-lc", "printf actual >out.txt"],
            ),
            command_completion("user_shell", &["/bin/zsh", "-lc", "echo human"]),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    let items = semantics(db);
    assert_eq!(
        items.len(),
        3,
        "outer call plus actual child call and output"
    );
    let shell = items
        .iter()
        .find(|s| s["role"] == "agent.tool_call.shell")
        .expect("Shell call");
    assert_eq!(shell["value"]["tool_name"], "exec_command");
    assert_eq!(shell["value"]["working_directory"], "/workspace/a b");
    assert!(
        shell["value"].get("arguments").is_none(),
        "do not invent Agent-authored arguments from argv"
    );
    assert!(
        !shell["value"]["shell_fragments"]
            .as_array()
            .expect("fragments")
            .is_empty()
    );
    let output = items
        .iter()
        .find(|s| s["role"] == "tool.output")
        .expect("output");
    assert_eq!(output["value"]["exit_code"], 7);
    assert_eq!(output["value"]["duration_ms"], 2150);
    assert_eq!(output["value"]["failed"], true);
    assert!(output["value"]["call_item_id"].is_number());
    assert_eq!(text(db, output), "native output");
}

#[test]
fn codex_scripts_without_execution_evidence_are_not_shell_calls() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-script.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-script"}}),
            json!({"type":"response_item","payload":{"type":"custom_tool_call","call_id":"outer","name":"exec","input":"await tools.exec_command({cmd: 'echo candidate'});"}}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let items = semantics(store.connection());
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["role"], "agent.tool_call");
}

#[test]
fn codex_command_twins_deduplicate_in_either_order() {
    for completion_first in [false, true] {
        let traces = tempdir().expect("traces");
        let trace = traces.path().join("codex-twins.jsonl");
        let response = json!({"type":"response_item","payload":{"type":"function_call","call_id":"command-1","name":"exec_command","arguments":"{\"cmd\":\"echo actual\"}"}});
        let output = json!({"type":"response_item","payload":{"type":"function_call_output","call_id":"command-1","output":"Warning: truncated output (original token count: 1000)\nresponse output"}});
        let completion = command_completion("agent", &["/bin/bash", "-c", "echo actual"]);
        let mut records = vec![
            json!({"type":"session_meta","payload":{"id":"codex-twins"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"turn_context","payload":{"turn_id":"one","model":"model"}}),
        ];
        if completion_first {
            records.extend([completion.clone(), response, output]);
        } else {
            records.extend([response, output, completion.clone()]);
        }
        records.push(completion);
        write_jsonl(&trace, &records);
        let (_directory, store) = index_fixture(&trace);
        let db = store.connection();
        let items = semantics(db);
        assert_eq!(items.len(), 2, "one invocation and one output");
        let call = items
            .iter()
            .find(|s| s["role"] == "agent.tool_call.shell")
            .expect("call");
        assert_eq!(call["value"]["arguments"], json!({"cmd":"echo actual"}));
        assert_eq!(call["value"]["working_directory"], "/workspace/a b");
        let result = items
            .iter()
            .find(|s| s["role"] == "tool.output")
            .expect("output");
        assert_eq!(result["value"]["exit_code"], 7);
        assert_eq!(result["value"]["runtime_truncated"], true);
        assert_eq!(result["value"]["runtime_output_tokens"], 1000);
        assert_eq!(text(db, result), "native output");
        let argument_witness: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM items i, json_each(i.record_ids) witness
             JOIN records r ON r.record_id = witness.value
             WHERE json_extract(i.semantic, '$.role') = 'agent.tool_call.shell'
               AND r.source_position = ?1",
                [if completion_first { 4 } else { 3 }],
                |row| row.get(0),
            )
            .expect("Agent argument provenance");
        assert_eq!(
            argument_witness, 1,
            "native repeats must not evict the argument witness"
        );
        let citations: Vec<i64> = db
            .prepare("SELECT json_array_length(record_ids) FROM items")
            .expect("citations")
            .query_map([], |row| row.get(0))
            .expect("query citations")
            .collect::<Result<_, _>>()
            .expect("citation rows");
        assert_eq!(citations, [2, 2]);
    }
}

#[test]
fn codex_command_response_completes_shell_fields_in_either_order() {
    for completion_first in [false, true] {
        for argv in [
            vec!["/bin/bash", "-c", "echo actual", "label"],
            vec!["/bin/bash", "-e", "-c", "echo actual"],
        ] {
            let traces = tempdir().expect("traces");
            let trace = traces.path().join("codex-shell-twins.jsonl");
            let response = json!({"type":"response_item","payload":{"type":"function_call","call_id":"command-1","name":"exec_command","arguments":"{\"cmd\":\"echo actual\"}"}});
            let completion = command_completion("agent", &argv);
            let mut records = vec![
                json!({"type":"session_meta","payload":{"id":"codex-shell-twins"}}),
                json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            ];
            if completion_first {
                records.extend([completion, response]);
            } else {
                records.extend([response, completion]);
            }
            write_jsonl(&trace, &records);
            let (_directory, store) = index_fixture(&trace);
            let items = semantics(store.connection());
            assert_eq!(items.len(), 2);
            assert!(items.iter().any(|s| s["role"] == "agent.tool_call.shell"));
        }
    }
}

#[test]
fn codex_late_native_completion_keeps_its_original_loop() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-late.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-late"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            json!({"type":"event_msg","payload":{"type":"task_complete","turn_id":"one"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"two"}}),
            command_completion("unified_exec_startup", &["/bin/bash", "-c", "echo late"]),
            command_completion("unified_exec_startup", &["/bin/bash", "-c", "echo late"]),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    assert_eq!(count(db, "loops"), 2);
    assert_eq!(count(db, "items"), 2);
    let positions: Vec<i64> = db
        .prepare("SELECT l.session_position FROM items i JOIN loops l USING(loop_id)")
        .expect("scope")
        .query_map([], |row| row.get(0))
        .expect("scope rows")
        .collect::<Result<_, _>>()
        .expect("positions");
    assert_eq!(positions, [0, 0]);
}

#[test]
fn codex_non_shell_argv_and_unknown_sources_do_not_invent_shell_structure() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("codex-argv.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"session_meta","payload":{"id":"codex-argv"}}),
            json!({"type":"event_msg","payload":{"type":"task_started","turn_id":"one"}}),
            command_completion("agent", &["python3", "-c", "print('not shell')"]),
            command_completion("unknown", &["/bin/bash", "-c", "echo ignored"]),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let items = semantics(store.connection());
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["role"], "agent.tool_call");
}

#[test]
fn claude_model_and_progress_attachments_have_known_runtime_roles() {
    let traces = tempdir().expect("traces");
    let trace = traces.path().join("claude.jsonl");
    write_jsonl(
        &trace,
        &[
            json!({"type":"attachment","sessionId":"claude-context","attachment":{"type":"model","text":"Model identity"}}),
            json!({"type":"attachment","sessionId":"claude-context","attachment":{"type":"silent_turn_reminder","text":"Report progress"}}),
            json!({"type":"attachment","sessionId":"claude-context","attachment":{"type":"elapsed_time_reminder","text":"Elapsed time"}}),
        ],
    );
    let (_directory, store) = index_fixture(&trace);
    let db = store.connection();
    let items = semantics(db);
    assert_eq!(
        items
            .iter()
            .map(|s| s["role"].as_str().expect("role"))
            .collect::<Vec<_>>(),
        ["runtime.context", "runtime.notice", "runtime.notice"]
    );
    assert_eq!(text(db, &items[0]), "Model identity");
    assert_eq!(text(db, &items[1]), "Report progress");
    assert_eq!(text(db, &items[2]), "Elapsed time");
    assert_eq!(count(db, "loops"), 0);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one Source evolves from a fork header to a child execution"
)]
fn claude_fork_context_header_preserves_identity_evidence_and_append_behavior() {
    let traces = tempdir().expect("traces");
    let child = traces.path().join("child.jsonl");
    let parent = traces.path().join("parent.jsonl");
    let header = json!({"type":"fork-context-ref","agentId":"child","parentSessionId":"parent","parentLastUuid":"parent-message","contextLength":20});
    write_jsonl(&child, std::slice::from_ref(&header));
    let (_directory, mut store) = index_fixture(&child);
    assert_eq!(count(store.connection(), "sessions"), 1);
    assert_eq!(count(store.connection(), "loops"), 0);
    let initial = semantics(store.connection());
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0]["role"], "runtime.context");
    assert_eq!(initial[0]["value"]["category"], "session_reference");
    assert_eq!(
        serde_json::from_str::<Value>(&text(store.connection(), &initial[0])).unwrap(),
        header
    );
    let (identity, fork): (String, Option<String>) = store
        .connection()
        .query_row("SELECT native_id, forked_from FROM sessions", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })
        .unwrap();
    assert_eq!(identity, "child");
    assert!(fork.is_none(), "a missing parent remains unresolved");

    write_jsonl(
        &parent,
        &[json!({"type":"mode","sessionId":"parent","mode":"default"})],
    );
    index_paths(&mut store, &[parent], &options(false)).expect("index parent later");
    let (fork_parent, delegated_parent, witness_position): (String, String, i64) = store
        .connection()
        .query_row(
            "SELECT p.native_id, d.native_id, r.source_position
           FROM sessions c
           JOIN sessions p ON p.session_id=json_extract(c.forked_from, '$.session_id')
           JOIN sessions d ON d.session_id=json_extract(c.delegated_from, '$.session_id')
           JOIN records r ON r.record_id=json_extract(c.forked_from, '$.record_id')
          WHERE c.native_id='child'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        (
            fork_parent.as_str(),
            delegated_parent.as_str(),
            witness_position
        ),
        ("parent", "parent", 0)
    );

    write_jsonl(
        &child,
        &[
            header,
            json!({"type":"assistant","sessionId":"parent","agentId":"child","isSidechain":true,"message":{"id":"response","role":"assistant","model":"test-model","stop_reason":"tool_use","content":[{"type":"tool_use","id":"call","name":"Bash","input":{"command":"echo ok"}}],"usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":1,"output_tokens":7}}}),
            json!({"type":"user","sessionId":"parent","agentId":"child","isSidechain":true,"message":{"role":"user","content":[{"type":"tool_result","tool_use_id":"call","content":"ok"}]}}),
            json!({"type":"assistant","sessionId":"parent","agentId":"child","isSidechain":true,"message":{"id":"final","role":"assistant","model":"test-model","stop_reason":"end_turn","content":[{"type":"text","text":"done"}],"usage":{"input_tokens":1,"output_tokens":1}}}),
        ],
    );
    let report =
        index_paths(&mut store, &[child], &options(false)).expect("append child execution");
    assert_eq!(report.indexed_records, 3);
    assert_eq!(report.failed_files, 0);
    assert_eq!(
        count(store.connection(), "records"),
        5,
        "referenced parent context is not copied"
    );
    assert_eq!(count(store.connection(), "sessions"), 2);
    assert_eq!(count(store.connection(), "loops"), 1);
    let items = semantics(store.connection());
    assert_eq!(
        items
            .iter()
            .map(|item| item["role"].as_str().unwrap())
            .collect::<Vec<_>>(),
        [
            "runtime.context",
            "agent.tool_call.shell",
            "tool.output",
            "agent.final_answer"
        ]
    );
    let (input, output): (i64, i64) = store
        .connection()
        .query_row(
            "SELECT json_extract(usage,'$.input'), json_extract(usage,'$.output') FROM loops",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!((input, output), (7, 8));
    let (native_id, parent_id, fork_witness, delegation_witness, item_witness, locator):
        (String, String, i64, i64, i64, String) = store.connection().query_row(
        "SELECT c.native_id, p.native_id,
                json_extract(c.forked_from,'$.record_id'),
                json_extract(c.delegated_from,'$.record_id'),
                json_extract(i.record_ids,'$[0]'), src.locator
           FROM sessions c
           JOIN sessions p ON p.session_id=json_extract(c.forked_from,'$.session_id')
           JOIN items i ON i.session_id=c.session_id AND json_extract(i.semantic,'$.role')='runtime.context'
           JOIN records r ON r.record_id=json_extract(i.record_ids,'$[0]')
           JOIN sources src USING(source_id)
          WHERE c.native_id='child'", [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?, row.get(5)?)),
    ).unwrap();
    assert_eq!(
        (native_id.as_str(), parent_id.as_str()),
        ("child", "parent")
    );
    assert_eq!(
        (fork_witness, delegation_witness),
        (item_witness, item_witness)
    );
    assert_eq!(
        locator,
        traces
            .path()
            .join("child.jsonl")
            .canonicalize()
            .unwrap()
            .to_string_lossy()
    );
}
