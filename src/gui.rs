#[cfg(test)]
use crate::gib_to_bytes;
use crate::{
    memory_headroom_bytes, randomx_memory_bytes, BYTES_PER_GIB, MIN_MEMORY_HEADROOM_GIB,
    RANDOMX_DATASET_GIB, RANDOMX_WORKER_MIB,
};
use eframe::egui::{
    self, Align, Color32, CornerRadius, FontId, Frame, Layout, Margin, RichText, Sense, Stroke,
    Vec2,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::env;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{self, Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{MemoryRefreshKind, System};

const WINDOW_TITLE: &str = "XUS Miner";
const DEFAULT_POOL: &str = "127.0.0.1:3333";
const DEFAULT_USER: &str = "xus-miner";
const MAX_LOG_LINES: usize = 500;
const MAX_CHILD_LINE_BYTES: usize = 8 * 1024;
const MAX_STORED_LOG_BYTES: usize = 8 * 1024;
const CHILD_OUTPUT_QUEUE_CAPACITY: usize = 256;
const MAX_PROCESS_MESSAGES_PER_FRAME: usize = 64;
const OUTPUT_DISCONNECT_GRACE: Duration = Duration::from_millis(500);
const OBSERVATION_FAILURE_DRAIN_GRACE: Duration = Duration::from_secs(2);
const MAX_CRASH_REPORT_BYTES: usize = 128 * 1024;
const MAX_CRASH_REPORT_LOG_LINES: usize = 100;
const MAX_METER_SAMPLES: usize = 180;
const SETTINGS_DIRECTORY: &str = ".xus-miner";
const SETTINGS_FILE: &str = "gui-settings.json";
const CRASH_REPORT_PREFIX: &str = "crash-report";
const MAX_SETTINGS_BYTES: u64 = 64 * 1024;
const MEMORY_REFRESH_INTERVAL: Duration = Duration::from_secs(10);
const BLOCK_FOUND_HIGHLIGHT: Duration = Duration::from_secs(30);
const MAX_ACTIVITY_CELLS: usize = 48;
const WAITING_FOR_WORK: &str = "Waiting for work";
const ACTIVITY_CELL_SUMMARY: &str =
    "Each cell is one telemetry time slice—not an incoming job or an individual hash. Up to 48 cells summarize recent telemetry history.";
static SETTINGS_TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static CRASH_REPORT_SEQUENCE: AtomicU64 = AtomicU64::new(0);

const BG: Color32 = Color32::from_rgb(9, 11, 18);
const PANEL: Color32 = Color32::from_rgb(15, 18, 29);
const CARD: Color32 = Color32::from_rgb(21, 25, 39);
const CARD_HOVER: Color32 = Color32::from_rgb(25, 30, 47);
const BORDER: Color32 = Color32::from_rgb(43, 50, 71);
const TEXT: Color32 = Color32::from_rgb(235, 239, 248);
const MUTED: Color32 = Color32::from_rgb(142, 151, 174);
const PURPLE: Color32 = Color32::from_rgb(139, 104, 246);
const CYAN: Color32 = Color32::from_rgb(62, 211, 209);
const GREEN: Color32 = Color32::from_rgb(74, 222, 128);
const AMBER: Color32 = Color32::from_rgb(251, 191, 36);
const RED: Color32 = Color32::from_rgb(248, 113, 113);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Starting,
    Connecting,
    Authenticating,
    Mining,
    Reconnecting,
    Stopping,
    Error,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Idle => "IDLE",
            Self::Starting => "STARTING",
            Self::Connecting => "CONNECTING",
            Self::Authenticating => "AUTHENTICATING",
            Self::Mining => "MINING",
            Self::Reconnecting => "RECONNECTING",
            Self::Stopping => "STOPPING",
            Self::Error => "ATTENTION",
        }
    }

    fn color(self) -> Color32 {
        match self {
            Self::Mining => GREEN,
            Self::Starting | Self::Connecting | Self::Authenticating | Self::Reconnecting => AMBER,
            Self::Error => RED,
            Self::Idle | Self::Stopping => MUTED,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum RewardRoute {
    #[default]
    OwnedBridge,
    ExternalPool,
}

impl RewardRoute {
    fn persisted(self) -> &'static str {
        match self {
            Self::OwnedBridge => "owned_bridge",
            Self::ExternalPool => "external_pool",
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::OwnedBridge => "Bridge coinbase (operator asserted)",
            Self::ExternalPool => "No automatic payout verified",
        }
    }
}

#[derive(Clone)]
struct Settings {
    reward_route: RewardRoute,
    pool: String,
    user: String,
    password: String,
    workers: u64,
    reconnect_secs: u64,
    report_secs: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            reward_route: RewardRoute::OwnedBridge,
            pool: DEFAULT_POOL.into(),
            user: DEFAULT_USER.into(),
            password: "x".into(),
            workers: 1,
            reconnect_secs: 5,
            report_secs: 2,
        }
    }
}

impl Settings {
    fn from_json(value: &Value) -> Self {
        let mut settings = Self::default();
        if value.get("reward_route").and_then(Value::as_str) == Some("external_pool") {
            settings.reward_route = RewardRoute::ExternalPool;
        }
        if let Some(pool) = value.get("pool").and_then(Value::as_str) {
            settings.pool = pool.to_owned();
        }
        if let Some(user) = value.get("user").and_then(Value::as_str) {
            settings.user = user.to_owned();
        }
        if let Some(workers) = value.get("workers").and_then(Value::as_u64) {
            settings.workers = workers.clamp(1, 64);
        }
        if let Some(seconds) = value.get("reconnect_secs").and_then(Value::as_u64) {
            settings.reconnect_secs = seconds.clamp(1, 3_600);
        }
        if let Some(seconds) = value.get("report_secs").and_then(Value::as_u64) {
            settings.report_secs = seconds.clamp(1, 3_600);
        }
        settings
    }

    fn persisted_json(&self) -> Value {
        json!({
            "reward_route": self.reward_route.persisted(),
            "pool": sanitize_endpoint(self.pool.trim()),
            "user": self.user.trim(),
            "workers": self.workers,
            "reconnect_secs": self.reconnect_secs,
            "report_secs": self.report_secs,
        })
    }

    fn validate(&self) -> Result<(), String> {
        validate_pool(&self.pool)?;
        if self.user.trim().is_empty() {
            return Err("Worker label cannot be empty.".into());
        }
        if self.user.len() > 256 {
            return Err("Worker label must be 256 characters or fewer.".into());
        }
        if self.password.contains(['\n', '\r']) {
            return Err("Stratum password cannot contain a line break.".into());
        }
        if self.password.len() > 4_096 {
            return Err("Stratum password is unexpectedly large.".into());
        }
        if !(1..=64).contains(&self.workers) {
            return Err("Worker count must be between 1 and 64.".into());
        }
        Ok(())
    }
}

fn validate_pool(raw: &str) -> Result<(), String> {
    let address = raw
        .trim()
        .strip_prefix("stratum+tcp://")
        .or_else(|| raw.trim().strip_prefix("tcp://"))
        .or_else(|| raw.trim().strip_prefix("rpc://"))
        .or_else(|| raw.trim().strip_prefix("http://"))
        .unwrap_or(raw.trim());
    if address.is_empty() || address.chars().any(char::is_whitespace) {
        return Err("Endpoint must be a host and port, such as 192.168.0.244:8645.".into());
    }
    if address.contains('@') {
        return Err(
            "Endpoint credentials are not allowed. Enter only the host and port; use the separate password field."
                .into(),
        );
    }
    let (host, port) = address
        .rsplit_once(':')
        .ok_or_else(|| "Pool is missing its TCP port.".to_string())?;
    if host.is_empty() {
        return Err("Pool host cannot be empty.".into());
    }
    let port = port
        .parse::<u16>()
        .map_err(|_| "Pool port must be a number from 1 to 65535.".to_string())?;
    if port == 0 {
        return Err("Pool port cannot be zero.".into());
    }
    Ok(())
}

fn is_direct_node_endpoint(raw: &str) -> bool {
    let raw = raw.trim();
    raw.starts_with("rpc://")
        || raw.starts_with("http://")
        || raw.starts_with("https://")
        || raw
            .trim_end_matches('/')
            .rsplit_once(':')
            .is_some_and(|(_, port)| port == "8645")
}

fn settings_path() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(SETTINGS_DIRECTORY).join(SETTINGS_FILE))
}

fn metadata_is_link(metadata: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
        metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
    }
    #[cfg(not(windows))]
    {
        metadata.file_type().is_symlink()
    }
}

fn verify_settings_directory(directory: &Path) -> Result<fs::Metadata, String> {
    let metadata = fs::symlink_metadata(directory)
        .map_err(|error| format!("cannot inspect settings directory: {error}"))?;
    if metadata_is_link(&metadata) {
        return Err("settings directory is a symbolic link or reparse point".into());
    }
    if !metadata.is_dir() {
        return Err("settings path parent is not a directory".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let parent = directory
            .parent()
            .ok_or_else(|| "settings directory has no parent".to_string())?;
        let parent_metadata = fs::metadata(parent)
            .map_err(|error| format!("cannot inspect settings directory parent: {error}"))?;
        if metadata.uid() != parent_metadata.uid() {
            return Err("settings directory is not owned by the home-directory owner".into());
        }
        if metadata.mode() & 0o022 != 0 {
            return Err("settings directory is writable by another account".into());
        }
    }
    Ok(metadata)
}

fn ensure_settings_directory(directory: &Path) -> Result<(), String> {
    match fs::symlink_metadata(directory) {
        Ok(_) => return verify_settings_directory(directory).map(drop),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(format!("cannot inspect settings directory: {error}")),
    }

    #[cfg(unix)]
    let builder = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o700);
        builder
    };
    #[cfg(not(unix))]
    let builder = fs::DirBuilder::new();
    match builder.create(directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("cannot create settings directory: {error}")),
    }
    verify_settings_directory(directory).map(drop)
}

fn inspect_settings_file(path: &Path) -> Result<Option<fs::Metadata>, String> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(format!("cannot inspect settings file: {error}")),
    };
    if metadata_is_link(&metadata) {
        return Err("settings file is a symbolic link or reparse point".into());
    }
    if !metadata.is_file() {
        return Err("settings file is not a regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let directory = path
            .parent()
            .ok_or_else(|| "settings file has no parent".to_string())?;
        let directory_metadata = verify_settings_directory(directory)?;
        if metadata.uid() != directory_metadata.uid() {
            return Err("settings file is not owned by the settings-directory owner".into());
        }
        if metadata.mode() & 0o022 != 0 {
            return Err("settings file is writable by another account".into());
        }
    }
    Ok(Some(metadata))
}

fn load_settings() -> Settings {
    settings_path()
        .and_then(|path| load_settings_at(&path).ok())
        .unwrap_or_default()
}

fn load_settings_at(path: &Path) -> Result<Settings, String> {
    let directory = path
        .parent()
        .ok_or_else(|| "settings file has no parent".to_string())?;
    verify_settings_directory(directory)?;
    #[cfg(windows)]
    recover_windows_settings(path)?;
    let initial =
        inspect_settings_file(path)?.ok_or_else(|| "settings file does not exist".to_string())?;
    if initial.len() > MAX_SETTINGS_BYTES {
        return Err("settings file is unexpectedly large".into());
    }

    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
    let mut file = options
        .open(path)
        .map_err(|error| format!("cannot open settings: {error}"))?;
    let opened = file
        .metadata()
        .map_err(|error| format!("cannot inspect opened settings: {error}"))?;
    if metadata_is_link(&opened) || !opened.is_file() || opened.len() > MAX_SETTINGS_BYTES {
        return Err("opened settings path is not a bounded regular file".into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if initial.dev() != opened.dev() || initial.ino() != opened.ino() {
            return Err("settings file changed while it was being opened".into());
        }
    }

    let mut bytes = Vec::with_capacity(opened.len() as usize);
    Read::by_ref(&mut file)
        .take(MAX_SETTINGS_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| format!("cannot read settings: {error}"))?;
    if bytes.len() as u64 > MAX_SETTINGS_BYTES {
        return Err("settings file is unexpectedly large".into());
    }
    serde_json::from_slice::<Value>(&bytes)
        .map(|value| Settings::from_json(&value))
        .map_err(|error| format!("cannot decode settings: {error}"))
}

fn save_settings(settings: &Settings) -> Result<(), String> {
    let Some(path) = settings_path() else {
        return Ok(());
    };
    save_settings_at(&path, settings)
}

fn save_settings_at(path: &Path, settings: &Settings) -> Result<(), String> {
    let directory = path
        .parent()
        .ok_or_else(|| "settings file has no parent".to_string())?;
    ensure_settings_directory(directory)?;
    #[cfg(windows)]
    recover_windows_settings(path)?;
    inspect_settings_file(path)?;

    let bytes = serde_json::to_vec_pretty(&settings.persisted_json())
        .map_err(|error| format!("cannot encode settings: {error}"))?;
    let mut temporary = None;
    for attempt in 0..32_u64 {
        let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let candidate = directory.join(format!(
            ".gui-settings.{}.{nonce}.{sequence}.{attempt}.tmp",
            process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&candidate) {
            Ok(file) => {
                temporary = Some((candidate, file));
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot create temporary settings: {error}")),
        }
    }
    let (temporary_path, mut temporary_file) =
        temporary.ok_or_else(|| "cannot create unique temporary settings".to_string())?;
    temporary_file
        .write_all(&bytes)
        .and_then(|()| temporary_file.sync_all())
        .map_err(|error| format!("cannot write temporary settings: {error}"))?;
    drop(temporary_file);

    // Recheck after creating the sibling file. rename never opens the existing
    // destination for writing, so a symlink target is not followed.
    verify_settings_directory(directory)?;
    inspect_settings_file(path)?;
    replace_settings_file(path, &temporary_path)?;

    #[cfg(unix)]
    File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|error| format!("cannot sync settings directory: {error}"))?;
    Ok(())
}

#[cfg(not(windows))]
fn replace_settings_file(path: &Path, temporary: &Path) -> Result<(), String> {
    fs::rename(temporary, path)
        .map_err(|error| format!("cannot install settings atomically: {error}"))
}

#[cfg(windows)]
fn replace_settings_file(path: &Path, temporary: &Path) -> Result<(), String> {
    // std has no atomic replace primitive on Windows. A deterministic backup
    // makes either possible crash point recoverable on the next load/save.
    recover_windows_settings(path)?;
    let backup = windows_settings_backup(path);
    if inspect_settings_file(path)?.is_some() {
        fs::rename(path, &backup).map_err(|error| format!("cannot stage old settings: {error}"))?;
    }
    match fs::rename(temporary, path) {
        Ok(()) => {
            if backup.exists() {
                fs::remove_file(backup)
                    .map_err(|error| format!("cannot remove old settings backup: {error}"))?;
            }
            Ok(())
        }
        Err(error) => {
            let _ = fs::rename(&backup, path);
            Err(format!("cannot install settings: {error}"))
        }
    }
}

#[cfg(windows)]
fn windows_settings_backup(path: &Path) -> PathBuf {
    path.with_file_name(format!(".{SETTINGS_FILE}.backup"))
}

#[cfg(windows)]
fn recover_windows_settings(path: &Path) -> Result<(), String> {
    let backup = windows_settings_backup(path);
    let current = inspect_settings_file(path)?;
    let staged = inspect_settings_file(&backup)?;
    match (current.is_some(), staged.is_some()) {
        (false, true) => fs::rename(&backup, path)
            .map_err(|error| format!("cannot recover staged settings: {error}")),
        (true, true) => fs::remove_file(&backup)
            .map_err(|error| format!("cannot remove recovered settings backup: {error}")),
        _ => Ok(()),
    }
}

const TRUNCATED_LINE_SUFFIX: &str = " … [line truncated at 8 KiB]";
const TRUNCATED_REPORT_SUFFIX: &str = "\n[crash report truncated]\n";

fn bounded_text(raw: &str, max_bytes: usize, suffix: &str, force_suffix: bool) -> String {
    if !force_suffix && raw.len() <= max_bytes {
        return raw.to_owned();
    }
    if max_bytes == 0 {
        return String::new();
    }
    if suffix.len() >= max_bytes {
        let mut keep = max_bytes.min(suffix.len());
        while !suffix.is_char_boundary(keep) {
            keep -= 1;
        }
        return suffix[..keep].to_owned();
    }

    let mut keep = max_bytes - suffix.len();
    keep = keep.min(raw.len());
    while !raw.is_char_boundary(keep) {
        keep -= 1;
    }
    let mut bounded = String::with_capacity(max_bytes);
    bounded.push_str(&raw[..keep]);
    bounded.push_str(suffix);
    bounded
}

/// Reads one logical child-process line without ever retaining more than 8 KiB.
///
/// Oversized input is drained through the terminating newline so the following
/// JSON/log event starts at a clean boundary.
fn read_bounded_child_line<R: BufRead>(reader: &mut R) -> io::Result<Option<String>> {
    let mut captured = Vec::with_capacity(MAX_CHILD_LINE_BYTES);
    let mut saw_bytes = false;
    let mut truncated = false;
    let mut reached_line_end = false;

    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            break;
        }
        saw_bytes = true;
        let newline = available.iter().position(|byte| *byte == b'\n');
        let content_len = newline.unwrap_or(available.len());
        let remaining = MAX_CHILD_LINE_BYTES.saturating_sub(captured.len());
        let retained = content_len.min(remaining);
        captured.extend_from_slice(&available[..retained]);
        truncated |= retained < content_len;

        let consumed = newline.map_or(available.len(), |position| position + 1);
        reader.consume(consumed);
        if newline.is_some() {
            reached_line_end = true;
            break;
        }
    }

    if !saw_bytes {
        return Ok(None);
    }
    if reached_line_end && !truncated && captured.last() == Some(&b'\r') {
        captured.pop();
    }
    let decoded = String::from_utf8_lossy(&captured);
    Ok(Some(bounded_text(
        &decoded,
        MAX_CHILD_LINE_BYTES,
        TRUNCATED_LINE_SUFFIX,
        truncated || decoded.len() > MAX_CHILD_LINE_BYTES,
    )))
}

#[cfg(windows)]
fn windows_ntstatus_description(code: u32) -> Option<&'static str> {
    match code {
        0xC000_0005 => Some("access violation (native memory fault)"),
        0xC000_0017 => Some("not enough virtual memory"),
        0xC000_001D => Some("illegal CPU instruction"),
        0xC000_00FD => Some("stack overflow"),
        0xC000_0135 => Some("required DLL was not found"),
        0xC000_0374 => Some("heap corruption"),
        0xC000_0409 => Some("fail-fast termination or stack buffer overrun"),
        _ => None,
    }
}

