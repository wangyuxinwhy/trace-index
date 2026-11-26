//! Match-centered verbatim snippet extraction for `search_snippet()`.
//!
//! `item_search` is contentless and indexes pre-tokenized terms, so `SQLite`'s
//! built-in FTS5 `snippet()` cannot reconstruct excerpts from `blobs.text`.
//! This module shares `search.rs`'s clause parsing and tokenization rules to
//! locate match spans in the original text and slice a bounded Unicode-scalar
//! window framed only by `…` (`U+2026`).

use std::collections::{HashMap, HashSet};

use super::search::{QueryClause, for_each_word_token_span, parse_query_clauses};

const ELLIPSIS: char = '…';
const WEIGHT_SCALE: u64 = 1_000_000;
const MAX_CANDIDATE_HITS: usize = 512;

#[derive(Debug, Clone, Copy)]
struct MatchHit {
    start: usize,
    end: usize,
    group: usize,
    mandatory: bool,
    bonus: bool,
}

/// Extracts a verbatim substring of `text` centered around the best match for
/// `query` (and optional exact `literal` from `:contains`), strictly bounded to
/// at most `max_chars` Unicode scalar values inclusive of any `…` markers.
///
/// Falls back cleanly to a head prefix when no match is present in `text`.
pub(crate) fn extract_search_snippet(
    text: &str,
    query: Option<&str>,
    max_chars: usize,
    literal: Option<&str>,
) -> String {
    if text.is_empty() || max_chars == 0 {
        return String::new();
    }

    let (chars, char_starts) = index_chars(text);
    let total = chars.len();
    if total <= max_chars {
        return text.to_owned();
    }
    if max_chars <= 2 {
        return text[..char_starts[max_chars]].to_owned();
    }

    let hits = collect_hits(text, &chars, query, literal);
    let Some((anchor_start, anchor_end)) = select_best_anchor(&hits, max_chars) else {
        return head_fallback(text, &char_starts, max_chars);
    };

    frame_window(
        text,
        &chars,
        &char_starts,
        anchor_start,
        anchor_end,
        max_chars,
    )
}

fn index_chars(text: &str) -> (Vec<char>, Vec<usize>) {
    let mut chars = Vec::new();
    let mut starts = Vec::new();
    for (byte_idx, ch) in text.char_indices() {
        chars.push(ch);
        starts.push(byte_idx);
    }
    starts.push(text.len());
    (chars, starts)
}

fn head_fallback(text: &str, char_starts: &[usize], max_chars: usize) -> String {
    let mut end = max_chars - 1;
    end = avoid_splitting_combining_end(text, char_starts, 0, end);
    let mut out = String::with_capacity(char_starts[end] + ELLIPSIS.len_utf8());
    out.push_str(&text[..char_starts[end]]);
    out.push(ELLIPSIS);
    out
}

fn collect_hits(
    text: &str,
    chars: &[char],
    query: Option<&str>,
    literal: Option<&str>,
) -> Vec<MatchHit> {
    let mut hits = Vec::new();
    let mut next_group = 0_usize;

    let lit_trimmed = literal.filter(|lit| !lit.is_empty());
    if let Some(lit) = lit_trimmed {
        let group = next_group;
        next_group += 1;
        collect_ascii_ci_spans(chars, lit, group, true, true, &mut hits);
    }

    if let Some(raw_query) = query.map(str::trim).filter(|q| !q.is_empty()) {
        let clauses = parse_query_clauses(raw_query);
        let clause_base = next_group;
        next_group += clauses.len();

        let mut word_targets: Vec<(usize, &str, bool)> = Vec::new();
        for (idx, clause) in clauses.iter().enumerate() {
            let group = clause_base + idx;
            match clause {
                QueryClause::Word(word) => word_targets.push((group, word.as_str(), false)),
                QueryClause::Prefix(prefix) => word_targets.push((group, prefix.as_str(), true)),
                QueryClause::CjkPhrase(phrase) => {
                    collect_exact_spans(chars, phrase, group, false, false, &mut hits);
                }
            }
        }

        if !word_targets.is_empty() {
            for_each_word_token_span(text, |token, start, end| {
                for &(group, target, is_prefix) in &word_targets {
                    let matched = if is_prefix {
                        token.starts_with(target)
                    } else {
                        token == target
                    };
                    if matched {
                        hits.push(MatchHit {
                            start,
                            end,
                            group,
                            mandatory: false,
                            bonus: false,
                        });
                    }
                }
            });
        }

        // Exact phrase or punctuation literal match (e.g. `->` or multi-word
        // phrase) acts as a Tier-1 bonus anchor when no explicit `literal`
        // overrides it.
        if lit_trimmed.is_none() && clauses.len() != 1 {
            let group = next_group;
            collect_ascii_ci_spans(chars, raw_query, group, false, true, &mut hits);
        }
    }

    hits.sort_by_key(|hit| (hit.start, hit.end, hit.group));
    cap_hits_per_group(hits)
}

