use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::time::Duration;

use tempfile::tempdir;

use super::params::{parse_time_instant_ms, resolve_template_params_at};
use super::parser::{parse_template_source, render_nodes};
use super::{
    BUILTIN_SOURCES, TemplateOrigin, TemplateRegistry, execute_template, render_template,
    validate_user_templates,
};
use crate::interface::query::{QueryBudget, query_sql_with_params};
use crate::storage::db::Store;
use crate::storage::public_schema::PUBLIC_RELATIONS;

const TEST_BUDGET: QueryBudget = QueryBudget {
    limit: 50,
    max_cell_bytes: 4096,
    max_output_bytes: 256 * 1024,
    timeout: Duration::from_secs(5),
};

fn seed_rich_fixture(store: &Store) {
    store
        .connection()
        .execute_batch(
            r#"
            INSERT INTO trace_sources(id, adapter, path)
            VALUES (1, 'codex', '/tmp/codex-a.jsonl'),
                   (2, 'claude', '/tmp/claude-b.jsonl');
            INSERT INTO trace_records(
                id, source_id, seq, byte_offset, byte_length, raw_hash, native_type, parse_status
            ) VALUES
                (1, 1, 0, 0, 100, 'h1', 'session_meta', 'ok'),
                (2, 1, 1, 100, 100, 'h2', 'user_message', 'ok'),
                (3, 1, 2, 200, 100, 'h3', 'function_call', 'ok'),
                (4, 1, 3, 300, 100, 'h4', 'function_call_output', 'ok'),
                (5, 1, 4, 400, 100, 'h5', 'agent_message', 'ok'),
                (6, 2, 0, 0, 100, 'h6', 'user', 'ok');
            INSERT INTO domain_sessions(id, runtime, native_id)
            VALUES (1, 'codex', 'aaaa1111-2222-3333-4444-555566667777'),
                   (2, 'claude', 'bbbb1111-2222-3333-4444-555566667777');
            INSERT INTO session_sources(session_id, source_id, identity_record_id, created_at, name, working_directory)
            VALUES (1, 1, 1, 1763500000000, 'alpha session', '/workspace/trace-index'),
                   (2, 2, 6, 1763600000000, 'beta session', '/workspace/other');
            INSERT INTO domain_loops(id, session_id, source_id, ordinal, session_position, start_record_id)
            VALUES (1, 1, 1, 0, 0, 2),
                   (2, 1, 1, 1, 1, 5);
            INSERT INTO content_blobs(id, hash, text, full_bytes, estimated_tokens)
            VALUES (1, X'01', 'Please investigate gateway relay timeout in trace-index', 55, 14),
                   (2, X'02', 'Found relay configuration and fixed gateway relay routing', 57, 14),
                   (3, X'03', 'Session-level runtime instructions context', 42, 10),
                   (4, X'04', 'cargo test failed with exit code 101', 36, 9);
            INSERT INTO domain_items(id, session_id, source_id, loop_id, loop_position, occurred_at, semantic)
            VALUES
                (1, 1, 1, NULL, NULL, 1763500000100,
                 json('{"role":"runtime.instructions","value":{"category":"system","text":{"blob_id":3}},"evidence_strength":"structural"}')),
                (2, 1, 1, 1, 0, 1763500001000,
                 json('{"role":"human.request","value":{"text":{"blob_id":1},"has_images":false},"evidence_strength":"structural"}')),
                (3, 1, 1, 1, 1, 1763500002000,
                 json('{"role":"agent.tool_call.shell","value":{"tool_name":"exec_command","shell_fragments":[{"completeness":"complete","text":{"value":"cargo test --locked"},"statements":[{"invocations":[{"program":"cargo"}]}]}]},"evidence_strength":"structural"}')),
                (4, 1, 1, 1, 2, 1763500003000,
                 json('{"role":"tool.output","value":{"call_item_id":3,"exit_code":101,"failed":true,"duration_ms":420,"text":{"blob_id":4}},"evidence_strength":"structural"}')),
                (5, 1, 1, 2, 0, 1763500004000,
                 json('{"role":"agent.final_answer","value":{"text":{"blob_id":2},"has_images":false},"evidence_strength":"structural"}'));
            INSERT INTO item_records(item_id, record_id)
            VALUES (1, 1), (2, 2), (3, 3), (4, 4), (5, 5);
            INSERT INTO item_search(rowid, text)
            VALUES (1, 'session level runtime instructions context'),
                   (2, 'please investigate gateway relay timeout in trace index'),
                   (5, 'found relay configuration and fixed gateway relay routing');
            "#,
        )
        .expect("seed fixture");
}

