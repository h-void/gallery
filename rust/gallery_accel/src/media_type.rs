//! Media type classification (mirrors `app/role_extractor.py`).

use regex::Regex;
use std::sync::OnceLock;

const IMAGE: &[&str] = &[
    "png", "jpg", "jpeg", "jpe", "jfif", "gif", "webp", "bmp", "tiff", "tif", "avif", "heic",
    "heif",
];
const VIDEO: &[&str] = &[
    "mp4", "mkv", "mov", "webm", "avi", "wmv", "m4v", "mpg", "mpeg", "ts", "m2ts", "flv", "3gp",
];
const SOURCE: &[&str] = &["psd", "psb", "clip", "tga", "dds"];
const ARCHIVE: &[&str] = &["rar", "zip", "7z", "tar", "gz", "bz2", "xz"];
const TEXT: &[&str] = &["txt", "md", "html", "htm"];

pub fn media_type_for_file(filename: &str) -> Option<&'static str> {
    let ext = filename
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase();
    if IMAGE.contains(&ext.as_str()) {
        Some("image")
    } else if VIDEO.contains(&ext.as_str()) {
        Some("video")
    } else if SOURCE.contains(&ext.as_str()) {
        Some("source")
    } else if ARCHIVE.contains(&ext.as_str()) {
        Some("archive")
    } else if TEXT.contains(&ext.as_str()) {
        Some("text")
    } else {
        None
    }
}

/// A recognized date that preserves its original precision.
///
/// `raw` is the precision-preserving form (`YYYY-MM` for month-only sources,
/// `YYYY-MM-DD` for full dates); `canonical` is always `YYYY-MM-DD`, using
/// day `01` only when the raw value is month-only.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateValue {
    pub raw: String,
    pub canonical: String,
}

impl DateValue {
    /// The `YYYY-MM` month key used for folder-plan target derivation.
    pub fn month_key(&self) -> String {
        self.raw.get(..7).unwrap_or_default().to_string()
    }
}

