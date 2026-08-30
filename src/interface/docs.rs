//! Version-matched documentation exposed through the CLI.

use std::borrow::Cow;

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Debug, Clone, Copy, Serialize)]
pub struct TopicSummary {
    pub name: &'static str,
    pub category: &'static str,
    pub title: &'static str,
    pub summary: &'static str,
}

#[derive(Debug, Serialize)]
pub struct TopicList {
    pub returned: usize,
    pub topics: Vec<TopicSummary>,
}

#[derive(Debug, Serialize)]
pub struct TopicDocument {
    pub name: &'static str,
    pub title: &'static str,
    pub media_type: &'static str,
    pub content: Cow<'static, str>,
}

struct Topic {
    summary: TopicSummary,
    content: &'static str,
}

const TOPICS: &[Topic] = &[
    Topic {
        summary: TopicSummary {
            name: "start-here",
            category: "tutorial",
            title: "Start Here",
            summary: "Build an index and answer from its bounded public query plane.",
        },
        content: include_str!("../../docs/start-here.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/install",
            category: "how-to",
            title: "Install Trace Index",
            summary: "Install a verified prebuilt binary or build Trace Index from crates.io.",
        },
        content: include_str!("../../docs/how-to/install.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/configure-and-sync",
            category: "how-to",
            title: "Configure and Synchronize Traces",
            summary: "Configure Codex, Pi, and Claude Code roots, synchronize them, and inspect coverage.",
        },
        content: include_str!("../../docs/how-to/configure-and-sync.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/query-evidence",
            category: "how-to",
            title: "Query Indexed Facts",
            summary: "Explore public relations with selective, bounded, read-only SQL.",
        },
        content: include_str!("../../docs/how-to/query-evidence.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/search-literals",
            category: "how-to",
            title: "Search Literal Text",
            summary: "Recover broad memories through bounded literal candidates and confirmation.",
        },
        content: include_str!("../../docs/how-to/search-literals.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/inspect-and-export",
            category: "how-to",
            title: "Inspect and Export Physical Evidence",
            summary: "Audit or explicitly materialize exact Records and Assets on demand.",
        },
        content: include_str!("../../docs/how-to/inspect-and-export.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/cli",
            category: "reference",
            title: "CLI Reference",
            summary: "Stable command tree and capability boundaries.",
        },
        content: include_str!("../../docs/reference/cli.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/configuration",
            category: "reference",
            title: "Configuration Reference",
            summary: "Configuration fields, path resolution, defaults, and precedence.",
        },
        content: include_str!("../../docs/reference/configuration.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/public-schema",
            category: "reference",
            title: "Public SQL Schema",
            summary: "Public relations and their evidence semantics.",
        },
        content: include_str!("../../docs/reference/public-schema.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/glossary",
            category: "reference",
            title: "Glossary",
            summary: "Canonical meanings of Runtime, Adapter, Source, Record, Session, Loop, and Item.",
        },
        content: include_str!("../../docs/reference/glossary.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/output-contract",
            category: "reference",
            title: "Output Contract",
            summary: "Compact JSON, Markdown docs, stderr, and process exit codes.",
        },
        content: include_str!("../../docs/reference/output-contract.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "reference/supported-traces",
            category: "reference",
            title: "Supported Trace Formats",
            summary: "Recognized Codex, Pi, and Claude Code JSONL Sources and edge-case behavior.",
        },
        content: include_str!("../../docs/reference/supported-traces.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "explanation/evidence-model",
            category: "explanation",
            title: "Evidence Model",
            summary: "The five domain objects, typed Semantic values, and Record evidence.",
        },
        content: include_str!("../../docs/explanation/evidence-model.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "explanation/runtime-adapters",
            category: "explanation",
            title: "Runtime Adapters",
            summary: "Why Codex, Pi, and Claude Code share physical structure without one universal Message model.",
        },
        content: include_str!("../../docs/explanation/runtime-adapters.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "explanation/bounded-observation",
            category: "explanation",
            title: "Bounded Observation",
            summary: "Resource budgets, truncation facts, and explicit materialization.",
        },
        content: include_str!("../../docs/explanation/bounded-observation.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "explanation/architecture",
            category: "explanation",
            title: "Architecture",
            summary: "Module boundaries, SQLite lifecycle, and feature admission principles.",
        },
        content: include_str!("../../docs/explanation/architecture.md"),
    },
];

