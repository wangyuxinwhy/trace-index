use serde_json::json;

use super::CodexProjector;
use super::parse::{
    classify_message, linked_session_from_tagged_text, runtime_arguments, shell_command,
    structured_tool_end_facts, tool_output_facts,
};
use crate::adapters::{projection::ItemDetail, semantic};

#[test]
fn nested_tool_metadata_does_not_invent_zero_duration() {
    let output = "Script completed\nOutput:\n{\"exit_code\":0,\"original_token_count\":4}";
    let facts = tool_output_facts(&[output]);
    assert_eq!(facts.duration_ms, None);
    assert_eq!(facts.output_tokens, Some(4));
    assert_eq!(facts.nonzero_exit, Some(false));
    assert_eq!(facts.truncated, Some(false));
}

#[test]
fn nested_legacy_metadata_preserves_exit_and_duration() {
    let output = r#"{"output":"","metadata":{"exit_code":1,"duration_seconds":0.1}}"#;
    let facts = tool_output_facts(&[output]);
    assert_eq!(facts.exit_code, Some(1));
    assert_eq!(facts.nonzero_exit, Some(true));
    assert_eq!(facts.duration_ms, Some(100));
}

#[test]
fn legacy_and_provider_tool_arguments_remain_queryable() {
    let shell_payload = json!({
        "arguments": {"command": ["/bin/zsh", "-lc", "printf hello"]}
    });
    let shell_args = runtime_arguments(&shell_payload, "function_call").unwrap();
    let shell_args = serde_json::from_str(&shell_args).unwrap();
    assert_eq!(
        shell_command(Some("shell"), Some(&shell_args)).as_deref(),
        Some("printf hello")
    );

    let web_payload = json!({
        "action": {"type": "open_page", "url": "https://example.com"}
    });
    assert_eq!(
        runtime_arguments(&web_payload, "web_search_call").as_deref(),
        Some(r#"{"type":"open_page","url":"https://example.com"}"#)
    );

    let image_payload = json!({"prompt": "cat", "revised_prompt": "orange cat"});
    assert_eq!(
        runtime_arguments(&image_payload, "image_generation_call").as_deref(),
        Some(r#"{"prompt":"cat","revised_prompt":"orange cat"}"#)
    );
}

#[test]
fn structured_end_is_authoritative_for_duration_and_outcome() {
    let facts = structured_tool_end_facts(&json!({
        "type": "exec_command_end",
        "duration": {"secs": 600, "nanos": 191_955_000},
        "exit_code": 1,
        "status": "failed"
    }));
    assert_eq!(facts.duration_ms, Some(600_191));
    assert_eq!(facts.exit_code, Some(1));
    assert_eq!(facts.nonzero_exit, Some(true));

    let mcp = structured_tool_end_facts(&json!({
        "type": "mcp_tool_call_end",
        "duration": {"secs": 0, "nanos": 339_252_833},
        "result": {"Ok": {"isError": true}}
    }));
    assert_eq!(mcp.duration_ms, Some(339));
    assert_eq!(mcp.nonzero_exit, Some(true));
}

#[test]
fn structured_end_merges_into_existing_tool_output() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "call",
                "output": "Chunk ID: x\nWall time: 1 seconds\nProcess exited with code 0\nOutput:\nrunning"
            }
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "event_msg",
            "payload": {
                "type": "exec_command_end",
                "call_id": "call",
                "duration": {"secs": 60, "nanos": 1_000_000},
                "exit_code": 1,
                "status": "failed"
            }
        }),
    );
    let sessions = projector.finish();
    let item = &sessions[0].loops[0].items[0];
    let ItemDetail::ToolOutput { facts, .. } = &item.detail else {
        panic!("expected tool output")
    };
    assert_eq!(facts.duration_ms, Some(60_001));
    assert_eq!(facts.exit_code, Some(1));
    assert_eq!(facts.nonzero_exit, Some(true));
    assert_eq!(item.ui_seq, Some(3));
}

#[test]
fn unpaired_inner_mcp_end_synthesizes_existing_tool_roles() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "timestamp": "1970-01-01T00:00:02Z",
            "type": "event_msg",
            "payload": {
                "type": "mcp_tool_call_end",
                "call_id": "inner-call",
                "invocation": {
                    "server": "codex",
                    "tool": "list_mcp_resources",
                    "arguments": {"cursor": "next"}
                },
                "duration": {"secs": 2, "nanos": 0},
                "result": {"Ok": {"content": [{"type": "text", "text": "done"}], "isError": false}}
            }
        }),
    );
    let sessions = projector.finish();
    let items = &sessions[0].loops[0].items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].seq, 2);
    assert_eq!(items[1].seq, 2);
    assert_eq!(items[0].ts_ms, Some(2_000));
    assert_eq!(items[1].ts_ms, Some(2_000));
    assert_eq!(items[0].semantic_role, semantic::AGENT_TOOL_CALL);
    assert_eq!(items[1].semantic_role, semantic::TOOL_OUTPUT);
    let ItemDetail::ToolCall { name, args, .. } = &items[0].detail else {
        panic!("expected tool call")
    };
    assert_eq!(name.as_deref(), Some("codex.list_mcp_resources"));
    assert!(args.is_some());
    let ItemDetail::ToolOutput { facts, .. } = &items[1].detail else {
        panic!("expected tool output")
    };
    assert_eq!(facts.duration_ms, Some(2_000));
    assert_eq!(facts.nonzero_exit, Some(false));
}

