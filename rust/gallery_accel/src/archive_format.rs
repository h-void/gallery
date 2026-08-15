//! Validated, versioned folder archive naming profiles.
//!
//! Profiles live in `app_settings` as JSON.  This module deliberately keeps
//! rendering separate from database and filesystem work so a preview can use
//! exactly the same validation as an eventual plan update.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Datelike, NaiveDate};
use serde_json::{json, Map, Value};

pub const ARCHIVE_FORMAT_SETTINGS_KEY: &str = "folder_rename_format_profiles_v1";
const SETTINGS_VERSION: i64 = 1;
const DEFAULT_PROFILE_ID: &str = "default";
const MAX_TEMPLATE_LEN: usize = 512;
const MAX_PROFILE_NAME_LEN: usize = 80;
const MAX_TOKEN_VALUE_LEN: usize = 160;

const TOKENS: [&str; 8] = [
    "artist", "date", "year", "month", "tags", "title", "folder", "index",
];

#[derive(Debug, Clone)]
pub(crate) struct RenderContext {
    pub artist: String,
    pub date: String,
    pub tags: Vec<String>,
    pub title: String,
    pub folder: String,
    pub index: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct RenderedTarget {
    pub target_folder: String,
    pub tokens: Value,
}

#[derive(Debug, Clone)]
enum TemplatePart {
    Text(String),
    Token(String),
}

pub(crate) fn default_settings() -> Value {
    json!({
        "version": SETTINGS_VERSION,
        "active_profile_id": DEFAULT_PROFILE_ID,
        "default_profile_id": DEFAULT_PROFILE_ID,
        "profiles": [default_profile()],
        "artist_profile_ids": {},
    })
}

fn default_profile() -> Value {
    json!({
        "id": DEFAULT_PROFILE_ID,
        "name": "Default archive",
        "template": "{year}/{date} {tags}",
        "empty_value": "untitled",
        "tag_separator": "&",
        "date_format": "iso",
        "collision_strategy": "suffix",
    })
}

/// Normalize and validate a settings JSON document before it is persisted.
/// Unknown fields are intentionally not persisted so a typo cannot silently
/// change behavior after a later version starts using that field name.
pub(crate) fn normalize_settings(input: &Value) -> Result<Value> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("archive format settings must be an object"))?;
    let version = match object.get("version") {
        Some(value) => value
            .as_i64()
            .ok_or_else(|| anyhow!("archive format settings version must be an integer"))?,
        None => SETTINGS_VERSION,
    };
    if version != SETTINGS_VERSION {
        bail!("unsupported archive format settings version: {version}");
    }

    let profiles_value = object
        .get("profiles")
        .cloned()
        .unwrap_or_else(|| json!([default_profile()]));
    let profiles_array = profiles_value
        .as_array()
        .ok_or_else(|| anyhow!("profiles must be an array"))?;
    if profiles_array.is_empty() {
        bail!("at least one archive format profile is required");
    }
    if profiles_array.len() > 64 {
        bail!("too many archive format profiles");
    }

    let mut profiles = Vec::with_capacity(profiles_array.len());
    let mut ids = std::collections::HashSet::new();
    for profile in profiles_array {
        let profile = normalize_profile(profile)?;
        let id = profile["id"].as_str().unwrap_or_default().to_string();
        if !ids.insert(id.clone()) {
            bail!("duplicate archive format profile id: {id}");
        }
        profiles.push(profile);
    }

    let active_profile_id = object
        .get("active_profile_id")
        .or_else(|| object.get("default_profile_id"))
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROFILE_ID)
        .trim()
        .to_string();
    validate_profile_id(&active_profile_id)?;
    if !ids.contains(&active_profile_id) {
        bail!("active_profile_id does not reference a profile");
    }

    let artist_profile_ids_value = object
        .get("artist_profile_ids")
        .or_else(|| object.get("artist_overrides"))
        .or_else(|| object.get("artist_profiles"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let artist_profile_ids_object = artist_profile_ids_value
        .as_object()
        .ok_or_else(|| anyhow!("artist_profile_ids must be an object"))?;
    let mut artist_profile_ids = Map::new();
    for (artist_id, profile_id) in artist_profile_ids_object {
        let artist_id_num = artist_id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("artist profile selection has an invalid artist id"))?;
        let profile_id = profile_id
            .as_str()
            .ok_or_else(|| anyhow!("artist profile selection must be a profile id"))?
            .trim()
            .to_string();
        validate_profile_id(&profile_id)?;
        if !ids.contains(&profile_id) {
            bail!("artist profile selection references an unknown profile");
        }
        artist_profile_ids.insert(artist_id_num.to_string(), Value::String(profile_id));
    }

    Ok(json!({
        "version": SETTINGS_VERSION,
        // default_profile_id remains as a compatibility alias for older callers.
        "active_profile_id": active_profile_id,
        "default_profile_id": active_profile_id,
        "profiles": profiles,
        "artist_profile_ids": artist_profile_ids,
    }))
}