#[cfg(windows)]
fn describe_exit_status(status: &ExitStatus) -> String {
    match status.code() {
        Some(code) => {
            let raw = code as u32;
            windows_ntstatus_description(raw).map_or_else(
                || format!("Windows exit code {code} (0x{raw:08X})"),
                |meaning| format!("Windows exception 0x{raw:08X}: {meaning}"),
            )
        }
        None => "Windows terminated the engine without an exit code".into(),
    }
}

#[cfg(unix)]
fn describe_exit_status(status: &ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    if let Some(signal) = status.signal() {
        return format!("signal {signal}");
    }
    status
        .code()
        .map_or_else(|| status.to_string(), |code| format!("exit code {code}"))
}

#[cfg(not(any(unix, windows)))]
fn describe_exit_status(status: &ExitStatus) -> String {
    status
        .code()
        .map_or_else(|| status.to_string(), |code| format!("exit code {code}"))
}

#[cfg(unix)]
fn exit_status_matches_requested_stop(status: &ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    // `Child::kill` is SIGKILL on Unix. Checking the reaped status closes the
    // race where kill succeeds for a child that already exited as a zombie.
    status.signal() == Some(9)
}

#[cfg(windows)]
fn exit_status_matches_requested_stop(status: &ExitStatus) -> bool {
    // The standard library's Windows `Child::kill` uses TerminateProcess with
    // exit code 1. TerminateProcess fails for an already-terminated process,
    // so a successful kill plus this status identifies the requested stop.
    status.code().is_some_and(|code| code as u32 == 1)
}

#[cfg(not(any(unix, windows)))]
fn exit_status_matches_requested_stop(_status: &ExitStatus) -> bool {
    false
}

/// Attempts termination and only performs a blocking reap after termination is
/// confirmed. If termination fails, a single non-blocking observation can
/// still prove that the child already exited; otherwise waiting is deliberately
/// skipped so GUI cleanup cannot hang on a process that may remain live.
fn terminate_and_reap_child(child: &mut Child) -> Result<ExitStatus, String> {
    match child.kill() {
        Ok(()) => child
            .wait()
            .map_err(|error| format!("termination succeeded but reaping failed: {error}")),
        Err(kill_error) => match child.try_wait() {
            Ok(Some(status)) => Ok(status),
            Ok(None) => Err(format!(
                "termination failed: {kill_error}; child still appears live, so blocking reap was skipped"
            )),
            Err(observe_error) => Err(format!(
                "termination failed: {kill_error}; exit could not be verified: {observe_error}; blocking reap was skipped"
            )),
        },
    }
}

fn sanitize_endpoint(raw: &str) -> String {
    let trimmed = raw.trim();
    if let Some((scheme, remainder)) = trimmed.split_once("://") {
        let authority_end = remainder.find('/').unwrap_or(remainder.len());
        let (authority, tail) = remainder.split_at(authority_end);
        let host = authority
            .rsplit_once('@')
            .map_or(authority, |(_, host)| host);
        return format!("{scheme}://{host}{tail}");
    }
    trimmed
        .rsplit_once('@')
        .map_or_else(|| trimmed.to_owned(), |(_, host)| host.to_owned())
}

fn endpoint_userinfo(raw: &str) -> Option<&str> {
    let trimmed = raw.trim();
    let remainder = trimmed
        .split_once("://")
        .map_or(trimmed, |(_, remainder)| remainder);
    let authority_end = remainder.find('/').unwrap_or(remainder.len());
    remainder[..authority_end]
        .rsplit_once('@')
        .map(|(userinfo, _)| userinfo)
        .filter(|userinfo| !userinfo.is_empty())
}

fn redact_secret(raw: &str, secret: &str) -> String {
    if secret.is_empty() {
        raw.to_owned()
    } else {
        let replacement = if "[REDACTED]".contains(secret) {
            ""
        } else {
            "[REDACTED]"
        };
        if secret.chars().count() > 2 {
            return raw.replace(secret, replacement);
        }

        // Very short credentials (including the conventional Stratum "x")
        // must still be removed when reflected as a token, without erasing the
        // same character from ordinary words such as "exit" or "xus-miner".
        let mut redacted = String::with_capacity(raw.len());
        let mut copied_until = 0;
        for (start, _) in raw.match_indices(secret) {
            let end = start + secret.len();
            let before_is_word = raw[..start]
                .chars()
                .next_back()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            let after_is_word = raw[end..]
                .chars()
                .next()
                .is_some_and(|ch| ch.is_alphanumeric() || ch == '_');
            if before_is_word || after_is_word {
                continue;
            }
            redacted.push_str(&raw[copied_until..start]);
            redacted.push_str(replacement);
            copied_until = end;
        }
        redacted.push_str(&raw[copied_until..]);
        redacted
    }
}