#[test]
fn parses_and_renders_line_granular_conditionals_and_rejects_malformed_directives() {
    let source = r#"/* trace-index-template
name = "sample-query"
description = "Sample conditional template"
default_limit = 25

[params.session]
type = "int"

[params.include_extra]
type = "bool"
default = false
*/
SELECT item_id
FROM items
WHERE item_id > 0
-- #if session
  AND session_id = :session
-- #endif
-- #if include_extra
  AND loop_id IS NOT NULL
-- #else
  AND loop_id IS NULL
-- #endif
"#;
    let parsed = parse_template_source(source, "sample.sql").expect("parse valid template");
    assert_eq!(parsed.header.name, "sample-query");
    assert_eq!(parsed.header.default_limit, 25);

    let mut flags = BTreeMap::new();
    flags.insert("session".to_owned(), true);
    flags.insert("include_extra".to_owned(), false);
    let bound = BTreeMap::new();
    let rendered = render_nodes(&parsed.nodes, &flags, &bound);
    assert!(rendered.contains("AND session_id = :session"));
    assert!(rendered.contains("AND loop_id IS NULL"));
    assert!(!rendered.contains("AND loop_id IS NOT NULL"));

    // Value equality directive (`-- #if param=value`) validates enum values and branches cleanly.
    let eq_source = r#"/* trace-index-template
name = "eq-sample"
description = "Equality conditional sample"

[params.sort]
type = "enum"
values = ["relevance", "recent"]
default = "relevance"
*/
SELECT 1
-- #if sort=recent
ORDER BY occurred_at DESC
-- #else
ORDER BY rank
-- #endif
"#;
    let eq_parsed = parse_template_source(eq_source, "eq.sql").expect("parse equality directive");
    let mut eq_bound = BTreeMap::new();
    eq_bound.insert(
        "sort".to_owned(),
        crate::interface::query::SqlParamValue::Text("recent".to_owned()),
    );
    let rendered_recent = render_nodes(&eq_parsed.nodes, &BTreeMap::new(), &eq_bound);
    assert!(rendered_recent.contains("ORDER BY occurred_at DESC"));
    assert!(!rendered_recent.contains("ORDER BY rank"));

    eq_bound.insert(
        "sort".to_owned(),
        crate::interface::query::SqlParamValue::Text("relevance".to_owned()),
    );
    let rendered_rel = render_nodes(&eq_parsed.nodes, &BTreeMap::new(), &eq_bound);
    assert!(rendered_rel.contains("ORDER BY rank"));
    assert!(!rendered_rel.contains("ORDER BY occurred_at DESC"));

    let bad_enum_eq = eq_source.replace("sort=recent", "sort=bogus");
    assert!(
        parse_template_source(&bad_enum_eq, "bad_eq.sql")
            .expect_err("invalid enum value in directive")
            .to_string()
            .contains("is not in")
    );

    // Undeclared parameter in `-- #if` fails at load time with line number.
    let bad_undeclared = "/* trace-index-template\nname = \"bad\"\ndescription = \"bad\"\n*/\nSELECT 1\n-- #if ghost\nAND 1 = 1\n-- #endif\n";
    let err = parse_template_source(bad_undeclared, "bad.sql").expect_err("undeclared param");
    assert!(err.to_string().contains("bad.sql:6"), "{err}");

    // Nested `-- #if` fails clearly.
    let bad_nested = "/* trace-index-template\nname = \"bad\"\ndescription = \"bad\"\n[params.a]\ntype = \"int\"\n[params.b]\ntype = \"int\"\n*/\nSELECT 1\n-- #if a\n-- #if b\n-- #endif\n-- #endif\n";
    assert!(
        parse_template_source(bad_nested, "nested.sql")
            .expect_err("nested if")
            .to_string()
            .contains("nested `-- #if`")
    );

    // Bool without explicit default fails clearly.
    let bad_bool = "/* trace-index-template\nname = \"bad\"\ndescription = \"bad\"\n[params.flag]\ntype = \"bool\"\n*/\nSELECT 1\n";
    assert!(
        parse_template_source(bad_bool, "bool.sql")
            .expect_err("bool without default")
            .to_string()
            .contains("explicit boolean default")
    );
}