pub(crate) fn normalize_profile(input: &Value) -> Result<Value> {
    let object = input
        .as_object()
        .ok_or_else(|| anyhow!("archive format profile must be an object"))?;
    let id = required_string(object, "id", 64)?;
    validate_profile_id(&id)?;
    let name = required_string(object, "name", MAX_PROFILE_NAME_LEN)?;
    let template = required_string(object, "template", MAX_TEMPLATE_LEN)?;
    validate_template(&template)?;

    let empty_value = optional_string(object, "empty_value", "untitled", 80)?;
    let empty_value = sanitize_token_value(&empty_value, "untitled");
    if empty_value.is_empty() || empty_value == "." || empty_value == ".." {
        bail!("empty_value cannot be empty or a traversal segment");
    }
    let tag_separator = optional_string(object, "tag_separator", "&", 32)?;
    validate_tag_separator(&tag_separator)?;
    let date_format = optional_string(object, "date_format", "iso", 32)?;
    if !matches!(date_format.as_str(), "iso" | "compact" | "year_month") {
        bail!("date_format must be iso, compact, or year_month");
    }
    let collision_strategy = match object
        .get("collision_strategy")
        .or_else(|| object.get("conflict_strategy"))
        .or_else(|| object.get("collision"))
    {
        Some(value) => {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow!("collision_strategy must be a string"))?
                .trim()
                .to_string();
            if value.chars().count() > 32 || value.chars().any(char::is_control) {
                bail!("collision_strategy is invalid");
            }
            value
        }
        None => "suffix".to_string(),
    };
    if !matches!(collision_strategy.as_str(), "reject" | "suffix") {
        bail!("collision_strategy must be reject or suffix");
    }

    Ok(json!({
        "id": id,
        "name": name,
        "template": template,
        "empty_value": empty_value,
        "tag_separator": tag_separator,
        "date_format": date_format,
        "collision_strategy": collision_strategy,
    }))
}

fn required_string(object: &Map<String, Value>, key: &str, max_len: usize) -> Result<String> {
    let value = object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow!("{key} must be a string"))?
        .trim()
        .to_string();
    if value.is_empty() {
        bail!("{key} cannot be empty");
    }
    if value.chars().count() > max_len {
        bail!("{key} is too long");
    }
    if value.chars().any(char::is_control) {
        bail!("{key} cannot contain control characters");
    }
    Ok(value)
}

fn optional_string(
    object: &Map<String, Value>,
    key: &str,
    default: &str,
    max_len: usize,
) -> Result<String> {
    match object.get(key) {
        Some(value) => {
            let value = value
                .as_str()
                .ok_or_else(|| anyhow!("{key} must be a string"))?
                .trim()
                .to_string();
            if value.chars().count() > max_len {
                bail!("{key} is too long");
            }
            if value.chars().any(char::is_control) {
                bail!("{key} cannot contain control characters");
            }
            Ok(value)
        }
        None => Ok(default.to_string()),
    }
}

fn validate_profile_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 64
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        bail!("profile id must use ASCII letters, numbers, underscore, or hyphen");
    }
    Ok(())
}

fn validate_tag_separator(value: &str) -> Result<()> {
    if value.chars().any(|ch| {
        ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*')
    }) {
        bail!("tag_separator contains unsafe filename characters");
    }
    Ok(())
}