fn write_crash_report_at(directory: &Path, report: &str) -> Result<PathBuf, String> {
    ensure_settings_directory(directory)?;
    let report = bounded_text(
        report,
        MAX_CRASH_REPORT_BYTES,
        TRUNCATED_REPORT_SUFFIX,
        report.len() > MAX_CRASH_REPORT_BYTES,
    );
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();

    for attempt in 0..32_u64 {
        let sequence = CRASH_REPORT_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = directory.join(format!(
            "{CRASH_REPORT_PREFIX}-{timestamp}-{}-{sequence}-{attempt}.log",
            process::id()
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        match options.open(&path) {
            Ok(mut file) => {
                let metadata = file
                    .metadata()
                    .map_err(|error| format!("cannot inspect crash report: {error}"))?;
                if metadata_is_link(&metadata) || !metadata.is_file() {
                    return Err("created crash report is not a regular file".into());
                }
                verify_settings_directory(directory)?;
                file.write_all(report.as_bytes())
                    .and_then(|()| file.sync_all())
                    .map_err(|error| format!("cannot write crash report: {error}"))?;
                #[cfg(unix)]
                File::open(directory)
                    .and_then(|file| file.sync_all())
                    .map_err(|error| format!("cannot sync crash report directory: {error}"))?;
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("cannot create crash report: {error}")),
        }
    }
    Err("cannot create a unique crash report".into())
}

#[derive(Debug)]
enum ProcessMessage {
    Telemetry(Value),
    Log(String),
}

struct PendingObservationFailure {
    report_detail: String,
    drain_deadline: Instant,
}

struct LogLine {
    at: Instant,
    text: String,
    error: bool,
}

#[derive(Clone, Copy, Debug)]
struct MeterSample {
    hashrate: f64,
    smoothed_hashrate: f64,
    mempool: Option<u64>,
    height: Option<u64>,
    round_probability: Option<f64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct MemorySnapshot {
    available: u64,
    total: u64,
}

struct MinerApp {
    settings: Settings,
    phase: Phase,
    child: Option<Child>,
    quarantined_child: Option<Child>,
    pending_exit: Option<ExitStatus>,
    pending_observation_failure: Option<PendingObservationFailure>,
    receiver: Option<Receiver<ProcessMessage>>,
    reader_threads: Vec<thread::JoinHandle<()>>,
    output_disconnected_at: Option<Instant>,
    requested_stop: bool,
    started_at: Option<Instant>,
    hashrate: f64,
    smoothed_hashrate: f64,
    total_hashes: u64,
    submitted: u64,
    accepted: u64,
    rejected: u64,
    height: Option<u64>,
    algorithm: String,
    job_id: String,
    job_observed_in_metrics: bool,
    jobless_metrics_seen: u8,
    last_found_block: Option<(u64, Instant)>,
    coinbase: Option<String>,
    expected_hashes: Option<f64>,
    round_hashes: u64,
    round_probability: Option<f64>,
    height_started_at: Option<Instant>,
    network_hashrate: Option<f64>,
    target_block_ms: Option<u64>,
    network_difficulty: Option<String>,
    last_error: String,
    logs: VecDeque<LogLine>,
    mempool_size: Option<u64>,
    peer_count: Option<u64>,
    meter_history: VecDeque<MeterSample>,
    last_metrics_at: Option<Instant>,
    ready_workers: u64,
    ready_worker_ids: BTreeSet<u64>,
    worker_errors: BTreeMap<u64, String>,
    worker_error_can_restore_mining: bool,
    engine_fatal_seen: bool,
    reveal_password: bool,
    memory_acknowledged: bool,
    memory_system: System,
    memory_snapshot: Option<MemorySnapshot>,
    last_memory_refresh: Option<Instant>,
    node_link_live: bool,
}

impl Default for MinerApp {
    fn default() -> Self {
        let mut memory_system = System::new();
        let memory_snapshot = capture_memory_snapshot(&mut memory_system);
        Self {
            settings: load_settings(),
            phase: Phase::Idle,
            child: None,
            quarantined_child: None,
            pending_exit: None,
            pending_observation_failure: None,
            receiver: None,
            reader_threads: Vec::new(),
            output_disconnected_at: None,
            requested_stop: false,
            started_at: None,
            hashrate: 0.0,
            smoothed_hashrate: 0.0,
            total_hashes: 0,
            submitted: 0,
            accepted: 0,
            rejected: 0,
            height: None,
            algorithm: "—".into(),
            job_id: WAITING_FOR_WORK.into(),
            job_observed_in_metrics: false,
            jobless_metrics_seen: 0,
            last_found_block: None,
            coinbase: None,
            expected_hashes: None,
            round_hashes: 0,
            round_probability: None,
            height_started_at: None,
            network_hashrate: None,
            target_block_ms: None,
            network_difficulty: None,
            last_error: String::new(),
            logs: VecDeque::new(),
            mempool_size: None,
            peer_count: None,
            meter_history: VecDeque::new(),
            last_metrics_at: None,
            ready_workers: 0,
            ready_worker_ids: BTreeSet::new(),
            worker_errors: BTreeMap::new(),
            worker_error_can_restore_mining: false,
            engine_fatal_seen: false,
            reveal_password: false,
            memory_acknowledged: false,
            memory_system,
            memory_snapshot,
            last_memory_refresh: Some(Instant::now()),
            node_link_live: false,
        }
    }
}

impl MinerApp {
    fn is_running(&self) -> bool {
        self.child.is_some()
            || self.quarantined_child.is_some()
            || self.pending_exit.is_some()
            || self.pending_observation_failure.is_some()
    }

    fn refresh_memory_now(&mut self) {
        self.memory_snapshot = capture_memory_snapshot(&mut self.memory_system);
        self.last_memory_refresh = Some(Instant::now());
    }

    fn refresh_memory_if_due(&mut self) {
        if self
            .last_memory_refresh
            .is_none_or(|last| last.elapsed() >= MEMORY_REFRESH_INTERVAL)
        {
            self.refresh_memory_now();
        }
    }

    fn memory_preflight_error(&self) -> Option<String> {
        let workers = self.settings.workers;
        let randomx_estimate = randomx_memory_bytes(workers);
        match self.memory_snapshot {
            Some(snapshot)
                if snapshot.available
                    < randomx_estimate + memory_headroom_bytes(snapshot.total) =>
            {
                Some(format!(
                    "Start blocked: RandomX may need about {} total (one shared {RANDOMX_DATASET_GIB:.1} GiB dataset + {RANDOMX_WORKER_MIB:.0} MiB per worker), plus {} of system headroom, but the live scan reports only {} available for {workers} worker{}.",
                    format_memory(randomx_estimate),
                    format_memory(memory_headroom_bytes(snapshot.total)),
                    format_memory(snapshot.available),
                    if workers == 1 { "" } else { "s" },
                ))
            }
            Some(_) => None,
            None if !self.memory_acknowledged => Some(format!(
                "The operating-system memory scan is unavailable. Confirm at least {:.1} GiB is available before starting {workers} RandomX worker{} (one shared dataset, not one dataset per worker).",
                randomx_estimate as f64 / BYTES_PER_GIB + MIN_MEMORY_HEADROOM_GIB,
                if workers == 1 { "" } else { "s" },
            )),
            None => None,
        }
    }

    fn memory_preflight_summary(&self) -> String {
        let dataset = randomx_memory_bytes(self.settings.workers);
        self.memory_snapshot.map_or_else(
            || {
                format!(
                    "Memory preflight manually confirmed: {} RandomX estimate plus at least {:.1} GiB reserved.",
                    format_memory(dataset),
                    MIN_MEMORY_HEADROOM_GIB,
                )
            },
            |snapshot| {
                format!(
                    "Memory preflight PASS: {} available / {} total; {} RandomX estimate + {} reserved.",
                    format_memory(snapshot.available),
                    format_memory(snapshot.total),
                    format_memory(dataset),
                    format_memory(memory_headroom_bytes(snapshot.total)),
                )
            },
        )
    }

    fn reset_telemetry(&mut self) {
        self.hashrate = 0.0;
        self.smoothed_hashrate = 0.0;
        self.total_hashes = 0;
        self.submitted = 0;
        self.accepted = 0;
        self.rejected = 0;
        self.height = None;
        self.algorithm = "—".into();
        self.job_id = WAITING_FOR_WORK.into();
        self.job_observed_in_metrics = false;
        self.jobless_metrics_seen = 0;
        self.last_found_block = None;
        self.coinbase = None;
        self.expected_hashes = None;
        self.round_hashes = 0;
        self.round_probability = None;
        self.height_started_at = None;
        self.network_hashrate = None;
        self.target_block_ms = None;
        self.network_difficulty = None;
        self.last_error.clear();
        self.mempool_size = None;
        self.peer_count = None;
        self.meter_history.clear();
        self.last_metrics_at = None;
        self.ready_workers = 0;
        self.ready_worker_ids.clear();
        self.worker_errors.clear();
        self.worker_error_can_restore_mining = false;
        self.engine_fatal_seen = false;
        self.output_disconnected_at = None;
        self.pending_observation_failure = None;
        self.node_link_live = false;
    }

    fn clear_active_work(&mut self) {
        self.height = None;
        self.algorithm = "—".into();
        self.job_id = WAITING_FOR_WORK.into();
        self.job_observed_in_metrics = false;
        self.jobless_metrics_seen = 0;
        self.coinbase = None;
        self.expected_hashes = None;
        self.round_hashes = 0;
        self.round_probability = None;
        self.height_started_at = None;
    }

    fn mark_worker_unready(&mut self, worker: u64) {
        self.ready_worker_ids.remove(&worker);
        self.ready_workers = self.ready_worker_ids.len() as u64;
    }

    fn mark_worker_ready(&mut self, worker: u64) -> bool {
        if worker >= self.settings.workers || self.engine_fatal_seen {
            return false;
        }
        let newly_ready = self.ready_worker_ids.insert(worker);
        self.ready_workers = self.ready_worker_ids.len() as u64;
        let recovered = self.worker_errors.remove(&worker).is_some();
        if recovered
            && self.worker_error_can_restore_mining
            && !self.engine_fatal_seen
            && matches!(self.phase, Phase::Error)
        {
            if let Some((failed_worker, message)) = self.worker_errors.iter().next() {
                self.last_error = format!("Worker {} error: {message}", failed_worker + 1);
            } else {
                self.phase = Phase::Mining;
                self.last_error.clear();
            }
        }
        if self.worker_errors.is_empty() {
            self.worker_error_can_restore_mining = false;
        }
        newly_ready || recovered
    }

    fn enter_mining_phase(&mut self) {
        if self.engine_fatal_seen {
            self.phase = Phase::Error;
            self.worker_error_can_restore_mining = false;
        } else if let Some((worker, message)) = self.worker_errors.iter().next() {
            self.phase = Phase::Error;
            self.worker_error_can_restore_mining = true;
            self.last_error = format!("Worker {} error: {message}", worker + 1);
        } else {
            self.phase = Phase::Mining;
            self.worker_error_can_restore_mining = false;
        }
    }

    fn enter_connection_phase(&mut self, phase: Phase) {
        if self.engine_fatal_seen {
            self.phase = Phase::Error;
        } else if let Some((worker, message)) = self.worker_errors.iter().next() {
            self.phase = Phase::Error;
            self.last_error = format!("Worker {} error: {message}", worker + 1);
        } else {
            self.phase = phase;
        }
        self.worker_error_can_restore_mining = false;
    }

    fn redact_sensitive_text(&self, raw: &str) -> String {
        let redacted = redact_secret(raw, &self.settings.password);
        if let Some(userinfo) = endpoint_userinfo(&self.settings.pool) {
            redact_secret(&redacted, userinfo)
        } else {
            redacted
        }
    }

    fn push_log(&mut self, text: impl Into<String>, error: bool) {
        let text = self.redact_sensitive_text(&text.into());
        let text = bounded_text(&text, MAX_STORED_LOG_BYTES, TRUNCATED_LINE_SUFFIX, false);
        if text.trim().is_empty() {
            return;
        }
        self.logs.push_back(LogLine {
            at: Instant::now(),
            text,
            error,
        });
        while self.logs.len() > MAX_LOG_LINES {
            self.logs.pop_front();
        }
    }

    fn headless_command(&self, executable: &Path) -> Command {
        let mut command = Command::new(executable);
        command
            .arg("--headless")
            .arg("--json-events")
            .arg("--password-stdin")
            .arg("--pool")
            .arg(self.settings.pool.trim())
            .arg("--user")
            .arg(self.settings.user.trim())
            .arg("--workers")
            .arg(self.settings.workers.to_string())
            .arg("--reconnect-secs")
            .arg(self.settings.reconnect_secs.to_string())
            .arg("--report-secs")
            .arg(self.settings.report_secs.to_string());
        if self.memory_snapshot.is_none() && self.memory_acknowledged {
            command.arg("--confirm-randomx-memory");
        }
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    fn start(&mut self) {
        if self.is_running() {
            return;
        }
        if let Err(error) = self.settings.validate() {
            self.last_error.clone_from(&error);
            self.phase = Phase::Error;
            self.push_log(error, true);
            return;
        }
        self.refresh_memory_now();
        if let Some(error) = self.memory_preflight_error() {
            self.last_error.clone_from(&error);
            self.phase = Phase::Error;
            self.push_log(error, true);
            return;
        }
        let memory_preflight = self.memory_preflight_summary();
        if let Err(error) = save_settings(&self.settings) {
            self.push_log(error, true);
        }

        self.reset_telemetry();
        self.phase = Phase::Starting;
        self.requested_stop = false;
        self.started_at = Some(Instant::now());
        self.logs.clear();
        self.push_log("Starting isolated mining engine…", false);
        self.push_log(memory_preflight, false);

        let executable = match env::current_exe() {
            Ok(path) => path,
            Err(error) => {
                self.phase = Phase::Error;
                self.last_error = format!("Cannot locate miner executable: {error}");
                self.push_log(self.last_error.clone(), true);
                return;
            }
        };
        let mut command = self.headless_command(&executable);

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                self.phase = Phase::Error;
                self.last_error = format!("Cannot start mining engine: {error}");
                self.push_log(self.last_error.clone(), true);
                return;
            }
        };

        if let Some(mut stdin) = child.stdin.take() {
            if let Err(error) = stdin.write_all(self.settings.password.as_bytes()) {
                self.phase = Phase::Error;
                self.last_error = format!("Cannot pass credentials to mining engine: {error}");
                if let Err(cleanup_error) = terminate_and_reap_child(&mut child) {
                    self.last_error
                        .push_str(&format!(" Child cleanup failed: {cleanup_error}."));
                }
                self.push_log(self.last_error.clone(), true);
                return;
            }
        }

        let (sender, receiver) = mpsc::sync_channel(CHILD_OUTPUT_QUEUE_CAPACITY);
        let mut reader_threads = Vec::with_capacity(2);
        let mut reader_start_errors = Vec::new();
        if let Some(stdout) = child.stdout.take() {
            let tx = sender.clone();
            match thread::Builder::new()
                .name("xus-gui-telemetry".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stdout);
                    loop {
                        match read_bounded_child_line(&mut reader) {
                            Ok(Some(line)) => match serde_json::from_str::<Value>(&line) {
                                Ok(value) => {
                                    if tx.send(ProcessMessage::Telemetry(value)).is_err() {
                                        break;
                                    }
                                }
                                Err(_) => {
                                    if tx.send(ProcessMessage::Log(line)).is_err() {
                                        break;
                                    }
                                }
                            },
                            Ok(None) => break,
                            Err(error) => {
                                let _ = tx.send(ProcessMessage::Log(format!(
                                    "Telemetry channel failed: {error}"
                                )));
                                break;
                            }
                        }
                    }
                }) {
                Ok(handle) => reader_threads.push(handle),
                Err(error) => reader_start_errors
                    .push(format!("Cannot start the telemetry reader thread: {error}")),
            }
        } else {
            reader_start_errors.push("Mining engine stdout pipe is unavailable.".into());
        }
        if let Some(stderr) = child.stderr.take() {
            let tx = sender.clone();
            match thread::Builder::new()
                .name("xus-gui-engine-log".into())
                .spawn(move || {
                    let mut reader = BufReader::new(stderr);
                    loop {
                        match read_bounded_child_line(&mut reader) {
                            Ok(Some(line)) => {
                                if tx.send(ProcessMessage::Log(line)).is_err() {
                                    break;
                                }
                            }
                            Ok(None) => break,
                            Err(error) => {
                                let _ = tx.send(ProcessMessage::Log(format!(
                                    "Engine log channel failed: {error}"
                                )));
                                break;
                            }
                        }
                    }
                }) {
                Ok(handle) => reader_threads.push(handle),
                Err(error) => reader_start_errors.push(format!(
                    "Cannot start the engine-log reader thread: {error}"
                )),
            }
        } else {
            reader_start_errors.push("Mining engine stderr pipe is unavailable.".into());
        }
        drop(sender);

        if !reader_start_errors.is_empty() {
            let cleanup = terminate_and_reap_child(&mut child);
            drop(receiver);
            let reaped = cleanup.is_ok();
            if let Err(error) = cleanup {
                reader_start_errors.push(format!("Child cleanup failed: {error}."));
            }
            let mut detached_readers = 0_usize;
            for handle in reader_threads {
                if reaped || handle.is_finished() {
                    if handle.join().is_err() {
                        reader_start_errors
                            .push("An output reader panicked during startup cleanup.".into());
                    }
                } else {
                    detached_readers += 1;
                }
            }
            if detached_readers > 0 {
                reader_start_errors.push(format!(
                    "{detached_readers} output reader(s) could not be joined without blocking."
                ));
            }
            self.phase = Phase::Error;
            self.last_error = reader_start_errors.join(" ");
            self.push_log(self.last_error.clone(), true);
            return;
        }

        self.receiver = Some(receiver);
        self.reader_threads = reader_threads;
        self.child = Some(child);
    }

    fn stop(&mut self) {
        if self.child.is_none() {
            if let Some(mut child) = self.quarantined_child.take() {
                self.push_log("Retrying cleanup of the unobserved mining engine…", false);
                match terminate_and_reap_child(&mut child) {
                    Ok(status) => {
                        self.push_log(
                            format!(
                                "Previously unobserved mining engine is now stopped ({}).",
                                describe_exit_status(&status)
                            ),
                            false,
                        );
                    }
                    Err(error) => {
                        self.last_error =
                            format!("Mining engine cleanup is still unconfirmed: {error}");
                        self.push_log(self.last_error.clone(), true);
                        self.quarantined_child = Some(child);
                    }
                }
            }
            return;
        }
        let already_exited = match self.child.as_mut().expect("child checked above").try_wait() {
            Ok(status) => status,
            Err(error) => {
                self.begin_observation_failure(
                    "Cannot observe mining engine before stopping",
                    error,
                );
                return;
            }
        };
        if let Some(status) = already_exited {
            self.child = None;
            self.pending_exit = Some(status);
            return;
        }

        self.worker_error_can_restore_mining = false;
        self.node_link_live = false;
        self.phase = Phase::Stopping;
        self.push_log("Stopping mining engine and releasing worker memory…", false);
        match self.child.as_mut().expect("child checked above").kill() {
            Ok(()) => self.requested_stop = true,
            Err(error) => {
                self.engine_fatal_seen = true;
                self.last_error = format!("Could not stop mining engine: {error}");
                self.phase = Phase::Error;
                self.push_log(self.last_error.clone(), true);
            }
        }
    }

    fn apply_process_message(&mut self, message: ProcessMessage) {
        match message {
            ProcessMessage::Telemetry(value) => self.apply_telemetry(&value),
            ProcessMessage::Log(line) => {
                // Structured `metrics` telemetry already drives the dashboard.
                // Do not duplicate its two-second stderr summary into the event log.
                if line.starts_with("height ") && line.contains(" | total ") {
                    return;
                }
                let lower = line.to_ascii_lowercase();
                let error = lower.contains("error:")
                    || lower.contains("failed")
                    || lower.contains("submission rejected")
                    || lower.contains("share rejected #")
                    || lower.contains("session ended");
                self.push_log(line, error);
            }
        }
    }

    /// Processes a bounded amount of child output so a noisy engine cannot
    /// monopolize a GUI frame. `true` means every sender has exited and the
    /// bounded queue is empty.
    fn drain_process_messages(&mut self) -> bool {
        let Some(receiver) = self.receiver.take() else {
            return true;
        };
        let mut disconnected = false;
        for _ in 0..MAX_PROCESS_MESSAGES_PER_FRAME {
            match receiver.try_recv() {
                Ok(message) => self.apply_process_message(message),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    disconnected = true;
                    break;
                }
            }
        }
        self.receiver = Some(receiver);
        disconnected
    }

    fn join_reader_threads(&mut self) {
        for handle in std::mem::take(&mut self.reader_threads) {
            if handle.join().is_err() {
                self.push_log("A mining-engine output reader stopped unexpectedly.", true);
            }
        }
    }

    fn join_finished_reader_threads(&mut self) -> usize {
        let mut detached = 0_usize;
        for handle in std::mem::take(&mut self.reader_threads) {
            if handle.is_finished() {
                if handle.join().is_err() {
                    self.push_log("A mining-engine output reader stopped unexpectedly.", true);
                }
            } else {
                // Dropping a JoinHandle detaches the exceptional reader. This
                // path is only used after termination/reap failed and a bounded
                // drain grace expired; joining could otherwise hang the GUI.
                detached += 1;
            }
        }
        detached
    }

    fn begin_observation_failure(&mut self, context: &str, error: io::Error) {
        if self.pending_observation_failure.is_some() {
            return;
        }
        self.engine_fatal_seen = true;
        self.node_link_live = false;
        self.requested_stop = false;
        self.worker_error_can_restore_mining = false;
        self.phase = Phase::Error;
        self.last_error = format!("{context}: {error}");
        self.push_log(self.last_error.clone(), true);

        let cleanup_summary = match self.child.take() {
            None => "child handle was already unavailable".to_owned(),
            Some(mut child) => match terminate_and_reap_child(&mut child) {
                Ok(status) => format!("child cleanup observed {}", describe_exit_status(&status)),
                Err(cleanup_error) => {
                    let summary = format!("child cleanup incomplete: {cleanup_error}");
                    self.push_log(summary.clone(), true);
                    // Dropping a live Child handle would make Start available
                    // while the original memory-heavy engine may still exist.
                    // Quarantine it until a later nonblocking observation or
                    // explicit cleanup retry proves that it is gone.
                    self.quarantined_child = Some(child);
                    summary
                }
            },
        };
        self.pending_observation_failure = Some(PendingObservationFailure {
            report_detail: format!("{}; {cleanup_summary}", self.last_error),
            drain_deadline: Instant::now() + OBSERVATION_FAILURE_DRAIN_GRACE,
        });
    }

    fn finish_observation_failure(&mut self, report_detail: &str) {
        self.hashrate = 0.0;
        self.smoothed_hashrate = 0.0;
        self.last_metrics_at = None;
        self.clear_active_work();
        self.ready_worker_ids.clear();
        self.ready_workers = 0;
        self.output_disconnected_at = None;
        self.node_link_live = false;
        self.phase = Phase::Error;
        match self.write_crash_report(report_detail) {
            Ok(path) => self.push_log(
                format!("Crash report saved securely to {}.", path.display()),
                false,
            ),
            Err(error) => self.push_log(format!("Could not save crash report: {error}"), true),
        }
    }

    fn finish_exited_child(&mut self, status: ExitStatus) {
        self.worker_error_can_restore_mining = false;
        self.hashrate = 0.0;
        self.smoothed_hashrate = 0.0;
        self.last_metrics_at = None;
        self.clear_active_work();
        self.ready_worker_ids.clear();
        self.ready_workers = 0;
        self.output_disconnected_at = None;
        self.node_link_live = false;
        if self.requested_stop
            && !self.engine_fatal_seen
            && exit_status_matches_requested_stop(&status)
        {
            self.phase = Phase::Idle;
            self.push_log("Mining stopped.", false);
        } else {
            self.phase = Phase::Error;
            let detail = describe_exit_status(&status);
            self.last_error = format!("Mining engine exited unexpectedly: {detail}.");
            self.push_log(self.last_error.clone(), true);
            match self.write_crash_report(&detail) {
                Ok(path) => self.push_log(
                    format!("Crash report saved securely to {}.", path.display()),
                    false,
                ),
                Err(error) => self.push_log(format!("Could not save crash report: {error}"), true),
            }
        }
    }

    fn poll_process(&mut self, ctx: &egui::Context) {
        let had_output_receiver = self.receiver.is_some();
        let output_disconnected = self.drain_process_messages();

        if self.pending_exit.is_none() && self.pending_observation_failure.is_none() {
            let observation = self.child.as_mut().map(Child::try_wait);
            match observation {
                Some(Ok(Some(status))) => {
                    self.child = None;
                    self.node_link_live = false;
                    self.pending_exit = Some(status);
                }
                Some(Err(error)) => {
                    self.begin_observation_failure("Cannot observe mining engine", error);
                }
                Some(Ok(None)) | None => {}
            }
        }

        let observation_drain_timed_out = self
            .pending_observation_failure
            .as_ref()
            .is_some_and(|failure| Instant::now() >= failure.drain_deadline);
        if self.pending_observation_failure.is_some()
            && (output_disconnected || observation_drain_timed_out)
        {
            if output_disconnected {
                self.join_reader_threads();
            } else {
                self.receiver = None;
                let detached = self.join_finished_reader_threads();
                self.push_log(
                    format!(
                        "Crash report output drain timed out; {detached} reader thread(s) were detached and the final tail may be incomplete."
                    ),
                    true,
                );
            }
            self.receiver = None;
            let failure = self
                .pending_observation_failure
                .take()
                .expect("observation failure checked above");
            self.finish_observation_failure(&failure.report_detail);
        }

        let quarantined_exit = self
            .quarantined_child
            .as_mut()
            .and_then(|child| child.try_wait().ok().flatten());
        if let Some(status) = quarantined_exit {
            self.quarantined_child = None;
            self.push_log(
                format!(
                    "Previously unobserved mining engine exit is now confirmed ({}); restart is safe.",
                    describe_exit_status(&status)
                ),
                false,
            );
        }

        // Once the process exits, the reader threads drain the pipe tail into
        // the bounded channel. Only snapshot a crash report after all senders
        // are gone and the queued tail has been processed.
        if output_disconnected {
            if let Some(status) = self.pending_exit.take() {
                self.join_reader_threads();
                self.receiver = None;
                self.finish_exited_child(status);
            }
        }

        // Either reader ending before the child is waitable is suspicious, but
        // pipe EOF can lead process waitability by a few scheduler ticks. Give
        // normal/requested exits a short grace window before failing closed.
        let reader_ended = output_disconnected
            || self
                .reader_threads
                .iter()
                .any(thread::JoinHandle::is_finished);
        if self.pending_exit.is_none() && self.child.is_some() && reader_ended {
            let ended_at = self.output_disconnected_at.get_or_insert_with(Instant::now);
            if ended_at.elapsed() >= OUTPUT_DISCONNECT_GRACE
                && !self.requested_stop
                && !self.engine_fatal_seen
            {
                self.engine_fatal_seen = true;
                self.node_link_live = false;
                self.worker_error_can_restore_mining = false;
                self.phase = Phase::Error;
                self.last_error =
                    "A mining engine output reader ended while the engine was still running."
                        .into();
                self.push_log(self.last_error.clone(), true);
                if let Err(error) = self.child.as_mut().expect("child checked above").kill() {
                    self.push_log(
                        format!("Could not stop the unobserved mining engine: {error}"),
                        true,
                    );
                }
            }
        } else if had_output_receiver && !reader_ended {
            self.output_disconnected_at = None;
        }

        if self.is_running() {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
    }

    fn apply_telemetry(&mut self, value: &Value) {
        let event = value.get("event").and_then(Value::as_str);
        // Fatal is terminal for this child. Reader threads may still deliver
        // telemetry that raced with the fatal event, but it must not repaint
        // the stopped engine as healthy or repopulate cleared work.
        if self.engine_fatal_seen || (self.requested_stop && event != Some("engine_fatal")) {
            return;
        }
        match event {
            Some("startup") => {
                self.node_link_live = false;
                self.enter_connection_phase(Phase::Connecting);
                self.push_log("Mining engine online.", false);
            }
            Some("connecting") => {
                self.node_link_live = false;
                self.enter_connection_phase(Phase::Connecting);
            }
            Some("pool_connected") => {
                self.enter_connection_phase(Phase::Authenticating);
                self.push_log("TCP connection established; authenticating…", false);
            }
            Some("node_connected") => {
                self.node_link_live = true;
                self.enter_mining_phase();
                self.push_log(
                    "Connected directly to SOV node RPC; requesting work…",
                    false,
                );
            }
            Some("worker_initializing") => {
                if let Some(worker) = value.get("worker").and_then(Value::as_u64) {
                    self.mark_worker_unready(worker);
                    self.push_log(
                        format!(
                            "Worker {} is preparing RandomX (shared {RANDOMX_DATASET_GIB:.1} GiB dataset; ~{RANDOMX_WORKER_MIB:.0} MiB worker overhead)…",
                            worker + 1,
                        ),
                        false,
                    );
                }
            }
            Some("worker_ready") => {
                if let Some(worker) = value.get("worker").and_then(Value::as_u64) {
                    if self.mark_worker_ready(worker) {
                        self.push_log(format!("Worker {} ready; hashing.", worker + 1), false);
                    }
                }
            }
            Some("worker_error") => {
                if let Some(worker) = value
                    .get("worker")
                    .and_then(Value::as_u64)
                    .filter(|worker| *worker < self.settings.workers)
                {
                    let message = value
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("proof-of-work worker failed");
                    let message = self.redact_sensitive_text(message);
                    if self.worker_errors.is_empty() {
                        self.worker_error_can_restore_mining = matches!(self.phase, Phase::Mining);
                    }
                    self.mark_worker_unready(worker);
                    self.worker_errors.insert(worker, message.clone());
                    self.phase = Phase::Error;
                    self.last_error = format!("Worker {} error: {message}", worker + 1);
                    self.push_log(self.last_error.clone(), true);
                }
            }
            Some("worker_recovered") => {
                if let Some(worker) = value.get("worker").and_then(Value::as_u64) {
                    if self.mark_worker_ready(worker) {
                        self.push_log(format!("Worker {} recovered; hashing.", worker + 1), false);
                    }
                }
            }
            Some("engine_fatal") => {
                self.engine_fatal_seen = true;
                self.node_link_live = false;
                self.worker_error_can_restore_mining = false;
                self.phase = Phase::Error;
                self.hashrate = 0.0;
                self.smoothed_hashrate = 0.0;
                self.last_metrics_at = None;
                self.clear_active_work();
                self.ready_worker_ids.clear();
                self.ready_workers = 0;
                self.last_error = value.get("message").and_then(Value::as_str).map_or_else(
                    || "Mining engine reported a fatal error.".to_owned(),
                    |message| self.redact_sensitive_text(message),
                );
                self.push_log(self.last_error.clone(), true);
            }
            Some("job") => {
                self.node_link_live = true;
                self.enter_mining_phase();
                let next_height = value.get("height").and_then(Value::as_u64);
                if next_height.is_some() && next_height != self.height {
                    self.round_hashes = 0;
                    self.round_probability = Some(0.0);
                    self.height_started_at = Some(Instant::now());
                }
                self.height = next_height;
                self.algorithm = value
                    .get("algorithm")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                self.job_id = value
                    .get("job_id")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown")
                    .to_owned();
                // A reporter sample can be assembled just before this event but
                // reach stdout just after it. Until metrics confirms this job,
                // require two jobless samples before clearing it so one raced
                // `height: null` sample cannot erase newer work.
                self.job_observed_in_metrics = false;
                self.jobless_metrics_seen = 0;
                self.coinbase = value
                    .get("coinbase")
                    .and_then(Value::as_str)
                    .filter(|account| !account.is_empty())
                    .map(str::to_owned);
                self.expected_hashes = value
                    .get("expected_hashes")
                    .and_then(Value::as_f64)
                    .filter(|expected| expected.is_finite() && *expected > 0.0);
                if self.expected_hashes.is_none() {
                    self.round_probability = None;
                }
            }
            Some("job_cleared") => {
                let cleared_job_id = value.get("job_id").and_then(Value::as_str);
                let cleared_height = value.get("height").and_then(Value::as_u64);
                if let Some(height) = cleared_height.filter(|height| {
                    cleared_job_id == Some(self.job_id.as_str()) && self.height == Some(*height)
                }) {
                    if value.get("reason").and_then(Value::as_str) == Some("accepted_block") {
                        let destination = self
                            .coinbase
                            .as_deref()
                            .map_or_else(String::new, |account| format!(" to {account}"));
                        self.last_found_block = Some((height, Instant::now()));
                        self.push_log(
                            format!("★ BLOCK FOUND AND ACCEPTED at height {height}{destination}."),
                            false,
                        );
                    }
                    self.clear_active_work();
                }
            }
            Some("network") => {
                self.network_hashrate = value
                    .get("hashrate")
                    .and_then(Value::as_f64)
                    .filter(|rate| rate.is_finite() && *rate >= 0.0);
                self.target_block_ms = value.get("target_block_ms").and_then(Value::as_u64);
                self.network_difficulty = value
                    .get("difficulty")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
            }
            Some("peers") => {
                let next = value.get("count").and_then(Value::as_u64);
                if next != self.peer_count {
                    if let Some(count) = next {
                        self.push_log(
                            format!(
                                "SOV node reports {count} authenticated P2P peer{}.",
                                if count == 1 { "" } else { "s" }
                            ),
                            false,
                        );
                    }
                }
                self.peer_count = next;
            }
            Some("metrics") => {
                let now = Instant::now();
                let elapsed = self
                    .last_metrics_at
                    .map_or(self.settings.report_secs as f64, |last| {
                        now.saturating_duration_since(last).as_secs_f64()
                    });
                self.last_metrics_at = Some(now);
                self.hashrate = value.get("hashrate").and_then(Value::as_f64).unwrap_or(0.0);
                let alpha = 1.0 - (-elapsed / 8.0).exp();
                self.smoothed_hashrate = if self.smoothed_hashrate <= 0.0 && self.hashrate > 0.0 {
                    self.hashrate
                } else {
                    self.smoothed_hashrate + alpha * (self.hashrate - self.smoothed_hashrate)
                }
                .max(0.0);
                self.total_hashes = value
                    .get("total_hashes")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.total_hashes);
                self.submitted = value
                    .get("submitted")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.submitted);
                self.accepted = value
                    .get("accepted")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.accepted);
                self.rejected = value
                    .get("rejected")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.rejected);
                let metrics_height = value.get("height").and_then(Value::as_u64);
                if self.height.is_none() {
                    self.height = metrics_height;
                }
                match metrics_height {
                    Some(height)
                        if self.height == Some(height) && self.job_id != WAITING_FOR_WORK =>
                    {
                        self.job_observed_in_metrics = true;
                        self.jobless_metrics_seen = 0;
                    }
                    None if value.get("height").is_some_and(Value::is_null)
                        && self.job_id != WAITING_FOR_WORK =>
                    {
                        self.jobless_metrics_seen = self.jobless_metrics_seen.saturating_add(1);
                        if self.job_observed_in_metrics || self.jobless_metrics_seen >= 2 {
                            self.clear_active_work();
                        }
                    }
                    _ => {}
                }
                let round_height = value.get("round_height").and_then(Value::as_u64);
                if round_height == metrics_height && metrics_height == self.height {
                    self.round_hashes = value
                        .get("round_hashes")
                        .and_then(Value::as_u64)
                        .unwrap_or(self.round_hashes);
                    self.round_probability = self.expected_hashes.and_then(|_| {
                        value
                            .get("round_probability")
                            .and_then(Value::as_f64)
                            .filter(|probability| probability.is_finite())
                            .map(|probability| probability.clamp(0.0, 1.0))
                    });
                }
                self.mempool_size = value.get("mempool_size").and_then(Value::as_u64);
                self.peer_count = value.get("peer_count").and_then(Value::as_u64);
                self.meter_history.push_back(MeterSample {
                    hashrate: self.hashrate,
                    smoothed_hashrate: self.smoothed_hashrate,
                    mempool: self.mempool_size,
                    height: self.height,
                    round_probability: self.round_probability,
                });
                while self.meter_history.len() > MAX_METER_SAMPLES {
                    self.meter_history.pop_front();
                }
            }
            Some("share") => {
                self.submitted = value
                    .get("submitted")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.submitted);
                self.accepted = value
                    .get("accepted")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.accepted);
                self.rejected = value
                    .get("rejected")
                    .and_then(Value::as_u64)
                    .unwrap_or(self.rejected);
            }
            Some("session_error") => {
                self.node_link_live = false;
                self.worker_error_can_restore_mining = false;
                self.hashrate = 0.0;
                self.smoothed_hashrate = 0.0;
                self.last_metrics_at = None;
                self.clear_active_work();
                self.mempool_size = None;
                self.peer_count = None;
                let session_error = value.get("message").and_then(Value::as_str).map_or_else(
                    || "Pool session ended.".to_owned(),
                    |message| self.redact_sensitive_text(message),
                );
                if !self.engine_fatal_seen {
                    self.phase = Phase::Reconnecting;
                    self.last_error = session_error;
                }
            }
            _ => {}
        }
    }

    fn solo_block_eta_seconds(&self) -> Option<f64> {
        if !self.telemetry_is_fresh()
            || !self.node_link_live
            || self.engine_fatal_seen
            || self.requested_stop
            || self.ready_workers == 0
        {
            return None;
        }
        let expected = self.expected_hashes?;
        (self.smoothed_hashrate > 0.0).then_some(expected / self.smoothed_hashrate)
    }

    fn current_round_probability(&self) -> Option<f64> {
        self.expected_hashes?;
        (self.telemetry_is_fresh()
            && self.node_link_live
            && !self.engine_fatal_seen
            && !self.requested_stop
            && self.ready_workers > 0)
            .then_some(self.round_probability)
            .flatten()
    }

    fn current_round_elapsed(&self) -> Option<Duration> {
        (self.telemetry_is_fresh()
            && self.node_link_live
            && !self.engine_fatal_seen
            && !self.requested_stop
            && self.ready_workers > 0)
            .then(|| self.height_started_at.map(|started| started.elapsed()))
            .flatten()
    }

    fn telemetry_is_fresh(&self) -> bool {
        self.last_metrics_at.is_some_and(|at| {
            at.elapsed() < Duration::from_secs(self.settings.report_secs.saturating_mul(3).max(8))
        })
    }

    fn uptime(&self) -> String {
        if !self.is_running() {
            return "00:00:00".into();
        }
        let Some(started) = self.started_at else {
            return "00:00:00".into();
        };
        let seconds = started.elapsed().as_secs();
        format!(
            "{:02}:{:02}:{:02}",
            seconds / 3_600,
            seconds / 60 % 60,
            seconds % 60
        )
    }

    fn formatted_logs(&self) -> String {
        let origin = self.started_at;
        self.logs
            .iter()
            .map(|line| {
                let seconds = origin
                    .map(|start| line.at.saturating_duration_since(start).as_secs())
                    .unwrap_or(0);
                format!("[{:02}:{:02}] {}", seconds / 60, seconds % 60, line.text)
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn crash_report_text(&self, exit_detail: &str) -> String {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let header = format!(
            "XUS MINER CRASH REPORT\n\
             version: {}\n\
             platform: {} {}\n\
             timestamp_unix: {timestamp}\n\
             engine_exit: {exit_detail}\n\
             endpoint: {}\n\
             configured_workers: {}\n\
             phase: {}\n\
             \nRECENT ENGINE LOG (up to the last {} bounded lines)\n",
            env!("CARGO_PKG_VERSION"),
            env::consts::OS,
            env::consts::ARCH,
            sanitize_endpoint(&self.settings.pool),
            self.settings.workers,
            self.phase.label(),
            MAX_CRASH_REPORT_LOG_LINES,
        );
        let mut report = self.redact_sensitive_text(&header);
        let origin = self.started_at;
        let omission_marker = "[older engine log lines omitted to keep this report bounded]\n";
        let mut remaining = MAX_CRASH_REPORT_BYTES
            .saturating_sub(report.len())
            .saturating_sub(omission_marker.len());
        let mut recent_lines = Vec::new();
        let mut omitted = self.logs.len() > MAX_CRASH_REPORT_LOG_LINES;
        for line in self.logs.iter().rev().take(MAX_CRASH_REPORT_LOG_LINES) {
            let seconds = origin
                .map(|start| line.at.saturating_duration_since(start).as_secs())
                .unwrap_or(0);
            let formatted = format!("[{:02}:{:02}] {}\n", seconds / 60, seconds % 60, line.text);
            let formatted = self.redact_sensitive_text(&formatted);
            if formatted.len() > remaining {
                omitted = true;
                break;
            }
            remaining -= formatted.len();
            recent_lines.push(formatted);
        }
        for line in recent_lines.iter().rev() {
            report.push_str(line);
        }
        if omitted {
            report.push_str(omission_marker);
        }

        bounded_text(
            &report,
            MAX_CRASH_REPORT_BYTES,
            TRUNCATED_REPORT_SUFFIX,
            report.len() > MAX_CRASH_REPORT_BYTES,
        )
    }

    fn write_crash_report(&self, exit_detail: &str) -> Result<PathBuf, String> {
        let directory = settings_path()
            .and_then(|path| path.parent().map(Path::to_path_buf))
            .ok_or_else(|| "cannot locate the user settings directory".to_string())?;
        write_crash_report_at(&directory, &self.crash_report_text(exit_detail))
    }

    fn diagnostics_text(&self) -> String {
        let mempool = self
            .mempool_size
            .map_or_else(|| "unavailable".into(), |size| size.to_string());
        let peers = self
            .peer_count
            .map_or_else(|| "unavailable".into(), |count| count.to_string());
        let system_memory = self.memory_snapshot.map_or_else(
            || "unavailable".into(),
            |snapshot| {
                format!(
                    "{} available / {} total",
                    format_memory(snapshot.available),
                    format_memory(snapshot.total),
                )
            },
        );
        let height = self
            .height
            .map_or_else(|| "unavailable".into(), |height| height.to_string());
        let last_error = if self.last_error.is_empty() {
            "none"
        } else {
            &self.last_error
        };
        let block_eta = self
            .solo_block_eta_seconds()
            .map(format_eta)
            .unwrap_or_else(|| "unavailable".into());
        let round_chance = self
            .current_round_probability()
            .map(format_probability)
            .unwrap_or_else(|| "unavailable".into());
        let diagnostics = format!(
            "XUS MINER DIAGNOSTICS\n\
             phase: {}\n\
             node link: {}\n\
             endpoint: {}\n\
             algorithm: {}\n\
             height: {}\n\
             job: {}\n\
             coinbase (template confirmed): {}\n\
             hashrate: {}\n\
             smoothed hashrate: {}\n\
             expected solo block time: {}\n\
             current round probability: {}\n\
             expected hashes per block: {}\n\
             network hashrate: {}\n\
             target block cadence: {}\n\
             network difficulty: {}\n\
             total hashes: {}\n\
             workers ready/configured: {}/{}\n\
             system memory: {}\n\
             authenticated node peers: {}\n\
             mempool pending: {}\n\
             submitted/accepted/rejected: {}/{}/{}\n\
             uptime: {}\n\
             last error: {}\n\
             \nEVENT LOG\n{}",
            self.phase.label(),
            if self.node_link_live {
                "connected"
            } else {
                "disconnected"
            },
            sanitize_endpoint(&self.settings.pool),
            self.algorithm,
            height,
            self.job_id,
            self.coinbase.as_deref().unwrap_or("not disclosed"),
            format_hashrate(self.hashrate),
            format_hashrate(self.smoothed_hashrate),
            block_eta,
            round_chance,
            self.expected_hashes
                .map(format_count_f64)
                .unwrap_or_else(|| "unavailable".into()),
            self.network_hashrate
                .map(format_hashrate)
                .unwrap_or_else(|| "unavailable".into()),
            self.target_block_ms
                .map(|ms| format_eta(ms as f64 / 1_000.0))
                .unwrap_or_else(|| "unavailable".into()),
            self.network_difficulty.as_deref().unwrap_or("unavailable"),
            self.total_hashes,
            self.ready_workers,
            self.settings.workers,
            system_memory,
            peers,
            mempool,
            self.submitted,
            self.accepted,
            self.rejected,
            self.uptime(),
            last_error,
            self.formatted_logs(),
        );
        self.redact_sensitive_text(&diagnostics)
    }

    fn engine_log_panel(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(RichText::new("ENGINE LOG").size(11.0).strong().color(MUTED));
            ui.label(
                RichText::new("drag the top edge to resize • select text normally")
                    .size(9.0)
                    .color(MUTED),
            );
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button("Clear").clicked() {
                    self.logs.clear();
                }
                if ui.small_button("Copy diagnostics").clicked() {
                    ui.ctx().copy_text(self.diagnostics_text());
                }
            });
        });
        ui.separator();

        egui::ScrollArea::vertical()
            .id_salt("engine-log-scroll-v2")
            .stick_to_bottom(true)
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if self.logs.is_empty() {
                    ui.add(
                        egui::Label::new(
                            RichText::new("Engine events will appear here.")
                                .monospace()
                                .size(11.0)
                                .color(MUTED),
                        )
                        .selectable(true),
                    );
                    return;
                }

                let origin = self.started_at;
                let mut job = egui::text::LayoutJob::default();
                for (index, line) in self.logs.iter().enumerate() {
                    let seconds = origin
                        .map(|start| line.at.saturating_duration_since(start).as_secs())
                        .unwrap_or(0);
                    if index > 0 {
                        job.append("\n", 0.0, egui::TextFormat::default());
                    }
                    job.append(
                        &format!("[{:02}:{:02}] {}", seconds / 60, seconds % 60, line.text),
                        0.0,
                        egui::TextFormat {
                            font_id: FontId::monospace(11.0),
                            color: if line.error {
                                RED
                            } else if line.text.contains("COINBASE CONFIRMED") {
                                GREEN
                            } else {
                                MUTED
                            },
                            ..Default::default()
                        },
                    );
                }
                ui.add(egui::Label::new(job).selectable(true).wrap());
            });
    }

    fn controls(&mut self, ui: &mut egui::Ui) {
        ui.add_space(12.0);
        ui.label(
            RichText::new("REWARD ROUTE")
                .size(11.0)
                .color(MUTED)
                .strong(),
        );
        ui.add_space(6.0);
        ui.add_enabled_ui(!self.is_running(), |ui| {
            ui.selectable_value(
                &mut self.settings.reward_route,
                RewardRoute::OwnedBridge,
                "My bridge / my coinbase",
            );
            ui.selectable_value(
                &mut self.settings.reward_route,
                RewardRoute::ExternalPool,
                "External pool / contributor",
            );
        });
        ui.add_space(7.0);
        let direct_node = is_direct_node_endpoint(&self.settings.pool);
        let (callout, callout_color) = if direct_node {
            (
                "Direct node mode: the miner reads the coinbase from each block template and refuses any requested-account mismatch. No wallet password or private key is used.",
                GREEN,
            )
        } else {
            match self.settings.reward_route {
            RewardRoute::OwnedBridge => (
                "The bridge—not this miner—sets the public coinbase account. Keep its spending keys in SOV Station.",
                GREEN,
            ),
            RewardRoute::ExternalPool => (
                "Current SOV Stratum has no automatic payout layer. A worker label does not earn XUS by itself.",
                AMBER,
            ),
            }
        };
        Frame::new()
            .fill(callout_color.gamma_multiply(0.08))
            .stroke(Stroke::new(1.0_f32, callout_color.gamma_multiply(0.35)))
            .corner_radius(CornerRadius::same(8))
            .inner_margin(Margin::same(9))
            .show(ui, |ui| {
                ui.label(RichText::new(callout).size(10.0).color(callout_color));
            });

        ui.add_space(16.0);
        ui.label(RichText::new("CONNECTION").size(11.0).color(MUTED).strong());
        ui.add_space(8.0);
        let running = self.is_running();
        ui.add_enabled(
            !running,
            egui::TextEdit::singleline(&mut self.settings.pool)
                .hint_text(DEFAULT_POOL)
                .desired_width(240.0),
        )
        .on_hover_text("SOV node RPC (normally :8645) or Stratum pool/bridge address");
        ui.add_space(8.0);
        ui.add_enabled(
            !running,
            egui::TextEdit::singleline(&mut self.settings.user)
                .hint_text(DEFAULT_USER)
                .desired_width(240.0),
        )
        .on_hover_text(
            "Direct node: public coinbase account, or xus-miner to use the node-configured account. Stratum: worker label.",
        );
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            let mut edit = egui::TextEdit::singleline(&mut self.settings.password)
                .hint_text("Stratum password")
                .desired_width(ui.available_width() - 42.0);
            if !self.reveal_password {
                edit = edit.password(true);
            }
            ui.add_enabled(!running, edit);
            if ui
                .add_enabled(
                    !running,
                    egui::Button::new(if self.reveal_password { "Hide" } else { "Show" }),
                )
                .clicked()
            {
                self.reveal_password = !self.reveal_password;
            }
        });
        ui.label(
            RichText::new(if direct_node {
                "Unused in direct node mode; no wallet credential is sent."
            } else {
                "Password is passed over stdin and never saved."
            })
            .size(10.0)
            .color(MUTED),
        );

        ui.add_space(14.0);
        ui.label(RichText::new("COMPUTE").size(11.0).color(MUTED).strong());
        ui.add_space(8.0);
        ui.horizontal(|ui| {
            ui.label("CPU workers");
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let response = ui.add_enabled(
                    !running,
                    egui::DragValue::new(&mut self.settings.workers)
                        .range(1..=64)
                        .speed(0.1),
                );
                if response.changed() {
                    self.memory_acknowledged = false;
                }
            });
        });
        ui.add(
            egui::ProgressBar::new(self.settings.workers as f32 / 64.0)
                .fill(PURPLE)
                .desired_width(240.0)
                .show_percentage(),
        );
        ui.label(
            RichText::new(format!(
                "RandomX ≈ {} total • shared {RANDOMX_DATASET_GIB:.1} GiB + {:.0} MiB workers",
                format_memory(randomx_memory_bytes(self.settings.workers)),
                self.settings.workers as f64 * RANDOMX_WORKER_MIB,
            ))
            .size(10.0)
            .color(MUTED),
        );
        ui.add_space(5.0);
        if let Some(snapshot) = self.memory_snapshot {
            let reserve = memory_headroom_bytes(snapshot.total);
            let safe_workers = recommended_worker_limit(snapshot);
            let has_capacity =
                snapshot.available >= randomx_memory_bytes(self.settings.workers) + reserve;
            let color = if running {
                CYAN
            } else if has_capacity {
                GREEN
            } else {
                RED
            };
            Frame::new()
                .fill(color.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0, color.gamma_multiply(0.42)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(9))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("SYSTEM MEMORY")
                            .size(9.0)
                            .strong()
                            .color(MUTED),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} available / {} total",
                            format_memory(snapshot.available),
                            format_memory(snapshot.total),
                        ))
                        .size(12.0)
                        .strong()
                        .color(color),
                    );
                    ui.label(
                        RichText::new(if running {
                            "Available now, after miner allocation".into()
                        } else {
                            format!(
                                "Safe preflight: up to {} worker{}",
                                safe_workers,
                                if safe_workers == 1 { "" } else { "s" }
                            )
                        })
                        .size(9.5)
                        .color(TEXT),
                    );
                    ui.label(
                        RichText::new(format!(
                            "{} OS/app reserve • RAM-only scan every 10s",
                            format_memory(reserve),
                        ))
                        .size(8.5)
                        .color(MUTED),
                    );
                });
        } else {
            Frame::new()
                .fill(AMBER.gamma_multiply(0.08))
                .stroke(Stroke::new(1.0, AMBER.gamma_multiply(0.42)))
                .corner_radius(CornerRadius::same(8))
                .inner_margin(Margin::same(9))
                .show(ui, |ui| {
                    ui.label(
                        RichText::new("SYSTEM MEMORY • SCAN UNAVAILABLE")
                            .size(9.0)
                            .strong()
                            .color(AMBER),
                    );
                    ui.add_enabled(
                        !running,
                        egui::Checkbox::new(
                            &mut self.memory_acknowledged,
                            format!(
                                "I confirm ≥ {:.1} GiB available",
                                randomx_memory_bytes(self.settings.workers) as f64 / BYTES_PER_GIB
                                    + MIN_MEMORY_HEADROOM_GIB,
                            ),
                        ),
                    );
                    ui.label(
                        RichText::new("Fallback confirmation is never saved.")
                            .size(8.5)
                            .color(MUTED),
                    );
                });
        }

        ui.add_space(14.0);
        egui::CollapsingHeader::new("Advanced")
            .default_open(false)
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label("Reconnect delay");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_enabled(
                            !running,
                            egui::DragValue::new(&mut self.settings.reconnect_secs)
                                .range(1..=3_600)
                                .suffix(" s"),
                        );
                    });
                });
                ui.horizontal(|ui| {
                    ui.label("Telemetry interval");
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.add_enabled(
                            !running,
                            egui::DragValue::new(&mut self.settings.report_secs)
                                .range(1..=3_600)
                                .suffix(" s"),
                        );
                    });
                });
            });

        ui.add_space(14.0);
        let action = match self.settings.reward_route {
            RewardRoute::OwnedBridge => "▶  START MINING",
            RewardRoute::ExternalPool => "▶  START CONTRIBUTING",
        };
        let button = if running {
            egui::Button::new(RichText::new("■  STOP MINING").strong().color(TEXT))
                .fill(Color32::from_rgb(102, 42, 55))
        } else {
            egui::Button::new(RichText::new(action).strong().color(TEXT))
                .fill(Color32::from_rgb(20, 126, 133))
        };
        if ui.add_sized([ui.available_width(), 44.0], button).clicked() {
            if running {
                self.stop();
            } else {
                self.start();
            }
        }
    }

    fn header(&self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(42.0), Sense::hover());
            ui.painter().circle_filled(rect.center(), 20.0, PURPLE);
            ui.painter().text(
                rect.center(),
                egui::Align2::CENTER_CENTER,
                "X",
                FontId::proportional(23.0),
                TEXT,
            );
            ui.add_space(8.0);
            ui.vertical(|ui| {
                ui.label(RichText::new("XUS MINER").size(21.0).strong().color(TEXT));
                ui.label(
                    RichText::new("SOV external proof-of-work console")
                        .size(11.0)
                        .color(MUTED),
                );
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                Frame::new()
                    .fill(self.phase.color().gamma_multiply(0.13))
                    .stroke(Stroke::new(
                        1.0_f32,
                        self.phase.color().gamma_multiply(0.55),
                    ))
                    .corner_radius(CornerRadius::same(20))
                    .inner_margin(Margin::symmetric(14, 7))
                    .show(ui, |ui| {
                        ui.label(
                            RichText::new(self.phase.label())
                                .size(11.0)
                                .strong()
                                .color(self.phase.color()),
                        );
                    });
            });
        });
    }

    fn status_rail(&self, ui: &mut egui::Ui) {
        let metrics_fresh = self.telemetry_is_fresh();
        let connected = self.node_link_live && !self.engine_fatal_seen && !self.requested_stop;
        let has_job = connected && self.height.is_some() && self.job_id != WAITING_FOR_WORK;
        let hashing = has_job && metrics_fresh && self.hashrate > 0.0 && self.ready_workers > 0;
        let degraded = hashing && !self.worker_errors.is_empty();
        let recently_found = self
            .last_found_block
            .filter(|(_, found_at)| found_at.elapsed() <= BLOCK_FOUND_HIGHLIGHT);
        let pulse_time = ui.input(|input| input.time);

        ui.columns(4, |columns| {
            heartbeat_card(
                &mut columns[0],
                "NODE LINK",
                if connected {
                    "CONNECTED"
                } else {
                    self.phase.label()
                },
                if connected {
                    "RPC telemetry live"
                } else {
                    "Waiting for node"
                },
                if connected { GREEN } else { self.phase.color() },
                connected,
                pulse_time,
            );
            heartbeat_card(
                &mut columns[1],
                "PROOF OF WORK",
                if degraded {
                    "HASHING • DEGRADED"
                } else if hashing {
                    "HASHING"
                } else if connected {
                    "WARMING"
                } else {
                    "OFFLINE"
                },
                if hashing {
                    format!(
                        "{} • {}/{} workers",
                        format_hashrate(self.hashrate),
                        self.ready_workers,
                        self.settings.workers,
                    )
                } else if connected {
                    "Preparing workers".into()
                } else {
                    "No active engine".into()
                },
                if degraded {
                    AMBER
                } else if hashing {
                    CYAN
                } else if connected {
                    AMBER
                } else {
                    RED
                },
                hashing,
                pulse_time + 0.17,
            );
            heartbeat_card(
                &mut columns[2],
                "JOB FEED",
                if recently_found.is_some() {
                    "★ BLOCK FOUND"
                } else if has_job {
                    "ACTIVE"
                } else {
                    "WAITING"
                },
                recently_found.map_or_else(
                    || {
                        self.height.map_or_else(
                            || "No current template".into(),
                            |height| {
                                format!(
                                    "Block #{height} • {}",
                                    self.solo_block_eta_seconds()
                                        .map(format_eta)
                                        .unwrap_or_else(|| {
                                            if self.expected_hashes.is_none() {
                                                "ETA unavailable".into()
                                            } else {
                                                "ETA locking".into()
                                            }
                                        })
                                )
                            },
                        )
                    },
                    |(height, _)| {
                        self.height.map_or_else(
                            || format!("Accepted block #{height} • reward submitted"),
                            |current| format!("Accepted block #{height} • now mining #{current}"),
                        )
                    },
                ),
                if recently_found.is_some() {
                    GREEN
                } else if has_job {
                    PURPLE
                } else {
                    AMBER
                },
                has_job || recently_found.is_some(),
                pulse_time + 0.34,
            );
            let pending = self.mempool_size.unwrap_or(0);
            heartbeat_card(
                &mut columns[3],
                "TX QUEUE",
                if pending > 0 { "TRANSACTIONS" } else { "CLEAR" },
                self.mempool_size
                    .map(|size| format!("{size} pending for mining"))
                    .unwrap_or_else(|| "Awaiting node data".into()),
                if pending > 0 { GREEN } else { MUTED },
                connected && self.mempool_size.is_some(),
                pulse_time + 0.51,
            );
        });
    }

    fn dashboard(&mut self, ui: &mut egui::Ui) {
        self.header(ui);
        ui.add_space(14.0);
        self.status_rail(ui);
        ui.add_space(12.0);
        ui.columns(4, |columns| {
            metric_card(
                &mut columns[0],
                "HASHRATE",
                &format_hashrate(self.hashrate),
                CYAN,
            );
            metric_card(
                &mut columns[1],
                "CHAIN HEIGHT",
                &self
                    .height
                    .map(|height| format!("#{height}"))
                    .unwrap_or_else(|| "—".into()),
                PURPLE,
            );
            metric_card(
                &mut columns[2],
                "SOLO BLOCK ETA",
                &self
                    .solo_block_eta_seconds()
                    .map(format_eta)
                    .unwrap_or_else(|| {
                        if self.height.is_some() && self.expected_hashes.is_none() {
                            "N/A (POOL)".into()
                        } else {
                            "LOCKING…".into()
                        }
                    }),
                GREEN,
            );
            metric_card(&mut columns[3], "UPTIME", &self.uptime(), AMBER);
        });

        ui.add_space(12.0);
        Frame::new()
            .fill(CARD)
            .stroke(Stroke::new(1.0_f32, BORDER))
            .corner_radius(CornerRadius::same(12))
            .inner_margin(Margin::same(16))
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(
                        RichText::new("LIVE MINING METERS")
                            .size(11.0)
                            .strong()
                            .color(MUTED),
                    );
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!(
                                "{} total hashes",
                                format_count(self.total_hashes)
                            ))
                            .size(11.0)
                            .color(MUTED),
                        );
                    });
                });
                ui.add_space(8.0);
                let eta = self.solo_block_eta_seconds();
                let probability = self.current_round_probability();
                ui.columns(4, |columns| {
                    meter_readout(
                        &mut columns[0],
                        "SMOOTHED HASHRATE",
                        &format_hashrate(self.smoothed_hashrate),
                        "8-second signal lock",
                        CYAN,
                    );
                    meter_readout(
                        &mut columns[1],
                        "EXPECTED SOLO FIND",
                        &eta.map(format_eta).unwrap_or_else(|| "—".into()),
                        "probabilistic mean",
                        GREEN,
                    );
                    meter_readout(
                        &mut columns[2],
                        "50% SOLO WINDOW",
                        &eta.map(|seconds| format_eta(seconds * std::f64::consts::LN_2))
                            .unwrap_or_else(|| "—".into()),
                        "median at current rate",
                        PURPLE,
                    );
                    meter_readout(
                        &mut columns[3],
                        "CURRENT ROUND CHANCE",
                        &probability
                            .map(format_probability)
                            .unwrap_or_else(|| "—".into()),
                        &self
                            .current_round_elapsed()
                            .map(|elapsed| format!("{} on block", format_elapsed(elapsed)))
                            .unwrap_or_else(|| "waiting for work".into()),
                        AMBER,
                    );
                });
                ui.add_space(9.0);
                let width = ui.available_width();
                egui::Resize::default()
                    .id_salt("live-mining-meter-height-v3")
                    .default_size([width, 205.0])
                    .min_size([width, 175.0])
                    .max_size([width, 430.0])
                    .resizable([false, true])
                    .with_stroke(false)
                    .show(ui, |ui| {
                        let meter_height = ui.available_height().max(170.0);
                        mining_meter(
                            ui,
                            &self.meter_history,
                            self.settings.report_secs,
                            meter_height,
                        );
                    });
                ui.add_space(7.0);
                activity_cell_legend(ui);
                ui.add_space(7.0);
                ui.horizontal_wrapped(|ui| {
                    telemetry_chip(
                        ui,
                        "NETWORK HASHRATE",
                        &self
                            .network_hashrate
                            .map(format_hashrate)
                            .unwrap_or_else(|| "—".into()),
                        PURPLE,
                        "Estimated by the connected SOV node; separate from this miner's local hashrate.",
                    );
                    let peer_color = match self.peer_count {
                        Some(0) => AMBER,
                        Some(_) => GREEN,
                        None => MUTED,
                    };
                    telemetry_chip(
                        ui,
                        "NODE PEERS",
                        &self.peer_count.map_or_else(
                            || "—".into(),
                            |count| format!("{count} AUTH"),
                        ),
                        peer_color,
                        "Authenticated P2P peers reported by the connected SOV node; separate from this miner's RPC connection.",
                    );
                    ui.label(
                        RichText::new(format!(
                            "TARGET CADENCE {}",
                            self.target_block_ms
                                .map(|ms| format_eta(ms as f64 / 1_000.0))
                                .unwrap_or_else(|| "—".into())
                        ))
                        .size(9.0)
                        .monospace()
                        .color(MUTED),
                    );
                    ui.label(RichText::new("•").size(9.0).color(BORDER));
                    ui.label(
                        RichText::new(format!(
                            "WORK / BLOCK {}",
                            self.expected_hashes
                                .map(format_count_f64)
                                .unwrap_or_else(|| "—".into())
                        ))
                        .size(9.0)
                        .monospace()
                        .color(MUTED),
                    );
                    ui.label(RichText::new("•").size(9.0).color(BORDER));
                    ui.label(
                        RichText::new("ETA is statistical, never a guaranteed countdown")
                            .size(9.0)
                            .color(MUTED),
                    );
                });
            });

        ui.add_space(12.0);
        let width = ui.available_width();
        egui::Resize::default()
            .id_salt("work-health-height-v2")
            .default_size([width, 205.0])
            .min_size([width, 155.0])
            .max_size([width, 360.0])
            .resizable([false, true])
            .with_stroke(false)
            .show(ui, |ui| {
                let card_height = ui.available_height().max(150.0);
                ui.columns(2, |columns| {
                    Frame::new()
                        .fill(CARD)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::same(16))
                        .show(&mut columns[0], |ui| {
                            ui.set_min_height(card_height - 32.0);
                            ui.horizontal(|ui| {
                                ui.label(
                                    RichText::new("ACTIVE WORK")
                                        .size(11.0)
                                        .strong()
                                        .color(MUTED),
                                );
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    ui.label(
                                        RichText::new("↘ resize section")
                                            .size(9.0)
                                            .color(MUTED),
                                    );
                                });
                            });
                            ui.add_space(10.0);
                            ui.columns(2, |chips| {
                                work_chip(
                                    &mut chips[0],
                                    "ALGORITHM",
                                    &self.redact_sensitive_text(&self.algorithm),
                                    CYAN,
                                );
                                work_chip(
                                    &mut chips[1],
                                    "HEIGHT",
                                    &self
                                        .height
                                        .map(|height| format!("#{height}"))
                                        .unwrap_or_else(|| "—".into()),
                                    PURPLE,
                                );
                            });
                            ui.add_space(10.0);
                            copyable_detail_row(
                                ui,
                                "Job ID",
                                &self.redact_sensitive_text(&self.job_id),
                            );
                            copyable_detail_row(
                                ui,
                                "Endpoint",
                                &sanitize_endpoint(&self.settings.pool),
                            );
                            copyable_detail_row(
                                ui,
                                "Coinbase",
                                &self.redact_sensitive_text(
                                    self.coinbase
                                        .as_deref()
                                        .unwrap_or("awaiting template confirmation"),
                                ),
                            );
                            copyable_detail_row(
                                ui,
                                "Mempool",
                                &self
                                    .mempool_size
                                    .map(|size| format!("{size} pending transaction(s)"))
                                    .unwrap_or_else(|| "unavailable".into()),
                            );
                            copyable_detail_row(
                                ui,
                                "Route",
                                if is_direct_node_endpoint(&self.settings.pool) {
                                    "Direct SOV node (template verified)"
                                } else {
                                    self.settings.reward_route.summary()
                                },
                            );
                        });
                    Frame::new()
                        .fill(CARD)
                        .stroke(Stroke::new(1.0_f32, BORDER))
                        .corner_radius(CornerRadius::same(12))
                        .inner_margin(Margin::same(16))
                        .show(&mut columns[1], |ui| {
                            ui.set_min_height(card_height - 32.0);
                            ui.label(
                                RichText::new("SUBMISSION HEALTH")
                                    .size(11.0)
                                    .strong()
                                    .color(MUTED),
                            );
                            ui.add_space(10.0);
                            detail_row(ui, "Submitted", &self.submitted.to_string());
                            detail_row(ui, "Accepted", &self.accepted.to_string());
                            detail_row(ui, "Rejected", &self.rejected.to_string());
                            let ratio = if self.accepted + self.rejected == 0 {
                                "—".into()
                            } else {
                                format!(
                                    "{:.2}%",
                                    self.accepted as f64 * 100.0
                                        / (self.accepted + self.rejected) as f64
                                )
                            };
                            detail_row(ui, "Acceptance", &ratio);
                            ui.add_space(12.0);
                            ui.label(
                                RichText::new(
                                    "Direct-node counters advance only when a network-difficulty block is submitted.",
                                )
                                .size(10.0)
                                .color(MUTED),
                            );
                        });
                });
            });

        if !self.last_error.is_empty() && matches!(self.phase, Phase::Error | Phase::Reconnecting) {
            ui.add_space(12.0);
            Frame::new()
                .fill(RED.gamma_multiply(0.09))
                .stroke(Stroke::new(1.0_f32, RED.gamma_multiply(0.45)))
                .corner_radius(CornerRadius::same(10))
                .inner_margin(Margin::same(12))
                .show(ui, |ui| {
                    ui.label(RichText::new(&self.last_error).color(RED));
                });
        }
    }
}

