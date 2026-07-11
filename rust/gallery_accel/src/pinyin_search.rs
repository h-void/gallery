//! Search text / pinyin helpers (mirrors `app/sort_utils.py` search_text).

use pinyin::{ToPinyin, ToPinyinMulti};
use regex::Regex;
use std::sync::OnceLock;

pub fn search_text_for_values(values: &[&str]) -> String {
    let mut parts: Vec<String> = Vec::new();
    for value in values {
        let text = value.trim();
        if text.is_empty() {
            continue;
        }
        parts.push(text.to_lowercase());
        let syllables: Vec<String> = text
            .to_pinyin()
            .flatten()
            .map(|p| p.plain().to_string().to_lowercase())
            .collect();
        if !syllables.is_empty() {
            parts.push(syllables.join(""));
            parts.push(syllables.join(" "));
            parts.push(
                syllables
                    .iter()
                    .filter_map(|s| s.chars().next())
                    .collect::<String>(),
            );
        }
        // Multi-pronunciation initials (compact)
        let multi: Vec<String> = text
            .to_pinyin_multi()
            .flatten()
            .flat_map(|m| m.into_iter().map(|p| p.plain().to_string().to_lowercase()))
            .collect();
        if !multi.is_empty() {
            parts.push(multi.join(""));
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
    let compact = ws.replace_all(&needle, "");
    let haystack = search_text_for_values(values);
    if haystack.contains(&needle) {
        return true;
    }
    !compact.is_empty() && ws.replace_all(&haystack, "").contains(compact.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_matches_chinese_name() {
        let hay = search_text_for_values(&["泳装"]);
        assert!(hay.contains("yong") || hay.contains("zhuang") || hay.contains("yz") || hay.contains("泳装"));
        assert!(text_matches_search("yong", &["泳装"]) || text_matches_search("泳", &["泳装"]));
    }
}