fn days_in_month(year: u32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
            if leap {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn normalized_full(year: u32, month: u32, day: u32) -> Option<String> {
    if (1..=12).contains(&month) && day >= 1 && day <= days_in_month(year, month) {
        Some(format!("{year:04}-{month:02}-{day:02}"))
    } else {
        None
    }
}

fn normalized_month(year: u32, month: u32) -> Option<String> {
    if (1..=12).contains(&month) {
        Some(format!("{year:04}-{month:02}"))
    } else {
        None
    }
}

/// Extract a precision-preserving date from a folder name.
///
/// Full dates (`YYYY-MM-DD` and `YYYYMMDD`) produce a raw full date; compact
/// adjacent `YYYYMM/DD_...` parent/child folders produce a raw full date; month
/// forms (`YYYYMM`, `YYYY-MM`, `YYYY/MM`, `YYYY.MM`) produce a raw `YYYY-MM`
/// with a canonical `YYYY-MM-01`. Full-date matching comes first. Impossible
/// calendar dates are rejected.
pub fn extract_date_value_from_folder(folder: &str) -> Option<DateValue> {
    static FULL: OnceLock<Regex> = OnceLock::new();
    static FULL2: OnceLock<Regex> = OnceLock::new();
    static COMPACT: OnceLock<Regex> = OnceLock::new();
    static YEAR_MONTH: OnceLock<Regex> = OnceLock::new();
    let full = FULL.get_or_init(|| {
        Regex::new(
            r"(?:^|\D)(?P<y>20\d{2})[-._](?P<m>1[0-2]|0?[1-9])[-._](?P<d>3[01]|[12]\d|0?[1-9])(?:\D|$)",
        )
        .unwrap()
    });
    let full2 = FULL2.get_or_init(|| {
        Regex::new(r"(?:^|\D)(?P<y>20\d{2})(?P<m>0[1-9]|1[0-2])(?P<d>[0-2]\d|3[01])(?:\D|$)")
            .unwrap()
    });
    let compact = COMPACT.get_or_init(|| {
        Regex::new(
            r"(?:^|[/\\])(?P<y>20\d{2})(?P<m>0[1-9]|1[0-2])[/\\](?P<d>0[1-9]|[12]\d|3[01])(?:[-._\s/\\]|$)",
        )
        .unwrap()
    });
    let year_month = YEAR_MONTH.get_or_init(|| {
        Regex::new(r"(?:^|\D)(?P<y>20\d{2})(?P<s>[-/_.])?(?P<m>0?[1-9]|1[0-2])(?:\D|$)").unwrap()
    });
    if let Some(c) = full.captures(folder) {
        let y = c["y"].parse::<u32>().ok()?;
        let m = c["m"].parse::<u32>().ok()?;
        let d = c["d"].parse::<u32>().ok()?;
        let raw = normalized_full(y, m, d)?;
        return Some(DateValue {
            canonical: raw.clone(),
            raw,
        });
    }
    if let Some(c) = full2.captures(folder) {
        let y = c["y"].parse::<u32>().ok()?;
        let m = c["m"].parse::<u32>().ok()?;
        let d = c["d"].parse::<u32>().ok()?;
        let raw = normalized_full(y, m, d)?;
        return Some(DateValue {
            canonical: raw.clone(),
            raw,
        });
    }
    if let Some(c) = compact.captures(folder) {
        if let (Ok(y), Ok(m), Ok(d)) = (
            c["y"].parse::<u32>(),
            c["m"].parse::<u32>(),
            c["d"].parse::<u32>(),
        ) {
            if let Some(raw) = normalized_full(y, m, d) {
                return Some(DateValue {
                    canonical: raw.clone(),
                    raw,
                });
            }
        }
    }
    if let Some(c) = year_month.captures(folder) {
        let matched = c.get(0).unwrap();
        let last_char = matched.as_str().chars().last().unwrap_or(' ');
        let is_path_boundary = last_char == '/' || last_char == '\\';
        let next = folder
            .get(matched.end()..)
            .and_then(|suffix| suffix.chars().next())
            .unwrap_or(' ');
        if !is_path_boundary && next.is_ascii_digit() {
            return None;
        }
        let y = c["y"].parse::<u32>().ok()?;
        let m = c["m"].parse::<u32>().ok()?;
        let raw = normalized_month(y, m)?;
        return Some(DateValue {
            canonical: format!("{raw}-01"),
            raw,
        });
    }
    None
}

/// Canonical `YYYY-MM-DD` form of a folder-recognized date (day 01 for
/// month-only sources), or empty when no valid date is recognized.
pub fn extract_date_from_folder(folder: &str) -> String {
    extract_date_value_from_folder(folder)
        .map(|value| value.canonical)
        .unwrap_or_default()
}

/// Parse and normalize a user-supplied date override.
///
/// Accepts only real calendar `YYYY-MM` or `YYYY-MM-DD` (dash-separated),
/// normalizes zero padding, and rejects impossible dates. Returns the
/// precision-preserving raw form, or `None` for any invalid input. This is
/// the single server-side source of truth for manual date input.
pub fn parse_manual_date_input(input: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(?P<y>20\d{2})-(?P<m>1[0-2]|0?[1-9])(?:-(?P<d>3[01]|[12]\d|0?[1-9]))?$")
            .unwrap()
    });
    let captures = re.captures(input.trim())?;
    let y = captures["y"].parse::<u32>().ok()?;
    let m = captures["m"].parse::<u32>().ok()?;
    match captures.name("d") {
        Some(d) => {
            let day = d.as_str().parse::<u32>().ok()?;
            normalized_full(y, m, day)
        }
        None => normalized_month(y, m),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_common_extensions() {
        assert_eq!(media_type_for_file("a.JPG"), Some("image"));
        assert_eq!(media_type_for_file("v.mp4"), Some("video"));
        assert_eq!(media_type_for_file("x.zip"), Some("archive"));
        assert_eq!(media_type_for_file("n.exe"), None);
    }

    #[test]
    fn extracts_dates() {
        assert_eq!(extract_date_from_folder("2026-05-01_title"), "2026-05-01");
        assert_eq!(
            extract_date_from_folder("2026/2026-02-28 tag"),
            "2026-02-28"
        );
        assert_eq!(extract_date_from_folder("20260515_x"), "2026-05-15");
        assert_eq!(extract_date_from_folder("x2026-02-280"), "");
        assert_eq!(extract_date_from_folder("2026/06"), "2026-06-01");
        assert_eq!(extract_date_from_folder("202606"), "2026-06-01");
        assert_eq!(extract_date_from_folder("2026-06"), "2026-06-01");
        assert_eq!(extract_date_from_folder("2026.06"), "2026-06-01");
        assert_eq!(extract_date_from_folder("2026.00"), "");
        assert_eq!(extract_date_from_folder("2026.13"), "");
        assert_eq!(extract_date_from_folder("2026.061"), "");
        assert_eq!(extract_date_from_folder("画师/202608_1"), "");
        assert_eq!(extract_date_from_folder("2026-02-31"), "");
        assert_eq!(extract_date_from_folder("2023-02-29"), "");
        assert_eq!(extract_date_from_folder("2024-02-29"), "2024-02-29");
        assert_eq!(
            extract_date_from_folder("202508/01_1536_title"),
            "2025-08-01"
        );
        assert_eq!(
            extract_date_from_folder("202508/01_1539_title"),
            "2025-08-01"
        );
        assert_eq!(
            extract_date_from_folder("202508/011536_title"),
            "2025-08-01"
        );
        assert_eq!(extract_date_from_folder("202502/29_title"), "2025-02-01");
        assert_eq!(extract_date_from_folder("202402/29_title"), "2024-02-29");
    }

    #[test]
    fn preserves_precision() {
        let month = extract_date_value_from_folder("202607").unwrap();
        assert_eq!(month.raw, "2026-07");
        assert_eq!(month.canonical, "2026-07-01");
        assert_eq!(month.month_key(), "2026-07");
        for source in ["2026-07", "2026/07", "2026.07"] {
            let value = extract_date_value_from_folder(source).unwrap();
            assert_eq!(value.raw, "2026-07");
            assert_eq!(value.canonical, "2026-07-01");
        }
        for source in ["2026-05-01_title", "20260515_x", "2026/2026-02-28 tag"] {
            let value = extract_date_value_from_folder(source).unwrap();
            assert_eq!(value.raw, value.canonical);
            assert_eq!(value.raw.len(), 10);
        }
        for source in [
            "202508/01_1536_title",
            "202508/01_1539_title",
            "202402/29_leap",
            "202508/01-hyphen",
            "202508/01.dot",
            "202508/01 space",
            "202508/01",
        ] {
            let value = extract_date_value_from_folder(source).unwrap();
            assert_eq!(value.raw.len(), 10, "{source} should have full precision");
            assert_eq!(value.raw, value.canonical);
        }
        let compact_invalid_leap = extract_date_value_from_folder("202502/29_invalid").unwrap();
        assert_eq!(compact_invalid_leap.raw, "2025-02");
        assert_eq!(compact_invalid_leap.canonical, "2025-02-01");
        let compact_non_day = extract_date_value_from_folder("202508/011536_title").unwrap();
        assert_eq!(compact_non_day.raw, "2025-08");
        assert_eq!(compact_non_day.canonical, "2025-08-01");
        assert_eq!(extract_date_value_from_folder("x2026-02-280"), None);
        assert_eq!(extract_date_value_from_folder("2026.061"), None);
        assert_eq!(extract_date_value_from_folder("2026-02-31"), None);
        assert_eq!(extract_date_value_from_folder("2026.13"), None);
    }

    #[test]
    fn parses_manual_date_input() {
        assert_eq!(
            parse_manual_date_input("2026-07"),
            Some("2026-07".to_string())
        );
        assert_eq!(
            parse_manual_date_input("2026-7"),
            Some("2026-07".to_string())
        );
        assert_eq!(
            parse_manual_date_input("2026-7-1"),
            Some("2026-07-01".to_string())
        );
        assert_eq!(
            parse_manual_date_input("2026-05-15"),
            Some("2026-05-15".to_string())
        );
        assert_eq!(
            parse_manual_date_input(" 2026-05-15 "),
            Some("2026-05-15".to_string())
        );
        assert_eq!(parse_manual_date_input("2026-13"), None);
        assert_eq!(parse_manual_date_input("2026-02-31"), None);
        assert_eq!(parse_manual_date_input("2023-02-29"), None);
        assert_eq!(
            parse_manual_date_input("2024-02-29"),
            Some("2024-02-29".to_string())
        );
        assert_eq!(parse_manual_date_input("202607"), None);
        assert_eq!(parse_manual_date_input("2026/07"), None);
        assert_eq!(parse_manual_date_input("2026-07-"), None);
        assert_eq!(parse_manual_date_input(""), None);
        assert_eq!(parse_manual_date_input("not-a-date"), None);
    }
}