fn cap_hits_per_group(hits: Vec<MatchHit>) -> Vec<MatchHit> {
    if hits.len() <= MAX_CANDIDATE_HITS {
        return hits;
    }
    let mut distinct_groups = HashSet::new();
    for hit in &hits {
        distinct_groups.insert(hit.group);
    }
    let per_group_cap = (MAX_CANDIDATE_HITS / distinct_groups.len().max(1)).max(1);
    let mut kept_per_group: HashMap<usize, usize> = HashMap::new();
    let mut capped = Vec::with_capacity(MAX_CANDIDATE_HITS);
    for hit in hits {
        let count = kept_per_group.entry(hit.group).or_insert(0);
        if *count < per_group_cap {
            *count += 1;
            capped.push(hit);
        }
    }
    capped
}

fn collect_ascii_ci_spans(
    chars: &[char],
    needle: &str,
    group: usize,
    mandatory: bool,
    bonus: bool,
    out: &mut Vec<MatchHit>,
) {
    let needle_chars: Vec<char> = needle.chars().map(|ch| ch.to_ascii_lowercase()).collect();
    if needle_chars.is_empty() || needle_chars.len() > chars.len() {
        return;
    }
    let first = needle_chars[0];
    let len = needle_chars.len();
    for start in 0..=(chars.len() - len) {
        if chars[start].to_ascii_lowercase() != first {
            continue;
        }
        if needle_chars[1..]
            .iter()
            .enumerate()
            .all(|(offset, expected)| chars[start + 1 + offset].to_ascii_lowercase() == *expected)
        {
            out.push(MatchHit {
                start,
                end: start + len,
                group,
                mandatory,
                bonus,
            });
        }
    }
}

fn collect_exact_spans(
    chars: &[char],
    needle: &str,
    group: usize,
    mandatory: bool,
    bonus: bool,
    out: &mut Vec<MatchHit>,
) {
    let needle_chars: Vec<char> = needle.chars().collect();
    if needle_chars.is_empty() || needle_chars.len() > chars.len() {
        return;
    }
    let first = needle_chars[0];
    let len = needle_chars.len();
    for start in 0..=(chars.len() - len) {
        if chars[start] == first && chars[start..start + len] == needle_chars[..] {
            out.push(MatchHit {
                start,
                end: start + len,
                group,
                mandatory,
                bonus,
            });
        }
    }
}