pub(crate) fn profile_for_artist(settings: &Value, artist_id: i64) -> Result<(Value, String)> {
    let settings = normalize_settings(settings)?;
    let artist_key = artist_id.to_string();
    let (profile_id, source) = settings["artist_profile_ids"]
        .get(&artist_key)
        .and_then(Value::as_str)
        .map(|id| (id, "artist"))
        .unwrap_or_else(|| {
            (
                settings["active_profile_id"]
                    .as_str()
                    .unwrap_or(DEFAULT_PROFILE_ID),
                "global",
            )
        });
    let profile = settings["profiles"]
        .as_array()
        .and_then(|profiles| {
            profiles
                .iter()
                .find(|profile| profile["id"].as_str() == Some(profile_id))
        })
        .cloned()
        .ok_or_else(|| anyhow!("archive format profile is not configured"))?;
    Ok((profile, source.to_string()))
}

pub(crate) fn profile_by_id(settings: &Value, profile_id: &str) -> Result<Value> {
    let settings = normalize_settings(settings)?;
    let profile_id = profile_id.trim();
    validate_profile_id(profile_id)?;
    settings["profiles"]
        .as_array()
        .and_then(|profiles| {
            profiles
                .iter()
                .find(|profile| profile["id"].as_str() == Some(profile_id))
        })
        .cloned()
        .ok_or_else(|| anyhow!("archive format profile not found: {profile_id}"))
}

pub(crate) fn settings_response(settings: &Value, artist_id: Option<i64>) -> Result<Value> {
    let settings = normalize_settings(settings)?;
    let (effective_profile, profile_source) = match artist_id {
        Some(artist_id) => profile_for_artist(&settings, artist_id)?,
        None => {
            let profile = profile_by_id(
                &settings,
                settings["active_profile_id"]
                    .as_str()
                    .unwrap_or(DEFAULT_PROFILE_ID),
            )?;
            (profile, "global".to_string())
        }
    };
    Ok(json!({
        "ok": true,
        "settings": settings,
        "artist_id": artist_id,
        "effective_profile_id": effective_profile["id"],
        "effective_profile": effective_profile,
        "profile_source": profile_source,
        "tokens": TOKENS,
    }))
}

/// Parse a template and reject unknown/unmatched tokens before any plan data is
/// read.  The placeholder rendering also catches literal path traversal.
pub(crate) fn validate_template(template: &str) -> Result<()> {
    let parts = parse_template(template)?;
    let sample = parts
        .iter()
        .map(|part| match part {
            TemplatePart::Text(value) => value.clone(),
            TemplatePart::Token(_) => "sample".to_string(),
        })
        .collect::<String>();
    validate_rendered_target(&sample).context("unsafe archive format template")?;
    Ok(())
}

fn parse_template(template: &str) -> Result<Vec<TemplatePart>> {
    if template.trim().is_empty() {
        bail!("template cannot be empty");
    }
    if template.chars().count() > MAX_TEMPLATE_LEN {
        bail!("template is too long");
    }
    if template.chars().any(char::is_control) {
        bail!("template cannot contain control characters");
    }

    let mut parts = Vec::new();
    let mut text_start = 0usize;
    let mut offset = 0usize;
    while offset < template.len() {
        let rest = &template[offset..];
        let ch = rest
            .chars()
            .next()
            .ok_or_else(|| anyhow!("template parse failed"))?;
        match ch {
            '{' => {
                if text_start < offset {
                    parts.push(TemplatePart::Text(template[text_start..offset].to_string()));
                }
                let token_start = offset + ch.len_utf8();
                let close_relative = template[token_start..]
                    .find('}')
                    .ok_or_else(|| anyhow!("unmatched '{{' in archive format template"))?;
                let close = token_start + close_relative;
                let token = &template[token_start..close];
                if token.is_empty() || token.contains('{') || !TOKENS.contains(&token) {
                    bail!("unknown archive format token: {{{token}}}");
                }
                parts.push(TemplatePart::Token(token.to_string()));
                offset = close + '}'.len_utf8();
                text_start = offset;
            }
            '}' => bail!("unmatched '}}' in archive format template"),
            _ => offset += ch.len_utf8(),
        }
    }
    if text_start < template.len() {
        parts.push(TemplatePart::Text(template[text_start..].to_string()));
    }
    Ok(parts)
}

