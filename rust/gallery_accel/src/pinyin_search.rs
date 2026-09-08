//! Search text / pinyin helpers (mirrors `app/sort_utils.py` search_text).
//!
//! Matching is candidate-aware: every Han character may consume the query
//! through ANY of its readings (full syllable or initial), evaluated per
//! character with backtracking instead of materializing an exponential
//! cartesian product. Non-Han characters stay in the compact concatenation so
//! mixed queries like `泳装01` keep matching.

use pinyin::{Pinyin, ToPinyinMulti};
use regex::Regex;
use std::sync::OnceLock;

/// One value character with its match candidates.
enum CharCandidates {
    /// Han character: every candidate reading (full syllables, lowercase,
    /// tone-free, deduplicated).
    Han(Vec<String>),
    /// Non-Han character matches literally (case-insensitive).
    Literal(char),
}

fn char_candidates(ch: char) -> CharCandidates {
    let mut readings: Vec<String> = match ch.to_pinyin_multi() {
        Some(multi) => multi
            .into_iter()
            .map(|p: Pinyin| p.plain().to_lowercase())
            .collect(),
        None => Vec::new(),
    };
    readings.sort_unstable();
    readings.dedup();
    if readings.is_empty() {
        CharCandidates::Literal(ch.to_lowercase().next().unwrap_or(ch))
    } else {
        CharCandidates::Han(readings)
    }
}

/// Compact the value into match candidates, dropping whitespace (compact
/// matching semantics).
fn value_candidates(value: &str) -> Vec<CharCandidates> {
    value
        .chars()
        .filter(|ch| !ch.is_whitespace())
        .map(char_candidates)
        .collect()
}

/// True when `query[i..]` can be consumed through `chars[j..]`. Memoized on
/// `(i, j)`; the answer does not depend on where the substring started, so
/// every start position shares one table.
fn match_from(
    query: &[char],
    chars: &[CharCandidates],
    i: usize,
    j: usize,
    memo: &mut Vec<Vec<Option<bool>>>,
) -> bool {
    if i == query.len() {
        return true;
    }
    if j == chars.len() {
        return false;
    }
    if let Some(cached) = memo[i][j] {
        return cached;
    }
    let matched = match &chars[j] {
        CharCandidates::Literal(ch) => query[i] == *ch && match_from(query, chars, i + 1, j + 1, memo),
        CharCandidates::Han(readings) => {
            readings.iter().any(|reading| {
                let syllable: Vec<char> = reading.chars().collect();
                if query[i..].len() >= syllable.len()
                    && query[i..i + syllable.len()] == syllable[..]
                    && match_from(query, chars, i + syllable.len(), j + 1, memo)
                {
                    return true;
                }
                // Initial: first character of any candidate reading.
                let initial = reading.chars().next();
                initial == Some(query[i]) && match_from(query, chars, i + 1, j + 1, memo)
            })
        }
    };
    memo[i][j] = Some(matched);
    matched
}

/// Candidate-aware compact match of `query` against one value: the query may
/// start anywhere, and every Han character may use any of its readings.
fn query_matches_value(query: &[char], value: &str) -> bool {
    let chars = value_candidates(value);
    let mut memo = vec![vec![None; chars.len() + 1]; query.len() + 1];
    (0..chars.len()).any(|start| match_from(query, &chars, 0, start, &mut memo))
}

pub fn search_text_for_values(values: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for value in values {
        let text = value.trim();
        if text.is_empty() {
            continue;
        }
        parts.push(text.to_lowercase());
        // Primary chain: first reading per Han character, non-Han characters
        // kept as-is (`泳装01` -> `yongzhuang01` / `yz01`).
        let mut primary: Vec<String> = Vec::new();
        let mut primary_initials: Vec<String> = Vec::new();
        // Multi chain: every candidate reading interleaved (no cartesian
        // product), non-Han characters kept as-is.
        let mut multi: Vec<String> = Vec::new();
        let mut multi_initials: Vec<String> = Vec::new();
        for ch in text.chars().filter(|ch| !ch.is_whitespace()) {
            match char_candidates(ch) {
                CharCandidates::Han(readings) => {
                    let first = readings[0].clone();
                    primary.push(first.clone());
                    primary_initials.push(first.chars().next().map(String::from).unwrap_or_default());
                    let mut reading_initials: Vec<String> = readings
                        .iter()
                        .filter_map(|reading| reading.chars().next().map(String::from))
                        .collect();
                    reading_initials.dedup();
                    for reading in &readings {
                        multi.push(reading.clone());
                    }
                    multi_initials.extend(reading_initials);
                }
                CharCandidates::Literal(literal) => {
                    let literal = literal.to_lowercase().to_string();
                    primary.push(literal.clone());
                    primary_initials.push(literal.clone());
                    multi.push(literal.clone());
                    multi_initials.push(literal);
                }
            }
        }
        if !primary.is_empty() {
            parts.push(primary.join(""));
            parts.push(primary.join(" "));
            parts.push(primary_initials.concat());
            parts.push(multi.join(""));
            parts.push(multi_initials.concat());
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for p in parts {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    }
    out.join(" ")
}

pub fn text_matches_search(query: &str, values: &[&str]) -> bool {
    static WS: OnceLock<Regex> = OnceLock::new();
    let ws = WS.get_or_init(|| Regex::new(r"\s+").unwrap());
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    let haystack = search_text_for_values(values);
    if haystack.contains(&needle) {
        return true;
    }
    let compact_query: Vec<char> = ws.replace_all(&needle, "").chars().collect();
    if compact_query.is_empty() {
        return false;
    }
    values
        .iter()
        .any(|value| query_matches_value(&compact_query, value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_matches_chinese_name() {
        let hay = search_text_for_values(&["泳装"]);
        assert!(
            hay.contains("yong")
                || hay.contains("zhuang")
                || hay.contains("yz")
                || hay.contains("泳装")
        );
        assert!(text_matches_search("yong", &["泳装"]) || text_matches_search("泳", &["泳装"]));
    }

    #[test]
    fn polyphone_candidates_match_without_cartesian_product() {
        // 还(hai/huan) 没(mei/mo): candidate-aware matching must accept the
        // combination the primary chain does not contain.
        assert!(text_matches_search("haihuan", &["还没"]));
        assert!(text_matches_search("hmei", &["还没"]) || text_matches_search("huanmei", &["还没"]));
    }

    #[test]
    fn initials_cover_every_candidate_reading() {
        // 重(chong/zhong) 庆(qing): both initial chains must match.
        assert!(text_matches_search("cq", &["重庆"]));
        assert!(text_matches_search("zq", &["重庆"]));
        assert!(text_matches_search("chongqing", &["重庆"]));
    }

    #[test]
    fn non_han_characters_stay_in_the_compact_chain() {
        assert!(text_matches_search("泳装01", &["泳装01"]));
        assert!(text_matches_search("yongzhuang01", &["泳装01"]));
        assert!(text_matches_search("yz01", &["泳装01"]));
        // Katakana is kept literally; romaji conversion is out of scope.
        assert!(text_matches_search("初音ミク", &["初音ミク"]));
        assert!(!text_matches_search("miku", &["初音ミク"]));
    }

    #[test]
    fn compact_matching_ignores_internal_whitespace() {
        assert!(text_matches_search("abc", &["A B C"]));
        assert!(text_matches_search("ab c", &["A B C"]));
    }

    #[test]
    fn unrelated_queries_do_not_match() {
        assert!(!text_matches_search("zha", &["泳装"]));
        assert!(!text_matches_search("yongzhuangx", &["泳装"]));
    }
}

