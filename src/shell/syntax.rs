//! Adds structured shell fragments to Runtime-declared shell tool calls.
//!
//! The Adapter decides whether a native argument is shell code; this module
//! only parses arguments whose Runtime contract declares that language. The
//! resulting fragments, statements, invocations, and redirects are nested in
//! `ShellToolCall.shell_fragments` inside the Item's `semantic.value`. They do
//! not create parallel public Relations.
//!
//! The stored shape contains the facts callers query, not a complete parser
//! tree. Byte ranges preserve where each parsed fact came from; the Item's
//! Record evidence points back to the Runtime's original tool-call value.

use tree_sitter::{Node, Parser};

use crate::adapters::projection::{
    BoundedText, FragmentProjection, InvocationProjection, RedirectProjection, ShellStatement,
    StatementProjection, SyntaxProjection,
};

/// Languages an Adapter may declare for a slot.
///
/// `SHELL`, not `BASH`. All three runtimes run a shell the user chose:
/// Codex's `exec_command` takes a `shell` parameter defaulting to "the user's
/// default shell" (`core/src/tools/handlers/shell_spec.rs`), Claude Code
/// searches `['zsh', 'bash']` unless `SHELL` prefers bash
/// (`utils/Shell.ts:85-120`), and Pi resolves `/bin/bash`, then bash on PATH,
/// then `sh`, overridable by a setting (`utils/shell.ts:51-118`). Recording
/// `bash` would name a dialect none of them promises.
pub(crate) mod lang {
    pub(crate) const SHELL: &str = "shell";
    pub(crate) const POWERSHELL: &str = "powershell";
}

pub(crate) mod parse_status {
    pub(crate) const PARSED: &str = "parsed";
    /// The grammar recovered, but the tree carries error nodes.
    pub(crate) const PARTIAL: &str = "partial";
    /// A declared language this build has no grammar for.
    pub(crate) const UNSUPPORTED: &str = "unsupported";
}

/// Per-fragment statement bound. When it fires, the Fragment is marked partial
/// so a bounded parse cannot be mistaken for complete structure.
const MAX_STATEMENTS: usize = 512;

/// Maximum nested shell fragments (including the root) extracted from one tool call.
const MAX_FRAGMENTS: usize = 16;

/// Maximum nesting depth for wrapper unwrapping and static `sh`/`bash`/`zsh -c` fragments.
const MAX_NESTING_DEPTH: usize = 4;

/// Statement kinds that carry a fact worth a row.
fn is_semantic_statement(kind: &str) -> bool {
    matches!(
        kind,
        "command"
            | "variable_assignment"
            | "declaration_command"
            | "unset_command"
            | "if_statement"
            | "for_statement"
            | "while_statement"
            | "case_statement"
            | "function_definition"
            // The grammar's `_statement` set also holds these two. Leaving them
            // out lost the loop in `for ((i=0;i<3;i++))` entirely and made
            // `[[ -f x ]]` produce no statement at all.
            | "c_style_for_statement"
            | "test_command"
    )
}

/// Reusable tree-sitter parser for declared shell tool calls.
///
/// Constructing a `tree_sitter::Parser` and binding its grammar once per sync
/// run avoids thousands of repeated parser allocations across shell commands.
pub(crate) struct ShellParser {
    bash: Option<Parser>,
}

impl ShellParser {
    pub(crate) fn new() -> Self {
        let mut parser = Parser::new();
        let bash = parser
            .set_language(&tree_sitter_bash::LANGUAGE.into())
            .ok()
            .map(|()| parser);
        Self { bash }
    }

