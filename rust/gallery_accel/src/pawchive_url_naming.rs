//! Pawchive creator/file URL normalization, redirect SSRF policy, and post naming templates.

use super::*;

/// Normalize creator or post URLs (e.g. from `.st` alias or full web address)
/// to `(service, user_id)`.
pub fn parse_creator_url(input: &str) -> Result<(String, String)> {
    let trimmed = input.trim();
    let url_str = if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{}", trimmed)
    };

    let parsed = url::Url::parse(&url_str).context("invalid creator url format")?;
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    if !host.contains("pawchive") && !host.contains("kemono") && !host.contains("coomer") {
        return Err(anyhow!("unrecognized archive domain: {host}"));
    }

    let segments: Vec<&str> = parsed
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();

    // Expected paths: /{service}/user/{userId} or /user/{userId} with service query
    if segments.len() >= 3 && segments[1] == "user" {
        let service = segments[0].to_lowercase();
        let user_id = segments[2].to_string();
        return Ok((service, user_id));
    }

    Err(anyhow!(
        "expected URL pattern /{{service}}/user/{{userId}}, got: {url_str}"
    ))
}

/// Build official API profile URL.
pub fn build_profile_url(service: &str, user_id: &str) -> String {
    format!("{PAWCHIVE_API_BASE}/{service}/user/{user_id}/profile")
}

/// Build official API posts pagination URL.
pub fn build_posts_url(service: &str, user_id: &str, offset: u64) -> String {
    format!("{PAWCHIVE_API_BASE}/{service}/user/{user_id}?o={offset}")
}

/// The remote path as a resource key: without the URL decoration that names the
/// same bytes.
///
/// A source may attach a download name or a fragment (`.../a.png?f=a.png`,
/// `...#page=1`). Those are how the file is *served*, not which file it is: two
/// reads that differ only in that decoration are the same resource, and keying
/// them apart would insert a second row for a file already in the ledger. The
/// path itself is otherwise left exactly as the source wrote it — a
/// normalization that rewrote real path segments would break the identity of
/// every resource already recorded.
pub fn normalize_remote_path(path: &str) -> String {
    let trimmed = path.trim();
    let without_fragment = trimmed.split('#').next().unwrap_or(trimmed);
    let without_query = without_fragment
        .split('?')
        .next()
        .unwrap_or(without_fragment);
    without_query.to_string()
}

/// Build official file download URL.
pub fn build_file_url(path: &str, filename: Option<&str>) -> String {
    let clean_path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    if let Some(name) = filename {
        let encoded_name =
            url::form_urlencoded::byte_serialize(name.as_bytes()).collect::<String>();
        format!("{PAWCHIVE_FILE_BASE}{clean_path}?f={encoded_name}")
    } else {
        format!("{PAWCHIVE_FILE_BASE}{clean_path}")
    }
}

/// The hosts an outbound Pawchive request may end up talking to.
///
/// The initial URL is always built from the two fixed constants above, so a
/// redirect is the only way a request can reach a host nobody chose. Following
/// one is how a fixed base URL stops being a boundary: the server being talked
/// to is then whatever the previous hop named, including a private address the
/// host can reach and this install cannot check. Every hop is therefore held to
/// the same two hosts, and a redirect anywhere else fails the request instead of
/// quietly reading another origin.
const PAWCHIVE_ALLOWED_HOSTS: [&str; 2] = ["pawchive.pw", "file.pawchive.pw"];

/// Whether an IP address is a private, loopback, or link-local restricted target.
pub(crate) fn is_ip_private_or_restricted(ip: std::net::IpAddr) -> bool {
    match ip {
        std::net::IpAddr::V4(ipv4) => {
            ipv4.is_loopback()
                || ipv4.is_private()
                || ipv4.is_link_local()
                || ipv4.is_broadcast()
                || ipv4.is_unspecified()
                || ipv4.octets()[0] == 0
        }
        std::net::IpAddr::V6(ipv6) => {
            ipv6.is_loopback()
                || ipv6.is_unspecified()
                || (ipv6.segments()[0] & 0xffc0) == 0xfe80
                || (ipv6.segments()[0] & 0xfe00) == 0xfc00
        }
    }
}

