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

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SectionSummary {
    pub level: usize,
    pub heading: String,
    pub bytes: usize,
}

#[derive(Debug, Serialize)]
pub struct TopicSections {
    pub name: &'static str,
    pub title: &'static str,
    pub returned: usize,
    pub sections: Vec<SectionSummary>,
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
            summary: "Configure Runtime trace roots, synchronize them, and inspect coverage.",
        },
        content: include_str!("../../docs/how-to/configure-and-sync.md"),
    },
    Topic {
        summary: TopicSummary {
            name: "how-to/session-metadata",
            category: "how-to",
            title: "Session Metadata",
            summary: "Store caller-defined JSON independently and query Session associations.",
        },
        content: include_str!("../../docs/how-to/session-metadata.md"),
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
            summary: "Recognized Codex, Pi, Claude Code, and Antigravity CLI JSONL Sources and edge-case behavior.",
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
            summary: "Why Runtime traces share physical structure without one universal Message model.",
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
    let topic = find_topic(name)?;
    Ok(TopicDocument {
        name: topic.summary.name,
        title: topic.summary.title,
        media_type: "text/markdown",
        content: markdown_body(topic.content),
    })
}

/// Lists the Markdown heading sections of one bundled documentation topic.
///
/// # Errors
///
/// Returns an error when the topic does not exist.
pub fn list_sections(name: &str) -> Result<TopicSections> {
    let topic = find_topic(name)?;
    let body = markdown_body(topic.content);
    let sections = parse_sections(&body)
        .into_iter()
        .map(|section| SectionSummary {
            level: section.level,
            heading: section.heading,
            bytes: section.end - section.start,
        })
        .collect::<Vec<_>>();
    Ok(TopicSections {
        name: topic.summary.name,
        title: topic.summary.title,
        returned: sections.len(),
        sections,
    })
}

/// Renders a compact Markdown outline of one topic's section headings.
#[must_use]
pub fn format_sections_markdown(sections: &TopicSections) -> String {
    use std::fmt::Write as _;

    let mut out = format!("# {} (`{}`)\n\n", sections.title, sections.name);
    if sections.sections.is_empty() {
        out.push_str("(no subsections — run without --list-sections to read the full topic)\n");
        return out;
    }
    for section in &sections.sections {
        let indent = "  ".repeat(section.level.saturating_sub(2));
        let hashes = "#".repeat(section.level);
        let _ = writeln!(
            out,
            "{indent}- `{hashes} {}` ({} B)",
            section.heading, section.bytes
        );
    }
    out
}

/// Extracts matching Markdown heading section(s) from one bundled topic.
///
/// Matches headings case-insensitively by substring. When both a parent heading
/// and a nested subheading match, the parent slice already covers the child so
/// nested duplicates are omitted.
///
/// # Errors
///
/// Returns an error when the topic does not exist, `query` is empty, or no
/// heading matches `query`.
pub fn get_section(name: &str, query: &str) -> Result<TopicDocument> {
    let topic = find_topic(name)?;
    let body = markdown_body(topic.content);
    let extracted = extract_sections_from_body(topic.summary.name, &body, query)?;
    Ok(TopicDocument {
        name: topic.summary.name,
        title: topic.summary.title,
        media_type: "text/markdown",
        content: Cow::Owned(extracted),
    })
}

fn extract_sections_from_body(topic_name: &str, body: &str, query: &str) -> Result<String> {
    let needle = query.trim().to_lowercase();
    anyhow::ensure!(
        !needle.is_empty(),
        "section heading query must not be empty"
    );

    let sections = parse_sections(body);
    if sections.is_empty() {
        bail!(
            "documentation topic {topic_name:?} has no subsections\n\
             Hint: run `trace-index docs get {topic_name}` without `--section` to read the full topic"
        );
    }

    let mut matched: Vec<&ParsedSection> = Vec::new();
    for section in &sections {
        if !section.heading.to_lowercase().contains(&needle) {
            continue;
        }
        if matched
            .last()
            .is_some_and(|prev| section.start >= prev.start && section.end <= prev.end)
        {
            continue;
        }
        matched.push(section);
    }

    if matched.is_empty() {
        let available = sections
            .iter()
            .map(|section| section.heading.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        bail!(
            "section {query:?} not found in documentation topic {topic_name:?}\n\
             Available sections: {available}\n\
             Hint: run `trace-index docs get {topic_name} --list-sections` to inspect headings"
        );
    }

    let extracted = matched
        .iter()
        .map(|section| body[section.start..section.end].trim_end())
        .collect::<Vec<_>>()
        .join("\n\n");
    Ok(format!("{extracted}\n"))
}

fn find_topic(name: &str) -> Result<&'static Topic> {
    let Some(topic) = TOPICS.iter().find(|topic| topic.summary.name == name) else {
        bail!(
            "documentation topic not found: {name:?}\n\
             Hint: run `trace-index docs list` to discover valid topic names"
        );
    };
    Ok(topic)
}

struct ParsedSection {
    level: usize,
    heading: String,
    start: usize,
    end: usize,
}