    /// Fill in the parsed shape of every tool call whose Adapter declared a
    /// language for its command.
    ///
    /// Runs between projection and persistence: the Adapter states the language,
    /// this states the structure, and `persist` only writes. Keeping the parse
    /// out of the Adapter is what stops a projector from having to know four
    /// grammars, and keeping it out of `persist` is what lets it be tested
    /// without a database.
    pub(crate) fn parse_declared_commands(
        &mut self,
        session: &mut crate::adapters::projection::SessionProjection,
        max_text_bytes: usize,
    ) {
        for item in &mut session.items {
            self.parse_item_command(item, max_text_bytes);
        }
        for turn in &mut session.loops {
            for item in &mut turn.items {
                self.parse_item_command(item, max_text_bytes);
            }
        }
    }

    fn parse_item_command(
        &mut self,
        item: &mut crate::adapters::projection::ItemProjection,
        max_text_bytes: usize,
    ) {
        let crate::adapters::projection::ItemDetail::ToolCall {
            cmd,
            cmd_lang,
            syntax,
            ..
        } = &mut item.detail
        else {
            return;
        };
        let (Some(text), Some(declared)) = (cmd.as_deref(), *cmd_lang) else {
            return;
        };
        let parsed = self.extract(text, declared, max_text_bytes);
        if !parsed.fragments.is_empty() {
            *syntax = Some(parsed);
        }
    }

    /// Parse one Runtime-declared shell text into nested Semantic shell values.
    ///
    /// Pure: no database, no filesystem. `text` is the slot's content and `lang`
    /// is what the Adapter declared it to be.
    pub(crate) fn extract(
        &mut self,
        text: &str,
        lang: &'static str,
        max_text_bytes: usize,
    ) -> SyntaxProjection {
        let mut out = SyntaxProjection::default();
        if text.is_empty() {
            return out;
        }
        match lang {
            lang::SHELL => self.extract_shell(text, max_text_bytes, &mut out),
            // Declared but unparsed. Naming a language this build cannot read is a
            // fact; filing it as something else would not be.
            _ => {
                out.fragments.push(root_fragment(
                    text,
                    max_text_bytes,
                    parse_status::UNSUPPORTED,
                ));
            }
        }
        out
    }

    fn extract_shell(&mut self, text: &str, max_text_bytes: usize, out: &mut SyntaxProjection) {
        self.parse_shell_fragment(text, 0, max_text_bytes, out);
    }

    fn parse_shell_fragment(
        &mut self,
        text: &str,
        depth: usize,
        max_text_bytes: usize,
        out: &mut SyntaxProjection,
    ) {
        let Some(parser) = self.bash.as_mut() else {
            out.fragments.push(root_fragment(
                text,
                max_text_bytes,
                parse_status::UNSUPPORTED,
            ));
            return;
        };
        let Some(tree) = parser.parse(text, None) else {
            out.fragments.push(root_fragment(
                text,
                max_text_bytes,
                parse_status::UNSUPPORTED,
            ));
            return;
        };
        let status = if tree.root_node().has_error() {
            parse_status::PARTIAL
        } else {
            parse_status::PARSED
        };
        let fragment_index = out.fragments.len();
        out.fragments
            .push(root_fragment(text, max_text_bytes, status));
        let nested_scripts = collect(tree.root_node(), text, fragment_index, out);
        for script in nested_scripts {
            if depth >= MAX_NESTING_DEPTH || out.fragments.len() >= MAX_FRAGMENTS {
                out.fragments[fragment_index].parse_status = parse_status::PARTIAL;
                break;
            }
            self.parse_shell_fragment(&script, depth + 1, max_text_bytes, out);
        }
    }
}

#[cfg(test)]
pub(crate) fn extract(text: &str, lang: &'static str, max_text_bytes: usize) -> SyntaxProjection {
    ShellParser::new().extract(text, lang, max_text_bytes)
}

fn root_fragment(text: &str, max_text_bytes: usize, status: &'static str) -> FragmentProjection {
    FragmentProjection {
        content: BoundedText::bounded(text, max_text_bytes),
        parse_status: status,
    }
}