impl Drop for MinerApp {
    fn drop(&mut self) {
        let _ = save_settings(&self.settings);
        let active_child_reaped = self
            .child
            .take()
            .is_none_or(|mut child| terminate_and_reap_child(&mut child).is_ok());
        let quarantined_child_reaped = self
            .quarantined_child
            .take()
            .is_none_or(|mut child| terminate_and_reap_child(&mut child).is_ok());
        let cleanup_reaped = active_child_reaped && quarantined_child_reaped;
        // Closing the receiver releases any reader blocked by bounded-channel
        // backpressure. If process cleanup could not be confirmed, only join
        // readers already known to be finished so application shutdown cannot
        // hang on a still-live pipe.
        self.receiver = None;
        if cleanup_reaped {
            self.join_reader_threads();
        } else {
            self.join_finished_reader_threads();
        }
    }
}

impl eframe::App for MinerApp {
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.refresh_memory_if_due();
        self.poll_process(ctx);
        ctx.request_repaint_after(Duration::from_secs(1));
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        egui::Panel::left("miner-controls")
            .default_size(285.0)
            .min_size(250.0)
            .max_size(460.0)
            .resizable(true)
            .frame(
                Frame::new()
                    .fill(PANEL)
                    .inner_margin(Margin::same(18))
                    .stroke(Stroke::new(1.0_f32, BORDER)),
            )
            .show(ui, |ui| {
                ui.label(
                    RichText::new("OPERATOR CONTROLS")
                        .size(13.0)
                        .strong()
                        .color(TEXT),
                );
                ui.label(
                    RichText::new("Hash here. Keep keys elsewhere.")
                        .size(11.0)
                        .color(MUTED),
                );
                egui::ScrollArea::vertical().show(ui, |ui| {
                    self.controls(ui);
                    ui.add_space(22.0);
                    ui.label(
                        RichText::new("No wallet seed or private key is used.")
                            .size(10.0)
                            .color(GREEN),
                    );
                    ui.label(
                        RichText::new(format!("xus-miner v{}", crate::VERSION))
                            .size(10.0)
                            .color(MUTED),
                    );
                });
            });
        egui::Panel::bottom("engine-log-panel-v2")
            .default_size(220.0)
            .min_size(100.0)
            .max_size(560.0)
            .resizable(true)
            .frame(
                Frame::new()
                    .fill(CARD)
                    .inner_margin(Margin::same(14))
                    .stroke(Stroke::new(1.0, BORDER)),
            )
            .show(ui, |ui| self.engine_log_panel(ui));
        egui::CentralPanel::default()
            .frame(Frame::new().fill(BG).inner_margin(Margin::same(22)))
            .show(ui, |ui| {
                egui::ScrollArea::vertical().show(ui, |ui| self.dashboard(ui));
            });
    }
}