#[test]
fn child_history_before_declared_boundary_does_not_project_items() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({
            "ordinal": 0,
            "type": "session_meta",
            "payload": {
                "id": "child-session",
                "thread_source": "subagent",
                "agent_path": "/root/child",
                "subagent_history_start_ordinal": 3
            }
        }),
    );
    projector.push(
        1,
        &json!({
            "ordinal": 1,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "parent copy"}]
            }
        }),
    );
    projector.push(
        3,
        &json!({"ordinal": 3, "type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        4,
        &json!({
            "ordinal": 4,
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": "child answer"}]
            }
        }),
    );

    let sessions = projector.finish();
    let item_seqs = sessions[0]
        .loops
        .iter()
        .flat_map(|turn| turn.items.iter().map(|item| item.record_seq))
        .collect::<Vec<_>>();
    assert_eq!(item_seqs, vec![4]);
}

#[test]
fn structured_subagent_items_keep_native_session_links() {
    let text = concat!(
        "<subagent_notification>\n",
        r#"{"agent_path":"child-session","status":{"completed":"done"}}"#,
        "\n</subagent_notification>"
    );
    assert_eq!(
        linked_session_from_tagged_text(semantic::SUBAGENT_REPORT, Some(text)).as_deref(),
        Some("child-session")
    );

    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({
            "type": "session_meta",
            "payload": {"id": "child-session", "agent_path": "/root/child"}
        }),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "inter_agent_communication",
            "payload": {
                "author": "/root",
                "recipient": "/root/child",
                "content": "Message Type: NEW_TASK\nPayload:\nwork"
            }
        }),
    );
    let sessions = projector.finish();
    let item = &sessions[0].loops[0].items[0];
    assert_eq!(item.semantic_role, semantic::AGENT_DELEGATION);
    assert_eq!(
        item.linked_session_native_id.as_deref(),
        Some("child-session")
    );
    assert!(item.basis.contains("agent_path"));
}

#[test]
fn spawn_end_and_prior_activity_link_existing_items() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "spawn-call",
                "name": "spawn_agent",
                "arguments": "{\"message\":\"work\"}"
            }
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "event_msg",
            "payload": {
                "type": "collab_agent_spawn_end",
                "call_id": "spawn-call",
                "new_thread_id": "child-session"
            }
        }),
    );
    projector.push(
        4,
        &json!({
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "kind": "interacted",
                "agent_path": "/root/child",
                "agent_thread_id": "child-session"
            }
        }),
    );
    projector.push(
        5,
        &json!({
            "type": "inter_agent_communication",
            "payload": {
                "author": "/root/child",
                "recipient": "/root",
                "content": "Message Type: FINAL_ANSWER\nPayload:\ndone"
            }
        }),
    );

    let sessions = projector.finish();
    let items = &sessions[0].loops[0].items;
    assert_eq!(items[0].semantic_role, semantic::AGENT_DELEGATION);
    assert_eq!(
        items[0].linked_session_native_id.as_deref(),
        Some("child-session")
    );
    assert_eq!(items[0].ui_seq, Some(3));
    assert_eq!(items[1].semantic_role, semantic::SUBAGENT_ACTIVITY);
    assert_eq!(items[2].semantic_role, semantic::SUBAGENT_REPORT);
    assert_eq!(
        items[2].linked_session_native_id.as_deref(),
        Some("child-session")
    );
}