fn select_best_anchor(hits: &[MatchHit], max_chars: usize) -> Option<(usize, usize)> {
    if hits.is_empty() {
        return None;
    }
    let mut freq: HashMap<usize, u64> = HashMap::new();
    let mut any_mandatory = false;
    for hit in hits {
        *freq.entry(hit.group).or_insert(0) += 1;
        any_mandatory |= hit.mandatory;
    }

    let probe_width = max_chars.saturating_sub(2).max(1);
    let mut best: Option<(bool, u64, usize, usize, usize)> = None;
    let mut seen_groups: HashSet<usize> = HashSet::new();

    for i in 0..hits.len() {
        let win_start = hits[i].start;
        let mut win_end = hits[i].end;
        let mut has_mandatory = hits[i].mandatory;
        seen_groups.clear();
        let mut score = group_weight(hits[i], &freq);
        seen_groups.insert(hits[i].group);

        for hit in &hits[i + 1..] {
            if hit.end > win_start + probe_width {
                break;
            }
            win_end = win_end.max(hit.end);
            has_mandatory |= hit.mandatory;
            if seen_groups.insert(hit.group) {
                score = score.saturating_add(group_weight(*hit, &freq));
            }
        }

        if any_mandatory && !has_mandatory {
            continue;
        }
        let candidate = (has_mandatory, score, seen_groups.len(), win_start, win_end);
        let better = match best {
            None => true,
            Some((b_mand, b_score, b_dist, b_start, _)) => {
                (
                    candidate.0,
                    candidate.1,
                    candidate.2,
                    std::cmp::Reverse(candidate.3),
                ) > (b_mand, b_score, b_dist, std::cmp::Reverse(b_start))
            }
        };
        if better {
            best = Some(candidate);
        }
    }

    best.map(|(_, _, _, start, end)| (start, end))
}

fn group_weight(hit: MatchHit, freq: &HashMap<usize, u64>) -> u64 {
    let base = WEIGHT_SCALE / freq.get(&hit.group).copied().unwrap_or(1).max(1);
    if hit.bonus {
        base.saturating_add(WEIGHT_SCALE * 2)
    } else {
        base
    }
}

fn frame_window(
    text: &str,
    chars: &[char],
    char_starts: &[usize],
    anchor_start: usize,
    anchor_end: usize,
    max_chars: usize,
) -> String {
    let total = chars.len();
    let anchor_len = anchor_end.saturating_sub(anchor_start);
    let inner_budget = max_chars.saturating_sub(2);
    let slack = inner_budget.saturating_sub(anchor_len);
    let ideal_lead = if anchor_len > 0 && slack < max_chars / 3 {
        slack / 2
    } else {
        (max_chars / 3).min(slack)
    };
    let mut win_start = anchor_start.saturating_sub(ideal_lead);
    let mut need_left = win_start > 0;
    let avail_without_right = max_chars - usize::from(need_left);

    let (win_end, need_right) = if total - win_start <= avail_without_right {
        // Reaching the end of the text: shift left to use the full budget.
        let tail_budget = max_chars - 1;
        win_start = total.saturating_sub(tail_budget);
        need_left = win_start > 0;
        (total, false)
    } else {
        let content_budget = max_chars - usize::from(need_left) - 1;
        (win_start + content_budget, true)
    };

    let win_start = if need_left {
        avoid_splitting_combining_start(chars, win_start, win_end)
    } else {
        win_start
    };
    let win_end = if need_right {
        avoid_splitting_combining_end(text, char_starts, win_start, win_end)
    } else {
        win_end
    };

    let mut out = String::with_capacity(
        char_starts[win_end] - char_starts[win_start] + 2 * ELLIPSIS.len_utf8(),
    );
    if need_left {
        out.push(ELLIPSIS);
    }
    out.push_str(&text[char_starts[win_start]..char_starts[win_end]]);
    if need_right {
        out.push(ELLIPSIS);
    }
    debug_assert!(out.chars().count() <= max_chars);
    out
}

fn is_combining_or_joiner(ch: char) -> bool {
    matches!(
        ch as u32,
        0x0300..=0x036F
            | 0x1AB0..=0x1AFF
            | 0x1DC0..=0x1DFF
            | 0x200D
            | 0x20D0..=0x20FF
            | 0xFE00..=0xFE0F
            | 0xFE20..=0xFE2F
    )
}