fn install_theme(ctx: &egui::Context) {
    ctx.set_theme(egui::Theme::Dark);
    ctx.global_style_mut(|style| {
        style.visuals = egui::Visuals::dark();
        style.visuals.panel_fill = BG;
        style.visuals.window_fill = PANEL;
        style.visuals.extreme_bg_color = Color32::from_rgb(11, 14, 23);
        style.visuals.faint_bg_color = CARD;
        style.visuals.widgets.inactive.bg_fill = CARD;
        style.visuals.widgets.inactive.bg_stroke = Stroke::new(1.0_f32, BORDER);
        style.visuals.widgets.hovered.bg_fill = CARD_HOVER;
        style.visuals.widgets.hovered.bg_stroke = Stroke::new(1.0_f32, PURPLE);
        style.visuals.widgets.active.bg_fill = PURPLE.gamma_multiply(0.35);
        style.visuals.widgets.active.bg_stroke = Stroke::new(1.0_f32, PURPLE);
        style.visuals.selection.bg_fill = PURPLE.gamma_multiply(0.55);
        style.visuals.hyperlink_color = CYAN;
        style.spacing.item_spacing = Vec2::new(8.0, 7.0);
    });
}

fn metric_card(ui: &mut egui::Ui, label: &str, value: &str, accent: Color32) {
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(1.0_f32, BORDER))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(Margin::same(14))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(10.0).strong().color(MUTED));
            ui.add_space(5.0);
            ui.label(RichText::new(value).size(21.0).strong().color(accent));
        });
}