#[test]
fn time_and_session_ref_params_validate_ambiguity_missing_and_naive_timestamps() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
    seed_rich_fixture(&store);

    // Relative time + explicit RFC 3339 offsets work; naive local timestamps fail.
    let now = 1_763_600_000_000_i64;
    assert_eq!(
        parse_time_instant_ms("since", "24h", now).expect("24h"),
        now - 86_400_000
    );
    assert_eq!(
        parse_time_instant_ms("since", "1763500000000", now).expect("epoch"),
        1_763_500_000_000
    );
    assert!(parse_time_instant_ms("since", "2026-11-26T00:00:00Z", now).is_ok());
    assert!(parse_time_instant_ms("since", "2026-11-26T08:00:00+08:00", now).is_ok());
    assert!(parse_time_instant_ms("since", "2026-11-26T08:00:00", now).is_err());
    assert!(parse_time_instant_ms("since", "2026-11-26", now).is_err());

    let registry = TemplateRegistry::load(&dir.path().join("config.toml")).expect("registry");
    let transcript = registry.get("session-transcript").expect("transcript");

    // Prefix resolution (>= 8 chars) resolves cleanly and records resolution metadata.
    let resolved = resolve_template_params_at(
        &transcript.parsed.header,
        &["session=aaaa1111".to_owned()],
        &store,
        now,
    )
    .expect("resolve prefix");
    assert!(resolved.resolutions.contains_key("session"));

    // Nonexistent session fails fast (never silently returns 0 rows).
    assert!(
        resolve_template_params_at(
            &transcript.parsed.header,
            &["session=does-not-exist".to_owned()],
            &store,
            now,
        )
        .is_err()
    );

    // Cross-runtime duplicate native_id must error as ambiguous unless qualified with runtime.
    store
        .connection()
        .execute(
            "INSERT INTO domain_sessions(id, runtime, native_id) VALUES (3, 'pi', 'aaaa1111-2222-3333-4444-555566667777')",
            [],
        )
        .expect("insert duplicate native_id in another runtime");
    let ambiguous = resolve_template_params_at(
        &transcript.parsed.header,
        &["session=aaaa1111-2222-3333-4444-555566667777".to_owned()],
        &store,
        now,
    )
    .expect_err("ambiguous across runtimes");
    assert!(ambiguous.to_string().contains("ambiguous"), "{ambiguous}");

    // Qualifying with `<runtime>:<native_id>` resolves cleanly.
    let qualified = resolve_template_params_at(
        &transcript.parsed.header,
        &["session=codex:aaaa1111-2222-3333-4444-555566667777".to_owned()],
        &store,
        now,
    )
    .expect("qualified runtime:native_id");
    assert_eq!(
        qualified.bound.get("session"),
        Some(&crate::interface::query::SqlParamValue::Int(1))
    );
}