/// Walk one fragment's tree, emitting statements and their per-kind detail.
/// Returns any statically quoted inner shell scripts (`sh`/`bash`/`zsh -c ...`)
/// discovered inside this fragment.
fn collect(root: Node, text: &str, fragment: usize, out: &mut SyntaxProjection) -> Vec<String> {
    let mut found: Vec<Node> = Vec::new();
    let mut cursor = root.walk();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if is_semantic_statement(node.kind()) {
            found.push(node);
        }
        for child in node.named_children(&mut cursor) {
            stack.push(child);
        }
    }
    found.sort_by_key(tree_sitter::Node::start_byte);

    if found.len() > MAX_STATEMENTS {
        out.fragments[fragment].parse_status = parse_status::PARTIAL;
        found.truncate(MAX_STATEMENTS);
    }

    // Statement ids are indices into `out.statements`; a parent is always
    // emitted before its children because the list is in source order and a
    // parent starts no later than the child it contains.
    let base = out.statements.len();
    // A pipeline is identified by its own node so several in one fragment stay
    // distinct: without this, `a | b; c | d` cannot say which stage was
    // upstream of which. Length is a COUNT over the identity, not a column.
    let mut pipelines: Vec<usize> = Vec::new();
    for node in &found {
        let parent = nearest_statement_ancestor(*node, &found).map(|index| base + index);
        let (pipeline_id, pipeline_pos) = pipeline_position(*node, &mut pipelines);
        out.statements.push(StatementProjection {
            fragment,
            parent,
            start_byte: u32::try_from(node.start_byte()).unwrap_or(u32::MAX),
            end_byte: u32::try_from(node.end_byte()).unwrap_or(u32::MAX),
            shell: Some(ShellStatement {
                connector: connector_of(*node),
                pipeline_id,
                pipeline_pos,
            }),
        });
    }

    let mut nested_scripts = Vec::new();
    for (index, node) in found.iter().enumerate() {
        let statement = base + index;
        if node.kind() == "command" {
            emit_command(*node, text, statement, out, &mut nested_scripts);
        }
        emit_redirects(*node, text, statement, out);
    }
    nested_scripts
}

/// This statement's place in the pipeline that contains it, if any.
///
/// The identity is the pipeline node's own start offset, interned per fragment
/// so ids are small and stable within one parse. The search stops at the first
/// pipeline above the statement, so a command nested inside a stage reports
/// that stage rather than inventing one of its own.
///
/// Every statement inside a compound stage therefore shares one
/// `pipeline_pos`. A pipeline's length is `COUNT(DISTINCT pipeline_pos)`, not
/// a row count: `(a; b) | c` is two stages holding three statements.
fn pipeline_position(node: Node, pipelines: &mut Vec<usize>) -> (Option<u32>, Option<u32>) {
    // Walk to the root, with no list of node kinds to step through. Reaching a
    // pipeline by any path means this statement is inside one of its stages,
    // and `current` is then whichever child of the pipeline that stage is --
    // a bare command, a redirected one, a negation, a subshell or a brace
    // group, without naming any of them.
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return (None, None);
        };
        if parent.kind() == "pipeline" {
            let start = parent.start_byte();
            let id = pipelines
                .iter()
                .position(|seen| *seen == start)
                .unwrap_or_else(|| {
                    pipelines.push(start);
                    pipelines.len() - 1
                });
            let mut cursor = parent.walk();
            let position = parent
                .named_children(&mut cursor)
                .position(|child| child.id() == current.id());
            return (
                u32::try_from(id).ok(),
                position.and_then(|index| u32::try_from(index).ok()),
            );
        }
        current = parent;
    }
}

/// The innermost emitted statement containing this one.
fn nearest_statement_ancestor(node: Node, found: &[Node]) -> Option<usize> {
    let mut current = node;
    loop {
        let parent = current.parent()?;
        if is_semantic_statement(parent.kind()) {
            return found.iter().position(|other| other.id() == parent.id());
        }
        current = parent;
    }
}