/// Validate whether an outbound redirect URL satisfies security boundaries:
/// - Maximum of 5 redirect hops
/// - HTTPS scheme only (no downgrade to HTTP)
/// - Standard HTTPS port (443) only
/// - Allowed Pawchive host/subdomain only (no external domains or IP literal targets)
/// - Host must not resolve to private, loopback, or link-local IP addresses
pub type DnsResolverFn = dyn Fn(&str) -> Vec<std::net::IpAddr>;
pub type ArcDnsResolver = std::sync::Arc<dyn Fn(&str) -> Vec<std::net::IpAddr> + Send + Sync>;

pub fn validate_redirect_url(
    url: &url::Url,
    hop_count: usize,
    dns_checker: Option<&DnsResolverFn>,
) -> Result<(), String> {
    if hop_count >= 5 {
        return Err(format!(
            "too many redirects: exceeded maximum hop limit of 5 ({url})"
        ));
    }
    if url.scheme() != "https" {
        return Err(format!(
            "redirect to non-HTTPS scheme '{}' is refused: {url}",
            url.scheme()
        ));
    }
    if url.port_or_known_default() != Some(443) {
        return Err(format!(
            "redirect to non-standard port {:?} is refused: {url}",
            url.port()
        ));
    }
    let host = match url.host_str() {
        Some(h) => h.to_ascii_lowercase(),
        None => return Err(format!("redirect URL missing host: {url}")),
    };
    if host.parse::<std::net::IpAddr>().is_ok() {
        return Err(format!("redirect to IP literal host is refused: {url}"));
    }
    let allowed = PAWCHIVE_ALLOWED_HOSTS
        .iter()
        .any(|allowed| host == *allowed || host.ends_with(&format!(".{allowed}")));
    if !allowed {
        return Err(format!("redirect to an unlisted host is refused: {url}"));
    }

    if let Some(checker) = dns_checker {
        for ip in checker(&host) {
            if is_ip_private_or_restricted(ip) {
                return Err(format!(
                    "redirect target host '{host}' resolves to restricted IP {ip}"
                ));
            }
        }
    }
    Ok(())
}

/// Whether a request that has already produced a response may follow it to `url`.
pub fn redirected_host_is_allowed(url: &url::Url) -> bool {
    validate_redirect_url(url, 0, None).is_ok()
}

/// The redirect policy both Pawchive clients use: the same fixed-host rule for
/// every hop, HTTPS-only, port 443 only, max 5 hops, and no private IP targets.
pub fn pawchive_redirect_policy() -> reqwest::redirect::Policy {
    pawchive_redirect_policy_with_resolver(None)
}

/// Redirect policy allowing an optional custom DNS resolver hook for testing.
pub fn pawchive_redirect_policy_with_resolver(
    custom_dns: Option<ArcDnsResolver>,
) -> reqwest::redirect::Policy {
    reqwest::redirect::Policy::custom(move |attempt| {
        let hop_count = attempt.previous().len();
        let checker = custom_dns
            .as_ref()
            .map(|f| f.as_ref() as &dyn Fn(&str) -> Vec<std::net::IpAddr>);
        match validate_redirect_url(attempt.url(), hop_count, checker) {
            Ok(()) => attempt.follow(),
            Err(message) => attempt.error(anyhow!(message)),
        }
    })
}