pub(crate) fn render_profile(profile: &Value, context: &RenderContext) -> Result<RenderedTarget> {
    let profile = normalize_profile(profile)?;
    let template = profile["template"]
        .as_str()
        .ok_or_else(|| anyhow!("profile template is missing"))?;
    let parts = parse_template(template)?;
    let empty_value = profile["empty_value"].as_str().unwrap_or("untitled");
    let tag_separator = profile["tag_separator"].as_str().unwrap_or("&");
    let (date, year, month) = formatted_date(
        &context.date,
        profile["date_format"].as_str().unwrap_or("iso"),
        empty_value,
    );
    let artist = sanitize_token_value(&context.artist, empty_value);
    let title = sanitize_token_value(&context.title, empty_value);
    let folder = sanitize_token_value(&context.folder, empty_value);
    let rendered_tags = context
        .tags
        .iter()
        .map(|tag| sanitize_token_value(tag, ""))
        .filter(|tag| !tag.is_empty())
        .collect::<Vec<_>>();
    let tags = if rendered_tags.is_empty() {
        empty_value.to_string()
    } else {
        sanitize_token_value(&rendered_tags.join(tag_separator), empty_value)
    };
    let index = context.index.max(1).to_string();

    let render_token = |name: &str| match name {
        "artist" => artist.as_str(),
        "date" => date.as_str(),
        "year" => year.as_str(),
        "month" => month.as_str(),
        "tags" => tags.as_str(),
        "title" => title.as_str(),
        "folder" => folder.as_str(),
        "index" => index.as_str(),
        _ => "",
    };
    let rendered = parts
        .iter()
        .map(|part| match part {
            TemplatePart::Text(value) => value.clone(),
            TemplatePart::Token(name) => render_token(name).to_string(),
        })
        .collect::<String>();
    let target_folder = validate_rendered_target(&rendered)?;
    Ok(RenderedTarget {
        target_folder,
        tokens: json!({
            "artist": artist,
            "date": date,
            "year": year,
            "month": month,
            "tags": tags,
            "tag_values": rendered_tags,
            "title": title,
            "folder": folder,
            "index": index,
        }),
    })
}

fn formatted_date(raw: &str, format: &str, empty_value: &str) -> (String, String, String) {
    let parsed = parse_date(raw)
        .map(|date| (date, true))
        .or_else(|| parse_year_month(raw).map(|date| (date, false)));
    let Some((date, has_day)) = parsed else {
        return (
            empty_value.to_string(),
            empty_value.to_string(),
            empty_value.to_string(),
        );
    };
    let year = format!("{:04}", date.year());
    let month = format!("{:02}", date.month());
    let date = match (format, has_day) {
        ("compact", true) => date.format("%Y%m%d").to_string(),
        ("compact", false) => date.format("%Y%m").to_string(),
        ("year_month", _) | (_, false) => date.format("%Y-%m").to_string(),
        _ => date.format("%Y-%m-%d").to_string(),
    };
    (date, year, month)
}

fn parse_date(raw: &str) -> Option<NaiveDate> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    for candidate in [raw, raw.get(..10).unwrap_or(raw)] {
        for format in ["%Y-%m-%d", "%Y/%m/%d", "%Y.%m.%d", "%Y%m%d"] {
            if let Ok(date) = NaiveDate::parse_from_str(candidate, format) {
                return Some(date);
            }
        }
    }
    let digits = raw.chars().filter(char::is_ascii_digit).collect::<String>();
    (digits.len() >= 8)
        .then(|| NaiveDate::parse_from_str(&digits[..8], "%Y%m%d").ok())
        .flatten()
}

fn parse_year_month(raw: &str) -> Option<NaiveDate> {
    let digits = raw
        .trim()
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() != 6 {
        return None;
    }
    NaiveDate::parse_from_str(&format!("{digits}01"), "%Y%m%d").ok()
}

/// Validate a rendered target as a portable artist-relative folder path.
pub(crate) fn validate_rendered_target(value: &str) -> Result<String> {
    let raw = value.replace('\\', "/").trim().to_string();
    if raw.is_empty() || raw.starts_with('/') || raw.starts_with("//") {
        bail!("rendered archive target must be an artist-relative folder");
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        bail!("rendered archive target must not use a drive path");
    }

    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() {
            bail!("rendered archive target cannot contain empty path segments");
        }
        if component == "." || component == ".." {
            bail!("rendered archive target cannot contain traversal segments");
        }
        if component.chars().count() > 180 {
            bail!("rendered archive target has an overlong path segment");
        }
        if component.ends_with(' ') || component.ends_with('.') {
            bail!("rendered archive target has an unsafe trailing path character");
        }
        if component
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            bail!("rendered archive target contains unsafe filename characters");
        }
        if is_windows_reserved_component(component) {
            bail!("rendered archive target contains a reserved filename");
        }
        components.push(component);
    }
    if components.is_empty() {
        bail!("rendered archive target must not be empty");
    }
    Ok(components.join("/"))
}