/// How this statement joins the one before it.
///
/// Only the combinators are named. Everything else — a program, a subshell, a
/// brace group, a `do` block, a branch, a command substitution — holds a
/// sequence, and treating an unrecognized parent as one is the safe default:
/// containers outnumber combinators and the grammar keeps growing them. Naming the
/// containers instead would let a statement inside `$(a; b)` inherit the
/// connector of the command the substitution sits in.
fn connector_of(node: Node) -> &'static str {
    let mut current = node;
    loop {
        let Some(parent) = current.parent() else {
            return "first";
        };
        match parent.kind() {
            // `&&` and `||` compose two statements into one; the operator
            // between them is the connector.
            "list" => {
                let mut cursor = parent.walk();
                let children: Vec<Node> = parent.children(&mut cursor).collect();
                let operator = children
                    .iter()
                    .position(|child| child.id() == current.id())
                    .and_then(|position| {
                        children[..position]
                            .iter()
                            .rev()
                            .find_map(|child| match child.kind() {
                                "&&" => Some("&&"),
                                "||" => Some("||"),
                                _ => None,
                            })
                    });
                match operator {
                    Some(found) => return found,
                    // The first element of `b && c` is joined by whatever
                    // precedes the whole list, the way a pipeline's stages
                    // inherit the pipeline's.
                    None => current = parent,
                }
            }
            // These compose rather than sequence, so the statement inherits
            // the connector of the construct around them.
            "pipeline" | "redirected_statement" | "negated_command" => current = parent,
            _ => return separator_before(parent, current),
        }
    }
}

/// The separator between this statement and the previous one in its sequence.
fn separator_before(parent: Node, current: Node) -> &'static str {
    let mut cursor = parent.walk();
    let children: Vec<Node> = parent.children(&mut cursor).collect();
    let Some(position) = children.iter().position(|c| c.id() == current.id()) else {
        return "first";
    };
    let Some(previous) = children[..position]
        .iter()
        .rposition(|child| is_semantic_statement(child.kind()))
    else {
        return "first";
    };
    let mut separator = "newline";
    for token in &children[previous + 1..position] {
        match token.kind() {
            ";" | "&" if separator == "newline" => separator = token.kind(),
            kind if opens_a_sequence(*token, kind) => return "first",
            _ => {}
        }
    }
    separator
}

/// Whether a token starts a new statement sequence rather than separating two.
///
/// The keywords and brackets that open a body: `then`, `do`, `else`, `in`, `{`,
/// `(`. Tested as a property of an anonymous token — the grammar spells a
/// keyword as its own text — so a grammar that grows another clause keyword
/// needs no edit here. Named nodes are excluded because `word` would otherwise
/// qualify.
fn opens_a_sequence(token: Node, kind: &str) -> bool {
    !token.is_named()
        && (matches!(kind, "{" | "(")
            || (!kind.is_empty() && kind.chars().all(|c| c.is_ascii_alphabetic())))
}

fn text_of<'a>(node: Node, text: &'a str) -> &'a str {
    node.utf8_text(text.as_bytes()).unwrap_or("")
}

/// The weaker of two staticness verdicts: one transformable part is enough.
fn weakest(left: &'static str, right: &'static str) -> &'static str {
    match (left, right) {
        ("dynamic", _) | (_, "dynamic") => "dynamic",
        ("quoted", _) | (_, "quoted") => "quoted",
        _ => "literal",
    }
}

