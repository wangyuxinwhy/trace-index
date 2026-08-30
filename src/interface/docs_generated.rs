//! Renders documentation fragments from the CLI and domain declarations.
//!
//! Generated regions in `docs/` restate something Rust states first: the command
//! tree, public Relations, and persistence metrics. Keeping a
//! second hand-written copy allows an interface contract to drift silently.
//!
//! These are generated into the Markdown source because that is the durable
//! artifact readers inspect and `src/interface/docs.rs` embeds with `include_str!` for
//! `docs get`. Generating a presentation layer would leave the source contract
//! stale.
//!
//! To adopt a change: `UPDATE_DOCS=1 cargo test generated_documentation`.

use std::fmt::Write as _;

use clap::{Command as ClapCommand, CommandFactory};

use super::output::IndexPersistMetrics;
use crate::interface::cli::Cli;
use crate::storage::public_schema::PUBLIC_RELATIONS;

/// Opens a generated region. The text after the colon is addressed to whoever
/// finds a stale one, so it names the command that fixes it.
const BEGIN: &str = "<!-- generated: ";
const END: &str = "<!-- /generated -->";

struct Region {
    path: &'static str,
    name: &'static str,
    body: String,
}

/// clap keeps the trailing period from a doc comment; `--help` drops it.
fn about(command: &ClapCommand) -> String {
    command
        .get_about()
        .expect("every command declares a doc comment")
        .to_string()
        .trim_end_matches('.')
        .to_string()
}

fn cli_table() -> String {
    let root = Cli::command();
    let mut out = String::from("| Command | What it does |\n| --- | --- |\n");
    for noun in root.get_subcommands().filter(|c| c.get_name() != "help") {
        let _ = writeln!(out, "| `{}` | {} |", noun.get_name(), about(noun));
        for verb in noun.get_subcommands().filter(|c| c.get_name() != "help") {
            let _ = writeln!(
                out,
                "| `{} {}` | {} |",
                noun.get_name(),
                verb.get_name(),
                about(verb)
            );
        }
    }
    out.trim_end().to_string()
}

/// The first paragraph of a declaration's documentation, whitespace collapsed:
/// a wrapped doc comment is one sentence across several lines.
fn summary(description: &str) -> String {
    description
        .split("\n\n")
        .next()
        .unwrap_or_default()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn relation_list() -> String {
    let mut out = String::new();
    for relation in PUBLIC_RELATIONS {
        let _ = writeln!(
            out,
            "- `{}`: {}",
            relation.name,
            summary(relation.description)
        );
    }
    out.trim_end().to_string()
}

/// Every key `metrics.persist_ms` carries, in the order the JSON object
/// presents them — `serde_json` keeps a map sorted, so this is alphabetical
/// rather than declaration order.
///
/// Taken from the serialized shape rather than from a list written here, so a
/// site added to the struct cannot be missing from the contract that promises
/// all of them are named.
fn persist_sites() -> String {
    let value =
        serde_json::to_value(IndexPersistMetrics::default()).expect("persist metrics serialize");
    let keys: Vec<String> = value
        .as_object()
        .expect("a JSON object")
        .keys()
        .map(|key| format!("`{key}`"))
        .collect();
    keys.join(", ")
}

fn regions() -> Vec<Region> {
    vec![
        Region {
            path: "docs/reference/cli.md",
            name: "cli-table",
            body: cli_table(),
        },
        Region {
            path: "docs/reference/public-schema.md",
            name: "relation-list",
            body: relation_list(),
        },
        Region {
            path: "docs/reference/output-contract.md",
            name: "persist-sites",
            body: persist_sites(),
        },
    ]
}

/// Replaces the body between a region's markers, keeping everything else.
fn splice(markdown: &str, region: &Region) -> String {
    let open = format!("{BEGIN}{}", region.name);
    let start = markdown
        .find(&open)
        .unwrap_or_else(|| panic!("{}: no `{open}` marker", region.path));
    let Some(after_open) = markdown[start..]
        .find("-->")
        .map(|offset| start + offset + "-->".len())
    else {
        panic!("{}: unterminated `{open}` marker", region.path)
    };
    let Some(end) = markdown[after_open..]
        .find(END)
        .map(|offset| after_open + offset)
    else {
        panic!("{}: no `{END}` for `{}`", region.path, region.name)
    };
    format!(
        "{}\n\n{}\n\n{}",
        &markdown[..after_open],
        region.body,
        &markdown[end..]
    )
}

#[cfg(test)]
mod tests {
    use super::{Region, regions, splice};
    use crate::domain::SEMANTIC_ROLES;
    use std::fs;

    /// Resolved against the manifest directory rather than the working one:
    /// `cargo test` happens to run from the repository root, and a relative
    /// path would make that a requirement nobody wrote down.
    fn full(path: &str) -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(path)
    }

    fn read(path: &str) -> String {
        fs::read_to_string(full(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
    }

    // Writes when `UPDATE_DOCS=1`, asserts otherwise. Nothing here is meant to
    // be edited by hand, so the failure message says what to run instead of
    // asking someone to retype a generated list.
    #[test]
    fn generated_documentation_is_current() {
        let updating = std::env::var_os("UPDATE_DOCS").is_some();
        let mut stale = Vec::new();

        for region in regions() {
            let current = read(region.path);
            let wanted = splice(&current, &region);
            if current == wanted {
                continue;
            }
            if updating {
                fs::write(full(region.path), &wanted).expect("write generated region");
            } else {
                stale.push(format!("{} ({})", region.path, region.name));
            }
        }

        assert!(
            stale.is_empty(),
            "generated documentation is stale in {}. Run `UPDATE_DOCS=1 cargo test generated_documentation`.",
            stale.join(", ")
        );
    }

    /// Semantic roles are a public contract whose value shapes are explained
    /// manually. A new Rust variant must not ship without the canonical meaning
    /// on the two discovery surfaces Agents actually read.
    #[test]
    fn agent_surfaces_explain_every_semantic_role() {
        let surfaces = [
            (
                "docs/reference/public-schema.md",
                read("docs/reference/public-schema.md"),
            ),
            (
                ".agents/skills/trace-index/SKILL.md",
                read(".agents/skills/trace-index/SKILL.md"),
            ),
        ];
        for role in SEMANTIC_ROLES {
            for (path, markdown) in &surfaces {
                assert!(
                    markdown.lines().any(|line| {
                        line.contains(&format!("`{}`", role.as_str()))
                            && line.contains(role.meaning())
                    }),
                    "{path} does not explain `{}` with its canonical meaning",
                    role.as_str()
                );
            }
        }
    }

    // A region that is generated but has no markers would silently stop being
    // generated, and `splice` panics rather than reporting that clearly.
    #[test]
    fn every_generated_region_is_marked() {
        for region in regions() {
            let markdown = read(region.path);
            assert!(
                markdown.contains(&format!("{}{}", super::BEGIN, region.name)),
                "{} lost its `{}` marker",
                region.path,
                region.name
            );
        }
    }

    /// Kept honest: a generator that produced nothing would satisfy every
    /// comparison above.
    #[test]
    fn the_generators_produce_the_expected_shapes() {
        let by_name = |name: &str| -> Region {
            regions()
                .into_iter()
                .find(|region| region.name == name)
                .expect("region")
        };
        assert_eq!(by_name("cli-table").body.lines().count(), 2 + 21);
        assert_eq!(by_name("relation-list").body.lines().count(), 7);
        assert_eq!(by_name("persist-sites").body.matches('`').count(), 6 * 2);
    }
}