fn telemetry_chip(ui: &mut egui::Ui, label: &str, value: &str, color: Color32, tooltip: &str) {
    Frame::new()
        .fill(color.gamma_multiply(0.18))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.72)))
        .corner_radius(CornerRadius::same(7))
        .inner_margin(Margin::symmetric(9, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("●").size(8.0).color(color));
                ui.label(RichText::new(label).size(9.0).strong().color(TEXT));
                ui.label(
                    RichText::new(value)
                        .size(11.0)
                        .strong()
                        .monospace()
                        .color(color),
                );
            });
        })
        .response
        .on_hover_text(tooltip);
}

fn heartbeat_card(
    ui: &mut egui::Ui,
    label: &str,
    value: &str,
    detail: impl Into<String>,
    color: Color32,
    live: bool,
    time: f64,
) {
    let detail = detail.into();
    Frame::new()
        .fill(CARD)
        .stroke(Stroke::new(
            1.0,
            if live {
                color.gamma_multiply(0.48)
            } else {
                BORDER
            },
        ))
        .corner_radius(CornerRadius::same(11))
        .inner_margin(Margin::same(11))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (pulse_rect, _) = ui.allocate_exact_size(Vec2::splat(32.0), Sense::hover());
                let painter = ui.painter_at(pulse_rect);
                let center = pulse_rect.center();
                if live {
                    for offset in [0.0, 0.5] {
                        let cycle = ((time * 1.15 + offset) % 1.0) as f32;
                        painter.circle_stroke(
                            center,
                            5.0 + cycle * 10.0,
                            Stroke::new(1.3, color.gamma_multiply((1.0 - cycle) * 0.55)),
                        );
                    }
                }
                painter.circle_filled(center, if live { 5.0 } else { 4.0 }, color);
                painter.circle_filled(center, 1.8, TEXT);

                ui.vertical(|ui| {
                    ui.label(RichText::new(label).size(9.0).strong().color(MUTED));
                    ui.label(RichText::new(value).size(12.0).strong().color(color));
                    ui.label(RichText::new(detail).size(9.0).color(MUTED));
                });
            });
        });
}

fn detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(11.0).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(value).size(11.0).color(TEXT).monospace());
        });
    });
}

fn work_chip(ui: &mut egui::Ui, label: &str, value: &str, color: Color32) {
    Frame::new()
        .fill(color.gamma_multiply(0.08))
        .stroke(Stroke::new(1.0, color.gamma_multiply(0.35)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 7))
        .show(ui, |ui| {
            ui.label(RichText::new(label).size(8.0).strong().color(MUTED));
            ui.label(
                RichText::new(value)
                    .size(13.0)
                    .strong()
                    .monospace()
                    .color(color),
            );
        });
}

fn copyable_detail_row(ui: &mut egui::Ui, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).size(10.0).color(MUTED));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(value.to_owned());
            }
            ui.add(
                egui::Label::new(RichText::new(value).size(10.0).monospace().color(TEXT))
                    .truncate()
                    .selectable(true),
            )
            .on_hover_text(value);
        });
    });
}

fn meter_readout(ui: &mut egui::Ui, label: &str, value: &str, detail: &str, color: Color32) {
    ui.label(RichText::new(label).size(8.0).strong().color(MUTED));
    ui.label(
        RichText::new(value)
            .size(15.0)
            .strong()
            .monospace()
            .color(color),
    );
    ui.label(RichText::new(detail).size(8.0).color(MUTED));
}

fn chart_grid(painter: &egui::Painter, rect: egui::Rect) {
    painter.rect_filled(rect, CornerRadius::same(8), BG);
    for step in 1..4 {
        let y = egui::lerp(rect.top()..=rect.bottom(), step as f32 / 4.0);
        painter.line_segment(
            [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
            Stroke::new(1.0_f32, BORDER.gamma_multiply(0.48)),
        );
    }
    for step in 1..6 {
        let x = egui::lerp(rect.left()..=rect.right(), step as f32 / 6.0);
        painter.line_segment(
            [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
            Stroke::new(1.0_f32, BORDER.gamma_multiply(0.22)),
        );
    }
}

fn mining_meter(ui: &mut egui::Ui, history: &VecDeque<MeterSample>, report_secs: u64, height: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, CornerRadius::same(8), BG);
    if history.len() < 2 {
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            "Live meter locks after the second telemetry sample",
            FontId::proportional(11.0),
            MUTED,
        );
        return;
    }

    let outer = rect.shrink2(Vec2::new(8.0, 7.0));
    let graph_top = outer.top() + 23.0;
    let cells_height = 14.0;
    let graph_bottom = outer.bottom() - cells_height - 17.0;
    let graph_height = (graph_bottom - graph_top).max(80.0);
    let rate_bottom = graph_top + graph_height * 0.67;
    let rate_rect = egui::Rect::from_min_max(
        egui::pos2(outer.left(), graph_top),
        egui::pos2(outer.right(), rate_bottom - 4.0),
    );
    let probability_rect = egui::Rect::from_min_max(
        egui::pos2(outer.left(), rate_bottom + 5.0),
        egui::pos2(outer.right(), graph_bottom),
    );
    chart_grid(&painter, rate_rect);
    painter.rect_filled(probability_rect, CornerRadius::same(4), BG);
    for step in 1..6 {
        let x = egui::lerp(outer.left()..=outer.right(), step as f32 / 6.0);
        painter.line_segment(
            [
                egui::pos2(x, rate_rect.top()),
                egui::pos2(x, probability_rect.bottom()),
            ],
            Stroke::new(1.0, BORDER.gamma_multiply(0.22)),
        );
    }
    painter.line_segment(
        [probability_rect.left_top(), probability_rect.right_top()],
        Stroke::new(1.0, BORDER.gamma_multiply(0.55)),
    );

    let max_rate = history
        .iter()
        .map(|sample| sample.hashrate.max(sample.smoothed_hashrate))
        .fold(0.0_f64, f64::max)
        .max(1.0);
    let average_rate = history
        .iter()
        .map(|sample| sample.smoothed_hashrate)
        .sum::<f64>()
        / history.len() as f64;
    let denominator = (history.len() - 1) as f32;
    let x_for = |index: usize| egui::lerp(outer.left()..=outer.right(), index as f32 / denominator);
    let raw_points: Vec<_> = history
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let y = egui::lerp(
                rate_rect.bottom()..=rate_rect.top(),
                (sample.hashrate / max_rate).clamp(0.0, 1.0) as f32,
            );
            egui::pos2(x_for(index), y)
        })
        .collect();
    let locked_points: Vec<_> = history
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            let y = egui::lerp(
                rate_rect.bottom()..=rate_rect.top(),
                (sample.smoothed_hashrate / max_rate).clamp(0.0, 1.0) as f32,
            );
            egui::pos2(x_for(index), y)
        })
        .collect();
    for pair in locked_points.windows(2) {
        painter.add(egui::Shape::convex_polygon(
            vec![
                egui::pos2(pair[0].x, rate_rect.bottom()),
                egui::pos2(pair[1].x, rate_rect.bottom()),
                pair[1],
                pair[0],
            ],
            CYAN.gamma_multiply(0.13),
            Stroke::NONE,
        ));
    }
    painter.add(egui::Shape::line(
        locked_points.clone(),
        Stroke::new(8.0, CYAN.gamma_multiply(0.07)),
    ));
    painter.add(egui::Shape::line(
        raw_points,
        Stroke::new(1.0, CYAN.gamma_multiply(0.38)),
    ));
    painter.add(egui::Shape::line(
        locked_points.clone(),
        Stroke::new(2.0, CYAN),
    ));

    let probability_scale = history
        .iter()
        .filter_map(|sample| sample.round_probability)
        .fold(0.0_f64, f64::max)
        .clamp(0.01, 1.0);
    let probability_points: Vec<_> = history
        .iter()
        .enumerate()
        .map(|(index, sample)| {
            sample.round_probability.map(|probability| {
                egui::pos2(
                    x_for(index),
                    egui::lerp(
                        probability_rect.bottom()..=probability_rect.top(),
                        (probability / probability_scale).clamp(0.0, 1.0) as f32,
                    ),
                )
            })
        })
        .collect();
    for pair in probability_points.windows(2) {
        if let [Some(left), Some(right)] = pair {
            painter.add(egui::Shape::convex_polygon(
                vec![
                    egui::pos2(left.x, probability_rect.bottom()),
                    egui::pos2(right.x, probability_rect.bottom()),
                    *right,
                    *left,
                ],
                PURPLE.gamma_multiply(0.16),
                Stroke::NONE,
            ));
            painter.line_segment([*left, *right], Stroke::new(1.7, PURPLE));
        }
    }

    let markers: Vec<_> = history
        .iter()
        .zip(history.iter().skip(1))
        .enumerate()
        .filter_map(|(index, (previous, current))| {
            (current.height.is_some() && current.height != previous.height)
                .then_some((index + 1, current.height.expect("height checked")))
        })
        .collect();
    let marker_skip = markers.len().saturating_sub(6);
    for (ordinal, (index, block_height)) in markers.iter().skip(marker_skip).enumerate() {
        let x = x_for(*index);
        painter.line_segment(
            [
                egui::pos2(x, rate_rect.top()),
                egui::pos2(x, probability_rect.bottom()),
            ],
            Stroke::new(1.4, AMBER.gamma_multiply(0.78)),
        );
        painter.text(
            egui::pos2(
                x + 4.0,
                rate_rect.top() + if ordinal % 2 == 0 { 3.0 } else { 14.0 },
            ),
            egui::Align2::LEFT_TOP,
            format!("B{block_height}"),
            FontId::monospace(8.5),
            AMBER,
        );
    }

    if let Some(latest) = locked_points.last() {
        let pulse = ((ui.input(|input| input.time) * 1.35) % 1.0) as f32;
        painter.circle_filled(*latest, 3.4, TEXT);
        painter.circle_stroke(
            *latest,
            5.0 + pulse * 6.0,
            Stroke::new(1.2, CYAN.gamma_multiply((1.0 - pulse) * 0.7)),
        );
    }

    if let Some(block_height) = history.back().and_then(|sample| sample.height) {
        painter.line_segment(
            [
                rate_rect.right_top(),
                egui::pos2(rate_rect.right(), probability_rect.bottom()),
            ],
            Stroke::new(1.5, PURPLE.gamma_multiply(0.9)),
        );
        painter.text(
            rate_rect.right_top() + Vec2::new(-5.0, 3.0),
            egui::Align2::RIGHT_TOP,
            format!("B{block_height} LIVE"),
            FontId::monospace(9.0),
            PURPLE,
        );
    }

    let cells = history.len().min(MAX_ACTIVITY_CELLS);
    let cell_top = outer.bottom() - cells_height;
    let cell_width = outer.width() / cells as f32;
    for cell in 0..cells {
        let index = if cells == 1 {
            history.len() - 1
        } else {
            cell * (history.len() - 1) / (cells - 1)
        };
        let sample = history.get(index).expect("meter sample index");
        let cell_rect = egui::Rect::from_min_max(
            egui::pos2(outer.left() + cell as f32 * cell_width + 1.0, cell_top),
            egui::pos2(
                outer.left() + (cell + 1) as f32 * cell_width - 1.0,
                outer.bottom(),
            ),
        );
        painter.rect_filled(
            cell_rect,
            CornerRadius::same(2),
            activity_cell_color(sample, max_rate),
        );
    }

    let span = Duration::from_secs(report_secs.saturating_mul((history.len() - 1) as u64));
    painter.text(
        outer.left_top(),
        egui::Align2::LEFT_TOP,
        format!("POW FLIGHT RECORDER  •  LAST {}", format_elapsed(span)),
        FontId::monospace(9.5),
        MUTED,
    );
    painter.text(
        outer.right_top(),
        egui::Align2::RIGHT_TOP,
        format!(
            "AVG {}  •  PEAK {}",
            format_hashrate(average_rate),
            format_hashrate(max_rate)
        ),
        FontId::monospace(9.5),
        CYAN,
    );
    painter.text(
        probability_rect.left_top() + Vec2::new(5.0, 3.0),
        egui::Align2::LEFT_TOP,
        format!(
            "BLOCK HUNT {}  •  SCALE 0–{}",
            history
                .back()
                .and_then(|sample| sample.round_probability)
                .map(format_probability)
                .unwrap_or_else(|| "—".into()),
            format_probability(probability_scale)
        ),
        FontId::monospace(9.0),
        PURPLE,
    );
    painter.text(
        egui::pos2(outer.left(), cell_top - 3.0),
        egui::Align2::LEFT_BOTTOM,
        format!(
            "MEMPOOL {}  •  ACTIVITY SLICES {}/{}",
            history
                .back()
                .and_then(|sample| sample.mempool)
                .map_or_else(|| "—".into(), |pending| pending.to_string()),
            cells,
            MAX_ACTIVITY_CELLS,
        ),
        FontId::monospace(8.5),
        if history
            .back()
            .and_then(|sample| sample.mempool)
            .unwrap_or(0)
            > 0
        {
            GREEN
        } else {
            MUTED
        },
    );
}

fn activity_cell_legend(ui: &mut egui::Ui) {
    Frame::new()
        .fill(BG.gamma_multiply(0.88))
        .stroke(Stroke::new(1.0, BORDER.gamma_multiply(0.75)))
        .corner_radius(CornerRadius::same(8))
        .inner_margin(Margin::symmetric(10, 8))
        .show(ui, |ui| {
            ui.label(RichText::new(ACTIVITY_CELL_SUMMARY).size(10.0).color(TEXT));
            ui.add_space(3.0);
            ui.horizontal_wrapped(|ui| {
                activity_legend_item(
                    ui,
                    CYAN.gamma_multiply(0.9),
                    "BRIGHT CYAN",
                    "stronger hashrate during that sample",
                );
                activity_legend_item(
                    ui,
                    CYAN.gamma_multiply(0.28),
                    "DIM CYAN",
                    "lower hashrate during that sample",
                );
                activity_legend_item(
                    ui,
                    GREEN.gamma_multiply(0.9),
                    "GREEN",
                    "the mempool contained transactions during that sample",
                );
            });
        });
}

fn activity_legend_item(ui: &mut egui::Ui, color: Color32, label: &str, meaning: &str) {
    Frame::new()
        .fill(CARD.gamma_multiply(0.7))
        .corner_radius(CornerRadius::same(5))
        .inner_margin(Margin::symmetric(7, 4))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                let (swatch, _) = ui.allocate_exact_size(Vec2::splat(9.0), Sense::hover());
                ui.painter()
                    .rect_filled(swatch, CornerRadius::same(2), color);
                ui.label(RichText::new(label).size(9.0).strong().color(color));
                ui.label(RichText::new(meaning).size(9.0).color(MUTED));
            });
        });
}