/// Extract external netdisk and cloud drive links from post body text/HTML.
///
/// Adapted from user requirement analysis (identifying Google Drive, Mega,
/// Baidu, etc.) and implemented with robust multi-provider URL regex extraction.
///
/// A link the publisher split across markup (`<a href="URL">URL</a>#KEY`) is
/// returned whole: the fragment that the anchor does not carry is the only part
/// that makes a MEGA URL usable, so dropping it would record a link nobody can
/// download from.
pub fn extract_external_links(content: &str) -> Vec<String> {
    static URL_RE: OnceLock<Regex> = OnceLock::new();
    let re = URL_RE
        .get_or_init(|| Regex::new(r#"https?://[^\s"'<>\(\)\[\]{}|\\^`]+"#).expect("valid regex"));

    let known_netdisk_domains = [
        "drive.google.com",
        "mega.nz",
        "pan.baidu.com",
        "dropbox.com",
        "mediafire.com",
        "1drv.ms",
        "onedrive.live.com",
        "gofile.io",
        "pixeldrain.com",
        "terabox.com",
        "qiwi.gg",
        "catbox.moe",
    ];

    let mut found = Vec::new();
    let mut seen = HashSet::new();

    for mat in re.find_iter(content) {
        let raw_url = mat
            .as_str()
            .trim_end_matches(&['.', ',', ';', ':', '!', '?'][..])
            .to_string();
        // Publishers write a MEGA link as `<a href="URL">URL</a>#KEY`, leaving the
        // key outside the anchor: the URL regex stops at the quote, and the tail
        // after `</a>` carries no scheme of its own, so the recognized link would
        // be the one form of a MEGA URL that cannot be downloaded. Reattach the
        // tail and drop the anchor's now-incomplete copy.
        let mut candidate = raw_url.clone();
        if let Some(suffix) = fragment_split_after_closing_tags(content, mat.end()) {
            let reattached = format!("{raw_url}{suffix}");
            if url::Url::parse(&reattached).is_ok() {
                seen.remove(&raw_url);
                found.retain(|existing| existing != &raw_url);
                candidate = reattached;
            }
        }
        if let Ok(parsed) = url::Url::parse(&candidate) {
            if let Some(host) = parsed.host_str() {
                let host_lower = host.to_lowercase();
                if known_netdisk_domains.iter().any(|domain| {
                    host_lower == *domain || host_lower.ends_with(&format!(".{domain}"))
                }) && seen.insert(candidate.clone())
                {
                    found.push(candidate);
                }
            }
        }
    }

    found
}

/// The fragment or query a publisher left outside a link's own anchor.
///
/// Matches the `#KEY` / `?pwd=x` tail of
/// `<a href="https://mega.nz/file/ID">https://mega.nz/file/ID</a>#KEY`: the tail
/// follows one or two closing tags, optionally separated by whitespace. Returns
/// the tail with its leading `#`/`?` and with trailing sentence punctuation
/// removed, or `None` when what follows the URL is ordinary text.
fn fragment_split_after_closing_tags(content: &str, from: usize) -> Option<String> {
    let rest = content.get(from..)?;
    let mut cursor = 0usize;
    for _ in 0..2 {
        let trimmed = rest[cursor..].trim_start();
        cursor += rest[cursor..].len() - trimmed.len();
        if !trimmed.starts_with("</") {
            break;
        }
        cursor += trimmed.find('>')? + 1;
    }
    let tail = &rest[cursor..];
    if !tail.starts_with('#') && !tail.starts_with('?') {
        return None;
    }
    let stop = tail
        .find(|ch: char| ch.is_whitespace() || matches!(ch, '"' | '\'' | '<' | '>'))
        .unwrap_or(tail.len());
    let suffix = tail[..stop].trim_end_matches(&['.', ',', ';', ':', '!', '?'][..]);
    // A bare `#` or `?` is punctuation, not a fragment.
    (suffix.len() > 1).then(|| suffix.to_string())
}

/// Extract potential archive extraction passwords from post body text.
///
/// Looks for patterns like:
/// - `pass: 1234`
/// - `password: abcd`
/// - `pwd: ...`
/// - `pw: ...`
/// - `解压密码: ...`
/// - `密码: ...`
pub fn extract_archive_passwords(content: &str) -> Vec<String> {
    static PASS_RE: OnceLock<Regex> = OnceLock::new();
    let re = PASS_RE.get_or_init(|| {
        Regex::new(r#"(?i)(?:pass(?:word)?|pwd|pw|解压密码|解压码|提取码|密码)\s*[:：]\s*([^\s\r\n,，;；]+)"#)
            .expect("valid regex")
    });

    let mut found = Vec::new();
    let mut seen = HashSet::new();

    for caps in re.captures_iter(content) {
        if let Some(mat) = caps.get(1) {
            let pass = mat
                .as_str()
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim();
            if !pass.is_empty() && !pass.starts_with("http") && seen.insert(pass.to_string()) {
                found.push(pass.to_string());
            }
        }
    }

    found
}

/// Normalize a post publication timestamp to the local target timezone (UTC+8 by default,
/// matching user/NAS browser downloader conventions) and format using the given format string.
///
/// Timezone offset rules:
/// - "fanbox" / "fantia": Pawchive returns native JST (UTC+9). Shift to UTC+8: -1 hour.
/// - "patreon" / "boosty" / "subscribestar" / "gumroad": Pawchive returns UTC (UTC+0). Shift to UTC+8: +8 hours.
/// - If the raw string already includes an ISO timezone offset (e.g. `+09:00`, `Z`, `+08:00`),
///   it is parsed with RFC 3339 and shifted to UTC+8.
/// - Fallback: If service is unspecified or parsing fails, preserves the date portion as-is.
pub fn normalize_post_datetime_format(
    published_at: Option<&str>,
    service: Option<&str>,
    format_str: &str,
) -> String {
    let raw = match published_at {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return String::new(),
    };

    let target_tz = match FixedOffset::east_opt(8 * 3600) {
        Some(tz) => tz,
        None => return raw.split(['T', ' ']).next().unwrap_or("").to_string(),
    };

    // 1. Try parsing full RFC 3339 / ISO 8601 with explicit timezone offset (e.g. "2025-02-13T00:37:33+09:00" or "...Z")
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        let local_dt = dt.with_timezone(&target_tz);
        return local_dt.format(format_str).to_string();
    }

    // 2. Parse naive datetime (e.g. "2025-02-13T00:37:33" or "2025-02-13 00:37:33")
    let s = raw.replace(' ', "T");
    let naive_res = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f"));

    if let Ok(ndt) = naive_res {
        let service_norm = service.map(|srv| srv.trim().to_lowercase());
        let shifted = match service_norm.as_deref() {
            Some("fanbox") | Some("fantia") => {
                // Stored in JST (UTC+9). Shift to UTC+8: minus 1 hour
                Some(ndt.checked_sub_signed(Duration::hours(1)).unwrap_or(ndt))
            }
            Some("patreon") | Some("boosty") | Some("subscribestar") | Some("gumroad") => {
                // Stored in UTC (UTC+0). Shift to UTC+8: plus 8 hours
                Some(ndt.checked_add_signed(Duration::hours(8)).unwrap_or(ndt))
            }
            _ => None,
        };

        if let Some(ndt_shifted) = shifted {
            return ndt_shifted.format(format_str).to_string();
        }
    }

    // 3. Fallback: if it's just a date "YYYY-MM-DD" or service was unspecified without explicit tz
    raw.split(['T', ' ']).next().unwrap_or("").to_string()
}

/// Normalize a post publication timestamp to the local target timezone (UTC+8 by default,
/// matching user/NAS browser downloader conventions) and format as YYYY-MM-DD.
pub fn normalize_post_date(published_at: Option<&str>, service: Option<&str>) -> String {
    normalize_post_datetime_format(published_at, service, "%Y-%m-%d")
}

/// Format the post timestamp for human inspection, displaying both the normalized local time (UTC+8)
/// and the original publication time with its source timezone annotation.
///
/// Returns `(local_display, original_display)`:
/// - `local_display`: e.g. `"2025-02-12 23:37:33 (UTC+8)"`
/// - `original_display`: e.g. `"2025-02-13 00:37:33 (JST / UTC+9)"` or `"2024-12-05 03:00:04 (UTC)"`
pub fn format_post_datetime_display(
    published_at: Option<&str>,
    service: Option<&str>,
) -> (Option<String>, Option<String>) {
    let raw = match published_at {
        Some(s) if !s.trim().is_empty() => s.trim(),
        _ => return (None, None),
    };

    let target_tz = match FixedOffset::east_opt(8 * 3600) {
        Some(tz) => tz,
        None => return (Some(raw.to_string()), Some(raw.to_string())),
    };

    // 1. Try parsing full RFC 3339 / ISO 8601 with explicit timezone offset
    if let Ok(dt) = DateTime::parse_from_rfc3339(raw) {
        let local_dt = dt.with_timezone(&target_tz);
        let local_str = format!("{} (UTC+8)", local_dt.format("%Y-%m-%d %H:%M:%S"));

        let offset_secs = dt.offset().local_minus_utc();
        let tz_label = if offset_secs == 9 * 3600 {
            "JST / UTC+9".to_string()
        } else if offset_secs == 0 {
            "UTC".to_string()
        } else if offset_secs == 8 * 3600 {
            "UTC+8".to_string()
        } else {
            // Take the sign from the offset itself: integer division
            // truncates toward zero, so -1800s would render as "UTC+00:30".
            let sign = if offset_secs < 0 { '-' } else { '+' };
            let abs_secs = offset_secs.abs();
            let hours = abs_secs / 3600;
            let minutes = (abs_secs % 3600) / 60;
            format!("UTC{sign}{hours:02}:{minutes:02}")
        };
        let orig_str = format!("{} ({})", dt.format("%Y-%m-%d %H:%M:%S"), tz_label);
        return (Some(local_str), Some(orig_str));
    }

    // 2. Parse naive datetime (e.g. "2025-02-13T00:37:33" or "2025-02-13 00:37:33")
    let s = raw.replace(' ', "T");
    let naive_res = NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S")
        .or_else(|_| NaiveDateTime::parse_from_str(&s, "%Y-%m-%dT%H:%M:%S%.f"));

    if let Ok(ndt) = naive_res {
        let service_norm = service.map(|srv| srv.trim().to_lowercase());
        let (shifted, tz_label) = match service_norm.as_deref() {
            Some("fanbox") | Some("fantia") => (
                // Upstream is JST (UTC+9). Shift to UTC+8: minus 1 hour
                Some(ndt.checked_sub_signed(Duration::hours(1)).unwrap_or(ndt)),
                "JST / UTC+9",
            ),
            Some("patreon") | Some("boosty") | Some("subscribestar") | Some("gumroad") => (
                // Upstream is UTC (UTC+0). Shift to UTC+8: plus 8 hours
                Some(ndt.checked_add_signed(Duration::hours(8)).unwrap_or(ndt)),
                "UTC",
            ),
            _ => (None, "Original"),
        };

        let orig_str = format!("{} ({})", ndt.format("%Y-%m-%d %H:%M:%S"), tz_label);
        let local_str = if let Some(local_ndt) = shifted {
            format!("{} (UTC+8)", local_ndt.format("%Y-%m-%d %H:%M:%S"))
        } else {
            orig_str.clone()
        };

        return (Some(local_str), Some(orig_str));
    }

    // 3. Fallback: date-only or unparseable string
    let date_part = raw.split(['T', ' ']).next().unwrap_or(raw);
    (
        Some(format!("{} (UTC+8)", date_part)),
        Some(format!("{} (Original)", date_part)),
    )
}

/// Naming context and default path/file template generator.
///
/// Specification:
/// - 文件夹的名字：`{user}/{date} {title}/`
/// - 图片的文件名：`{date} {index}`
/// - 压缩包、视频等附件的文件名：`{date} {name}`
/// - 外链的文件名：`links {title} {id}.txt`
/// - 正文（Content）的文件名：`content {id}.txt`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostNamingContext<'a> {
    pub user: &'a str,
    pub date: String,
    pub title: &'a str,
    pub post_id: &'a str,
    /// Creator id on the source service. Empty until `with_identity` supplies
    /// it, because `new` is also called where only the display name is known.
    pub user_id: &'a str,
    /// Source service (`fanbox`, `patreon`, ...). Empty until `with_identity`.
    pub service: &'a str,
}

impl<'a> PostNamingContext<'a> {
    pub fn new(
        user: &'a str,
        published_at: Option<&'a str>,
        title: Option<&'a str>,
        post_id: &'a str,
        service: Option<&'a str>,
    ) -> Self {
        let date = normalize_post_date(published_at, service);
        let title = title.unwrap_or("").trim();
        Self {
            user: user.trim(),
            date,
            title,
            post_id: post_id.trim(),
            user_id: "",
            service: "",
        }
    }

    /// Attach the creator id and source service so `{userID}` / `{user_id}` /
    /// `{service}` render the real values instead of the post id or a
    /// hard-coded service.
    pub fn with_identity(mut self, user_id: &'a str, service: &'a str) -> Self {
        self.user_id = user_id.trim();
        self.service = service.trim();
        self
    }

    pub fn with_date_format(
        user: &'a str,
        published_at: Option<&'a str>,
        title: Option<&'a str>,
        post_id: &'a str,
        service: Option<&'a str>,
        date_format: &str,
    ) -> Self {
        let date = normalize_post_datetime_format(published_at, service, date_format);
        let title = title.unwrap_or("").trim();
        Self {
            user: user.trim(),
            date,
            title,
            post_id: post_id.trim(),
            user_id: "",
            service: "",
        }
    }

    /// 文件夹的名字：`{user}/{date} {title}/`
    pub fn format_folder_path(&self) -> String {
        let safe_user = sanitize_path_component(self.user);
        let user_part = if safe_user.is_empty() {
            "unknown_user"
        } else {
            &safe_user
        };

        let safe_title = sanitize_path_component(self.title);
        let folder_sub = match (self.date.is_empty(), safe_title.is_empty()) {
            (false, false) => format!("{} {}", self.date, safe_title),
            (false, true) => format!("{} {}", self.date, self.post_id),
            (true, false) => safe_title,
            (true, true) => self.post_id.to_string(),
        };

        format!(
            "{user_part}/{}",
            safe_download_name(&folder_sub, "unknown_post")
        )
    }

    /// 图片的文件名：`{date} {index}` (保留扩展名)
    pub fn format_image_filename(&self, index: usize, ext: &str) -> String {
        let clean_ext = ext.trim_start_matches('.').trim();
        let ext_suffix = if clean_ext.is_empty() {
            String::new()
        } else {
            format!(".{clean_ext}")
        };
        if self.date.is_empty() {
            format!("{index}{ext_suffix}")
        } else {
            format!("{} {index}{ext_suffix}", self.date)
        }
    }

    /// 压缩包、视频等附件的文件名：`{date} {name}`
    pub fn format_attachment_filename(&self, raw_name: &str) -> String {
        let safe_name = sanitize_path_component(raw_name);
        let final_name = if safe_name.is_empty() {
            "unnamed_attachment"
        } else {
            &safe_name
        };
        // The date prefix is part of the name the filesystem sees, so the clamp
        // has to be applied to the rendered result and not only to the raw
        // attachment name — `{date} {name}` can exceed the cap on its own.
        clamp_name_bytes(
            &if self.date.is_empty() {
                final_name.to_string()
            } else {
                format!("{} {final_name}", self.date)
            },
            MAX_RENDERED_NAME_BYTES,
        )
    }

    /// 外链的文件名：`links {title} {id}.txt`
    pub fn format_links_filename(&self) -> String {
        let safe_title = sanitize_path_component(self.title);
        if safe_title.is_empty() {
            format!("links {}.txt", self.post_id)
        } else {
            format!("links {} {}.txt", safe_title, self.post_id)
        }
    }

    /// 正文（Content）的文件名：`content {id}.txt`
    pub fn format_content_filename(&self) -> String {
        format!("content {}.txt", self.post_id)
    }

    /// Sanitized creator id, falling back to the display name when the caller
    /// never supplied one (both are stable per-artist identifiers, so a
    /// template using `{userID}` still produces a usable path).
    fn user_id_value(&self) -> String {
        let raw = if self.user_id.is_empty() {
            self.user
        } else {
            self.user_id
        };
        sanitize_path_component(raw)
    }

    /// Source service for `{service}`. The only supported archive site is
    /// Pawchive, so an unknown service degrades to the site name rather than
    /// leaving an empty path segment.
    fn service_value(&self) -> String {
        if self.service.is_empty() {
            "pawchive".to_string()
        } else {
            sanitize_path_component(self.service)
        }
    }

    /// 文件夹的名字（支持自定义模板，如 `{user}/{date} {title}/`）
    pub fn format_folder_path_with_template(&self, template: &str) -> String {
        let trimmed = template.trim().trim_matches('/');
        if trimmed.is_empty() {
            return self.format_folder_path();
        }
        let safe_user = sanitize_path_component(self.user);
        let user_val = if safe_user.is_empty() {
            "unknown_user".to_string()
        } else {
            safe_user
        };
        let safe_title = sanitize_path_component(self.title);
        let title_val = if safe_title.is_empty() {
            self.post_id.to_string()
        } else {
            safe_title
        };
        let date_val = if self.date.is_empty() {
            "unknown_date".to_string()
        } else {
            self.date.clone()
        };

        let out = self.fill_template(
            trimmed,
            [&user_val, &date_val, &title_val, &title_val, "", ""],
        );
        // A template is a path, not a sentence: collapse the whitespace that
        // empty tokens leave behind and drop empty path segments.
        let rendered = out
            .split('/')
            .map(|segment| segment.split_whitespace().collect::<Vec<_>>().join(" "))
            .map(|segment| sanitize_path_component(&segment))
            .filter(|segment| !segment.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        if rendered.is_empty() {
            self.format_folder_path()
        } else {
            rendered
        }
    }

    /// 图片的文件名（支持自定义模板，如 `{date} {index}.{ext}`）
    pub fn format_image_filename_with_template(
        &self,
        index: usize,
        ext: &str,
        template: &str,
    ) -> String {
        self.format_image_filename_with_name(index, ext, self.title, template)
    }

    pub fn format_image_filename_with_name(
        &self,
        index: usize,
        ext: &str,
        name: &str,
        template: &str,
    ) -> String {
        let clean_ext = sanitize_path_component(ext.trim_start_matches('.').trim());
        let trimmed = template.trim();
        if trimmed.is_empty() {
            return safe_download_name(&self.format_image_filename(index, &clean_ext), "image");
        }
        let safe_user = sanitize_path_component(self.user);
        let safe_title = sanitize_path_component(self.title);
        let date_val = if self.date.is_empty() { "" } else { &self.date };

        let out = self.fill_template(
            trimmed,
            [
                &safe_user,
                date_val,
                &safe_title,
                &sanitize_path_component(name),
                &index.to_string(),
                &clean_ext,
            ],
        );
        let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
        let out = sanitize_path_component(&out);
        if out.is_empty() {
            return self.format_image_filename(index, &clean_ext);
        }
        let out = if !clean_ext.is_empty()
            && !out
                .to_lowercase()
                .ends_with(&format!(".{}", clean_ext.to_lowercase()))
        {
            format!("{out}.{clean_ext}")
        } else {
            out
        };
        safe_download_name(&out, &self.format_image_filename(index, &clean_ext))
    }

    /// 附件文件名（支持自定义模板，如 `{date} {name}`）
    pub fn format_attachment_filename_with_template(
        &self,
        raw_name: &str,
        template: &str,
    ) -> String {
        self.format_attachment_filename_with_index(raw_name, 0, template)
    }

    pub fn format_attachment_filename_with_index(
        &self,
        raw_name: &str,
        index: usize,
        template: &str,
    ) -> String {
        let safe_name = sanitize_path_component(raw_name);
        let trimmed = template.trim();
        if trimmed.is_empty() {
            return safe_download_name(
                &self.format_attachment_filename(raw_name),
                "unnamed_attachment",
            );
        }
        let safe_user = sanitize_path_component(self.user);
        let safe_title = sanitize_path_component(self.title);
        let date_val = if self.date.is_empty() { "" } else { &self.date };

        let ext = Path::new(&safe_name)
            .extension()
            .and_then(|s| s.to_str())
            .unwrap_or("");
        let out = self.fill_template(
            trimmed,
            [
                &safe_user,
                date_val,
                &safe_title,
                &safe_name,
                &index.to_string(),
                ext,
            ],
        );
        let out = out.split_whitespace().collect::<Vec<_>>().join(" ");
        let mut out =
            safe_download_name(&out, &safe_download_name(&safe_name, "unnamed_attachment"));
        if !ext.is_empty() && Path::new(&out).extension().is_none() {
            out.push('.');
            out.push_str(ext);
        }
        sanitize_path_component(&out)
    }

    /// Substitute only tokens in the template, never braces inside source data.
    fn fill_template(&self, template: &str, values: [&str; 6]) -> String {
        let [user, date, title, name, index, ext] = values;
        let creator = self.user_id_value();
        let service = self.service_value();
        let id = sanitize_path_component(self.post_id);
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let Some(end) = rest[start + 1..].find('}') else {
                out.push_str(&rest[start..]);
                return out;
            };
            let close = start + end + 2;
            let token = &rest[start + 1..close - 1];
            let value = match token {
                "user" | "artist" => user,
                "userID" | "user_id" | "creator_id" | "creator" => &creator,
                "date" => date,
                "year" => self.date.get(..4).unwrap_or(""),
                "month" => self.date.get(5..7).unwrap_or(""),
                "title" => title,
                "id" => &id,
                "name" => name,
                "index" => index,
                "ext" => ext,
                "site" => "pawchive",
                "service" => &service,
                _ => &rest[start..close],
            };
            out.push_str(value);
            rest = &rest[close..];
        }
        out.push_str(rest);
        out
    }
}

fn safe_download_name(raw: &str, fallback: &str) -> String {
    let name = sanitize_path_component(raw);
    if name.is_empty() {
        sanitize_path_component(fallback)
    } else {
        name
    }
}

/// The longest a single rendered path component may be, in bytes.
///
/// Filesystems cap a name at 255 *bytes* (ext4, NTFS, ZFS via fnOS), not 255
/// characters, and the difference is what a remote name can exploit: about 85
/// CJK characters reach the limit. A longer name does not fail loudly at the
/// point that matters — the resource downloads, then `create_new` at publish
/// time returns `ENAMETOOLONG`, which is classified as a permanent failure and
/// retried with backoff forever. Clamping at naming time keeps the failure out
/// of that loop, and the extension is kept because it is what the reader and
/// the media-type rules use.
pub const MAX_RENDERED_NAME_BYTES: usize = 255;

/// Truncate a rendered name to `max_bytes` on a character boundary, keeping the
/// extension when there is one.
pub(super) fn clamp_name_bytes(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_string();
    }
    let (stem, extension) = match name.rfind('.') {
        Some(index) if index > 0 => (&name[..index], &name[index..]),
        _ => (name, ""),
    };
    // A pathological extension (a name that is mostly a "." suffix) must not
    // consume the budget and leave an empty stem.
    let keep_extension = extension.len() <= max_bytes / 4;
    let (stem, extension) = if keep_extension {
        (stem, extension)
    } else {
        (name, "")
    };
    let budget = max_bytes.saturating_sub(extension.len());
    let mut end = budget.min(stem.len());
    while end > 0 && !stem.is_char_boundary(end) {
        end -= 1;
    }
    let mut out = String::with_capacity(end + extension.len());
    out.push_str(&stem[..end]);
    out.push_str(extension);
    out
}

/// Sanitize filename/path component by replacing forbidden characters
/// and trimming whitespace/dots, then clamping it to what a filesystem accepts.
///
/// The clamp is part of the contract, not a caller's job: this is the only
/// function every rendered name goes through, and a name that survives here but
/// not at `create_new` fails far away from the cause.
pub fn sanitize_path_component(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => out.push('_'),
            c if c.is_control() => out.push('_'),
            c => out.push(c),
        }
    }
    let trimmed = out.trim().trim_matches('.').trim();
    let basename = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = matches!(basename.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || (basename.len() == 4
            && (basename.starts_with("COM") || basename.starts_with("LPT"))
            && matches!(basename.as_bytes()[3], b'1'..=b'9'));
    let portable = if reserved {
        format!("_{trimmed}")
    } else {
        trimmed.to_string()
    };
    clamp_name_bytes(&portable, MAX_RENDERED_NAME_BYTES)
}