/// How much of a word this build can state without running a shell.
///
/// An allowlist, not a denylist. A denylist has to enumerate every construct
/// the shell transforms — expansions, arithmetic, globs, tilde, brace, ANSI-C
/// escapes — and it silently reports the one it forgot as a literal. Here a
/// word is `literal` only when it is demonstrably plain text, `quoted` when the
/// only transformation is removing the quotes around it, and `dynamic` for
/// everything else, including forms that are statically written but need a
/// decoding or expansion step this build does not perform.
///
/// `dynamic` therefore means "this build will not state the value", which is
/// weaker than "the value depends on run-time state" and is the honest reading
/// for a glob: `./bin/*` is written down in full and still names whichever
/// files exist.
fn basis_of(node: Node, text: &str) -> &'static str {
    match node.kind() {
        // `command_name` wraps the word; a concatenation joins several, and one
        // transformable part makes the whole word transformable.
        "command_name" | "concatenation" => {
            let mut cursor = node.walk();
            node.named_children(&mut cursor)
                .map(|child| basis_of(child, text))
                .reduce(weakest)
                .unwrap_or("dynamic")
        }
        // Plain text, unless it carries a character the shell would act on.
        "word" => {
            let raw = text_of(node, text);
            if raw.contains(['*', '?', '[', ']', '{', '}', '~', '$', '`', '\\']) {
                "dynamic"
            } else {
                "literal"
            }
        }
        "number" => "literal",
        // Quote removal is the whole transformation, and it is reversible.
        "raw_string" => "quoted",
        "string" => {
            let mut cursor = node.walk();
            let plain = node
                .named_children(&mut cursor)
                .all(|child| child.kind() == "string_content");
            if plain { "quoted" } else { "dynamic" }
        }
        // `$'...'` is static but needs escape decoding, `$"..."` needs a locale,
        // `$((...))` needs arithmetic, and the rest need the shell itself.
        _ => "dynamic",
    }
}

fn strip_quotes(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\'')
            || (bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"'))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

/// Stated rule: drop directories and surrounding quotes.
///
/// Lossy on purpose, and `program_raw` stays beside it: this merges a `python`
/// that is not installed with a `.venv/bin/python` that is.
fn normalize_program(raw: &str) -> String {
    let stripped = strip_quotes(raw);
    match stripped.rsplit('/').next() {
        Some(base) if !base.is_empty() => base.to_owned(),
        _ => stripped.to_owned(),
    }
}

fn emit_command(
    node: Node,
    text: &str,
    statement: usize,
    out: &mut SyntaxProjection,
    nested_scripts: &mut Vec<String>,
) {
    let Some(name_node) = node.child_by_field_name("name") else {
        return;
    };
    let raw = text_of(name_node, text);
    let basis = basis_of(name_node, text);
    // Normalizing a word the shell has not finished with would invent a value:
    // taking the basename of `./bin/*` yields `*`, which names nothing.
    let mut current_program = if basis == "dynamic" {
        raw.to_owned()
    } else {
        normalize_program(raw)
    };
    let mut cursor = node.walk();
    let arg_nodes: Vec<Node> = node
        .children_by_field_name("argument", &mut cursor)
        .collect();
    let argv: Vec<String> = arg_nodes
        .iter()
        .map(|argument| text_of(*argument, text).to_owned())
        .collect();
    out.invocations.push(InvocationProjection {
        statement,
        program: current_program.clone(),
        argv,
    });

    if basis == "dynamic" {
        return;
    }

    let mut slice = arg_nodes.as_slice();
    for _ in 0..MAX_NESTING_DEPTH {
        if let Some(script) = static_subshell_script(&current_program, slice, text) {
            nested_scripts.push(script);
            break;
        }
        let Some(next_idx) = unwrap_command_index(&current_program, slice, text) else {
            break;
        };
        let cmd_node = slice[next_idx];
        let cmd_raw = text_of(cmd_node, text);
        let cmd_basis = basis_of(cmd_node, text);
        let next_program = if cmd_basis == "dynamic" {
            cmd_raw.to_owned()
        } else {
            normalize_program(cmd_raw)
        };
        let next_slice = &slice[next_idx + 1..];
        out.invocations.push(InvocationProjection {
            statement,
            program: next_program.clone(),
            argv: next_slice
                .iter()
                .map(|argument| text_of(*argument, text).to_owned())
                .collect(),
        });
        if cmd_basis == "dynamic" {
            break;
        }
        current_program = next_program;
        slice = next_slice;
    }
}

/// Returns the argument index of the wrapped command for well-defined POSIX
/// command wrappers (`nice`, `timeout`, `env`, `nohup`, `command`).
fn unwrap_command_index(program: &str, args: &[Node], text: &str) -> Option<usize> {
    match program {
        "nice" => unwrap_nice(args, text),
        "timeout" => unwrap_timeout(args, text),
        "env" => unwrap_env(args, text),
        "nohup" => unwrap_nohup(args, text),
        "command" => unwrap_command(args, text),
        _ => None,
    }
}

fn unwrap_nice(args: &[Node], text: &str) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        let word = static_word(args[idx], text)?;
        if word == "--" {
            idx += 1;
            break;
        }
        if word == "-n" || word == "--adjustment" {
            idx += 2;
            continue;
        }
        if word.starts_with("-n")
            || word.starts_with("--adjustment=")
            || (word.starts_with('-')
                && word.len() > 1
                && word[1..].bytes().all(|b| b.is_ascii_digit()))
        {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        break;
    }
    (idx < args.len()).then_some(idx)
}