fn avoid_splitting_combining_start(chars: &[char], mut start: usize, end: usize) -> usize {
    let limit = end.saturating_sub(1);
    while start < limit && is_combining_or_joiner(chars[start]) {
        start += 1;
    }
    start
}

fn avoid_splitting_combining_end(
    text: &str,
    char_starts: &[usize],
    start: usize,
    mut end: usize,
) -> usize {
    while end > start + 1 {
        let next_ch = text[char_starts[end]..].chars().next();
        if next_ch.is_some_and(is_combining_or_joiner) {
            end -= 1;
        } else {
            break;
        }
    }
    end
}

#[cfg(test)]
mod tests {
    use super::extract_search_snippet;

    #[test]
    fn centers_around_late_match_within_hard_max_chars_cap() {
        let prefix = "alpha ".repeat(80);
        let suffix = " omega".repeat(80);
        let text = format!("{prefix}target_needle{suffix}");
        let snippet = extract_search_snippet(&text, Some("target_needle"), 60, None);
        assert!(snippet.contains("target_needle"), "snippet: {snippet}");
        assert!(snippet.starts_with('…'));
        assert!(snippet.ends_with('…'));
        assert!(snippet.chars().count() <= 60);
    }

    #[test]
    fn literal_takes_precedence_without_tokenizing_punctuation() {
        let text = format!(
            "{}query word here{}exact->arrow{}tail",
            "x ".repeat(100),
            "y ".repeat(100),
            "z ".repeat(100)
        );
        let snippet = extract_search_snippet(&text, Some("query"), 48, Some("exact->arrow"));
        assert!(snippet.contains("exact->arrow"), "snippet: {snippet}");
        assert!(snippet.chars().count() <= 48);
    }

    #[test]
    fn handles_unicode_case_expansion_cjk_camel_and_prefix() {
        // Turkish capital I with dot expands to 2 scalars under `.to_lowercase()`,
        // so spans must stay anchored in original text coordinates.
        let text = format!(
            "{}İSTANBUL parseTimestampMs 索引检查 cargo_clippy {}",
            "a ".repeat(60),
            "b ".repeat(60)
        );
        let s1 = extract_search_snippet(&text, Some("timestamp"), 50, None);
        assert!(s1.contains("parseTimestampMs"), "s1: {s1}");
        assert!(s1.chars().count() <= 50);

        let s2 = extract_search_snippet(&text, Some("索引检查"), 40, None);
        assert!(s2.contains("索引检查"), "s2: {s2}");
        assert!(s2.chars().count() <= 40);

        let s3 = extract_search_snippet(&text, Some("carg*"), 40, None);
        assert!(s3.contains("cargo_clippy"), "s3: {s3}");
        assert!(s3.chars().count() <= 40);
    }

    #[test]
    fn prefers_rare_discriminating_term_over_ubiquitous_boilerplate() {
        let mut text = String::new();
        for _ in 0..40 {
            text.push_str("trace-index status check ok. ");
        }
        text.push_str("root cause was deadlock_recovery_timeout in worker ");
        for _ in 0..20 {
            text.push_str("trace-index status check ok. ");
        }
        let snippet = extract_search_snippet(
            &text,
            Some("trace-index deadlock_recovery_timeout"),
            80,
            None,
        );
        assert!(
            snippet.contains("deadlock_recovery_timeout"),
            "snippet: {snippet}"
        );
        assert!(snippet.chars().count() <= 80);
    }

    #[test]
    fn handles_short_max_chars_and_unmatched_head_fallback() {
        assert_eq!(
            extract_search_snippet("abcdef", Some("zzz"), 4, None),
            "abc…"
        );
        assert_eq!(extract_search_snippet("abcdef", Some("zzz"), 2, None), "ab");
        assert_eq!(extract_search_snippet("abcdef", Some("zzz"), 1, None), "a");
        assert_eq!(extract_search_snippet("abcdef", None, 10, None), "abcdef");
        assert_eq!(extract_search_snippet("", Some("abc"), 10, None), "");
    }
}