#[test]
fn legacy_spawn_output_links_the_delegation() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "spawn-call",
                "name": "spawn_agent",
                "arguments": "{\"message\":\"work\"}"
            }
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call_output",
                "call_id": "spawn-call",
                "output": "{\"agent_id\":\"child-session\",\"nickname\":\"Euler\"}"
            }
        }),
    );
    let sessions = projector.finish();
    let delegation = &sessions[0].loops[0].items[0];
    assert_eq!(delegation.semantic_role, semantic::AGENT_DELEGATION);
    assert_eq!(
        delegation.linked_session_native_id.as_deref(),
        Some("child-session")
    );
    assert_eq!(delegation.ui_seq, Some(3));
    assert_eq!(
        super::spawn_output_native_id(
            "You are the newly spawned agent. The prior conversation history was forked.",
            Some("current-child")
        )
        .as_deref(),
        Some("current-child")
    );

    let mut activity_projector = CodexProjector::new(1024);
    activity_projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "root-session"}}),
    );
    activity_projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    activity_projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "function_call",
                "call_id": "spawn-call",
                "name": "spawn_agent",
                "arguments": "{\"message\":\"work\"}"
            }
        }),
    );
    activity_projector.push(
        3,
        &json!({
            "type": "event_msg",
            "payload": {
                "type": "sub_agent_activity",
                "kind": "started",
                "event_id": "spawn-call",
                "agent_path": "/root/child",
                "agent_thread_id": "child-session"
            }
        }),
    );
    let sessions = activity_projector.finish();
    assert_eq!(
        sessions[0].loops[0].items[0]
            .linked_session_native_id
            .as_deref(),
        Some("child-session")
    );
}

#[test]
fn guardian_transcript_and_user_action_are_runtime_context() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({
            "type": "session_meta",
            "payload": {
                "id": "guardian",
                "source": {"subagent": {"other": "guardian"}}
            }
        }),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "The following is the Codex agent history whose request action you are assessing."}]
            }
        }),
    );
    let sessions = projector.finish();
    assert_eq!(
        sessions[0].loops[0].items[0].semantic_role,
        semantic::RUNTIME_INTERNAL_CONTEXT
    );

    let (role, _) = classify_message(
        "user",
        Some("<user_action>\n<context>review</context>\n</user_action>"),
        None,
        false,
    );
    assert_eq!(role, semantic::RUNTIME_INTERNAL_CONTEXT);
}

#[test]
fn first_human_message_after_agent_output_is_steering() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": "working"}]
            }
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "change direction"}
        }),
    );
    projector.push(
        4,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "change direction"}]
            }
        }),
    );

    let sessions = projector.finish();
    let items = &sessions[0].loops[0].items;
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].semantic_role, semantic::AGENT_COMMENTARY);
    assert_eq!(items[1].semantic_role, semantic::HUMAN_STEERING);
    assert_eq!(items[1].record_seq, 4);
    assert_eq!(items[1].ui_seq, Some(3));
}

#[test]
fn empty_agent_conversation_records_do_not_project_items() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "event_msg",
            "payload": {"type": "agent_message", "phase": "final_answer", "message": ""}
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "final_answer",
                "content": [{"type": "output_text", "text": ""}]
            }
        }),
    );
    projector.push(
        4,
        &json!({
            "type": "event_msg",
            "payload": {"type": "agent_message", "phase": "commentary", "message": ""}
        }),
    );
    projector.push(
        5,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "phase": "commentary",
                "content": [{"type": "output_text", "text": ""}]
            }
        }),
    );

    let sessions = projector.finish();
    assert!(sessions[0].loops[0].items.is_empty());
}

#[test]
fn old_unphased_answer_resolves_at_next_human_boundary() {
    let mut projector = CodexProjector::new(1024);
    projector.push(
        0,
        &json!({"type": "session_meta", "payload": {"id": "session"}}),
    );
    projector.push(
        1,
        &json!({"type": "turn_context", "payload": {"turn_id": "turn"}}),
    );
    projector.push(
        2,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "first"}]
            }
        }),
    );
    projector.push(
        3,
        &json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "first"}
        }),
    );
    projector.push(
        4,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "done"}]
            }
        }),
    );
    projector.push(
        5,
        &json!({
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "next"}]
            }
        }),
    );
    projector.push(
        6,
        &json!({
            "type": "event_msg",
            "payload": {"type": "user_message", "message": "next"}
        }),
    );
    projector.push(
        7,
        &json!({"type": "turn_context", "payload": {"turn_id": "next-turn"}}),
    );
    let sessions = projector.finish();
    assert_eq!(sessions[0].loops.len(), 2);
    assert_eq!(
        sessions[0].loops[0].items[1].semantic_role,
        semantic::AGENT_FINAL_ANSWER
    );
    assert_eq!(
        sessions[0].loops[1].items[0].semantic_role,
        semantic::HUMAN_REQUEST
    );
    assert_eq!(sessions[0].loops[1].items[0].record_seq, 5);
}

#[test]
fn user_shell_command_is_runtime_context_not_unknown() {
    let (role, _) = classify_message(
        "user",
        Some("<user_shell_command>\necho hello\n</user_shell_command>"),
        None,
        false,
    );
    assert_eq!(role, semantic::RUNTIME_INTERNAL_CONTEXT);
}