fn parse_sections(body: &str) -> Vec<ParsedSection> {
    let mut raw: Vec<(usize, String, usize)> = Vec::new();
    let mut active_fence: Option<(char, usize)> = None;
    let mut offset = 0_usize;

    for line in body.split_inclusive('\n') {
        let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');
        if let Some((fence_char, fence_len)) = active_fence {
            if is_closing_fence(trimmed, fence_char, fence_len) {
                active_fence = None;
            }
            offset += line.len();
            continue;
        }
        if let Some(fence) = opening_fence(trimmed) {
            active_fence = Some(fence);
            offset += line.len();
            continue;
        }
        if let Some((level, heading)) = parse_atx_heading(trimmed)
            && level >= 2
        {
            raw.push((level, heading.to_owned(), offset));
        }
        offset += line.len();
    }

    let mut sections = Vec::with_capacity(raw.len());
    for (index, (level, heading, start)) in raw.iter().enumerate() {
        let end = raw[index + 1..]
            .iter()
            .find(|(next_level, _, _)| *next_level <= *level)
            .map_or(body.len(), |(_, _, next_start)| *next_start);
        sections.push(ParsedSection {
            level: *level,
            heading: heading.clone(),
            start: *start,
            end,
        });
    }
    sections
}

fn opening_fence(line: &str) -> Option<(char, usize)> {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return None;
    }
    let stripped = &line[leading_spaces..];
    let first = stripped.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let count = stripped.chars().take_while(|&ch| ch == first).count();
    if count < 3 {
        return None;
    }
    // Per CommonMark, a backtick code fence info string cannot contain backticks
    // (otherwise the line is an inline code span like ```code```).
    if first == '`' && stripped[count..].contains('`') {
        return None;
    }
    Some((first, count))
}

fn is_closing_fence(line: &str, fence_char: char, min_len: usize) -> bool {
    let leading_spaces = line.bytes().take_while(|&b| b == b' ').count();
    if leading_spaces > 3 {
        return false;
    }
    let stripped = &line[leading_spaces..];
    let count = stripped.chars().take_while(|&ch| ch == fence_char).count();
    count >= min_len && stripped[count..].trim().is_empty()
}

fn parse_atx_heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.bytes().take_while(|&byte| byte == b'#').count();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = line[hashes..].strip_prefix(' ')?.trim();
    let trimmed = rest.trim_end_matches('#');
    let title =
        if trimmed.len() < rest.len() && (trimmed.is_empty() || trimmed.ends_with([' ', '\t'])) {
            trimmed.trim_end()
        } else {
            rest
        };
    (!title.is_empty()).then_some((hashes, title))
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
    use super::{
        extract_sections_from_body, format_sections_markdown, get, get_section, list,
        list_sections, markdown_body, normalize_newlines, parse_sections, search,
    };

    #[test]
    fn lists_gets_and_searches_bundled_topics() {
        assert_eq!(list().returned, 17);
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

    #[test]
    fn lists_and_extracts_sections_ignoring_fenced_code_comments() {
        let sample = "# Page\n\n## Alpha\n```bash\n## not a heading\n```\n```inline``` text\nBody A\n\n### Alpha Sub\nSub A\n\n## C#\nBody B\n";
        let parsed = parse_sections(sample);
        assert_eq!(parsed.len(), 3);
        assert_eq!(parsed[0].heading, "Alpha");
        assert_eq!(parsed[0].level, 2);
        assert_eq!(parsed[1].heading, "Alpha Sub");
        assert_eq!(parsed[1].level, 3);
        assert_eq!(parsed[2].heading, "C#");

        // Matching both a parent (`## Alpha`) and its child (`### Alpha Sub`)
        // must emit the parent slice once without duplicating the child.
        let dedup = extract_sections_from_body("sample", sample, "alpha").expect("match alpha");
        assert_eq!(dedup.matches("### Alpha Sub").count(), 1);
        assert!(!dedup.contains("## C#"));

        let sections = list_sections("reference/public-schema").expect("public-schema sections");
        assert!(
            sections
                .sections
                .iter()
                .any(|s| s.heading == "Semantic SQL encoding")
        );
        let outline = format_sections_markdown(&sections);
        assert!(outline.contains("Semantic SQL encoding"));

        let extracted = get_section("reference/public-schema", "semantic sql encoding")
            .expect("extract Semantic SQL encoding");
        assert!(extracted.content.starts_with("## Semantic SQL encoding"));
        assert!(extracted.content.contains("agent.tool_call.shell"));
        assert!(extracted.content.contains("call_item_id"));
        assert!(
            !extracted
                .content
                .contains("## JSON values are typed domain values")
        );

        let err = get_section("reference/public-schema", "nonexistent-heading")
            .expect_err("unknown section must error");
        let msg = err.to_string();
        assert!(msg.contains("Available sections:"));
        assert!(msg.contains("--list-sections"));

        let no_sections = list_sections("reference/cli").expect("cli sections");
        assert!(no_sections.sections.is_empty());
        assert!(format_sections_markdown(&no_sections).contains("no subsections"));
        let err_empty = get_section("reference/cli", "anything").expect_err("no subsections");
        assert!(err_empty.to_string().contains("has no subsections"));
    }
}