/// Lists all bundled documentation topics.
#[must_use]
pub fn list() -> TopicList {
    let topics = TOPICS.iter().map(|topic| topic.summary).collect::<Vec<_>>();
    TopicList {
        returned: topics.len(),
        topics,
    }
}

/// Searches bundled topic metadata and content case-insensitively.
///
/// # Errors
///
/// Returns an error when the query is empty or whitespace-only.
pub fn search(query: &str) -> Result<TopicList> {
    let query = normalized_query(query)?;
    let topics = TOPICS
        .iter()
        .filter(|topic| {
            topic.summary.name.to_lowercase().contains(&query)
                || topic.summary.title.to_lowercase().contains(&query)
                || topic.summary.summary.to_lowercase().contains(&query)
                || topic.content.to_lowercase().contains(&query)
        })
        .map(|topic| topic.summary)
        .collect::<Vec<_>>();
    Ok(TopicList {
        returned: topics.len(),
        topics,
    })
}

/// Gets one bundled documentation topic by its stable name.
///
/// # Errors
///
/// Returns an error when the topic does not exist.
pub fn get(name: &str) -> Result<TopicDocument> {
    let Some(topic) = TOPICS.iter().find(|topic| topic.summary.name == name) else {
        bail!(
            "documentation topic not found: {name:?}\n\
             Hint: run `trace-index docs list` to discover valid topic names"
        );
    };
    Ok(TopicDocument {
        name: topic.summary.name,
        title: topic.summary.title,
        media_type: "text/markdown",
        content: markdown_body(topic.content),
    })
}

fn markdown_body(content: &'static str) -> Cow<'static, str> {
    match normalize_newlines(content) {
        Cow::Borrowed(content) => Cow::Borrowed(strip_frontmatter(content)),
        Cow::Owned(content) => Cow::Owned(strip_frontmatter(&content).to_owned()),
    }
}

fn strip_frontmatter(content: &str) -> &str {
    let Some(rest) = content.strip_prefix("---\n") else {
        return content;
    };
    let Some(end) = rest.find("\n---\n") else {
        return content;
    };
    &rest[end + "\n---\n".len()..]
}

fn normalize_newlines(content: &'static str) -> Cow<'static, str> {
    if content.contains("\r\n") {
        Cow::Owned(content.replace("\r\n", "\n"))
    } else {
        Cow::Borrowed(content)
    }
}

fn normalized_query(query: &str) -> Result<String> {
    let query = query.trim().to_lowercase();
    anyhow::ensure!(
        !query.is_empty(),
        "documentation search query must not be empty"
    );
    Ok(query)
}

#[cfg(test)]
mod tests {
    use super::{get, list, markdown_body, normalize_newlines, search};

    #[test]
    fn lists_gets_and_searches_bundled_topics() {
        assert_eq!(list().returned, 16);
        assert!(
            get("how-to/install")
                .expect("install docs")
                .content
                .contains("Install from crates.io")
        );
        assert!(
            get("how-to/query-evidence")
                .expect("query docs")
                .content
                .contains("query run")
        );
        assert!(
            get("reference/glossary")
                .expect("glossary")
                .content
                .contains("## Runtime")
        );
        let matches = search("read-only").expect("search docs");
        assert!(matches.returned >= 1);
        assert!(
            matches
                .topics
                .iter()
                .any(|topic| topic.name == "how-to/query-evidence")
        );

        let error = get("sessions").expect_err("reject unknown topic");
        let message = error.to_string();
        assert!(message.contains("documentation topic not found"));
        assert!(message.contains("trace-index docs list"));
    }

    #[test]
    fn normalizes_bundled_markdown_across_checkout_platforms() {
        assert_eq!(
            normalize_newlines("# Title\r\n\r\nBody\r\n"),
            "# Title\n\nBody\n"
        );
    }

    #[test]
    fn bundled_docs_hide_website_frontmatter() {
        assert_eq!(
            markdown_body("---\ntitle: Example\ndescription: Example page.\n---\n\n# Example\n"),
            "\n# Example\n"
        );
        assert!(
            !get("start-here")
                .expect("start docs")
                .content
                .starts_with("---")
        );
    }
}
