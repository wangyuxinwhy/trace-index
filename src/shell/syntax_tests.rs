use super::{MAX_STATEMENTS, extract, lang, parse_status};

const MAX_TEXT: usize = 64 * 1024;

#[test]
fn empty_text_has_no_shell_structure() {
    let parsed = extract("", lang::SHELL, MAX_TEXT);
    assert!(parsed.fragments.is_empty());
    assert!(parsed.statements.is_empty());
    assert!(parsed.invocations.is_empty());
    assert!(parsed.redirects.is_empty());
}

#[test]
fn extracts_programs_and_arguments_in_source_order() {
    let parsed = extract(
        "git -C /repo status && cargo test --all-targets",
        lang::SHELL,
        MAX_TEXT,
    );
    let programs = parsed
        .invocations
        .iter()
        .map(|invocation| invocation.program.as_str())
        .collect::<Vec<_>>();
    assert_eq!(programs, ["git", "cargo"]);

    let first_argv: Vec<String> =
        serde_json::from_str(&parsed.invocations[0].argv).expect("valid argv JSON");
    assert_eq!(first_argv, ["-C", "/repo", "status"]);
}

#[test]
fn keeps_dynamic_program_names_verbatim() {
    let parsed = extract("$TRACE_INDEX query run 'SELECT 1'", lang::SHELL, MAX_TEXT);
    assert_eq!(parsed.invocations[0].program, "$TRACE_INDEX");
}

#[test]
fn identifies_each_pipeline_and_stage() {
    let parsed = extract("a | b; c | d", lang::SHELL, MAX_TEXT);
    let stages = parsed
        .statements
        .iter()
        .filter_map(|statement| {
            let shell = statement.shell.as_ref()?;
            Some((shell.pipeline_id?, shell.pipeline_pos?))
        })
        .collect::<Vec<_>>();
    assert!(stages.contains(&(0, 0)));
    assert!(stages.contains(&(0, 1)));
    assert!(stages.contains(&(1, 0)));
    assert!(stages.contains(&(1, 1)));
}

#[test]
fn preserves_redirect_facts_and_source_range() {
    let command = "2>err.log echo ok >>out.log";
    let parsed = extract(command, lang::SHELL, MAX_TEXT);
    let facts = parsed
        .redirects
        .iter()
        .map(|redirect| {
            (
                redirect.source_fd_raw.as_deref(),
                redirect.operator.as_str(),
                redirect.target_raw.as_str(),
            )
        })
        .collect::<Vec<_>>();
    assert!(facts.contains(&(Some("2"), ">", "err.log")));
    assert!(facts.contains(&(None, ">>", "out.log")));
    assert!(parsed.redirects.iter().all(|redirect| {
        redirect.start_byte < redirect.end_byte
            && usize::try_from(redirect.end_byte).expect("byte offset") <= command.len()
    }));
}

#[test]
fn nested_statements_retain_parent_positions() {
    let parsed = extract(
        "for file in a b; do echo \"$file\"; done",
        lang::SHELL,
        MAX_TEXT,
    );
    assert!(
        parsed
            .statements
            .iter()
            .any(|statement| statement.parent.is_some())
    );
    assert!(
        parsed
            .invocations
            .iter()
            .any(|invocation| invocation.program == "echo")
    );
}

#[test]
fn malformed_shell_is_partial_instead_of_rejected() {
    let parsed = extract("if true; then echo unfinished", lang::SHELL, MAX_TEXT);
    assert_eq!(parsed.fragments.len(), 1);
    assert_eq!(parsed.fragments[0].parse_status, parse_status::PARTIAL);
    assert!(!parsed.invocations.is_empty());
}

#[test]
fn an_unavailable_declared_language_is_explicitly_unsupported() {
    let parsed = extract("Write-Host hello", lang::POWERSHELL, MAX_TEXT);
    assert_eq!(parsed.fragments.len(), 1);
    assert_eq!(parsed.fragments[0].parse_status, parse_status::UNSUPPORTED);
    assert!(parsed.statements.is_empty());
}

#[test]
fn exceeding_the_statement_bound_marks_the_fragment_partial() {
    let command = std::iter::repeat_n("true", MAX_STATEMENTS + 100)
        .collect::<Vec<_>>()
        .join(";");
    let parsed = extract(&command, lang::SHELL, command.len());
    assert_eq!(parsed.fragments[0].parse_status, parse_status::PARTIAL);
    assert_eq!(parsed.statements.len(), MAX_STATEMENTS);
}