fn unwrap_timeout(args: &[Node], text: &str) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        let word = static_word(args[idx], text)?;
        if word == "--" {
            idx += 1;
            break;
        }
        if matches!(word, "-s" | "--signal" | "-k" | "--kill-after") {
            idx += 2;
            continue;
        }
        if word.starts_with("--signal=")
            || word.starts_with("--kill-after=")
            || matches!(
                word,
                "--preserve-status" | "--foreground" | "-v" | "--verbose"
            )
        {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        break;
    }
    let cmd_idx = idx + 1;
    (cmd_idx < args.len()).then_some(cmd_idx)
}

fn unwrap_env(args: &[Node], text: &str) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        let word = static_word(args[idx], text)?;
        if word == "--" {
            idx += 1;
            break;
        }
        if matches!(word, "-u" | "--unset" | "-C" | "--chdir") {
            idx += 2;
            continue;
        }
        if word == "-S" || word.starts_with("-S") || word.starts_with("--split-string") {
            return None;
        }
        if word == "-"
            || matches!(
                word,
                "-i" | "--ignore-environment" | "-0" | "--null" | "-v" | "--debug"
            )
            || word.starts_with("--unset=")
            || word.starts_with("--chdir=")
        {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        if is_env_assignment(word) {
            idx += 1;
            continue;
        }
        break;
    }
    (idx < args.len()).then_some(idx)
}

fn unwrap_nohup(args: &[Node], text: &str) -> Option<usize> {
    let first = static_word(*args.first()?, text)?;
    if first == "--" {
        (args.len() > 1).then_some(1)
    } else if first.starts_with('-') {
        None
    } else {
        Some(0)
    }
}

fn unwrap_command(args: &[Node], text: &str) -> Option<usize> {
    let mut idx = 0;
    while idx < args.len() {
        let word = static_word(args[idx], text)?;
        if word == "--" {
            idx += 1;
            break;
        }
        if word == "-p" {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            return None;
        }
        break;
    }
    (idx < args.len()).then_some(idx)
}

/// Extracts a statically written inner script from `sh`/`bash`/`zsh -c <script>`.
fn static_subshell_script(program: &str, args: &[Node], text: &str) -> Option<String> {
    if !matches!(program, "sh" | "bash" | "zsh") {
        return None;
    }
    let mut idx = 0;
    let mut saw_c = false;
    while idx < args.len() {
        let word = static_word(args[idx], text)?;
        if word == "--" {
            idx += 1;
            break;
        }
        if matches!(word, "-o" | "+o") {
            idx += 2;
            continue;
        }
        if word.starts_with("--") {
            idx += 1;
            continue;
        }
        if word.starts_with('-') && word.len() > 1 {
            let flags = &word[1..];
            if flags.bytes().all(|b| b.is_ascii_alphabetic()) {
                if flags.contains('c') {
                    saw_c = true;
                }
                idx += 1;
                continue;
            }
            return None;
        }
        break;
    }
    if !saw_c || idx >= args.len() {
        return None;
    }
    let script_node = args[idx];
    let script_basis = basis_of(script_node, text);
    if script_basis == "dynamic" {
        return None;
    }
    let script = strip_quotes(text_of(script_node, text));
    (!script.trim().is_empty()).then(|| script.to_owned())
}