#[test]
fn all_builtin_templates_reference_only_public_relations_and_have_no_outer_limit() {
    let public_names: BTreeSet<&str> = PUBLIC_RELATIONS.iter().map(|rel| rel.name).collect();
    for &(name, source) in BUILTIN_SOURCES {
        let parsed = parse_template_source(source, name).expect("parse builtin");

        // Verify the final non-empty clause does not embed an outer SQL LIMIT
        // (which would defeat QueryBudget `complete=false, incomplete_reason=row_limit`).
        let tail_lines: Vec<&str> = parsed
            .sql_body
            .lines()
            .rev()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("--"))
            .take(3)
            .collect();
        for line in tail_lines {
            assert!(
                !line.to_ascii_uppercase().starts_with("LIMIT "),
                "built-in template {name} must not hardcode an outer LIMIT (found {line:?}); use default_limit metadata"
            );
        }

        // Static lint: every FROM/JOIN target in the template must be either a
        // CTE name defined in that template, `json_each`, or a declared PUBLIC_RELATION.
        let cte_names = collect_cte_names(&parsed.sql_body);
        for target in collect_from_join_targets(&parsed.sql_body) {
            assert!(
                public_names.contains(target.as_str())
                    || cte_names.contains(target.as_str())
                    || target == "json_each",
                "built-in template {name} references non-public relation {target:?}; only PUBLIC_RELATIONS or CTEs are allowed"
            );
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "verifies all six built-in templates, EXPLAIN QUERY PLAN, and SQL injection resistance together"
)]
fn all_builtin_templates_prepare_and_execute_across_optional_branches_and_use_indexes() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
    seed_rich_fixture(&store);
    let registry = TemplateRegistry::load(&dir.path().join("config.toml")).expect("registry");

    // 1. search-sessions (minimal + all filters)
    let ss = registry.get("search-sessions").expect("search-sessions");
    let r1 = execute_template(
        ss,
        &store,
        &["query=gateway relay".to_owned()],
        None,
        TEST_BUDGET,
    )
    .expect("search-sessions minimal");
    assert_eq!(r1.query.returned, 1);
    assert_eq!(r1.query.rows[0]["candidate_items"], 2);
    let r1_full = execute_template(
        ss,
        &store,
        &[
            "query=gateway relay".to_owned(),
            "runtime=codex".to_owned(),
            "cwd=trace-index".to_owned(),
            "roles=human.request,agent.final_answer".to_owned(),
            "since=1763500000000".to_owned(),
            "until=1763600000000".to_owned(),
            "min_hits=2".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("search-sessions full");
    assert_eq!(r1_full.query.returned, 1);

    // 2. search-items (minimal + exact contains + session filter)
    let si = registry.get("search-items").expect("search-items");
    let r2 = execute_template(
        si,
        &store,
        &[
            "query=gateway relay".to_owned(),
            "session=1".to_owned(),
            "contains=relay timeout".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("search-items");
    assert_eq!(r2.query.returned, 1);
    assert_eq!(r2.query.rows[0]["candidate_item_id"], 2);

    // 3. session-transcript preserves loop_id IS NULL items + supports keyset cursor + tools toggle
    let st = registry
        .get("session-transcript")
        .expect("session-transcript");
    let r3 = execute_template(st, &store, &["session=1".to_owned()], None, TEST_BUDGET)
        .expect("session-transcript default");
    // Item 1 (loop_id IS NULL runtime.instructions), Item 2 (human.request), Item 5 (agent.final_answer).
    assert_eq!(r3.query.returned, 3);
    assert_eq!(r3.query.rows[0]["item_id"], 1);
    let r3_cursor = execute_template(
        st,
        &store,
        &[
            "session=1".to_owned(),
            "include_tools=true".to_owned(),
            "after_item_id=2".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("session-transcript after cursor");
    assert_eq!(r3_cursor.query.returned, 3);
    assert_eq!(r3_cursor.query.rows[0]["item_id"], 3);

    // Verify default_limit / --limit produces truthful complete=false (no hidden outer LIMIT).
    let r3_capped = execute_template(
        st,
        &store,
        &["session=1".to_owned(), "include_tools=true".to_owned()],
        Some(2),
        TEST_BUDGET,
    )
    .expect("session-transcript capped");
    assert_eq!(r3_capped.query.returned, 2);
    assert!(!r3_capped.query.complete);
    assert_eq!(
        r3_capped.query.incomplete_reason.as_deref(),
        Some("row_limit")
    );

    // 4. item-context windows across session timeline including loop_id IS NULL & loop boundary.
    let ic = registry.get("item-context").expect("item-context");
    let r4 = execute_template(
        ic,
        &store,
        &["item_id=2".to_owned(), "window=2".to_owned()],
        None,
        TEST_BUDGET,
    )
    .expect("item-context");
    assert!(r4.query.returned >= 3);
    assert_eq!(r4.query.rows[0]["item_id"], 1); // preceding session-level item!

    // 5. shell-calls pairs tool.output via +call.item_id and filters by program + failed_only
    let sc = registry.get("shell-calls").expect("shell-calls");
    let r5 = execute_template(
        sc,
        &store,
        &[
            "program=cargo".to_owned(),
            "failed_only=true".to_owned(),
            "session=1".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("shell-calls");
    assert_eq!(r5.query.returned, 1);
    assert_eq!(r5.query.rows[0]["exit_code"], 101);
    assert_eq!(r5.query.rows[0]["duration_ms"], 420);

    // Verify EXPLAIN QUERY PLAN for shell-calls uses items_call_item_id and never full-scans domain_items.
    let rendered_sc = render_template(
        sc,
        &store,
        &["program=cargo".to_owned(), "session=1".to_owned()],
    )
    .expect("render shell-calls");
    let plan = explain_plan(&store, &rendered_sc.sql, &rendered_sc.params);
    assert!(
        plan.contains("items_call_item_id"),
        "shell-calls should hit items_call_item_id index; plan:\n{plan}"
    );
    assert!(
        !plan.contains("SCAN i\n") && !plan.contains("SCAN domain_items"),
        "scoped shell-calls must not full-scan domain_items; plan:\n{plan}"
    );

    // 6. recent-sessions
    let rs = registry.get("recent-sessions").expect("recent-sessions");
    let r6 = execute_template(rs, &store, &["runtime=codex".to_owned()], None, TEST_BUDGET)
        .expect("recent-sessions");
    assert_eq!(r6.query.returned, 1);
    assert_eq!(r6.query.rows[0]["loop_count"], 2);
    assert_eq!(r6.query.rows[0]["item_count"], 5);

    // 7. SQL injection payloads bind strictly as literals and cannot mutate the DB.
    let injected = execute_template(
        rs,
        &store,
        &["cwd='; DROP TABLE domain_items; --".to_owned()],
        None,
        TEST_BUDGET,
    )
    .expect("injection payload safely bound");
    assert_eq!(injected.query.returned, 0);

    // 8. Explicit `roles` on session-transcript takes precedence over default tool/reasoning exclusions.
    let r3_tools_via_roles = execute_template(
        st,
        &store,
        &[
            "session=1".to_owned(),
            "roles=agent.tool_call.shell,tool.output".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("explicit roles overrides default exclusions");
    assert_eq!(r3_tools_via_roles.query.returned, 2);

    // 9. Nonexistent or cross-session after_item_id and nonexistent item_id fail fast.
    assert!(
        execute_template(
            st,
            &store,
            &["session=1".to_owned(), "after_item_id=999999".to_owned()],
            None,
            TEST_BUDGET,
        )
        .is_err()
    );
    assert!(
        execute_template(
            ic,
            &store,
            &["item_id=999999".to_owned()],
            None,
            TEST_BUDGET
        )
        .is_err()
    );

    // 10. FTS templates reject unindexed tool roles instead of silently returning 0 rows.
    let fts_err = execute_template(
        si,
        &store,
        &["query=cargo".to_owned(), "roles=tool.output".to_owned()],
        None,
        TEST_BUDGET,
    )
    .expect_err("reject unindexed FTS role");
    assert!(
        fts_err
            .to_string()
            .contains("not indexed in full-text search"),
        "{fts_err}"
    );
}

#[test]
fn root_help_catalog_stays_synced_with_all_builtin_templates_and_params() {
    let help = crate::interface::cli::ROOT_AFTER_HELP;
    for &(name, source) in BUILTIN_SOURCES {
        let parsed = parse_template_source(source, name).expect("parse builtin");
        assert!(
            help.contains(name),
            "ROOT_AFTER_HELP must list built-in template {name:?}"
        );
        for param_name in parsed.header.params.keys() {
            assert!(
                help.contains(param_name),
                "ROOT_AFTER_HELP must mention parameter {param_name:?} of built-in template {name:?}"
            );
        }
    }
}

#[test]
fn user_templates_load_from_config_dir_and_cannot_shadow_builtins() {
    let dir = tempdir().expect("tempdir");
    let config_path = dir.path().join("config.toml");
    let templates_dir = dir.path().join("templates");
    fs::create_dir_all(&templates_dir).expect("mkdir templates");

    let custom_path = templates_dir.join("my-count.sql");
    fs::write(
        &custom_path,
        "/* trace-index-template\nname = \"my-count\"\ndescription = \"Count sessions\"\n*/\nSELECT COUNT(*) AS n FROM sessions\n",
    )
    .expect("write custom template");

    let registry = TemplateRegistry::load(&config_path).expect("load with custom template");
    let custom = registry.get("my-count").expect("find custom");
    assert!(matches!(custom.origin, TemplateOrigin::User { .. }));
    assert!(validate_user_templates(&config_path).is_empty());

    // Attempting to shadow a built-in name fails both load() and validate_user_templates().
    let shadow_path = templates_dir.join("shadow.sql");
    fs::write(
        &shadow_path,
        "/* trace-index-template\nname = \"shell-calls\"\ndescription = \"evil shadow\"\n*/\nSELECT 'clean' AS status\n",
    )
    .expect("write shadow template");

    assert!(TemplateRegistry::load(&config_path).is_err());
    let issues = validate_user_templates(&config_path);
    assert_eq!(issues.len(), 1);
    assert!(issues[0].message.contains("cannot shadow built-in"));
}

fn seed_session_two_late_match_item(store: &Store) {
    let long_text = format!(
        "{}gateway relay diagnostics matched at tail{}",
        "padding context ".repeat(40),
        " trailing detail".repeat(10)
    );
    store
        .connection()
        .execute_batch(&format!(
            r#"
            INSERT INTO trace_records(id, source_id, seq, byte_offset, byte_length, raw_hash, native_type, parse_status)
            VALUES (7, 2, 1, 100, 100, 'h7', 'user', 'ok');
            INSERT INTO domain_loops(id, session_id, source_id, ordinal, session_position, start_record_id)
            VALUES (3, 2, 2, 0, 0, 7);
            INSERT INTO content_blobs(id, hash, text, full_bytes, estimated_tokens)
            VALUES (5, X'05', '{long_text}', {}, 100);
            INSERT INTO domain_items(id, session_id, source_id, loop_id, loop_position, occurred_at, semantic)
            VALUES (6, 2, 2, 3, 0, 1763900000000,
                    json('{{"role":"human.request","value":{{"text":{{"blob_id":5}},"has_images":false}},"evidence_strength":"structural"}}'));
            INSERT INTO item_records(item_id, record_id) VALUES (6, 7);
            INSERT INTO item_search(rowid, text)
            VALUES (6, 'padding context gateway relay diagnostics matched at tail trailing detail');
            "#,
            long_text.len()
        ))
        .expect("insert session 2 item");
}

#[test]
fn search_items_uses_streaming_fts_rank_plan_and_centered_snippets() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
    seed_rich_fixture(&store);
    seed_session_two_late_match_item(&store);

    let registry = TemplateRegistry::load(&dir.path().join("config.toml")).expect("registry");
    let si = registry.get("search-items").expect("search-items");

    for combo in [
        vec!["query=gateway relay".to_owned()],
        vec![
            "query=gateway relay".to_owned(),
            "exclude_session=1".to_owned(),
        ],
        vec![
            "query=gateway relay".to_owned(),
            "runtime=claude".to_owned(),
        ],
        vec![
            "query=gateway relay".to_owned(),
            "since=1763500000000".to_owned(),
        ],
        vec![
            "query=gateway relay".to_owned(),
            "roles=human.request".to_owned(),
            "contains=diagnostics".to_owned(),
        ],
    ] {
        let rendered = render_template(si, &store, &combo).expect("render relevance");
        let plan = explain_plan(&store, &rendered.sql, &rendered.params);
        assert!(
            !plan.contains("USE TEMP B-TREE FOR ORDER BY"),
            "relevance plan for {combo:?} unexpectedly created TEMP B-TREE:\n{plan}"
        );
    }

    let late = execute_template(
        si,
        &store,
        &[
            "query=gateway relay".to_owned(),
            "exclude_session=1".to_owned(),
            "max_chars=70".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("execute search-items with exclude_session");
    assert_eq!(late.query.returned, 1);
    assert_eq!(late.query.rows[0]["candidate_item_id"], 6);
    let preview = late.query.rows[0]["preview"].as_str().expect("preview str");
    assert!(preview.contains("gateway relay"), "preview: {preview}");
    assert!(preview.starts_with('…'), "preview: {preview}");
    assert!(preview.chars().count() <= 70, "preview len > 70: {preview}");
}

#[test]
fn search_templates_support_exclude_session_recent_sort_and_snippet_sql_errors() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
    seed_rich_fixture(&store);
    seed_session_two_late_match_item(&store);

    let registry = TemplateRegistry::load(&dir.path().join("config.toml")).expect("registry");
    let si = registry.get("search-items").expect("search-items");
    let ss = registry.get("search-sessions").expect("search-sessions");

    let ss_excl = execute_template(
        ss,
        &store,
        &[
            "query=gateway relay".to_owned(),
            "exclude_session=1".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("execute search-sessions with exclude_session");
    assert_eq!(ss_excl.query.returned, 1);
    assert_eq!(ss_excl.query.rows[0]["session_id"], 2);

    let rendered_recent = render_template(
        si,
        &store,
        &["query=gateway relay".to_owned(), "sort=recent".to_owned()],
    )
    .expect("render recent");
    let recent_plan = explain_plan(&store, &rendered_recent.sql, &rendered_recent.params);
    assert!(
        recent_plan.contains("MATERIALIZE matched"),
        "recent plan should materialize matched before blobs:\n{recent_plan}"
    );
    let recent = execute_template(
        si,
        &store,
        &["query=gateway relay".to_owned(), "sort=recent".to_owned()],
        None,
        TEST_BUDGET,
    )
    .expect("execute search-items sort=recent");
    assert_eq!(recent.query.returned, 3);
    assert_eq!(recent.query.rows[0]["candidate_item_id"], 6);

    let null_res = query_sql_with_params(
        &store,
        "SELECT search_snippet(NULL, 'foo', 50) AS s",
        &BTreeMap::new(),
        TEST_BUDGET,
    )
    .expect("null snippet");
    assert!(null_res.rows[0]["s"].is_null());
    let err = query_sql_with_params(
        &store,
        "SELECT search_snippet('hello', 'hello', 0) AS s",
        &BTreeMap::new(),
        TEST_BUDGET,
    )
    .expect_err("max_chars=0 must error");
    assert!(
        err.to_string()
            .contains("max_chars must be an integer >= 1"),
        "{err}"
    );
}

#[test]
fn validate_item_ref_session_scope_ignores_exclude_session_ref() {
    let dir = tempdir().expect("tempdir");
    let store = Store::open(&dir.path().join("index.sqlite")).expect("open store");
    seed_rich_fixture(&store);

    let config_path = dir.path().join("config.toml");
    let templates_dir = dir.path().join("templates");
    fs::create_dir_all(&templates_dir).expect("mkdir templates");
    fs::write(
        templates_dir.join("excl-item.sql"),
        r#"/* trace-index-template
name = "excl-item"
description = "Template combining exclude_session and item_ref"

[params.exclude_session]
type = "session_ref"
required = true

[params.session]
type = "session_ref"
required = true

[params.cursor]
type = "item_ref"
required = true
*/
SELECT item_id
FROM items
WHERE session_id = :session
  AND session_id <> :exclude_session
  AND item_id >= :cursor
ORDER BY item_id
"#,
    )
    .expect("write template");

    let registry = TemplateRegistry::load(&config_path).expect("load registry");
    let tpl = registry.get("excl-item").expect("excl-item");
    // Item 2 belongs to session 1; exclude_session=2 sorts before "session" in
    // BTreeMap and must NOT trigger a false session mismatch on item_ref.
    let report = execute_template(
        tpl,
        &store,
        &[
            "exclude_session=2".to_owned(),
            "session=1".to_owned(),
            "cursor=2".to_owned(),
        ],
        None,
        TEST_BUDGET,
    )
    .expect("exclude_session must not hijack item_ref session scope");
    assert!(report.query.returned >= 1);
}

fn explain_plan(
    store: &Store,
    sql: &str,
    params: &BTreeMap<String, crate::interface::query::SqlParamValue>,
) -> String {
    let explain_sql = format!("EXPLAIN QUERY PLAN {sql}");
    let report = query_sql_with_params(store, &explain_sql, params, TEST_BUDGET)
        .expect("explain query plan");
    report
        .rows
        .iter()
        .filter_map(|row| row.get("detail").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

fn collect_cte_names(sql: &str) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    let tokens: Vec<&str> = sql
        .split(|ch: char| ch.is_whitespace() || matches!(ch, '(' | ')' | ','))
        .filter(|tok| !tok.is_empty())
        .collect();
    for window in tokens.windows(3) {
        if window[1].eq_ignore_ascii_case("AS")
            && (window[0]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_'))
        {
            // Check whether this looks like a CTE header (preceded by WITH or `)`).
            names.insert(window[0].to_ascii_lowercase());
        }
    }
    names
}

fn collect_from_join_targets(sql: &str) -> Vec<String> {
    let mut targets = Vec::new();
    let cleaned = sql
        .lines()
        .filter(|line| !line.trim_start().starts_with("--"))
        .collect::<Vec<_>>()
        .join(" ");
    let tokens: Vec<&str> = cleaned.split_whitespace().collect();
    for index in 0..tokens.len() {
        let keyword = tokens[index];
        if (keyword.eq_ignore_ascii_case("FROM") || keyword.eq_ignore_ascii_case("JOIN"))
            && let Some(next) = tokens.get(index + 1)
        {
            let candidate = next
                .split('(')
                .next()
                .unwrap_or("")
                .trim_matches(|ch: char| matches!(ch, '"' | '\'' | ',' | ';'));
            if !candidate.is_empty() && !candidate.starts_with('(') {
                targets.push(candidate.to_ascii_lowercase());
            }
        }
    }
    targets
}
