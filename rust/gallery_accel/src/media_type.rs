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

pub fn extract_date_from_folder(folder: &str) -> String {
    static FULL: OnceLock<Regex> = OnceLock::new();
    static FULL2: OnceLock<Regex> = OnceLock::new();
    let full = FULL.get_or_init(|| {
        Regex::new(r"(?P<y>20\d{2})[-._](?P<m>0?[1-9]|1[0-2])[-._](?P<d>0?[1-9]|[12]\d|3[01])")
            .unwrap()
    });
    let full2 = FULL2
        .get_or_init(|| Regex::new(r"(?P<y>20\d{2})(?P<m>0[1-9]|1[0-2])(?P<d>[0-2]\d|3[01])").unwrap());
    if let Some(c) = full.captures(folder) {
        return format!(
            "{}-{:02}-{:02}",
            &c["y"],
            c["m"].parse::<u32>().unwrap_or(1),
            c["d"].parse::<u32>().unwrap_or(1)
        );
    }
    if let Some(c) = full2.captures(folder) {
        return format!(
            "{}-{:02}-{:02}",
            &c["y"],
            c["m"].parse::<u32>().unwrap_or(1),
            c["d"].parse::<u32>().unwrap_or(1)
        );
    }
    String::new()
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
        assert_eq!(extract_date_from_folder("20260515_x"), "2026-05-15");
    }
}
