//! Persistent crash-diagnostic logging.
//!
//! A GUI-spawned engine has no visible stderr on Windows or macOS, so every
//! process (GUI and isolated engine) also appends timestamped, self-identifying
//! diagnostic lines to a bounded set of log files under the user's
//! `.xus-miner/logs` directory. Each line is flushed immediately and a panic
//! hook records the full backtrace before the process dies, so a crash in the
//! field leaves an actionable record instead of vanishing.
//!
//! This module is plain safe Rust: no signal handlers, no FFI, and no new
//! unsafe surface. Verbosity is controlled by `XUS_MINER_LOG`
//! (`error`|`warn`|`info`|`debug`; default `info`). Credentials are never
//! logged: the Stratum password arrives on stdin and no call site passes it
//! here.

use std::backtrace::Backtrace;
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::panic;
use std::path::PathBuf;
use std::process;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

const VERSION: &str = env!("CARGO_PKG_VERSION");
const LOG_DIRECTORY: &str = ".xus-miner";
const LOG_SUBDIRECTORY: &str = "logs";
const LOG_PREFIX: &str = "diag-";
/// Newest log files retained per prune (including the file being created).
const MAX_RETAINED_LOGS: usize = 10;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

impl Level {
    fn label(self) -> &'static str {
        match self {
            Self::Error => "ERROR",
            Self::Warn => "WARN",
            Self::Info => "INFO",
            Self::Debug => "DEBUG",
        }
    }
}

fn level_from_env(raw: Option<&str>) -> Level {
    match raw.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
        Some("error") => Level::Error,
        Some("warn") | Some("warning") => Level::Warn,
        Some("debug") | Some("trace") => Level::Debug,
        _ => Level::Info,
    }
}

struct Logger {
    file: Mutex<File>,
    path: PathBuf,
    level: Level,
}

static LOGGER: OnceLock<Logger> = OnceLock::new();

/// One stable identity token per line so a pasted log is diagnosable without
/// asking the reporter for their platform.
fn identity() -> String {
    format!(
        "xus-miner/{VERSION} {}-{}",
        env::consts::OS,
        env::consts::ARCH
    )
}

#[allow(clippy::vec_init_then_push)]
fn cpu_feature_summary() -> String {
    let mut features: Vec<&str> = Vec::new();
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("aes") {
            features.push("aes");
        }
        if std::arch::is_x86_feature_detected!("ssse3") {
            features.push("ssse3");
        }
        if std::arch::is_x86_feature_detected!("sse4.1") {
            features.push("sse4.1");
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            features.push("avx2");
        }
        if std::arch::is_x86_feature_detected!("avx512f") {
            features.push("avx512f");
        }
    }
    #[cfg(target_arch = "aarch64")]
    {
        features.push("neon");
        if std::arch::is_aarch64_feature_detected!("aes") {
            features.push("aes");
        }
        if std::arch::is_aarch64_feature_detected!("sha2") {
            features.push("sha2");
        }
    }
    if features.is_empty() {
        "none-detected".to_owned()
    } else {
        features.join("+")
    }
}

/// Decodes the RandomX C-API flag bits for the lifecycle log. Numeric values
/// follow `randomx.h`; this is diagnostic text only and is never fed back into
/// the native library.
pub(crate) fn describe_randomx_flags(bits: u32) -> String {
    const NAMES: [(u32, &str); 7] = [
        (1, "LARGE_PAGES"),
        (1 << 1, "HARD_AES"),
        (1 << 2, "FULL_MEM"),
        (1 << 3, "JIT"),
        (1 << 4, "SECURE"),
        (1 << 5, "ARGON2_SSSE3"),
        (1 << 6, "ARGON2_AVX2"),
    ];
    let mut parts: Vec<&str> = NAMES
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, name)| *name)
        .collect();
    if parts.is_empty() {
        parts.push("DEFAULT(interpreted,portable)");
    }
    format!("0x{bits:02x}={}", parts.join("|"))
}

/// Civil-from-days conversion (Howard Hinnant's algorithm) for UTC timestamps
/// without a date-time dependency.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

fn format_utc(unix_millis: u128) -> String {
    let secs = (unix_millis / 1_000) as i64;
    let millis = (unix_millis % 1_000) as u32;
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{millis:03}Z",
        tod / 3_600,
        (tod / 60) % 60,
        tod % 60,
    )
}

fn now_utc() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| format_utc(elapsed.as_millis()))
        .unwrap_or_else(|_| "pre-epoch".into())
}

fn log_directory() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .map(|home| home.join(LOG_DIRECTORY).join(LOG_SUBDIRECTORY))
}

/// Keeps the newest diagnostic logs and removes the rest so long-lived
/// installs cannot grow the directory without bound.
fn prune_old_logs(directory: &PathBuf) {
    let Ok(entries) = fs::read_dir(directory) else {
        return;
    };
    let mut logs: Vec<(SystemTime, PathBuf)> = entries
        .flatten()
        .filter_map(|entry| {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy().into_owned();
            if !name.starts_with(LOG_PREFIX) || !name.ends_with(".log") {
                return None;
            }
            let metadata = entry.metadata().ok()?;
            if !metadata.is_file() {
                return None;
            }
            let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
            Some((modified, path))
        })
        .collect();
    if logs.len() < MAX_RETAINED_LOGS {
        return;
    }
    logs.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, stale) in logs.split_off(MAX_RETAINED_LOGS - 1) {
        let _ = fs::remove_file(stale);
    }
}