fn capture_memory_snapshot(system: &mut System) -> Option<MemorySnapshot> {
    system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    memory_snapshot_from_readings(system.available_memory(), system.total_memory())
}

fn memory_snapshot_from_readings(available: u64, total: u64) -> Option<MemorySnapshot> {
    (total > 0 && available <= total).then_some(MemorySnapshot { available, total })
}

fn recommended_worker_limit(snapshot: MemorySnapshot) -> u64 {
    let usable = snapshot
        .available
        .saturating_sub(memory_headroom_bytes(snapshot.total));
    (1..=64)
        .rev()
        .find(|workers| randomx_memory_bytes(*workers) <= usable)
        .unwrap_or(0)
}

fn format_memory(bytes: u64) -> String {
    format!("{:.1} GiB", bytes as f64 / BYTES_PER_GIB)
}

fn activity_cell_color(sample: &MeterSample, max_rate: f64) -> Color32 {
    let activity = (sample.smoothed_hashrate / max_rate.max(1.0)).clamp(0.0, 1.0) as f32;
    let color = if sample.mempool.unwrap_or(0) > 0 {
        GREEN
    } else {
        CYAN
    };
    color.gamma_multiply(0.12 + activity * 0.76)
}

fn format_eta(seconds: f64) -> String {
    if !seconds.is_finite() || seconds <= 0.0 {
        return "—".into();
    }
    if seconds < 60.0 {
        return format!("≈{:.0}s", seconds.max(1.0));
    }
    if seconds < 3_600.0 {
        let total = seconds.round() as u64;
        return format!("≈{}m {:02}s", total / 60, total % 60);
    }
    if seconds < 86_400.0 {
        let total = seconds.round() as u64;
        return format!("≈{}h {:02}m", total / 3_600, total / 60 % 60);
    }
    if seconds < 31_536_000.0 {
        let total = seconds.round() as u64;
        return format!("≈{}d {:02}h", total / 86_400, total / 3_600 % 24);
    }
    format!("≈{:.1}y", seconds / 31_536_000.0)
}

fn format_elapsed(duration: Duration) -> String {
    let seconds = duration.as_secs();
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {:02}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3_600, seconds / 60 % 60)
    }
}

fn format_probability(probability: f64) -> String {
    let percent = probability.clamp(0.0, 1.0) * 100.0;
    if percent > 0.0 && percent < 0.01 {
        format!("{percent:.4}%")
    } else if percent < 1.0 {
        format!("{percent:.2}%")
    } else {
        format!("{percent:.1}%")
    }
}

fn format_count_f64(value: f64) -> String {
    if !value.is_finite() || value < 0.0 {
        "unavailable".into()
    } else if value >= 1_000_000_000_000_000.0 {
        format!("{value:.3e}")
    } else if value >= 1_000_000_000_000.0 {
        format!("{:.2}T", value / 1_000_000_000_000.0)
    } else if value >= 1_000_000_000.0 {
        format!("{:.2}B", value / 1_000_000_000.0)
    } else if value >= 1_000_000.0 {
        format!("{:.2}M", value / 1_000_000.0)
    } else if value >= 1_000.0 {
        format!("{:.2}K", value / 1_000.0)
    } else {
        format!("{value:.0}")
    }
}

fn format_hashrate(rate: f64) -> String {
    if rate >= 1_000_000_000.0 {
        format!("{:.2} GH/s", rate / 1_000_000_000.0)
    } else if rate >= 1_000_000.0 {
        format!("{:.2} MH/s", rate / 1_000_000.0)
    } else if rate >= 1_000.0 {
        format!("{:.2} kH/s", rate / 1_000.0)
    } else {
        format!("{rate:.2} H/s")
    }
}

fn format_count(value: u64) -> String {
    if value >= 1_000_000_000 {
        format!("{:.2}B", value as f64 / 1_000_000_000.0)
    } else if value >= 1_000_000 {
        format!("{:.2}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{:.2}K", value as f64 / 1_000.0)
    } else {
        value.to_string()
    }
}