fn static_word<'a>(node: Node, text: &'a str) -> Option<&'a str> {
    (basis_of(node, text) != "dynamic").then(|| strip_quotes(text_of(node, text)))
}

fn is_env_assignment(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    !name.is_empty()
        && !name.as_bytes()[0].is_ascii_digit()
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Redirections attached to a statement.
///
/// Two places hold them and both must be read. A redirection written after the
/// command lands on an enclosing `redirected_statement`; one written before it
/// — `2>err.log echo x` — lands on the command's own `redirect` field, as do a
/// function definition's. Reading only the first shape dropped every prefix
/// redirection silently.
///
/// They are read off the redirect nodes rather than the statement's text: a
/// `2>/dev/null` inside a quoted argument is an argument.
fn emit_redirects(node: Node, text: &str, statement: usize, out: &mut SyntaxProjection) {
    let mut cursor = node.walk();
    for redirect in node.named_children(&mut cursor) {
        if is_redirect(redirect.kind()) {
            push_redirect(redirect, text, statement, out);
        }
    }
    if let Some(parent) = node.parent()
        && parent.kind() == "redirected_statement"
        && parent.child_by_field_name("body").map(|body| body.id()) == Some(node.id())
    {
        let mut cursor = parent.walk();
        for redirect in parent.named_children(&mut cursor) {
            if is_redirect(redirect.kind()) {
                push_redirect(redirect, text, statement, out);
            }
        }
    }
}

fn is_redirect(kind: &str) -> bool {
    matches!(
        kind,
        "file_redirect" | "heredoc_redirect" | "herestring_redirect"
    )
}

fn push_redirect(redirect: Node, text: &str, statement: usize, out: &mut SyntaxProjection) {
    // Every redirect kind declares a `descriptor` field, and the operator is
    // either its own field or the one anonymous token in the node. Reading them
    // beats scanning characters off the front, which had to guess where the
    // descriptor stopped and the operator began.
    let mut cursor = redirect.walk();
    let operator = redirect.child_by_field_name("operator").or_else(|| {
        redirect
            .children(&mut cursor)
            .find(|child| !child.is_named())
    });
    // The target is whatever the redirect points at: a path, a descriptor, or a
    // heredoc's delimiter. The delimiter's quoting decides whether the body
    // expands, so `heredoc_body` is never it.
    let mut cursor = redirect.walk();
    let target = redirect
        .child_by_field_name("destination")
        .or_else(|| redirect.child_by_field_name("redirect"))
        .or_else(|| {
            redirect
                .named_children(&mut cursor)
                .find(|child| child.kind() != "heredoc_body" && child.kind() != "file_descriptor")
        });
    out.redirects.push(RedirectProjection {
        statement,
        source_fd_raw: redirect
            .child_by_field_name("descriptor")
            .map(|node| text_of(node, text).to_owned()),
        operator: operator.map_or_else(String::new, |node| node.kind().to_owned()),
        target_raw: target
            .map(|node| text_of(node, text).to_owned())
            .unwrap_or_default(),
        start_byte: u32::try_from(redirect.start_byte()).unwrap_or(u32::MAX),
        end_byte: u32::try_from(redirect.end_byte()).unwrap_or(u32::MAX),
    });
}

#[cfg(test)]
#[path = "syntax_tests.rs"]
mod tests;