fn write_line(logger: &Logger, level: Level, message: &str) {
    if level > logger.level {
        return;
    }
    let mut file = logger.file.lock().unwrap_or_else(|poisoned| {
        // Keep logging after a panicking thread poisoned the lock; the file
        // handle itself is still valid and every write is a full line.
        poisoned.into_inner()
    });
    let line = format!(
        "[{}] [{}] [{}] {message}\n",
        now_utc(),
        level.label(),
        identity(),
    );
    // Flush per line: a crash immediately after a write must not lose it.
    let _ = file.write_all(line.as_bytes());
    let _ = file.flush();
}

fn install_panic_hook() {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        if let Some(logger) = LOGGER.get() {
            let location = info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "unknown location".into());
            let payload = info
                .payload()
                .downcast_ref::<&str>()
                .map(|message| (*message).to_owned())
                .or_else(|| info.payload().downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "non-string panic payload".into());
            write_line(
                logger,
                Level::Error,
                &format!("PANIC at {location}: {payload}"),
            );
            // `force_capture` records frames even without RUST_BACKTRACE.
            let backtrace = Backtrace::force_capture().to_string();
            for frame_line in backtrace.lines() {
                write_line(logger, Level::Error, &format!("PANIC-BT {frame_line}"));
            }
            write_line(logger, Level::Error, "PANIC backtrace complete");
        }
        previous(info);
    }));
}

/// Opens the per-process diagnostic log and installs the panic hook. Safe to
/// call once per process; a second call is ignored. Failure to open the log is
/// reported on stderr and never blocks mining.
pub(crate) fn init(component: &str) {
    if LOGGER.get().is_some() {
        return;
    }
    let Some(directory) = log_directory() else {
        eprintln!("diagnostic log disabled: no HOME/USERPROFILE directory");
        return;
    };
    if let Err(error) = fs::create_dir_all(&directory) {
        eprintln!(
            "diagnostic log disabled: cannot create {}: {error}",
            directory.display()
        );
        return;
    }
    prune_old_logs(&directory);
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let path = directory.join(format!(
        "{LOG_PREFIX}{component}-{stamp}-{}.log",
        process::id()
    ));
    let file = match OpenOptions::new().append(true).create(true).open(&path) {
        Ok(file) => file,
        Err(error) => {
            eprintln!(
                "diagnostic log disabled: cannot open {}: {error}",
                path.display()
            );
            return;
        }
    };
    let level = level_from_env(env::var("XUS_MINER_LOG").ok().as_deref());
    let logger = Logger {
        file: Mutex::new(file),
        path,
        level,
    };
    if LOGGER.set(logger).is_err() {
        return;
    }
    install_panic_hook();
    let logger = LOGGER.get().expect("diagnostic logger just installed");
    eprintln!("diagnostic log: {}", logger.path.display());
    info(&format!(
        "log opened: component={component} pid={} level={} cpu-features={}",
        process::id(),
        logger.level.label(),
        cpu_feature_summary(),
    ));
}

pub(crate) fn log(level: Level, message: &str) {
    if let Some(logger) = LOGGER.get() {
        write_line(logger, level, message);
    }
}

pub(crate) fn error(message: &str) {
    log(Level::Error, message);
}

pub(crate) fn warn(message: &str) {
    log(Level::Warn, message);
}

pub(crate) fn info(message: &str) {
    log(Level::Info, message);
}

pub(crate) fn debug(message: &str) {
    log(Level::Debug, message);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_formatting_matches_known_instants() {
        assert_eq!(format_utc(0), "1970-01-01T00:00:00.000Z");
        // 2026-07-23T12:34:56.789Z
        assert_eq!(format_utc(1_784_810_096_789), "2026-07-23T12:34:56.789Z");
        // Leap-day coverage: 2024-02-29T00:00:00Z.
        assert_eq!(format_utc(1_709_164_800_000), "2024-02-29T00:00:00.000Z");
    }

    #[test]
    fn log_level_parsing_is_strict_and_defaults_to_info() {
        assert_eq!(level_from_env(None), Level::Info);
        assert_eq!(level_from_env(Some("debug")), Level::Debug);
        assert_eq!(level_from_env(Some(" WARN ")), Level::Warn);
        assert_eq!(level_from_env(Some("error")), Level::Error);
        assert_eq!(level_from_env(Some("nonsense")), Level::Info);
        assert!(Level::Error < Level::Debug);
    }

    #[test]
    fn randomx_flag_description_is_exact() {
        assert_eq!(
            describe_randomx_flags(0),
            "0x00=DEFAULT(interpreted,portable)"
        );
        assert_eq!(describe_randomx_flags(1 << 2), "0x04=FULL_MEM");
        assert_eq!(
            describe_randomx_flags((1 << 3) | (1 << 1) | (1 << 6)),
            "0x4a=HARD_AES|JIT|ARGON2_AVX2"
        );
    }

    #[test]
    fn identity_names_version_os_and_arch() {
        let identity = identity();
        assert!(identity.contains(env!("CARGO_PKG_VERSION")));
        assert!(identity.contains(env::consts::OS));
        assert!(identity.contains(env::consts::ARCH));
    }
}