pub fn run() -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1_120.0, 760.0])
            .with_min_inner_size([900.0, 620.0])
            .with_title(WINDOW_TITLE),
        ..Default::default()
    };
    eframe::run_native(
        WINDOW_TITLE,
        options,
        Box::new(|creation| {
            install_theme(&creation.egui_ctx);
            Ok(Box::<MinerApp>::default())
        }),
    )
    .map_err(|error| format!("GUI failed: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestSettingsHome(PathBuf);

    impl TestSettingsHome {
        fn new(label: &str) -> Self {
            for attempt in 0..32_u64 {
                let sequence = SETTINGS_TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                let path = env::temp_dir().join(format!(
                    "xus-miner-{label}-{}-{sequence}-{attempt}",
                    process::id()
                ));
                match fs::create_dir(&path) {
                    Ok(()) => return Self(path),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(error) => panic!("cannot create test settings home: {error}"),
                }
            }
            panic!("cannot allocate test settings home");
        }

        fn settings_path(&self) -> PathBuf {
            self.0.join(SETTINGS_DIRECTORY).join(SETTINGS_FILE)
        }
    }

    impl Drop for TestSettingsHome {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn accepts_supported_pool_forms() {
        assert!(validate_pool("127.0.0.1:3333").is_ok());
        assert!(validate_pool("192.168.0.244:8645").is_ok());
        assert!(validate_pool("rpc://192.168.0.244:8645").is_ok());
        assert!(validate_pool("http://127.0.0.1:8645").is_ok());
        assert!(validate_pool("tcp://pool.example:4444").is_ok());
        assert!(validate_pool("stratum+tcp://[::1]:3333").is_ok());
    }

    #[test]
    fn rejects_ambiguous_or_unsafe_pool_values() {
        assert!(validate_pool("").is_err());
        assert!(validate_pool("pool.example").is_err());
        assert!(validate_pool("pool.example:0").is_err());
        assert!(validate_pool("pool.example:99999").is_err());
        assert!(validate_pool("pool example:3333").is_err());
        assert!(validate_pool("http://operator:secret@127.0.0.1:8645").is_err());
    }

    #[test]
    fn child_line_reader_bounds_and_drains_oversized_records() {
        let mut input = vec![b'a'; MAX_CHILD_LINE_BYTES * 3];
        input.extend_from_slice(b"\r\n{\"event\":\"startup\"}\n");
        let mut reader = BufReader::with_capacity(257, io::Cursor::new(input));

        let oversized = read_bounded_child_line(&mut reader).unwrap().unwrap();
        assert!(oversized.len() <= MAX_CHILD_LINE_BYTES);
        assert!(oversized.ends_with(TRUNCATED_LINE_SUFFIX));
        assert_eq!(
            read_bounded_child_line(&mut reader).unwrap().unwrap(),
            r#"{"event":"startup"}"#
        );
        assert_eq!(read_bounded_child_line(&mut reader).unwrap(), None);
    }

    #[test]
    fn stored_engine_log_is_bounded_by_line_size_and_count() {
        let mut app = MinerApp::default();
        for index in 0..(MAX_LOG_LINES + 12) {
            app.push_log(
                format!("{index}:{}", "z".repeat(MAX_STORED_LOG_BYTES * 2)),
                false,
            );
        }
        assert_eq!(app.logs.len(), MAX_LOG_LINES);
        assert!(app
            .logs
            .iter()
            .all(|line| line.text.len() <= MAX_STORED_LOG_BYTES));
        assert!(app.logs.front().unwrap().text.starts_with("12:"));
        assert!(app
            .logs
            .back()
            .unwrap()
            .text
            .ends_with(TRUNCATED_LINE_SUFFIX));
    }

    #[test]
    fn child_output_queue_is_bounded_and_frames_have_a_work_budget() {
        let (sender, receiver) = mpsc::sync_channel(CHILD_OUTPUT_QUEUE_CAPACITY);
        for index in 0..CHILD_OUTPUT_QUEUE_CAPACITY {
            sender
                .try_send(ProcessMessage::Log(format!("queued-{index}")))
                .unwrap();
        }
        assert!(matches!(
            sender.try_send(ProcessMessage::Log("overflow".into())),
            Err(mpsc::TrySendError::Full(_))
        ));

        let mut app = MinerApp::default();
        app.receiver = Some(receiver);
        assert!(!app.drain_process_messages());
        assert_eq!(app.logs.len(), MAX_PROCESS_MESSAGES_PER_FRAME);
        assert_eq!(app.logs.front().unwrap().text, "queued-0");
        assert_eq!(
            app.logs.back().unwrap().text,
            format!("queued-{}", MAX_PROCESS_MESSAGES_PER_FRAME - 1)
        );
    }

    #[test]
    fn output_tail_is_drained_before_reader_handles_are_joined() {
        let (sender, receiver) = mpsc::sync_channel(CHILD_OUTPUT_QUEUE_CAPACITY);
        let (ready_sender, ready_receiver) = mpsc::sync_channel(0);
        let handle = thread::spawn(move || {
            sender
                .send(ProcessMessage::Log("before-tail".into()))
                .unwrap();
            sender
                .send(ProcessMessage::Log("final-output-tail".into()))
                .unwrap();
            drop(sender);
            ready_sender.send(()).unwrap();
        });
        ready_receiver.recv().unwrap();

        let mut app = MinerApp::default();
        app.receiver = Some(receiver);
        app.reader_threads.push(handle);
        assert!(app.drain_process_messages());
        assert_eq!(app.logs.back().unwrap().text, "final-output-tail");
        app.join_reader_threads();
        assert!(app.reader_threads.is_empty());
        assert!(app
            .crash_report_text("synthetic exit")
            .contains("final-output-tail"));
    }

    #[test]
    fn observation_failure_is_single_shot_terminal_and_preserves_context() {
        let mut app = MinerApp::default();
        app.phase = Phase::Mining;
        app.begin_observation_failure(
            "Cannot observe mining engine",
            io::Error::other("synthetic wait failure"),
        );

        assert!(app.child.is_none());
        assert!(app.engine_fatal_seen);
        assert_eq!(app.phase, Phase::Error);
        assert!(app.is_running());
        assert!(app.last_error.contains("synthetic wait failure"));
        let pending = app.pending_observation_failure.as_ref().unwrap();
        assert!(pending.report_detail.contains("synthetic wait failure"));
        assert!(pending
            .report_detail
            .contains("child handle was already unavailable"));

        // Re-entering the same terminal path cannot replace the preserved
        // failure or restart cleanup.
        app.begin_observation_failure("replacement", io::Error::other("must not replace"));
        assert!(!app.last_error.contains("must not replace"));
    }

    #[test]
    fn quarantined_child_blocks_restart_until_exit_is_proven() {
        let child = Command::new(env::current_exe().unwrap())
            .arg("--list")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut app = MinerApp::default();
        app.quarantined_child = Some(child);

        assert!(app.is_running());
        app.start();
        assert!(app.child.is_none());
        assert!(app.quarantined_child.is_some());

        let mut child = app.quarantined_child.take().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        assert!(!app.is_running());
    }

    #[test]
    fn worker_and_engine_failures_drive_health_until_recovery() {
        let mut app = MinerApp::default();
        app.settings.workers = 2;
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 42,
            "algorithm": "RandomX",
            "job_id": "job-42",
        }));
        app.apply_telemetry(&json!({"event": "worker_ready", "worker": 0}));
        app.apply_telemetry(&json!({"event": "worker_ready", "worker": 0}));
        app.apply_telemetry(&json!({"event": "worker_ready", "worker": 1}));
        assert_eq!(app.ready_workers, 2);
        assert_eq!(app.phase, Phase::Mining);

        app.apply_telemetry(&json!({
            "event": "worker_error",
            "worker": 1,
            "message": "RandomX VM unavailable",
        }));
        assert_eq!(app.ready_workers, 1);
        assert_eq!(app.phase, Phase::Error);
        assert!(
            app.node_link_live,
            "a worker failure must not falsely disconnect the live RPC session"
        );
        assert!(app.last_error.contains("RandomX VM unavailable"));

        // A later job must not paint the engine green while a worker remains
        // failed. Either recovery event restores that worker exactly once.
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 43,
            "algorithm": "RandomX",
            "job_id": "job-43",
        }));
        assert_eq!(app.phase, Phase::Error);
        app.apply_telemetry(&json!({"event": "worker_recovered", "worker": 1}));
        assert_eq!(app.ready_workers, 2);
        assert_eq!(app.phase, Phase::Mining);
        assert!(app.last_error.is_empty());

        app.apply_telemetry(&json!({
            "event": "worker_error",
            "worker": 0,
            "message": "worker supervisor restarted",
        }));
        app.apply_telemetry(&json!({"event": "worker_ready", "worker": 0}));
        assert_eq!(app.ready_workers, 2);
        assert_eq!(app.phase, Phase::Mining);

        app.apply_telemetry(&json!({
            "event": "worker_error",
            "worker": 0,
            "message": "temporary worker failure",
        }));
        app.apply_telemetry(&json!({
            "event": "session_error",
            "message": "connection lost",
        }));
        app.apply_telemetry(&json!({"event": "worker_recovered", "worker": 0}));
        assert_eq!(app.phase, Phase::Reconnecting);
        assert!(!app.node_link_live);
        assert_eq!(app.last_error, "connection lost");
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 44,
            "algorithm": "RandomX",
            "job_id": "job-44",
        }));
        assert_eq!(app.phase, Phase::Mining);
        assert!(app.node_link_live);

        app.apply_telemetry(&json!({
            "event": "engine_fatal",
            "message": "cannot start mining reporter",
        }));
        assert_eq!(app.ready_workers, 0);
        assert_eq!(app.phase, Phase::Error);
        assert!(!app.node_link_live);
        assert_eq!(app.hashrate, 0.0);
        app.apply_telemetry(&json!({"event": "worker_ready", "worker": 0}));
        app.apply_telemetry(&json!({
            "event": "worker_error",
            "worker": 1,
            "message": "late worker event",
        }));
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 45,
            "algorithm": "RandomX",
            "job_id": "late-job",
        }));
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 45,
            "hashrate": 999.0,
        }));
        assert_eq!(app.ready_workers, 0);
        assert_eq!(app.phase, Phase::Error);
        assert_eq!(app.last_error, "cannot start mining reporter");
        assert_eq!(app.hashrate, 0.0);
        assert_eq!(app.height, None);
    }

    #[test]
    fn queued_telemetry_cannot_repaint_a_requested_stop() {
        let mut app = MinerApp::default();
        app.phase = Phase::Stopping;
        app.requested_stop = true;
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 99,
            "algorithm": "RandomX",
            "job_id": "late-job",
        }));
        app.apply_telemetry(&json!({
            "event": "worker_error",
            "worker": 0,
            "message": "late error",
        }));
        assert_eq!(app.phase, Phase::Stopping);
        assert_eq!(app.height, None);
        assert!(app.worker_errors.is_empty());
    }

    #[test]
    fn requested_stop_still_records_a_raced_engine_fatal() {
        let mut app = MinerApp::default();
        app.phase = Phase::Stopping;
        app.requested_stop = true;
        app.node_link_live = true;
        app.apply_telemetry(&json!({
            "event": "engine_fatal",
            "message": "reporter failed during stop",
        }));
        assert!(app.engine_fatal_seen);
        assert_eq!(app.phase, Phase::Error);
        assert!(!app.node_link_live);
        assert_eq!(app.last_error, "reporter failed during stop");
    }

    #[cfg(unix)]
    #[test]
    fn requested_stop_does_not_hide_a_raced_unexpected_exit() {
        use std::os::unix::process::ExitStatusExt;

        let natural_failure = ExitStatus::from_raw(7 << 8);
        let gui_sigkill = ExitStatus::from_raw(9);
        assert!(!exit_status_matches_requested_stop(&natural_failure));
        assert!(exit_status_matches_requested_stop(&gui_sigkill));
    }

    #[cfg(windows)]
    #[test]
    fn requested_stop_requires_the_windows_termination_status() {
        use std::os::windows::process::ExitStatusExt;

        assert!(exit_status_matches_requested_stop(&ExitStatus::from_raw(1)));
        assert!(!exit_status_matches_requested_stop(&ExitStatus::from_raw(
            0xC000_0005
        )));
    }

    #[cfg(windows)]
    #[test]
    fn common_windows_native_failures_have_actionable_names() {
        assert_eq!(
            windows_ntstatus_description(0xC000_0005),
            Some("access violation (native memory fault)")
        );
        assert_eq!(
            windows_ntstatus_description(0xC000_0017),
            Some("not enough virtual memory")
        );
        assert_eq!(
            windows_ntstatus_description(0xC000_0374),
            Some("heap corruption")
        );
        assert_eq!(windows_ntstatus_description(7), None);
    }

    #[test]
    fn crash_reports_are_bounded_unique_and_password_free() {
        let home = TestSettingsHome::new("crash-report");
        let directory = home.settings_path().parent().unwrap().to_path_buf();
        let secret = "wallet-secret-must-never-be-written";
        let endpoint_secret = "endpoint-userinfo-must-never-be-written";
        let mut app = MinerApp::default();
        app.settings.pool =
            format!("http://operator:{endpoint_secret}@192.168.0.244:8645/private/path");
        app.settings.password = secret.into();
        app.settings.workers = 2;
        for _ in 0..MAX_CRASH_REPORT_LOG_LINES {
            app.push_log("q".repeat(MAX_STORED_LOG_BYTES), false);
        }
        app.push_log(
            format!(
                "native engine failed after receiving password={secret} and auth=operator:{endpoint_secret}"
            ),
            true,
        );

        let report = app.crash_report_text("Windows exception 0xC0000005");
        assert!(report.len() <= MAX_CRASH_REPORT_BYTES);
        assert!(!report.contains(secret));
        assert!(!report.contains(endpoint_secret));
        assert!(!report.contains("operator:"));
        assert!(report.contains("http://192.168.0.244:8645/private/path"));
        assert!(report.contains("[REDACTED]"));

        let first = write_crash_report_at(&directory, &report).unwrap();
        let second = write_crash_report_at(&directory, &report).unwrap();
        assert_ne!(first, second);
        assert!(first
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with(CRASH_REPORT_PREFIX));
        let written = fs::read_to_string(&first).unwrap();
        assert!(written.len() <= MAX_CRASH_REPORT_BYTES);
        assert!(!written.contains(secret));
        assert!(!written.contains(endpoint_secret));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&first).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn crash_redaction_never_reintroduces_even_a_one_character_password() {
        for secret in ["D", "[", "x", "REDACTED"] {
            let raw = format!("before-{secret}-after");
            assert!(!redact_secret(&raw, secret).contains(secret));
        }
        assert_eq!(
            redact_secret("xus-miner reported password=x", "x"),
            "xus-miner reported password=[REDACTED]"
        );
    }

    #[test]
    fn persisted_settings_never_include_password() {
        let settings = Settings {
            password: "super-secret".into(),
            pool: "http://operator:endpoint-secret@127.0.0.1:8645".into(),
            ..Settings::default()
        };
        let encoded = settings.persisted_json().to_string();
        assert!(!encoded.contains("super-secret"));
        assert!(!encoded.contains("endpoint-secret"));
        assert!(!encoded.contains("operator:"));
        assert!(encoded.contains("http://127.0.0.1:8645"));
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_settings_file_symlink_without_touching_target() {
        use std::os::unix::fs::symlink;

        let home = TestSettingsHome::new("file-link");
        let path = home.settings_path();
        ensure_settings_directory(path.parent().unwrap()).unwrap();
        let target = home.0.join("must-not-change");
        fs::write(&target, b"chain source").unwrap();
        symlink(&target, &path).unwrap();

        let error = save_settings_at(&path, &Settings::default()).unwrap_err();
        assert!(error.contains("symbolic link"));
        assert_eq!(fs::read(&target).unwrap(), b"chain source");
    }

    #[cfg(unix)]
    #[test]
    fn save_refuses_symlink_or_non_directory_settings_parent() {
        use std::os::unix::fs::symlink;

        let linked_home = TestSettingsHome::new("directory-link");
        let linked_path = linked_home.settings_path();
        let target_directory = linked_home.0.join("target");
        fs::create_dir(&target_directory).unwrap();
        symlink(&target_directory, linked_path.parent().unwrap()).unwrap();
        let error = save_settings_at(&linked_path, &Settings::default()).unwrap_err();
        assert!(error.contains("symbolic link"));

        let file_home = TestSettingsHome::new("parent-file");
        let file_path = file_home.settings_path();
        fs::write(file_path.parent().unwrap(), b"not a directory").unwrap();
        let error = save_settings_at(&file_path, &Settings::default()).unwrap_err();
        assert!(error.contains("not a directory"));
    }

    #[test]
    fn settings_save_is_bounded_atomic_and_password_free() {
        let home = TestSettingsHome::new("atomic");
        let path = home.settings_path();
        let mut settings = Settings {
            pool: "pool.example:4444".into(),
            password: "never-persist-this".into(),
            ..Settings::default()
        };
        save_settings_at(&path, &settings).unwrap();

        settings.pool = "new.example:5555".into();
        save_settings_at(&path, &settings).unwrap();
        let saved = fs::read_to_string(&path).unwrap();
        assert!(saved.contains("new.example:5555"));
        assert!(!saved.contains("never-persist-this"));
        assert_eq!(load_settings_at(&path).unwrap().pool, "new.example:5555");
        assert!(!fs::read_dir(path.parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".tmp")));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(path.parent().unwrap())
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }

        fs::write(&path, vec![b'x'; MAX_SETTINGS_BYTES as usize + 1]).unwrap();
        assert!(load_settings_at(&path)
            .err()
            .unwrap()
            .contains("unexpectedly large"));
    }

    #[cfg(windows)]
    #[test]
    fn windows_settings_recovers_both_replace_crash_points() {
        let home = TestSettingsHome::new("windows-recovery");
        let path = home.settings_path();
        let settings = Settings {
            pool: "recover.example:4444".into(),
            ..Settings::default()
        };
        save_settings_at(&path, &settings).unwrap();

        let backup = windows_settings_backup(&path);
        fs::rename(&path, &backup).unwrap();
        assert_eq!(load_settings_at(&path).unwrap().pool, settings.pool);
        assert!(path.is_file());
        assert!(!backup.exists());

        fs::copy(&path, &backup).unwrap();
        assert_eq!(load_settings_at(&path).unwrap().pool, settings.pool);
        assert!(path.is_file());
        assert!(!backup.exists());
    }

    #[test]
    fn copied_diagnostics_include_connection_state_but_never_password() {
        let mut app = MinerApp::default();
        app.settings.pool =
            "http://operator:endpoint-secret-must-not-copy@192.168.0.244:8645".into();
        app.settings.password = "wallet-secret-must-not-copy".into();
        app.node_link_live = true;
        app.algorithm = "RandomX".into();
        app.height = Some(10_126);
        app.coinbase = Some("confirmed-reward-account".into());
        app.peer_count = Some(4);
        app.memory_snapshot = Some(MemorySnapshot {
            available: gib_to_bytes(12.0),
            total: gib_to_bytes(16.0),
        });
        app.last_error = "server reflected wallet-secret-must-not-copy".into();
        app.push_log(
            "endpoint auth operator:endpoint-secret-must-not-copy failed",
            true,
        );
        let copied = app.diagnostics_text();
        assert!(copied.contains("http://192.168.0.244:8645"));
        assert!(copied.contains("node link: connected"));
        assert!(copied.contains("RandomX"));
        assert!(copied.contains("10126"));
        assert!(copied.contains("confirmed-reward-account"));
        assert!(copied.contains("authenticated node peers: 4"));
        assert!(copied.contains("12.0 GiB available / 16.0 GiB total"));
        assert!(!copied.contains("wallet-secret-must-not-copy"));
        assert!(!copied.contains("endpoint-secret-must-not-copy"));
        assert!(!copied.contains("operator:"));
    }

    #[test]
    fn memory_scan_validation_preflight_and_worker_recommendation_are_conservative() {
        let total = gib_to_bytes(16.0);
        let reserve = memory_headroom_bytes(total);
        assert_eq!(reserve, total / 10);
        assert_eq!(memory_headroom_bytes(gib_to_bytes(8.0)), gib_to_bytes(1.5));
        assert_eq!(
            memory_headroom_bytes(gib_to_bytes(128.0)),
            gib_to_bytes(4.0)
        );
        assert_eq!(
            memory_snapshot_from_readings(0, total).unwrap().available,
            0
        );
        assert_eq!(memory_snapshot_from_readings(1, 0), None);
        assert_eq!(memory_snapshot_from_readings(total + 1, total), None);

        let mut app = MinerApp::default();
        app.settings.workers = 3;
        let exact = randomx_memory_bytes(3) + reserve;
        app.memory_snapshot = Some(MemorySnapshot {
            available: exact,
            total,
        });
        assert_eq!(app.memory_preflight_error(), None);
        app.memory_snapshot = Some(MemorySnapshot {
            available: exact - 1,
            total,
        });
        assert!(app
            .memory_preflight_error()
            .unwrap()
            .contains("Start blocked"));

        let no_capacity = MemorySnapshot {
            available: reserve,
            total,
        };
        assert_eq!(recommended_worker_limit(no_capacity), 0);
        assert_eq!(
            recommended_worker_limit(MemorySnapshot {
                available: gib_to_bytes(256.0),
                total: gib_to_bytes(256.0),
            }),
            64
        );

        app.settings.workers = 1;
        app.memory_snapshot = None;
        app.memory_acknowledged = false;
        assert!(app.memory_preflight_error().is_some());
        app.memory_acknowledged = true;
        assert_eq!(app.memory_preflight_error(), None);
    }

    #[test]
    fn headless_memory_confirmation_is_only_forwarded_for_acknowledged_scan_failure() {
        fn args(app: &MinerApp) -> Vec<String> {
            app.headless_command(Path::new("xus-miner-test"))
                .get_args()
                .map(|arg| arg.to_string_lossy().into_owned())
                .collect()
        }

        let mut app = MinerApp::default();
        app.memory_snapshot = Some(MemorySnapshot {
            available: gib_to_bytes(12.0),
            total: gib_to_bytes(16.0),
        });
        app.memory_acknowledged = false;
        assert!(!args(&app)
            .iter()
            .any(|arg| arg == "--confirm-randomx-memory"));

        app.memory_acknowledged = true;
        assert!(!args(&app)
            .iter()
            .any(|arg| arg == "--confirm-randomx-memory"));

        app.memory_snapshot = None;
        app.memory_acknowledged = false;
        assert!(!args(&app)
            .iter()
            .any(|arg| arg == "--confirm-randomx-memory"));

        app.memory_acknowledged = true;
        let args = args(&app);
        assert_eq!(
            args.iter()
                .filter(|arg| *arg == "--confirm-randomx-memory")
                .count(),
            1
        );
        assert!(args.iter().any(|arg| arg == "--password-stdin"));
        assert!(!args.iter().any(|arg| arg == &app.settings.password));
    }

    #[test]
    fn activity_cell_semantics_and_peer_staleness_are_explicit() {
        assert_eq!(MAX_ACTIVITY_CELLS, 48);
        assert!(ACTIVITY_CELL_SUMMARY.contains("not an incoming job"));
        assert!(ACTIVITY_CELL_SUMMARY.contains("individual hash"));

        let low = MeterSample {
            hashrate: 100.0,
            smoothed_hashrate: 100.0,
            mempool: Some(0),
            height: Some(1),
            round_probability: Some(0.0),
        };
        let high = MeterSample {
            smoothed_hashrate: 1_000.0,
            ..low
        };
        let with_transactions = MeterSample {
            mempool: Some(1),
            ..high
        };
        let dim_cyan = activity_cell_color(&low, 1_000.0);
        let bright_cyan = activity_cell_color(&high, 1_000.0);
        assert!(bright_cyan.r() > dim_cyan.r());
        assert!(bright_cyan.g() > dim_cyan.g());
        assert_eq!(
            activity_cell_color(&with_transactions, 1_000.0),
            GREEN.gamma_multiply(0.88)
        );

        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({"event": "peers", "count": 4}));
        assert_eq!(app.peer_count, Some(4));
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 1,
            "hashrate": 1.0,
            "peer_count": null,
            "mempool_size": 0,
        }));
        assert_eq!(app.peer_count, None);
    }

    #[test]
    fn block_eta_and_round_probability_follow_verified_target_work() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 42,
            "algorithm": "RandomX",
            "job_id": "job-42-a",
            "expected_hashes": 1_000.0,
        }));
        app.apply_telemetry(&json!({
            "event": "worker_ready",
            "worker": 0,
            "mode": "fast-shared",
        }));
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 42,
            "hashrate": 100.0,
            "total_hashes": 100,
            "round_height": 42,
            "round_hashes": 100,
            "round_probability": 1.0 - (-0.1_f64).exp(),
            "mempool_size": 3,
        }));

        assert_eq!(app.smoothed_hashrate, 100.0);
        assert_eq!(app.solo_block_eta_seconds(), Some(10.0));
        let probability = app.current_round_probability().unwrap();
        assert!((probability - (1.0 - (-0.1_f64).exp())).abs() < 1e-12);
        assert_eq!(app.meter_history.len(), 1);
        assert_eq!(app.meter_history.back().unwrap().mempool, Some(3));

        // A refreshed template at the same height keeps the probabilistic round;
        // a new height starts a new round at the current total-hash counter.
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 42,
            "algorithm": "RandomX",
            "job_id": "job-42-b",
            "expected_hashes": 1_000.0,
        }));
        assert_eq!(app.round_hashes, 100);
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 43,
            "algorithm": "RandomX",
            "job_id": "job-43",
            "expected_hashes": 2_000.0,
        }));
        assert_eq!(app.round_hashes, 0);
        assert_eq!(app.current_round_probability(), Some(0.0));

        // A reporter sample assembled across a height transition must not
        // restore the previous round's counters after the new job event.
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 43,
            "hashrate": 100.0,
            "total_hashes": 200,
            "round_height": 42,
            "round_hashes": 200,
            "round_probability": 0.2,
        }));
        assert_eq!(app.round_hashes, 0);
        assert_eq!(app.current_round_probability(), Some(0.0));

        app.apply_telemetry(&json!({"event": "session_error", "message": "offline"}));
        assert_eq!(app.solo_block_eta_seconds(), None);
        assert_eq!(app.current_round_probability(), None);
    }

    #[test]
    fn explicit_jobless_metrics_clear_confirmed_active_work() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 42,
            "algorithm": "RandomX",
            "job_id": "job-42",
            "coinbase": "reward-account",
            "expected_hashes": 1_000.0,
        }));
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 42,
            "hashrate": 100.0,
            "round_height": 42,
            "round_hashes": 100,
            "round_probability": 0.1,
        }));
        assert!(app.job_observed_in_metrics);

        app.apply_telemetry(&json!({
            "event": "share",
            "submitted": 1,
            "accepted": 1,
            "rejected": 0,
        }));
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": null,
            "hashrate": 0.0,
            "round_height": 42,
            "round_hashes": 100,
            "round_probability": 0.1,
        }));

        assert_eq!(app.height, None);
        assert_eq!(app.algorithm, "—");
        assert_eq!(app.job_id, WAITING_FOR_WORK);
        assert_eq!(app.coinbase, None);
        assert_eq!(app.expected_hashes, None);
        assert_eq!(app.round_hashes, 0);
        assert_eq!(app.round_probability, None);
        assert_eq!(app.height_started_at, None);
        assert_eq!(app.meter_history.back().unwrap().height, None);
    }

    #[test]
    fn raced_jobless_metrics_do_not_erase_a_newer_unconfirmed_job() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 43,
            "algorithm": "RandomX",
            "job_id": "job-43",
            "expected_hashes": 2_000.0,
        }));

        // This can be a reporter sample assembled before the job event and
        // emitted after it. One such sample must not erase the newer work.
        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": null,
            "hashrate": 0.0,
            "round_height": 42,
        }));
        assert_eq!(app.height, Some(43));
        assert_eq!(app.job_id, "job-43");
        assert_eq!(app.jobless_metrics_seen, 1);

        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": 43,
            "hashrate": 100.0,
            "round_height": 43,
        }));
        assert!(app.job_observed_in_metrics);
        assert_eq!(app.jobless_metrics_seen, 0);

        app.apply_telemetry(&json!({
            "event": "metrics",
            "height": null,
            "hashrate": 0.0,
            "round_height": 43,
        }));
        assert_eq!(app.height, None);
        assert_eq!(app.job_id, WAITING_FOR_WORK);
    }

    #[test]
    fn repeated_jobless_metrics_or_session_loss_clear_unconfirmed_work() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 44,
            "algorithm": "RandomX",
            "job_id": "job-44",
        }));
        let no_job = json!({
            "event": "metrics",
            "height": null,
            "hashrate": 0.0,
        });
        app.apply_telemetry(&no_job);
        assert_eq!(app.job_id, "job-44");
        app.apply_telemetry(&no_job);
        assert_eq!(app.job_id, WAITING_FOR_WORK);

        app.apply_telemetry(&json!({
            "event": "job",
            "height": 45,
            "algorithm": "RandomX",
            "job_id": "job-45",
        }));
        app.apply_telemetry(&json!({
            "event": "session_error",
            "message": "connection lost",
        }));
        assert_eq!(app.height, None);
        assert_eq!(app.job_id, WAITING_FOR_WORK);
        assert_eq!(app.coinbase, None);
    }

    #[test]
    fn explicit_job_clear_requires_the_current_job_identity() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "job",
            "height": 42,
            "algorithm": "RandomX",
            "job_id": "job-42",
        }));

        app.apply_telemetry(&json!({
            "event": "job_cleared",
            "height": 42,
            "job_id": "different-job",
            "reason": "accepted_block",
        }));
        app.apply_telemetry(&json!({
            "event": "job_cleared",
            "height": 41,
            "job_id": "job-42",
            "reason": "accepted_block",
        }));
        assert_eq!(app.height, Some(42));
        assert_eq!(app.job_id, "job-42");
        assert_eq!(app.last_found_block, None);

        app.apply_telemetry(&json!({
            "event": "job",
            "height": 43,
            "algorithm": "RandomX",
            "job_id": "job-43",
        }));
        app.apply_telemetry(&json!({
            "event": "job_cleared",
            "height": 42,
            "job_id": "job-42",
            "reason": "accepted_block",
        }));
        assert_eq!(app.height, Some(43));
        assert_eq!(app.job_id, "job-43");
        assert_eq!(app.last_found_block, None);

        app.apply_telemetry(&json!({
            "event": "job_cleared",
            "height": 43,
            "job_id": "job-43",
            "reason": "accepted_block",
        }));
        assert_eq!(app.height, None);
        assert_eq!(app.job_id, WAITING_FOR_WORK);
        assert_eq!(app.algorithm, "—");
        assert!(app.last_found_block.is_some_and(|(height, _)| height == 43));
        assert!(app
            .logs
            .back()
            .is_some_and(|line| line.text.contains("BLOCK FOUND AND ACCEPTED")));
    }

    #[test]
    fn network_meter_telemetry_and_eta_labels_are_explicit() {
        let mut app = MinerApp::default();
        app.apply_telemetry(&json!({
            "event": "network",
            "hashrate": 1_361.5,
            "target_block_ms": 150_000,
            "difficulty": "228382",
        }));
        assert_eq!(app.network_hashrate, Some(1_361.5));
        assert_eq!(app.target_block_ms, Some(150_000));
        assert_eq!(app.network_difficulty.as_deref(), Some("228382"));
        assert_eq!(format_eta(462.0), "≈7m 42s");
        assert_eq!(format_probability(0.1234), "12.3%");
    }

    #[test]
    fn live_meter_paints_hashrate_round_resets_and_mempool_cells() {
        let history = VecDeque::from([
            MeterSample {
                hashrate: 1_800.0,
                smoothed_hashrate: 1_750.0,
                mempool: Some(3),
                height: Some(10_194),
                round_probability: Some(0.04),
            },
            MeterSample {
                hashrate: 2_050.0,
                smoothed_hashrate: 1_820.0,
                mempool: Some(1),
                height: Some(10_194),
                round_probability: Some(0.08),
            },
            MeterSample {
                hashrate: 1_920.0,
                smoothed_hashrate: 1_850.0,
                mempool: Some(0),
                height: Some(10_195),
                round_probability: Some(0.01),
            },
        ]);
        let context = egui::Context::default();
        let output = context.run_ui(egui::RawInput::default(), |ui| {
            mining_meter(ui, &history, 2, 220.0);
        });
        assert!(output.shapes.len() > 20);
    }

    #[test]
    fn saved_numeric_settings_are_clamped() {
        let settings = Settings::from_json(&json!({
            "workers": 999,
            "reconnect_secs": 0,
            "report_secs": 99_999,
        }));
        assert_eq!(settings.workers, 64);
        assert_eq!(settings.reconnect_secs, 1);
        assert_eq!(settings.report_secs, 3_600);
    }
}