fn is_windows_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .and_then(|number| number.parse::<u8>().ok())
            .map(|number| (1..=9).contains(&number))
            .unwrap_or(false)
}

fn sanitize_token_value(value: &str, fallback: &str) -> String {
    let mut out = String::with_capacity(value.len().min(MAX_TOKEN_VALUE_LEN));
    for ch in value.trim().chars() {
        if out.chars().count() >= MAX_TOKEN_VALUE_LEN {
            break;
        }
        if ch.is_control() || matches!(ch, '/' | '\\' | '<' | '>' | ':' | '"' | '|' | '?' | '*') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    let out = out
        .trim_matches(|ch: char| ch == ' ' || ch == '.')
        .to_string();
    if out.is_empty() || out == "." || out == ".." || is_windows_reserved_component(&out) {
        if fallback.is_empty() {
            return String::new();
        }
        return sanitize_token_value(fallback, "untitled");
    }
    out
}

pub(crate) fn rule_snapshot(profile: &Value, profile_source: &str) -> Value {
    json!({
        "version": SETTINGS_VERSION,
        "profile_id": profile["id"],
        "profile_name": profile["name"],
        "profile_source": profile_source,
        "profile": profile,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_all_tokens_with_safe_values() {
        let profile = json!({
            "id": "custom",
            "name": "Custom",
            "template": "{artist}/{year}/{month}/{date} - {title} - {tags} - {folder} - {index}",
            "tag_separator": "+",
            "date_format": "iso",
            "conflict_strategy": "reject",
        });
        let rendered = render_profile(
            &profile,
            &RenderContext {
                artist: "A/rtist".into(),
                date: "2024.03.04".into(),
                tags: vec!["one".into(), "two/two".into()],
                title: "..".into(),
                folder: "source/name".into(),
                index: 3,
            },
        )
        .unwrap();
        assert_eq!(
            rendered.target_folder,
            "A_rtist/2024/03/2024-03-04 - untitled - one+two_two - source_name - 3"
        );
        assert_eq!(rendered.tokens["year"], "2024");
        assert_eq!(rendered.tokens["tag_values"], json!(["one", "two_two"]));
    }

    #[test]
    fn renders_year_month_without_inventing_a_day() {
        let rendered = render_profile(
            &json!({
                "id": "month",
                "name": "Month",
                "template": "{year}/{date} {tags}",
            }),
            &RenderContext {
                artist: "Artist".into(),
                date: "2024-03".into(),
                tags: vec!["one".into(), "two".into()],
                title: "Title".into(),
                folder: "Folder".into(),
                index: 1,
            },
        )
        .unwrap();

        assert_eq!(rendered.target_folder, "2024/2024-03 one&two");
        assert_eq!(rendered.tokens["month"], "03");
    }

    #[test]
    fn rejects_unknown_unmatched_and_traversal_templates() {
        for template in [
            "{unknown}",
            "{title",
            "title}",
            "../{title}",
            "{title}/../x",
        ] {
            assert!(validate_template(template).is_err(), "{template}");
        }
        assert!(validate_rendered_target("x/../y").is_err());
        assert!(validate_rendered_target("C:/x").is_err());
    }

    #[test]
    fn settings_select_artist_profile_and_validate_references() {
        let settings = normalize_settings(&json!({
            "default_profile_id": "global",
            "profiles": [
                {"id":"global","name":"Global","template":"{title}"},
                {"id":"artist","name":"Artist","template":"{folder}/{index}","conflict_strategy":"suffix"}
            ],
            "artist_profile_ids": {"42":"artist"}
        }))
        .unwrap();
        let (profile, source) = profile_for_artist(&settings, 42).unwrap();
        assert_eq!(profile["id"], "artist");
        assert_eq!(source, "artist");
        assert!(normalize_settings(&json!({
            "profiles": [{"id":"only","name":"Only","template":"{title}"}],
            "default_profile_id":"missing"
        }))
        .is_err());
    }
}
