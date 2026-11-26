//! Deterministic, zero-`unsafe` pre-tokenization for mixed CJK, English, and
//! source-code full-text search over `SQLite` FTS5 `unicode61`.
//!
//! `item_search` (Format 5+) uses `SQLite`'s built-in `unicode61` tokenizer with
//! `tokenchars '_'` and `detail = 'full'`. Pre-tokenizing in Rust at write time and applying the matching transformation in the
//! `search_query()` SQL helper provides 2-character CJK recall, adjacent CJK
//! phrase matching, code identifier splitting (`camelCase`, `PascalCase`,
//! `snake_case`, underscore segments), prefix queries (`foo*`), multi-word
//! queries, and native `bm25()` ranking without C FTS5 extensions or `unsafe`.

use std::collections::HashSet;

/// Parsed search clause shared by `search_query()` FTS expression building and
/// `search_snippet()` match windowing.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum QueryClause {
    /// Lowercased word or identifier token of length >= 2.
    Word(String),
    /// Lowercased ASCII identifier prefix of length >= 4 (from trailing `*`).
    Prefix(String),
    /// Contiguous CJK character sequence of length >= 2.
    CjkPhrase(String),
}

/// Produces space-separated normalized tokens for `item_search` (Format 5+).
///
/// - Contiguous CJK runs of length >= 2 emit overlapping 2-grams in source
///   order so FTS5 `detail='full'` preserves exact character adjacency for 3+
///   character CJK phrase queries.
/// - ASCII/code identifier runs (`[A-Za-z0-9_]+`) emit their lowercased full
///   form (preserving `_`) plus distinct split sub-tokens (`snake_case`,
///   `camelCase`, `PascalCase`, and letter/digit transitions) of length >= 2.
/// - Other non-ASCII alphabetic runs (e.g. accented Latin, Cyrillic) emit their
///   lowercased form of length >= 2 so non-CJK natural language remains searchable.
pub(crate) fn tokenize_for_term_index(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.char_indices().peekable();

    while let Some(&(start, ch)) = chars.peek() {
        if is_cjk(ch) {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if is_cjk(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            emit_cjk_run(&text[start..end], &mut out);
        } else if is_ascii_ident(ch) {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if is_ascii_ident(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            emit_ascii_ident_run(&text[start..end], &mut out);
        } else if ch.is_alphabetic() {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if next.is_alphabetic() && !is_cjk(next) && !is_ascii_ident(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let lower = text[start..end].to_lowercase();
            if lower.chars().count() >= 2 {
                push_token(&mut out, &lower);
            }
        } else {
            chars.next();
        }
    }

    out
}

/// Parses a user search string into deduplicated clauses matching
/// `search_query()` semantics.
pub(crate) fn parse_query_clauses(query: &str) -> Vec<QueryClause> {
    let mut clauses = Vec::new();
    let mut seen = HashSet::new();

    for chunk in query.split_whitespace() {
        collect_chunk_clauses(chunk, &mut clauses, &mut seen);
    }

    clauses
}

/// Converts a user search string into an FTS5 `MATCH` expression for
/// `item_search` (Format 5+, or legacy Format 4 `item_search_terms`).
///
/// Whitespace-separated chunks and punctuation-delimited code segments are
/// joined with `AND` so identifier sub-tokens never break phrase adjacency.
/// Within a chunk, a contiguous CJK run of length >= 3 becomes an adjacent
/// bigram phrase (so `"智能体"` becomes `"智能 能体"` with exact adjacency).
pub(crate) fn make_term_search_query(query: &str) -> std::result::Result<String, &'static str> {
    let parsed = parse_query_clauses(query);
    if parsed.is_empty() {
        return Err(
            "search_query requires at least one 2+ character word, identifier, prefix (4+ chars with trailing '*'), or CJK term; \
             narrow structurally and scan blobs with instr() for single characters or punctuation literals",
        );
    }

    let clauses: Vec<String> = parsed
        .into_iter()
        .map(|clause| match clause {
            QueryClause::Word(word) => quote_fts_phrase(&word),
            QueryClause::Prefix(prefix) => format!("{}*", quote_fts_phrase(&prefix)),
            QueryClause::CjkPhrase(raw) => format_cjk_fts_phrase(&raw),
        })
        .collect();

    Ok(clauses.join(" AND "))
}

fn format_cjk_fts_phrase(raw: &str) -> String {
    let cjk_chars: Vec<char> = raw.chars().collect();
    if cjk_chars.len() == 2 {
        return quote_fts_phrase(raw);
    }
    let mut joined = String::new();
    for pair in cjk_chars.windows(2) {
        if !joined.is_empty() {
            joined.push(' ');
        }
        joined.push(pair[0]);
        joined.push(pair[1]);
    }
    quote_fts_phrase(&joined)
}

fn collect_chunk_clauses(
    chunk: &str,
    clauses: &mut Vec<QueryClause>,
    seen: &mut HashSet<QueryClause>,
) {
    let mut chars = chunk.char_indices().peekable();

    while let Some(&(start, ch)) = chars.peek() {
        if is_cjk(ch) {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if is_cjk(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            if chunk[start..end].chars().count() < 2 {
                continue;
            }
            let clause = QueryClause::CjkPhrase(chunk[start..end].to_owned());
            if seen.insert(clause.clone()) {
                clauses.push(clause);
            }
        } else if is_ascii_ident(ch) {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if is_ascii_ident(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let lower = chunk[start..end].to_ascii_lowercase();
            let wants_prefix = chars.peek().is_some_and(|&(_, next)| next == '*');
            if wants_prefix {
                chars.next();
            }
            let clause = if wants_prefix && lower.len() >= 4 {
                Some(QueryClause::Prefix(lower))
            } else if lower.len() >= 2 {
                Some(QueryClause::Word(lower))
            } else {
                None
            };
            if let Some(clause) = clause
                && seen.insert(clause.clone())
            {
                clauses.push(clause);
            }
        } else if ch.is_alphabetic() {
            let mut end = start + ch.len_utf8();
            chars.next();
            while let Some(&(idx, next)) = chars.peek() {
                if next.is_alphabetic() && !is_cjk(next) && !is_ascii_ident(next) {
                    end = idx + next.len_utf8();
                    chars.next();
                } else {
                    break;
                }
            }
            let lower = chunk[start..end].to_lowercase();
            if lower.chars().count() >= 2 {
                let clause = QueryClause::Word(lower);
                if seen.insert(clause.clone()) {
                    clauses.push(clause);
                }
            }
        } else {
            chars.next();
        }
    }
}

/// Visits every lowercased word/identifier token emitted by the term indexer
/// together with its half-open Unicode scalar span `(char_start, char_end)` in
/// the original `text`.
pub(super) fn for_each_word_token_span<F>(text: &str, mut visit: F)
where
    F: FnMut(&str, usize, usize),
{
    let mut iter = text.char_indices().enumerate().peekable();

    while let Some(&(char_start, (byte_start, ch))) = iter.peek() {
        if is_cjk(ch) {
            iter.next();
            while let Some(&(_, (_, next))) = iter.peek() {
                if is_cjk(next) {
                    iter.next();
                } else {
                    break;
                }
            }
        } else if is_ascii_ident(ch) {
            let mut byte_end = byte_start + ch.len_utf8();
            iter.next();
            while let Some(&(_, (idx, next))) = iter.peek() {
                if is_ascii_ident(next) {
                    byte_end = idx + next.len_utf8();
                    iter.next();
                } else {
                    break;
                }
            }
            visit_ascii_ident_spans(&text[byte_start..byte_end], char_start, &mut visit);
        } else if ch.is_alphabetic() {
            let mut byte_end = byte_start + ch.len_utf8();
            let mut char_end = char_start + 1;
            iter.next();
            while let Some(&(c_idx, (idx, next))) = iter.peek() {
                if next.is_alphabetic() && !is_cjk(next) && !is_ascii_ident(next) {
                    byte_end = idx + next.len_utf8();
                    char_end = c_idx + 1;
                    iter.next();
                } else {
                    break;
                }
            }
            let lower = text[byte_start..byte_end].to_lowercase();
            if lower.chars().count() >= 2 {
                visit(&lower, char_start, char_end);
            }
        } else {
            iter.next();
        }
    }
}

fn visit_ascii_ident_spans<F>(run: &str, run_char_start: usize, visit: &mut F)
where
    F: FnMut(&str, usize, usize),
{
    let lower = run.to_ascii_lowercase();
    if lower.len() >= 2 {
        visit(&lower, run_char_start, run_char_start + run.len());
    }
    let bytes = run.as_bytes();
    let mut seg_start = 0;
    while seg_start < bytes.len() {
        while seg_start < bytes.len() && bytes[seg_start] == b'_' {
            seg_start += 1;
        }
        if seg_start >= bytes.len() {
            break;
        }
        let mut seg_end = seg_start;
        while seg_end < bytes.len() && bytes[seg_end] != b'_' {
            seg_end += 1;
        }
        let segment = &run[seg_start..seg_end];
        if segment.len() >= 2 && (seg_start > 0 || seg_end < bytes.len()) {
            let seg_lower = segment.to_ascii_lowercase();
            visit(
                &seg_lower,
                run_char_start + seg_start,
                run_char_start + seg_end,
            );
        }
        for (sub_start, sub_end) in split_camel_and_digits_ranges(segment) {
            let sub_len = sub_end - sub_start;
            if sub_len >= 2 && !(sub_start == 0 && sub_end == segment.len()) {
                let sub_lower = segment[sub_start..sub_end].to_ascii_lowercase();
                visit(
                    &sub_lower,
                    run_char_start + seg_start + sub_start,
                    run_char_start + seg_start + sub_end,
                );
            }
        }
        seg_start = seg_end;
    }
}

fn emit_cjk_run(run: &str, out: &mut String) {
    let chars: Vec<char> = run.chars().collect();
    if chars.len() < 2 {
        return;
    }
    for pair in chars.windows(2) {
        if !out.is_empty() {
            out.push(' ');
        }
        out.push(pair[0]);
        out.push(pair[1]);
    }
}

fn emit_ascii_ident_run(run: &str, out: &mut String) {
    let lower = run.to_ascii_lowercase();
    if lower.len() >= 2 {
        push_token(out, &lower);
    }

    let mut emitted_extras: HashSet<String> = HashSet::new();
    let mut sub_tokens: Vec<String> = Vec::new();
    for segment in run.split('_') {
        if segment.is_empty() {
            continue;
        }
        // Preserve each underscore-delimited segment verbatim (e.g. `pkg_CoreEngine_Worker`
        // emits `coreengine`, and `slot_7t_context` preserves `7t` before digit splitting).
        let seg_lower = segment.to_ascii_lowercase();
        if seg_lower.len() >= 2 && seg_lower != lower && emitted_extras.insert(seg_lower.clone()) {
            push_token(out, &seg_lower);
        }
        split_camel_and_digits(segment, &mut sub_tokens);
    }

    if sub_tokens.len() > 1 || (sub_tokens.len() == 1 && sub_tokens[0] != lower) {
        for token in sub_tokens {
            if token.len() >= 2 && token != lower && emitted_extras.insert(token.clone()) {
                push_token(out, &token);
            }
        }
    }
}

/// Splits an underscore-free ASCII segment on `camelCase`, `PascalCase`,
/// acronym boundaries (`HTTPServer` -> `http`, `server`), and digit transitions
/// (`v4alpha` -> `v`, `4`, `alpha`).
fn split_camel_and_digits(segment: &str, out: &mut Vec<String>) {
    for (start, end) in split_camel_and_digits_ranges(segment) {
        out.push(segment[start..end].to_ascii_lowercase());
    }
}

fn split_camel_and_digits_ranges(segment: &str) -> Vec<(usize, usize)> {
    let bytes = segment.as_bytes();
    let len = bytes.len();
    let mut ranges = Vec::new();
    let mut start = 0;
    while start < len {
        let end = if bytes[start].is_ascii_digit() {
            let mut idx = start + 1;
            while idx < len && bytes[idx].is_ascii_digit() {
                idx += 1;
            }
            idx
        } else if bytes[start].is_ascii_uppercase() {
            let mut idx = start + 1;
            if idx < len && bytes[idx].is_ascii_uppercase() {
                while idx < len && bytes[idx].is_ascii_uppercase() {
                    idx += 1;
                }
                // `HTTPServer` -> split before `S` when followed by lowercase.
                if idx < len && bytes[idx].is_ascii_lowercase() && idx - start > 1 {
                    idx -= 1;
                }
            } else {
                while idx < len && bytes[idx].is_ascii_lowercase() {
                    idx += 1;
                }
            }
            idx
        } else {
            let mut idx = start + 1;
            while idx < len && bytes[idx].is_ascii_lowercase() {
                idx += 1;
            }
            idx
        };
        ranges.push((start, end));
        start = end;
    }
    ranges
}

fn push_token(out: &mut String, token: &str) {
    if !out.is_empty() {
        out.push(' ');
    }
    out.push_str(token);
}

fn quote_fts_phrase(phrase: &str) -> String {
    format!("\"{}\"", phrase.replace('"', "\"\""))
}

const fn is_ascii_ident(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_'
}

const fn is_cjk(ch: char) -> bool {
    matches!(
        ch as u32,
        0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xF900..=0xFAFF
            | 0x3040..=0x30FF
            | 0xAC00..=0xD7AF
    )
}

#[cfg(test)]
mod tests {
    use super::{make_term_search_query, tokenize_for_term_index};

    #[test]
    fn tokenizes_cjk_bigrams_and_code_identifiers() {
        assert_eq!(tokenize_for_term_index("索引"), "索引");
        assert_eq!(tokenize_for_term_index("智能体使用"), "智能 能体 体使 使用");
        assert_eq!(
            tokenize_for_term_index("parseTimestampMs"),
            "parsetimestampms parse timestamp ms"
        );
        assert_eq!(
            tokenize_for_term_index("HTTPServer"),
            "httpserver http server"
        );
        assert_eq!(
            tokenize_for_term_index("items_role_occurred"),
            "items_role_occurred items role occurred"
        );
        assert_eq!(
            tokenize_for_term_index("SQLite 覆盖索引"),
            "sqlite sq lite 覆盖 盖索 索引"
        );
        assert_eq!(
            tokenize_for_term_index("pkg_CoreEngine_Worker slot_7t_context"),
            "pkg_coreengine_worker pkg coreengine worker core engine slot_7t_context slot 7t context"
        );
    }

    #[test]
    fn builds_phrase_aware_cjk_and_multiword_queries() {
        assert_eq!(
            make_term_search_query("索引").expect("2-char CJK"),
            "\"索引\""
        );
        assert_eq!(
            make_term_search_query("智能体").expect("3-char CJK phrase"),
            "\"智能 能体\""
        );
        assert_eq!(
            make_term_search_query("智能体使用").expect("5-char CJK phrase"),
            "\"智能 能体 体使 使用\""
        );
        assert_eq!(
            make_term_search_query("SQLite 索引").expect("mixed CJK+English"),
            "\"sqlite\" AND \"索引\""
        );
        assert_eq!(
            make_term_search_query("src/storage/schema_v5.rs").expect("path query"),
            "\"src\" AND \"storage\" AND \"schema_v5\" AND \"rs\""
        );
        assert_eq!(
            make_term_search_query("--max-output-bytes").expect("flag query"),
            "\"max\" AND \"output\" AND \"bytes\""
        );
        assert_eq!(
            make_term_search_query("occurred_at").expect("snake_case exact"),
            "\"occurred_at\""
        );
        assert_eq!(
            make_term_search_query("a1b2c3*").expect("prefix query"),
            "\"a1b2c3\"*"
        );
        assert!(make_term_search_query("   ->  ").is_err());
        assert!(make_term_search_query("索").is_err());
    }
}
