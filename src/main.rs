#![deny(unsafe_code)]

mod blockflow;
mod diag;
mod gui;
mod pow;
// RandomX exposes a C API. Keep the required unsafe ownership proof isolated
// to one small module; unsafe code remains denied everywhere else.
#[allow(unsafe_code)]
mod randomx_native;
mod wire;

use borsh::BorshDeserialize;
use pow::{pow_hash_mining, MiningHash, MiningMode, PowAlgo};
use serde_json::{json, Value};
use std::any::Any;
use std::env;
use std::io::{self, BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::process;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant};
#[cfg(not(target_os = "macos"))]
use sysinfo::{MemoryRefreshKind, System};
use wire::{template_id_for_blob, validate_account_id, BlockHeaderWire};

pub(crate) const VERSION: &str = env!("CARGO_PKG_VERSION");
const MAX_LINE_BYTES: usize = 64 * 1024;
const MAX_BLOB_BYTES: usize = 1024 * 1024;
// A maximum-size template is hex-encoded in JSON (2 bytes per blob byte).
// Keep ample room for the remaining RPC fields without allowing a configured
// or malicious node to grow the miner's response buffer without bound.
const MAX_RPC_BODY_BYTES: usize = MAX_BLOB_BYTES * 2 + 256 * 1024;
const MAX_RPC_HEADER_BYTES: usize = 32 * 1024;
const MAX_RPC_RESPONSE_BYTES: usize = MAX_RPC_HEADER_BYTES + MAX_RPC_BODY_BYTES;
const BYTES_PER_GIB: f64 = 1_073_741_824.0;
const BYTES_PER_MIB: f64 = 1_048_576.0;
pub(crate) const RANDOMX_DATASET_GIB: f64 = 2.3;
pub(crate) const RANDOMX_LIGHT_CACHE_MIB: f64 = 256.0;
pub(crate) const RANDOMX_WORKER_MIB: f64 = 32.0;
const MIN_MEMORY_HEADROOM_GIB: f64 = 1.5;
const MAX_MEMORY_HEADROOM_GIB: f64 = 4.0;
const DIRECT_RPC_TIMEOUT: Duration = Duration::from_secs(5);
const WORK_BATCH: u64 = 256;
const LOGIN_ID: u64 = 1;
const KEEPALIVE_ID: u64 = 2;
const FIRST_SUBMIT_ID: u64 = 1_000;
#[cfg(target_os = "macos")]
const MACOS_MEMORY_PRESSURE_PATH: &str = "/usr/bin/memory_pressure";
#[cfg(target_os = "macos")]
const MAX_MACOS_MEMORY_PRESSURE_BYTES: usize = 4 * 1024;
#[cfg(target_os = "macos")]
const MACOS_MEMORY_PRESSURE_TIMEOUT: Duration = Duration::from_millis(500);

#[cfg(any(target_os = "macos", test))]
fn parse_macos_memory_pressure(raw: &str) -> Result<(u64, u64), String> {
    let mut total = None;
    let mut free_percent = None;
    for line in raw.lines().map(str::trim) {
        if let Some(rest) = line.strip_prefix("The system has ") {
            total = rest
                .split_whitespace()
                .next()
                .and_then(|value| value.parse::<u64>().ok());
        } else if let Some(rest) = line.strip_prefix("System-wide memory free percentage:") {
            free_percent = rest
                .trim()
                .strip_suffix('%')
                .and_then(|value| value.trim().parse::<u64>().ok());
        }
    }

    let total = total.filter(|value| *value > 0).ok_or_else(|| {
        "memory_pressure output did not contain a positive physical-memory total".to_owned()
    })?;
    let free_percent = free_percent.filter(|value| *value <= 100).ok_or_else(|| {
        "memory_pressure output did not contain a valid free percentage".to_owned()
    })?;
    let available = ((u128::from(total) * u128::from(free_percent)) / 100) as u64;
    Ok((available, total))
}

#[cfg(target_os = "macos")]
fn read_bounded_command_stream<R: Read>(
    mut stream: R,
    label: &'static str,
) -> Result<Vec<u8>, String> {
    let mut captured = Vec::with_capacity(MAX_MACOS_MEMORY_PRESSURE_BYTES);
    let mut buffer = [0_u8; 1024];
    let mut oversized = false;
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("cannot read memory_pressure {label}: {error}"))?;
        if count == 0 {
            break;
        }
        let remaining = MAX_MACOS_MEMORY_PRESSURE_BYTES.saturating_sub(captured.len());
        let retained = remaining.min(count);
        captured.extend_from_slice(&buffer[..retained]);
        oversized |= retained < count;
    }
    if oversized {
        Err(format!(
            "memory_pressure {label} exceeded {MAX_MACOS_MEMORY_PRESSURE_BYTES} bytes"
        ))
    } else {
        Ok(captured)
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn query_memory_counters() -> Result<(u64, u64), String> {
    let started = Instant::now();
    let result = query_memory_counters_impl();
    match &result {
        Ok((available, total)) => diag::debug(&format!(
            "memory_pressure probe ok in {} ms: available={available} total={total}",
            started.elapsed().as_millis(),
        )),
        Err(error) => diag::warn(&format!(
            "memory_pressure probe failed after {} ms: {error}",
            started.elapsed().as_millis(),
        )),
    }
    result
}

#[cfg(target_os = "macos")]
fn query_memory_counters_impl() -> Result<(u64, u64), String> {
    // libc 0.2.189 expanded vm_statistics64 for the macOS 27 SDK. Calling
    // host_statistics64 with that newer count on macOS 26 makes the kernel
    // report a potentially memory-corrupting ABI mismatch. Keep Apple memory
    // telemetry entirely outside application FFI and invoke the fixed,
    // absolute-path operating-system utility with an empty environment.
    let mut child = process::Command::new(MACOS_MEMORY_PRESSURE_PATH)
        .arg("-Q")
        .env_clear()
        .stdin(process::Stdio::null())
        .stdout(process::Stdio::piped())
        .stderr(process::Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run {MACOS_MEMORY_PRESSURE_PATH}: {error}"))?;

    let stdout = child
        .stdout
        .take()
        .ok_or_else(|| "memory_pressure stdout pipe was not created".to_owned())?;
    let stderr = child
        .stderr
        .take()
        .ok_or_else(|| "memory_pressure stderr pipe was not created".to_owned())?;
    let stdout_reader = thread::Builder::new()
        .name("xus-memory-pressure-out".into())
        .spawn(move || read_bounded_command_stream(stdout, "stdout"))
        .map_err(|error| {
            let _ = child.kill();
            let _ = child.wait();
            format!("cannot start memory_pressure stdout reader: {error}")
        })?;
    let stderr_reader = match thread::Builder::new()
        .name("xus-memory-pressure-err".into())
        .spawn(move || read_bounded_command_stream(stderr, "stderr"))
    {
        Ok(reader) => reader,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = stdout_reader.join();
            return Err(format!(
                "cannot start memory_pressure stderr reader: {error}"
            ));
        }
    };

    let deadline = Instant::now() + MACOS_MEMORY_PRESSURE_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(5));
            }
            Ok(None) => {
                return match child.kill().and_then(|()| child.wait()) {
                    Ok(_) => {
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        Err(format!(
                            "memory_pressure exceeded its {} ms deadline",
                            MACOS_MEMORY_PRESSURE_TIMEOUT.as_millis(),
                        ))
                    }
                    Err(error) => {
                        // Do not join readers whose pipe owner could still be
                        // live. Dropping the handles detaches them and keeps a
                        // failed OS cleanup from freezing the GUI indefinitely.
                        drop(stdout_reader);
                        drop(stderr_reader);
                        Err(format!(
                            "memory_pressure exceeded its {} ms deadline; cleanup failed: {error}",
                            MACOS_MEMORY_PRESSURE_TIMEOUT.as_millis(),
                        ))
                    }
                };
            }
            Err(error) => {
                return match child.kill().and_then(|()| child.wait()) {
                    Ok(_) => {
                        let _ = stdout_reader.join();
                        let _ = stderr_reader.join();
                        Err(format!("cannot observe memory_pressure: {error}"))
                    }
                    Err(cleanup_error) => {
                        drop(stdout_reader);
                        drop(stderr_reader);
                        Err(format!(
                            "cannot observe memory_pressure: {error}; cleanup failed: {cleanup_error}"
                        ))
                    }
                };
            }
        }
    };

    let stdout = stdout_reader
        .join()
        .map_err(|_| "memory_pressure stdout reader panicked".to_owned())??;
    let stderr = stderr_reader
        .join()
        .map_err(|_| "memory_pressure stderr reader panicked".to_owned())??;
    if !status.success() {
        let detail = String::from_utf8_lossy(&stderr);
        return Err(format!(
            "{MACOS_MEMORY_PRESSURE_PATH} returned {status}: {}",
            detail.trim()
        ));
    }
    let body = std::str::from_utf8(&stdout)
        .map_err(|error| format!("memory_pressure returned invalid UTF-8: {error}"))?;
    parse_macos_memory_pressure(body)
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn query_memory_counters() -> Result<(u64, u64), String> {
    let mut system = System::new();
    system.refresh_memory_specifics(MemoryRefreshKind::nothing().with_ram());
    let available = system.available_memory();
    let total = system.total_memory();
    if total == 0 || available > total {
        return Err(
            "operating-system RAM scan returned unavailable or inconsistent readings; refusing to start headless RandomX workers".into(),
        );
    }
    Ok((available, total))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HeadlessMemoryPreflight {
    available: Option<u64>,
    total: Option<u64>,
    engine: u64,
    reserve: u64,
}

struct RandomxMemoryGate {
    workers: u64,
    force_light: bool,
    unavailable_scan_confirmed: bool,
    approved: Option<HeadlessMemoryPreflight>,
}

impl RandomxMemoryGate {
    fn new(workers: u64, force_light: bool, unavailable_scan_confirmed: bool) -> Self {
        Self {
            workers,
            force_light,
            unavailable_scan_confirmed,
            approved: None,
        }
    }

    fn ensure_safe(&mut self, algo: PowAlgo) -> Result<(), String> {
        if algo != PowAlgo::RandomX || self.approved.is_some() {
            return Ok(());
        }
        let (available, total) = match query_memory_counters() {
            Ok(counters) => counters,
            Err(error) => {
                diag::warn(&format!(
                    "memory preflight scan unavailable before the first RandomX job: {error}"
                ));
                (0, 0)
            }
        };
        let preflight = resolve_headless_memory(
            self.workers,
            self.force_light,
            available,
            total,
            self.unavailable_scan_confirmed,
        )
        .inspect_err(|error| {
            diag::error(&format!(
                "memory preflight refused RandomX start for {} worker(s): {error}",
                self.workers
            ));
        })?;
        diag::info(&format!(
            "memory preflight approved: workers={} force-light={} available={:?} total={:?} engine-bytes={} reserve-bytes={}",
            self.workers,
            self.force_light,
            preflight.available,
            preflight.total,
            preflight.engine,
            preflight.reserve,
        ));
        match (preflight.available, preflight.total) {
            (Some(available), Some(total)) => eprintln!(
                "memory preflight PASS: {:.1} GiB available / {:.1} GiB total; {:.1} GiB RandomX estimate + {:.1} GiB reserved",
                available as f64 / BYTES_PER_GIB,
                total as f64 / BYTES_PER_GIB,
                preflight.engine as f64 / BYTES_PER_GIB,
                preflight.reserve as f64 / BYTES_PER_GIB,
            ),
            _ => eprintln!(
                "memory preflight manually confirmed: {:.1} GiB RandomX estimate + at least {:.1} GiB reserved",
                preflight.engine as f64 / BYTES_PER_GIB,
                preflight.reserve as f64 / BYTES_PER_GIB,
            ),
        }
        self.approved = Some(preflight);
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Config {
    pool: String,
    user: String,
    password: String,
    workers: u64,
    reconnect_secs: u64,
    report_secs: u64,
    randomx_light: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            pool: "127.0.0.1:3333".into(),
            user: "xus-miner".into(),
            password: "x".into(),
            // The fast RandomX path shares one read-only dataset. One worker
            // remains the quiet, conservative default.
            workers: 1,
            reconnect_secs: 5,
            report_secs: 10,
            randomx_light: false,
        }
    }
}

fn usage() -> &'static str {
    "xus-miner — native GUI and standalone SOV/XUS Stratum CPU miner\n\n\
     USAGE: xus-miner [--gui]\n\
            xus-miner --headless [OPTIONS]\n\n\
       --gui                    Open the native operator console [default with no args]\n\
       --headless               Run as a terminal/server process\n\
       --pool <host:port>       SOV node RPC (:8645) or Stratum pool [127.0.0.1:3333]\n\
       --user <label>           Public account/worker label [xus-miner]\n\
       --password <value>       Stratum password [x]\n\
       --password-stdin         Read the Stratum password from standard input\n\
       --workers <n>            Mining threads [1; one ~2.3 GiB dataset is shared]\n\
       --randomx-light          Portable low-memory RandomX recovery (slower)\n\
       --confirm-randomx-memory Confirm required RAM only if the OS scan is unavailable\n\
       --reconnect-secs <n>     Delay after disconnect [5]\n\
       --report-secs <n>        Hashrate reporting interval [10]\n\
       --version                Print the exact application/release version\n\
       --help                   Show this help\n\n\
     No wallet seed or private key is used by this program."
}

fn parse_u64(flag: &str, raw: &str, min: u64, max: u64) -> u64 {
    let parsed = raw.parse::<u64>().unwrap_or_else(|_| {
        eprintln!("error: {flag} expects an integer, got `{raw}`");
        process::exit(2);
    });
    if !(min..=max).contains(&parsed) {
        eprintln!("error: {flag} must be in {min}..={max}, got {parsed}");
        process::exit(2);
    }
    parsed
}

fn parse_args_from<I>(args: I) -> Config
where
    I: IntoIterator<Item = String>,
{
    let mut cfg = Config::default();
    let mut args = args.into_iter();
    while let Some(flag) = args.next() {
        let value = |flag: &str, args: &mut I::IntoIter| {
            args.next().unwrap_or_else(|| {
                eprintln!("error: {flag} requires a value");
                process::exit(2);
            })
        };
        match flag.as_str() {
            "--pool" => cfg.pool = value("--pool", &mut args),
            "--user" => cfg.user = value("--user", &mut args),
            "--password" => cfg.password = value("--password", &mut args),
            "--workers" => {
                cfg.workers = parse_u64("--workers", &value("--workers", &mut args), 1, 64)
            }
            "--randomx-light" => cfg.randomx_light = true,
            "--reconnect-secs" => {
                cfg.reconnect_secs = parse_u64(
                    "--reconnect-secs",
                    &value("--reconnect-secs", &mut args),
                    1,
                    3600,
                )
            }
            "--report-secs" => {
                cfg.report_secs =
                    parse_u64("--report-secs", &value("--report-secs", &mut args), 1, 3600)
            }
            "--help" | "-h" => {
                println!("{}", usage());
                process::exit(0);
            }
            "--version" | "-V" => {
                println!("xus-miner {VERSION}");
                process::exit(0);
            }
            other => {
                eprintln!("error: unknown option `{other}`\n\n{}", usage());
                process::exit(2);
            }
        }
    }
    if cfg.pool.trim().is_empty() || cfg.user.trim().is_empty() {
        eprintln!("error: --pool and --user cannot be empty");
        process::exit(2);
    }
    cfg
}

#[derive(Clone, Debug)]
struct Job {
    id: String,
    blob: Vec<u8>,
    nonce_offset: usize,
    /// Threshold used to decide whether a submitted share/block is valid.
    target: [u8; 32],
    /// Consensus block target when disclosed separately from a pool share target.
    block_target: Option<[u8; 32]>,
    algo: PowAlgo,
    pow_key: Vec<u8>,
    height: u64,
    timestamp_ms: Option<u64>,
    /// Reward destination disclosed by the node/pool for this exact job.
    coinbase: Option<String>,
}

impl Job {
    fn from_stratum(value: &Value) -> Result<Self, String> {
        let string = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| format!("job missing non-empty string `{name}`"))
        };
        let number = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("job missing integer `{name}`"))
        };

        let id = string("job_id")?.to_owned();
        if id.len() > 256 {
            return Err("job_id exceeds 256 bytes".into());
        }
        let blob = hex::decode(string("blob")?).map_err(|e| format!("invalid blob hex: {e}"))?;
        if blob.len() < 8 || blob.len() > MAX_BLOB_BYTES {
            return Err(format!(
                "blob length {} outside 8..={MAX_BLOB_BYTES}",
                blob.len()
            ));
        }
        let nonce_offset = usize::try_from(number("nonce_offset")?)
            .map_err(|_| "nonce_offset does not fit this platform".to_string())?;
        if number("nonce_size")? != 8 {
            return Err("SOV jobs require nonce_size 8".into());
        }
        if nonce_offset != blob.len() - 8 {
            return Err(format!(
                "nonce_offset {nonce_offset} does not identify the trailing u64 in {}-byte blob",
                blob.len()
            ));
        }

        let target_bytes = hex::decode(string("target_full")?)
            .map_err(|e| format!("invalid target_full hex: {e}"))?;
        let target: [u8; 32] = target_bytes
            .try_into()
            .map_err(|v: Vec<u8>| format!("target_full is {} bytes, want 32", v.len()))?;
        let block_target = value
            .get("network_target_full")
            .or_else(|| value.get("network_target"))
            .and_then(Value::as_str)
            .map(|encoded| {
                hex::decode(encoded)
                    .map_err(|e| format!("invalid network block target hex: {e}"))?
                    .try_into()
                    .map_err(|v: Vec<u8>| {
                        format!("network block target is {} bytes, want 32", v.len())
                    })
            })
            .transpose()?;

        let algo = match string("algo")? {
            "rx/0" => PowAlgo::RandomX,
            "sha256d" => PowAlgo::Sha256d,
            other => return Err(format!("unsupported algorithm `{other}`")),
        };
        let seed = value
            .get("seed_hash")
            .and_then(Value::as_str)
            .ok_or_else(|| "job missing string `seed_hash`".to_string())?;
        let pow_key = hex::decode(seed).map_err(|e| format!("invalid seed_hash hex: {e}"))?;
        if algo == PowAlgo::RandomX && pow_key.is_empty() {
            return Err("RandomX seed_hash cannot be empty".into());
        }

        Ok(Self {
            id,
            blob,
            nonce_offset,
            target,
            block_target,
            algo,
            pow_key,
            height: number("height")?,
            timestamp_ms: None,
            coinbase: value
                .get("coinbase")
                .or_else(|| value.get("coinbase_account"))
                .and_then(Value::as_str)
                .filter(|account| !account.is_empty())
                .map(str::to_owned),
        })
    }

    fn from_rpc(value: &Value) -> Result<Self, String> {
        let string = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_str)
                .ok_or_else(|| format!("template missing string `{name}`"))
        };
        let number = |name: &str| {
            value
                .get(name)
                .and_then(Value::as_u64)
                .ok_or_else(|| format!("template missing integer `{name}`"))
        };
        let template_id = string("templateId")?.to_owned();
        let height = number("height")?;
        let timestamp_ms = number("timestampMs")?;
        let blob =
            hex::decode(string("blob")?).map_err(|e| format!("invalid template blob: {e}"))?;
        if blob.len() < 8 || blob.len() > MAX_BLOB_BYTES {
            return Err(format!(
                "template blob is {} bytes; expected 8..={MAX_BLOB_BYTES}",
                blob.len()
            ));
        }
        let nonce_offset = usize::try_from(number("nonceOffset")?)
            .map_err(|_| "template nonceOffset does not fit this platform".to_string())?;
        if nonce_offset != blob.len() - 8 {
            return Err("node returned an incompatible nonce offset".into());
        }
        let target: [u8; 32] = hex::decode(string("target")?)
            .map_err(|e| format!("invalid template target: {e}"))?
            .try_into()
            .map_err(|v: Vec<u8>| format!("template target is {} bytes, want 32", v.len()))?;
        let algo = match string("powAlgo")? {
            "RandomX" => PowAlgo::RandomX,
            "Sha256d" => PowAlgo::Sha256d,
            other => return Err(format!("unsupported node PoW algorithm `{other}`")),
        };
        let pow_key =
            hex::decode(string("powKey")?).map_err(|e| format!("invalid template powKey: {e}"))?;
        if pow_key.len() != 32 {
            return Err(format!(
                "template powKey is {} bytes; SOV 0.1.99 requires 32",
                pow_key.len()
            ));
        }
        let coinbase = string("proposer")?.trim().to_owned();
        if coinbase.is_empty() {
            return Err("template proposer cannot be empty".into());
        }
        validate_account_id(&coinbase)
            .map_err(|error| format!("template proposer is not a valid SOV account: {error}"))?;
        let header = BlockHeaderWire::try_from_slice(&blob)
            .map_err(|e| format!("template blob is not a valid SOV block header: {e}"))?;
        if header.proposer != coinbase {
            return Err(format!(
                "template proposer `{coinbase}` does not match encoded header coinbase `{}`",
                header.proposer
            ));
        }
        if header.height != height {
            return Err(format!(
                "template height {height} does not match encoded header height {}",
                header.height
            ));
        }
        if header.timestamp_ms != timestamp_ms {
            return Err(format!(
                "template timestamp {timestamp_ms} does not match encoded header timestamp {}",
                header.timestamp_ms
            ));
        }
        if let Some(version_bits) = value.get("versionBits").and_then(Value::as_u64) {
            if version_bits > u64::from(u32::MAX) || header.version_bits != version_bits as u32 {
                return Err("template versionBits does not match encoded header".into());
            }
        }
        if let Some(bits) = value.get("bits").and_then(Value::as_u64) {
            if bits > u64::from(u32::MAX) || header.bits != bits as u32 {
                return Err("template bits does not match encoded header".into());
            }
        }
        let committed_target = target_from_compact(header.bits).ok_or_else(|| {
            "template header contains invalid compact difficulty bits".to_string()
        })?;
        if target != committed_target {
            return Err(format!(
                "template target `{}` does not match compact header difficulty `{}`",
                hex::encode(target),
                hex::encode(committed_target),
            ));
        }
        let encoded_template_id = template_id_for_blob(&blob);
        if encoded_template_id != template_id {
            return Err(format!(
                "template ID `{template_id}` does not match encoded header ID `{encoded_template_id}`"
            ));
        }
        Ok(Self {
            id: template_id,
            blob,
            nonce_offset,
            target,
            block_target: Some(target),
            algo,
            pow_key,
            height,
            timestamp_ms: Some(timestamp_ms),
            // `proposer` is encoded into the header preimage. Requiring it here
            // makes the displayed reward account a property of the work being
            // hashed, not an unverified copy of the user's input.
            coinbase: Some(coinbase),
        })
    }

    fn seal_blob<F>(&self, blob: &[u8], before_build: F) -> Result<MiningHash, String>
    where
        F: FnOnce(),
    {
        pow_hash_mining(self.algo, &self.pow_key, blob, before_build)
    }
}

/// Decode SOV's Bitcoin-style compact `nBits` commitment into the exact
/// big-endian target the miner must use. Kept client-side so an RPC cannot
/// substitute an easier target than the one authenticated by the header blob.
fn target_from_compact(bits: u32) -> Option<[u8; 32]> {
    let size = (bits >> 24) as usize;
    let mantissa = bits & 0x007f_ffff;
    if bits & 0x0080_0000 != 0 {
        return None;
    }
    if mantissa == 0 {
        return Some([0; 32]);
    }
    if size > 34 || (mantissa > 0xff && size > 33) || (mantissa > 0xffff && size > 32) {
        return None;
    }
    let mut target = [0u8; 32];
    if size <= 3 {
        let value = mantissa >> (8 * (3 - size));
        target[29] = (value >> 16) as u8;
        target[30] = (value >> 8) as u8;
        target[31] = value as u8;
    } else {
        for (offset, byte) in [
            (mantissa >> 16) as u8,
            (mantissa >> 8) as u8,
            mantissa as u8,
        ]
        .into_iter()
        .enumerate()
        {
            if let Some(index) = (32 + offset).checked_sub(size) {
                if index < 32 {
                    target[index] = byte;
                }
            }
        }
    }
    Some(target)
}

/// Expected independent hashes before a uniformly distributed 256-bit seal is
/// at or below this inclusive big-endian target. This is `(2^256)/(target+1)`.
fn expected_hashes_for_target(target: &[u8; 32]) -> f64 {
    let mut success_probability = 2.0_f64.powi(-256); // the inclusive `+ 1`
    for (index, byte) in target.iter().enumerate() {
        success_probability += f64::from(*byte) * 2.0_f64.powi(-8 * (index as i32 + 1));
    }
    success_probability.recip()
}

#[derive(Default)]
struct RoundStats {
    height: Option<u64>,
    hashes: u64,
    hazard: f64,
}

struct State {
    job: RwLock<Option<Arc<Job>>>,
    connection: RwLock<Option<Arc<Connection>>>,
    rpc_endpoint: RwLock<Option<String>>,
    generation: AtomicU64,
    next_request: AtomicU64,
    interval_hashes: AtomicU64,
    total_hashes: AtomicU64,
    submitted: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    mempool_size: AtomicU64,
    mempool_known: AtomicBool,
    peer_count: AtomicU64,
    peers_known: AtomicBool,
    round: Mutex<RoundStats>,
    confirmed_coinbase: RwLock<Option<String>>,
    /// Blocks this miner sealed and the node confirmed accepted; feeds the
    /// "yours" highlight of the block-flow strip.
    sealed_blocks: Mutex<blockflow::SealedBlocks>,
    json_events: bool,
}

impl State {
    fn new(json_events: bool) -> Self {
        Self {
            job: RwLock::new(None),
            connection: RwLock::new(None),
            rpc_endpoint: RwLock::new(None),
            generation: AtomicU64::new(0),
            next_request: AtomicU64::new(FIRST_SUBMIT_ID),
            interval_hashes: AtomicU64::new(0),
            total_hashes: AtomicU64::new(0),
            submitted: AtomicU64::new(0),
            accepted: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            mempool_size: AtomicU64::new(0),
            mempool_known: AtomicBool::new(false),
            peer_count: AtomicU64::new(0),
            peers_known: AtomicBool::new(false),
            round: Mutex::new(RoundStats::default()),
            confirmed_coinbase: RwLock::new(None),
            sealed_blocks: Mutex::new(blockflow::SealedBlocks::default()),
            json_events,
        }
    }

    fn record_sealed_block(&self, height: u64, hash: String) {
        self.sealed_blocks
            .lock()
            .expect("sealed blocks lock")
            .record(height, hash);
    }

    fn emit(&self, event: Value) {
        if self.json_events {
            let mut stdout = io::stdout().lock();
            if writeln!(stdout, "{event}")
                .and_then(|()| stdout.flush())
                .is_err()
            {
                // JSON stdout is the GUI supervision pipe when launched by the
                // desktop app. A broken pipe means the parent/reader vanished;
                // exit the isolated engine so it cannot become an orphan.
                diag::error(
                    "telemetry stdout pipe is broken (GUI parent or reader is gone); exiting the isolated engine with status 70",
                );
                process::exit(70);
            }
        }
    }

    fn install_job(&self, job: Job) {
        eprintln!("job {}: height {} ({:?})", job.id, job.height, job.algo);
        {
            let mut round = self.round.lock().expect("round telemetry lock");
            if round.height != Some(job.height) {
                *round = RoundStats {
                    height: Some(job.height),
                    ..RoundStats::default()
                };
            }
        }
        if let Some(account) = job.coinbase.as_deref() {
            let mut confirmed = self
                .confirmed_coinbase
                .write()
                .expect("confirmed coinbase lock");
            if confirmed.as_deref() != Some(account) {
                eprintln!(
                    "COINBASE CONFIRMED by block template: {account} | block reward destination for height {}",
                    job.height
                );
                *confirmed = Some(account.to_owned());
            }
        }
        let expected_hashes = job.block_target.as_ref().map(expected_hashes_for_target);
        let network_target = job.block_target.map(hex::encode);
        let event = json!({
            "event": "job",
            "job_id": job.id,
            "height": job.height,
            "algorithm": match job.algo {
                PowAlgo::RandomX => "RandomX",
                PowAlgo::Sha256d => "SHA-256d",
            },
            "coinbase": job.coinbase,
            "network_target": network_target,
            "expected_hashes": expected_hashes,
        });
        let mut job_slot = self.job.write().expect("job lock");
        *job_slot = Some(Arc::new(job));
        // Emit while holding the slot write lock: the reporter cannot publish a
        // new-height metrics line ahead of this job event, and workers are not
        // released onto the new generation until after the event is visible.
        self.emit(event);
        drop(job_slot);
        self.generation.fetch_add(1, Ordering::Release);
    }

    fn record_round_hashes(&self, job: &Job, completed: u64) {
        let Some(block_target) = job.block_target.as_ref() else {
            return;
        };
        let mut round = self.round.lock().expect("round telemetry lock");
        if round.height == Some(job.height) {
            round.hashes = round.hashes.saturating_add(completed);
            round.hazard += completed as f64 / expected_hashes_for_target(block_target);
        }
    }

    fn round_snapshot(&self) -> (Option<u64>, u64, f64) {
        let round = self.round.lock().expect("round telemetry lock");
        (round.height, round.hashes, -(-round.hazard).exp_m1())
    }

    fn clear_connection(&self, connection: &Arc<Connection>) {
        let mut current = self.connection.write().expect("connection lock");
        if current
            .as_ref()
            .is_some_and(|existing| Arc::ptr_eq(existing, connection))
        {
            *current = None;
            *self.job.write().expect("job lock") = None;
            *self
                .confirmed_coinbase
                .write()
                .expect("confirmed coinbase lock") = None;
            self.generation.fetch_add(1, Ordering::Release);
        }
    }

    fn clear_job_if_current(&self, completed: &Arc<Job>) -> bool {
        let mut current = self.job.write().expect("job lock");
        if current
            .as_ref()
            .is_some_and(|job| Arc::ptr_eq(job, completed))
        {
            *current = None;
            self.generation.fetch_add(1, Ordering::Release);
            self.emit(json!({
                "event": "job_cleared",
                "job_id": completed.id,
                "height": completed.height,
                "reason": "accepted_block",
            }));
            true
        } else {
            false
        }
    }
}

struct Connection {
    writer: Mutex<TcpStream>,
    pending: AtomicU64,
    max_pending: u64,
}

impl Connection {
    fn reserve_share(&self) -> bool {
        self.pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                (pending < self.max_pending).then_some(pending + 1)
            })
            .is_ok()
    }

    fn finish_share(&self) {
        let _ = self
            .pending
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |pending| {
                Some(pending.saturating_sub(1))
            });
    }
}

fn write_json(connection: &Connection, value: &Value) -> io::Result<()> {
    let mut payload = value.to_string();
    payload.push('\n');
    if payload.len() > MAX_LINE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "outbound JSON exceeds line limit",
        ));
    }
    let mut stream = connection.writer.lock().expect("socket writer lock");
    stream.write_all(payload.as_bytes())?;
    stream.flush()
}

fn read_json_line(reader: &mut BufReader<TcpStream>) -> io::Result<Option<Value>> {
    let mut bytes = Vec::new();
    let count = reader
        .by_ref()
        .take((MAX_LINE_BYTES + 1) as u64)
        .read_until(b'\n', &mut bytes)?;
    if count == 0 {
        return Ok(None);
    }
    if bytes.len() > MAX_LINE_BYTES || !bytes.ends_with(b"\n") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Stratum line exceeds 64 KiB or is unterminated",
        ));
    }
    let value = serde_json::from_slice::<Value>(&bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("invalid JSON: {e}")))?;
    Ok(Some(value))
}

fn install_job_value(
    state: &State,
    value: &Value,
    memory_gate: &mut RandomxMemoryGate,
) -> io::Result<()> {
    let job =
        Job::from_stratum(value).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    memory_gate
        .ensure_safe(job.algo)
        .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
    state.install_job(job);
    Ok(())
}

fn handle_message(
    state: &State,
    connection: &Connection,
    message: &Value,
    memory_gate: &mut RandomxMemoryGate,
) -> io::Result<()> {
    if message.get("method").and_then(Value::as_str) == Some("job") {
        let params = message
            .get("params")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "job push has no params"))?;
        return install_job_value(state, params, memory_gate);
    }

    let id = message.get("id").and_then(Value::as_u64);
    if id == Some(LOGIN_ID) {
        if !message.get("error").is_none_or(Value::is_null) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("pool rejected login: {}", message["error"]),
            ));
        }
        let job = message
            .pointer("/result/job")
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "login reply has no job"))?;
        return install_job_value(state, job, memory_gate);
    }

    if id.is_some_and(|id| id >= FIRST_SUBMIT_ID) {
        connection.finish_share();
        if message.get("error").is_none_or(Value::is_null) {
            state.accepted.fetch_add(1, Ordering::Relaxed);
        } else {
            let rejected = state.rejected.fetch_add(1, Ordering::Relaxed) + 1;
            if rejected <= 5 || rejected.is_multiple_of(100) {
                eprintln!("share rejected #{rejected}: {}", message["error"]);
            }
        }
        state.emit(json!({
            "event": "share",
            "submitted": state.submitted.load(Ordering::Relaxed),
            "accepted": state.accepted.load(Ordering::Relaxed),
            "rejected": state.rejected.load(Ordering::Relaxed),
        }));
    }
    Ok(())
}

fn pool_address(raw: &str) -> &str {
    raw.strip_prefix("stratum+tcp://")
        .or_else(|| raw.strip_prefix("tcp://"))
        .unwrap_or(raw)
}

fn endpoint_address(raw: &str) -> &str {
    raw.strip_prefix("rpc://")
        .or_else(|| raw.strip_prefix("http://"))
        .unwrap_or_else(|| pool_address(raw))
        .trim_end_matches('/')
}

fn is_direct_rpc(raw: &str) -> bool {
    raw.starts_with("rpc://")
        || raw.starts_with("http://")
        || endpoint_address(raw)
            .rsplit_once(':')
            .is_some_and(|(_, port)| port == "8645")
}

fn rpc_call(endpoint: &str, method: &str, params: Value) -> io::Result<Value> {
    rpc_call_with_timeout(endpoint, method, params, DIRECT_RPC_TIMEOUT)
}

fn rpc_call_with_timeout(
    endpoint: &str,
    method: &str,
    params: Value,
    read_timeout: Duration,
) -> io::Result<Value> {
    let address = endpoint_address(endpoint);
    let connect_timeout = read_timeout.min(Duration::from_secs(10));
    let mut stream = connect_with_timeout(address, connect_timeout).map_err(|e| {
        if e.kind() == io::ErrorKind::ConnectionRefused {
            io::Error::new(e.kind(), format!(
                "SOV node refused {address}. On SOV Station enable ‘Expose node RPC on LAN’, then restart its node ({e})"
            ))
        } else { e }
    })?;
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(read_timeout.min(DIRECT_RPC_TIMEOUT)))?;
    let request = json!({"jsonrpc":"2.0", "id":1, "method":method, "params":params});
    let body = request.to_string();
    let host = address.split(':').next().unwrap_or("sov-node");
    write!(stream,
        "POST / HTTP/1.1\r\nHost: {host}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(), body)?;
    stream.flush()?;
    let response = read_bounded_rpc_response(&mut stream)?;
    let split = response
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "node returned malformed HTTP")
        })?;
    if split > MAX_RPC_HEADER_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("node RPC HTTP headers exceed the {MAX_RPC_HEADER_BYTES}-byte safety limit"),
        ));
    }
    let body = &response[split + 4..];
    if body.len() > MAX_RPC_BODY_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("node RPC body exceeds the {MAX_RPC_BODY_BYTES}-byte safety limit"),
        ));
    }
    let value: Value = serde_json::from_slice(body).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("node returned invalid JSON: {e}"),
        )
    })?;
    if !value.get("error").is_none_or(Value::is_null) {
        return Err(io::Error::other(format!(
            "node RPC {method} failed: {}",
            value["error"]
        )));
    }
    value
        .get("result")
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "node RPC reply has no result"))
}

fn read_bounded_rpc_response<R: Read>(reader: &mut R) -> io::Result<Vec<u8>> {
    let mut response = Vec::new();
    reader
        .take((MAX_RPC_RESPONSE_BYTES + 1) as u64)
        .read_to_end(&mut response)?;
    if response.len() > MAX_RPC_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("node RPC response exceeds the {MAX_RPC_RESPONSE_BYTES}-byte safety limit"),
        ));
    }
    Ok(response)
}

fn requested_coinbase(user: &str) -> Option<&str> {
    let user = user.trim();
    (!user.is_empty() && user != "xus-miner").then_some(user)
}

fn validate_rpc_coinbase(requested: Option<&str>, job: &Job) -> Result<(), String> {
    let actual = job
        .coinbase
        .as_deref()
        .ok_or_else(|| "node template did not disclose a coinbase/proposer".to_string())?;
    if let Some(expected) = requested {
        if actual != expected {
            return Err(format!(
                "SECURITY: node template coinbase mismatch: requested `{expected}`, but the block header pays `{actual}`; refusing to hash"
            ));
        }
    }
    Ok(())
}

fn peer_count_from_info(info: &Value) -> Option<u64> {
    info.get("peers").and_then(Value::as_u64)
}

fn validate_rpc_submission(result: &Value, job: &Job, sealed_blob: &[u8]) -> Result<(), String> {
    match result.get("accepted").and_then(Value::as_bool) {
        Some(true) => {}
        Some(false) => {
            return Err(format!(
                "node returned accepted=false{}",
                result
                    .get("error")
                    .and_then(Value::as_str)
                    .map_or_else(String::new, |error| format!(": {error}"))
            ));
        }
        None => return Err("node submission reply has no boolean `accepted`".into()),
    }
    let height = result
        .get("height")
        .and_then(Value::as_u64)
        .ok_or_else(|| "node submission reply has no integer `height`".to_string())?;
    if height != job.height {
        return Err(format!(
            "node submission height {height} does not match mined height {}",
            job.height
        ));
    }
    let hash = result
        .get("hash")
        .and_then(Value::as_str)
        .ok_or_else(|| "node submission reply has no string `hash`".to_string())?;
    let expected_hash = template_id_for_blob(sealed_blob);
    if hash != expected_hash {
        return Err(format!(
            "node submission hash `{hash}` does not match locally sealed block `{expected_hash}`"
        ));
    }
    Ok(())
}

fn run_rpc_session(
    cfg: &Config,
    state: &Arc<State>,
    memory_gate: &mut RandomxMemoryGate,
) -> io::Result<()> {
    *state.rpc_endpoint.write().expect("rpc endpoint lock") = Some(cfg.pool.clone());
    let result = (|| {
        let mut previous_id = String::new();
        let mut chain_height = None;
        let mut announced = false;
        let mut refreshed = Instant::now() - Duration::from_secs(31);
        // Block-flow strip state: a bounded cache of recently confirmed blocks
        // and the last emitted snapshot (so an unchanged strip is not re-sent).
        let mut recent_blocks = blockflow::RecentBlocks::default();
        let mut last_block_flow: Option<Value> = None;
        loop {
            // One lightweight health call supplies the tip height and the
            // node's real mempool count every cycle.
            let health = rpc_call(&cfg.pool, "sov_health", json!({}))?;
            let current_height = health
                .get("height")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sov_health result has no integer height",
                    )
                })?;
            let mempool_size = health
                .get("mempool")
                .and_then(Value::as_u64)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "sov_health result has no integer mempool size",
                    )
                })?;
            state.mempool_size.store(mempool_size, Ordering::Relaxed);
            state.mempool_known.store(true, Ordering::Release);
            if !announced {
                state.emit(json!({"event":"node_connected"}));
                announced = true;
            }
            if previous_id.is_empty()
                || chain_height != Some(current_height)
                || refreshed.elapsed() >= Duration::from_secs(30)
            {
                let requested = requested_coinbase(&cfg.user);
                let params = requested
                    .map_or_else(|| json!({}), |account| json!({"coinbaseAccount": account}));
                let value = rpc_call(&cfg.pool, "sov_getBlockTemplate", params)?;
                let job = Job::from_rpc(&value)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                validate_rpc_coinbase(requested, &job)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
                memory_gate
                    .ensure_safe(job.algo)
                    .map_err(|e| io::Error::new(io::ErrorKind::OutOfMemory, e))?;
                previous_id = job.id.clone();
                let template_height = job.height;
                // Real transaction count of the block being formed, straight
                // from the template the node just served (its txIds/txCount).
                let template_tx_count = blockflow::tx_count(&value);
                state.install_job(job);
                chain_height = Some(current_height);
                refreshed = Instant::now();
                // Optional dashboard enrichment comes after work installation so
                // an older/slow node can never delay hashing a valid template.
                // Run the bounded reads together so one slow optional method
                // cannot add its timeout to the others and hold up tip polling.
                let (network, peer_info, fee_reply, mempool_reply) = thread::scope(|scope| {
                    let network = thread::Builder::new()
                        .name("xus-rpc-difficulty".into())
                        .spawn_scoped(scope, || {
                            rpc_call_with_timeout(
                                &cfg.pool,
                                "sov_getDifficulty",
                                json!({}),
                                Duration::from_secs(1),
                            )
                        });
                    let peers = thread::Builder::new()
                        .name("xus-rpc-peers".into())
                        .spawn_scoped(scope, || {
                            rpc_call_with_timeout(
                                &cfg.pool,
                                "sov_getPeerInfo",
                                json!({}),
                                Duration::from_secs(1),
                            )
                        });
                    // Optional block-flow enrichment: the real fee floor to
                    // make the next block and the node's own mempool counter.
                    // Both fail soft (placeholder), never blocking work.
                    let fee = thread::Builder::new()
                        .name("xus-rpc-fee".into())
                        .spawn_scoped(scope, || {
                            rpc_call_with_timeout(
                                &cfg.pool,
                                "sov_estimateFee",
                                json!({}),
                                Duration::from_secs(1),
                            )
                        });
                    let mempool = thread::Builder::new()
                        .name("xus-rpc-mempool".into())
                        .spawn_scoped(scope, || {
                            rpc_call_with_timeout(
                                &cfg.pool,
                                "sov_getMempoolSize",
                                json!({}),
                                Duration::from_secs(1),
                            )
                        });
                    let network = match network {
                        Ok(handle) => handle.join().ok().and_then(|result| result.ok()),
                        Err(error) => {
                            eprintln!("optional difficulty RPC worker unavailable: {error}");
                            None
                        }
                    };
                    let peers = match peers {
                        Ok(handle) => handle.join().ok().and_then(|result| result.ok()),
                        Err(error) => {
                            eprintln!("optional peer RPC worker unavailable: {error}");
                            None
                        }
                    };
                    let fee = fee
                        .ok()
                        .and_then(|handle| handle.join().ok())
                        .and_then(|result| result.ok());
                    let mempool = mempool
                        .ok()
                        .and_then(|handle| handle.join().ok())
                        .and_then(|result| result.ok());
                    (network, peers, fee, mempool)
                });
                if let Some(network) = network {
                    state.emit(json!({
                        "event": "network",
                        "hashrate": network.get("hashrate").cloned().unwrap_or(Value::Null),
                        "target_block_ms": network.get("targetBlockMs").cloned().unwrap_or(Value::Null),
                        "difficulty": network.get("sha256d").cloned().unwrap_or(Value::Null),
                    }));
                }
                let peer_count = peer_info.and_then(|info| peer_count_from_info(&info));
                match peer_count {
                    Some(peer_count) => {
                        state.peer_count.store(peer_count, Ordering::Relaxed);
                        state.peers_known.store(true, Ordering::Release);
                    }
                    None => state.peers_known.store(false, Ordering::Release),
                }
                state.emit(json!({
                    "event": "peers",
                    "count": peer_count,
                }));
                // Dedicated mempool RPC (when the node has it) refreshes the
                // same counter the required health poll keeps live; both are
                // real node responses, never a synthesized value.
                if let Some(size) = mempool_reply.as_ref().and_then(blockflow::mempool_size) {
                    state.mempool_size.store(size, Ordering::Relaxed);
                    state.mempool_known.store(true, Ordering::Release);
                }
                // Backfill the confirmed strip from real blocks. Bounded per
                // refresh (call cap + one wall-clock budget), fail-soft, and
                // strictly read-only; a missing RPC leaves placeholder tiles.
                let backfill_deadline = Instant::now() + Duration::from_secs(2);
                for height in recent_blocks.refresh_targets(current_height) {
                    if Instant::now() >= backfill_deadline {
                        break;
                    }
                    match rpc_call_with_timeout(
                        &cfg.pool,
                        "sov_getBlockByHeight",
                        json!({ "height": height }),
                        Duration::from_secs(1),
                    ) {
                        Ok(block) => recent_blocks.insert(
                            current_height,
                            height,
                            blockflow::BlockInfo {
                                hash: blockflow::block_hash(&block),
                                tx_count: blockflow::tx_count(&block),
                            },
                        ),
                        // An older node without this RPC (or a slow reply)
                        // ends this cycle's backfill; tiles stay placeholders.
                        Err(_) => break,
                    }
                }
                let block_flow = {
                    let sealed = state.sealed_blocks.lock().expect("sealed blocks lock");
                    let tiles = recent_blocks.tiles(current_height, &sealed);
                    json!({
                        "event": "block_flow",
                        "tip_height": current_height,
                        "template": {
                            "height": template_height,
                            "tx_count": template_tx_count,
                        },
                        "fee_estimate": fee_reply.as_ref().and_then(blockflow::fee_estimate),
                        "recent": tiles
                            .iter()
                            .map(|tile| json!({
                                "height": tile.height,
                                "tx_count": tile.tx_count,
                                "mine": tile.mine,
                            }))
                            .collect::<Vec<_>>(),
                    })
                };
                if last_block_flow.as_ref() != Some(&block_flow) {
                    state.emit(block_flow.clone());
                    last_block_flow = Some(block_flow);
                }
            }
            thread::sleep(Duration::from_secs(2));
        }
    })();
    *state.rpc_endpoint.write().expect("rpc endpoint lock") = None;
    *state
        .confirmed_coinbase
        .write()
        .expect("confirmed coinbase lock") = None;
    state.mempool_known.store(false, Ordering::Release);
    state.peers_known.store(false, Ordering::Release);
    *state.job.write().expect("job lock") = None;
    state.generation.fetch_add(1, Ordering::Release);
    result
}

fn connect(pool: &str) -> io::Result<TcpStream> {
    connect_with_timeout(pool, Duration::from_secs(10))
}

fn connect_with_timeout(pool: &str, timeout: Duration) -> io::Result<TcpStream> {
    let address = pool_address(pool);
    let mut last = None;
    for socket in address.to_socket_addrs()? {
        match TcpStream::connect_timeout(&socket, timeout) {
            Ok(stream) => return Ok(stream),
            Err(error) => last = Some(error),
        }
    }
    Err(last.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::AddrNotAvailable,
            "pool resolved to no addresses",
        )
    }))
}

fn run_session(
    cfg: &Config,
    state: &Arc<State>,
    memory_gate: &mut RandomxMemoryGate,
) -> io::Result<()> {
    let stream = connect(&cfg.pool)?;
    state.emit(json!({"event": "pool_connected"}));
    stream.set_nodelay(true)?;
    stream.set_read_timeout(Some(Duration::from_secs(30)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let connection = Arc::new(Connection {
        writer: Mutex::new(stream.try_clone()?),
        pending: AtomicU64::new(0),
        // Bound memory/socket growth and stale work: each worker may have at
        // most one share awaiting a pool response on this connection.
        max_pending: cfg.workers,
    });
    *state.connection.write().expect("connection lock") = Some(Arc::clone(&connection));

    write_json(
        &connection,
        &json!({
            "id": LOGIN_ID,
            "jsonrpc": "2.0",
            "method": "login",
            "params": {
                "login": cfg.user,
                "pass": cfg.password,
                "agent": format!("xus-miner/{VERSION}"),
            }
        }),
    )?;

    let result = (|| {
        let mut reader = BufReader::new(stream);
        loop {
            match read_json_line(&mut reader) {
                Ok(Some(message)) => handle_message(state, &connection, &message, memory_gate)?,
                Ok(None) => {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "pool closed connection",
                    ))
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    write_json(
                        &connection,
                        &json!({
                            "id": KEEPALIVE_ID,
                            "jsonrpc": "2.0",
                            "method": "keepalived",
                            "params": {}
                        }),
                    )?;
                }
                Err(error) => return Err(error),
            }
        }
    })();
    state.clear_connection(&connection);
    result
}

fn nonce_wire(nonce: u64) -> String {
    hex::encode(nonce.to_le_bytes())
}

fn worker_loop(state: Arc<State>, worker: u64) {
    let mut generation = u64::MAX;
    let mut job: Option<Arc<Job>> = None;
    let mut blob = Vec::new();
    let mut nonce = worker << 56;
    let mut ready_state: Option<(PowAlgo, Vec<u8>, MiningMode)> = None;
    let mut preparation_announced = false;
    let mut last_pow_error: Option<(String, Instant)> = None;

    loop {
        let latest_generation = state.generation.load(Ordering::Acquire);
        if latest_generation != generation {
            generation = latest_generation;
            preparation_announced = false;
            job = state.job.read().expect("job lock").clone();
            if let Some(current) = &job {
                blob.clone_from(&current.blob);
                nonce = worker << 56;
            }
        }

        let Some(current) = &job else {
            thread::sleep(Duration::from_millis(200));
            continue;
        };

        let mut completed = 0u64;
        for _ in 0..WORK_BATCH {
            if state.generation.load(Ordering::Acquire) != generation {
                break;
            }
            blob[current.nonce_offset..current.nonce_offset + 8]
                .copy_from_slice(&nonce.to_le_bytes());
            let seal = match current.seal_blob(&blob, || {
                if !preparation_announced {
                    ready_state = None;
                    preparation_announced = true;
                    eprintln!(
                        "worker {worker}: initializing RandomX work memory and VM; hashrate appears when ready"
                    );
                    state.emit(json!({"event":"worker_initializing", "worker":worker}));
                }
            }) {
                Ok(seal) => seal,
                Err(error) => {
                    let should_report = last_pow_error.as_ref().is_none_or(|(previous, at)| {
                        previous != &error || at.elapsed() >= Duration::from_secs(30)
                    });
                    if should_report {
                        eprintln!("worker {worker}: proof-of-work engine unavailable: {error}");
                        diag::warn(&format!(
                            "worker {worker}: proof-of-work engine unavailable: {error}"
                        ));
                        state.emit(json!({
                            "event": "worker_error",
                            "worker": worker,
                            "message": error,
                        }));
                        last_pow_error = Some((error, Instant::now()));
                    }
                    ready_state = None;
                    preparation_announced = true;
                    nonce = nonce.wrapping_add(1);
                    thread::sleep(Duration::from_secs(1));
                    break;
                }
            };
            preparation_announced = false;
            if last_pow_error.take().is_some() {
                eprintln!("worker {worker}: proof-of-work engine recovered; hashing");
                state.emit(json!({"event":"worker_recovered", "worker":worker}));
            }
            if ready_state.as_ref().is_none_or(|(algo, key, mode)| {
                *algo != current.algo
                    || key.as_slice() != current.pow_key.as_slice()
                    || *mode != seal.mode
            }) {
                ready_state = Some((current.algo, current.pow_key.clone(), seal.mode));
                diag::info(&format!(
                    "worker {worker}: sealing engine ready: algo={:?} mode={}",
                    current.algo,
                    seal.mode.telemetry_name(),
                ));
                match seal.mode {
                    MiningMode::RandomXFastShared => eprintln!(
                        "worker {worker}: RandomX VM ready on the shared full dataset; hashing"
                    ),
                    MiningMode::RandomXLightFallback => eprintln!(
                        "worker {worker}: RandomX VM ready in light-memory fallback; hashing while fast mode retries"
                    ),
                    MiningMode::RandomXLightRecovery => eprintln!(
                        "worker {worker}: RandomX VM ready in locked portable light-memory recovery; hashing without a full-dataset retry"
                    ),
                    MiningMode::Sha256d => {
                        eprintln!("worker {worker}: SHA-256d engine ready; hashing")
                    }
                }
                state.emit(json!({
                    "event":"worker_ready",
                    "worker":worker,
                    "mode": seal.mode.telemetry_name(),
                }));
            }
            completed += 1;
            if seal.digest <= current.target
                && state.generation.load(Ordering::Acquire) == generation
            {
                if let Some(endpoint) = state
                    .rpc_endpoint
                    .read()
                    .expect("rpc endpoint lock")
                    .clone()
                {
                    state.submitted.fetch_add(1, Ordering::Relaxed);
                    let params = json!({
                        "templateId": current.id,
                        "nonce": nonce,
                        "timestampMs": current.timestamp_ms,
                    });
                    let submission = rpc_call(&endpoint, "sov_submitBlock", params)
                        .map_err(|error| error.to_string())
                        .and_then(|result| validate_rpc_submission(&result, current, &blob));
                    match submission {
                        Ok(()) => {
                            state.accepted.fetch_add(1, Ordering::Relaxed);
                            // The node's reply was verified to echo this exact
                            // locally sealed header hash; remember it so the
                            // block-flow strip can highlight our own block.
                            state.record_sealed_block(current.height, template_id_for_blob(&blob));
                            state.emit(json!({"event":"share", "submitted":state.submitted.load(Ordering::Relaxed), "accepted":state.accepted.load(Ordering::Relaxed), "rejected":state.rejected.load(Ordering::Relaxed)}));
                            state.clear_job_if_current(current);
                        }
                        Err(error) => {
                            state.rejected.fetch_add(1, Ordering::Relaxed);
                            eprintln!("block submission rejected: {error}");
                            diag::warn(&format!(
                                "worker {worker}: block submission rejected: {error}"
                            ));
                        }
                    }
                    nonce = nonce.wrapping_add(1);
                    continue;
                }
                let request_id = state.next_request.fetch_add(1, Ordering::Relaxed);
                let message = json!({
                    "id": request_id,
                    "jsonrpc": "2.0",
                    "method": "submit",
                    "params": {
                        "job_id": current.id,
                        "nonce": nonce_wire(nonce),
                        "result": hex::encode(seal.digest),
                    }
                });
                if let Some(connection) = state
                    .connection
                    .read()
                    .expect("connection lock")
                    .clone()
                    .filter(|connection| connection.reserve_share())
                {
                    match write_json(&connection, &message) {
                        Ok(()) => {
                            state.submitted.fetch_add(1, Ordering::Relaxed);
                        }
                        Err(error) => {
                            connection.finish_share();
                            eprintln!("submit failed: {error}");
                        }
                    }
                }
            }
            nonce = nonce.wrapping_add(1);
        }
        state
            .interval_hashes
            .fetch_add(completed, Ordering::Relaxed);
        state.total_hashes.fetch_add(completed, Ordering::Relaxed);
        state.record_round_hashes(current, completed);
    }
}

fn panic_payload_message(payload: &(dyn Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|message| (*message).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic payload".into())
}

fn worker_supervisor(state: Arc<State>, worker: u64) {
    let mut restarts = 0_u64;
    loop {
        let result = catch_unwind(AssertUnwindSafe(|| {
            worker_loop(Arc::clone(&state), worker);
        }));
        let Err(payload) = result else {
            return;
        };
        restarts = restarts.saturating_add(1);
        let message = panic_payload_message(&*payload);
        let retry_secs = restarts.min(30);
        eprintln!(
            "worker {worker}: recovered from an internal panic ({message}); restarting in {retry_secs} seconds"
        );
        diag::error(&format!(
            "worker {worker}: recovered from an internal panic ({message}); supervised restart {restarts} in {retry_secs} seconds"
        ));
        state.emit(json!({
            "event": "worker_error",
            "worker": worker,
            "message": format!("internal worker panic: {message}"),
            "retry_in_secs": retry_secs,
        }));
        thread::sleep(Duration::from_secs(retry_secs));
    }
}

fn reporter_loop(state: Arc<State>, every: Duration) {
    let mut previous = Instant::now();
    loop {
        thread::sleep(every);
        let elapsed = previous.elapsed().as_secs_f64();
        previous = Instant::now();
        let hashes = state.interval_hashes.swap(0, Ordering::Relaxed);
        let rate = hashes as f64 / elapsed.max(f64::EPSILON);
        let (round_height, round_hashes, round_probability) = state.round_snapshot();
        let height = state
            .job
            .read()
            .expect("job lock")
            .as_ref()
            .map_or_else(|| "-".into(), |job| job.height.to_string());
        state.emit(json!({
            "event": "metrics",
            "hashrate": rate,
            "height": state
                .job
                .read()
                .expect("job lock")
                .as_ref()
                .map(|job| job.height),
            "total_hashes": state.total_hashes.load(Ordering::Relaxed),
            "round_height": round_height,
            "round_hashes": round_hashes,
            "round_probability": round_probability,
            "submitted": state.submitted.load(Ordering::Relaxed),
            "accepted": state.accepted.load(Ordering::Relaxed),
            "rejected": state.rejected.load(Ordering::Relaxed),
            "mempool_size": state.mempool_known.load(Ordering::Acquire)
                .then(|| state.mempool_size.load(Ordering::Relaxed)),
            "peer_count": state.peers_known.load(Ordering::Acquire)
                .then(|| state.peer_count.load(Ordering::Relaxed)),
        }));
        eprintln!(
            "height {height} | {rate:.2} H/s | total {} | shares {}/{}/{} submitted/accepted/rejected",
            state.total_hashes.load(Ordering::Relaxed),
            state.submitted.load(Ordering::Relaxed),
            state.accepted.load(Ordering::Relaxed),
            state.rejected.load(Ordering::Relaxed),
        );
    }
}

fn gib_to_bytes(gib: f64) -> u64 {
    (gib * BYTES_PER_GIB).ceil() as u64
}

fn mib_to_bytes(mib: f64) -> u64 {
    (mib * BYTES_PER_MIB).ceil() as u64
}

pub(crate) fn randomx_memory_bytes(workers: u64) -> u64 {
    randomx_memory_bytes_for(workers, false)
}

pub(crate) fn randomx_memory_bytes_for(workers: u64, force_light: bool) -> u64 {
    let shared = if force_light {
        mib_to_bytes(RANDOMX_LIGHT_CACHE_MIB)
    } else {
        gib_to_bytes(RANDOMX_DATASET_GIB)
    };
    shared.saturating_add(mib_to_bytes(RANDOMX_WORKER_MIB).saturating_mul(workers))
}

fn memory_headroom_bytes(total: u64) -> u64 {
    (total / 10).clamp(
        gib_to_bytes(MIN_MEMORY_HEADROOM_GIB),
        gib_to_bytes(MAX_MEMORY_HEADROOM_GIB),
    )
}

fn validate_headless_memory(
    workers: u64,
    force_light: bool,
    available: u64,
    total: u64,
) -> Result<HeadlessMemoryPreflight, String> {
    if total == 0 || available > total {
        return Err(
            "operating-system RAM scan returned unavailable or inconsistent readings; refusing to start headless RandomX workers"
                .into(),
        );
    }
    let engine = randomx_memory_bytes_for(workers, force_light);
    let reserve = memory_headroom_bytes(total);
    let required = engine.saturating_add(reserve);
    if available < required {
        let profile = if force_light {
            "portable light-memory RandomX"
        } else {
            "shared full-dataset RandomX"
        };
        return Err(format!(
            "the {profile} engine for {workers} worker{} may need ≈{:.1} GiB plus {:.1} GiB of system headroom, but only {:.1} GiB is available; refusing to start",
            if workers == 1 { "" } else { "s" },
            engine as f64 / BYTES_PER_GIB,
            reserve as f64 / BYTES_PER_GIB,
            available as f64 / BYTES_PER_GIB,
        ));
    }
    Ok(HeadlessMemoryPreflight {
        available: Some(available),
        total: Some(total),
        engine,
        reserve,
    })
}

fn resolve_headless_memory(
    workers: u64,
    force_light: bool,
    available: u64,
    total: u64,
    unavailable_scan_confirmed: bool,
) -> Result<HeadlessMemoryPreflight, String> {
    if total > 0 && available <= total {
        return validate_headless_memory(workers, force_light, available, total);
    }
    if !unavailable_scan_confirmed {
        return Err(
            "operating-system RAM scan is unavailable; refusing to initialize RandomX without explicit memory confirmation"
                .into(),
        );
    }
    Ok(HeadlessMemoryPreflight {
        available: None,
        total: None,
        engine: randomx_memory_bytes_for(workers, force_light),
        reserve: gib_to_bytes(MIN_MEMORY_HEADROOM_GIB),
    })
}

fn run_headless(
    cfg: Config,
    json_events: bool,
    parent_pipe_watchdog: bool,
    unavailable_scan_confirmed: bool,
) -> Result<(), String> {
    if parent_pipe_watchdog && !json_events {
        return Err("parent-pipe watchdog requires JSON event output".into());
    }
    randomx_native::configure_runtime_mode(cfg.randomx_light)?;
    let randomx_backend = if cfg.randomx_light {
        "light-recovery"
    } else {
        "optimized"
    };
    eprintln!(
        "xus-miner {VERSION} | pool {} | user {} | workers {} | RandomX {randomx_backend}",
        cfg.pool, cfg.user, cfg.workers,
    );
    diag::info(&format!(
        "engine starting: workers={} backend={randomx_backend} report-secs={} reconnect-secs={} parent-pipe-watchdog={parent_pipe_watchdog}",
        cfg.workers, cfg.report_secs, cfg.reconnect_secs,
    ));

    let state = Arc::new(State::new(json_events));
    let mut memory_gate =
        RandomxMemoryGate::new(cfg.workers, cfg.randomx_light, unavailable_scan_confirmed);
    state.emit(json!({
        "event": "startup",
        "version": VERSION,
        "pool": cfg.pool,
        "user": cfg.user,
        "workers": cfg.workers,
        "randomx_backend": randomx_backend,
    }));
    if parent_pipe_watchdog {
        let watchdog_state = Arc::clone(&state);
        thread::Builder::new()
            .name("xus-parent-watchdog".into())
            .spawn(move || loop {
                thread::sleep(Duration::from_secs(2));
                watchdog_state.emit(json!({"event": "parent_watchdog"}));
            })
            .map_err(|error| format!("cannot start parent-pipe watchdog: {error}"))?;
    }
    for worker in 0..cfg.workers {
        let worker_state = Arc::clone(&state);
        if let Err(error) = thread::Builder::new()
            .name(format!("xus-worker-{worker}"))
            .spawn(move || worker_supervisor(worker_state, worker))
        {
            let message = format!("cannot start mining worker {worker}: {error}");
            eprintln!("{message}");
            diag::error(&message);
            state.emit(json!({"event":"engine_fatal", "message":message}));
            return Err(message);
        }
    }
    {
        let report_state = Arc::clone(&state);
        let interval = Duration::from_secs(cfg.report_secs);
        if let Err(error) = thread::Builder::new()
            .name("xus-reporter".into())
            .spawn(move || reporter_loop(report_state, interval))
        {
            let message = format!("cannot start mining reporter: {error}");
            eprintln!("{message}");
            state.emit(json!({"event":"engine_fatal", "message":message}));
            return Err(message);
        }
    }

    loop {
        eprintln!("connecting to {}", cfg.pool);
        state.emit(json!({"event": "connecting", "pool": cfg.pool}));
        let direct = is_direct_rpc(&cfg.pool);
        if direct {
            eprintln!("direct SOV node RPC mode");
        }
        let session = if direct {
            run_rpc_session(&cfg, &state, &mut memory_gate)
        } else {
            run_session(&cfg, &state, &mut memory_gate)
        };
        if let Err(error) = session {
            eprintln!(
                "{} session ended: {error}",
                if direct { "node RPC" } else { "pool" }
            );
            state.emit(json!({
                "event": "session_error",
                "message": error.to_string(),
                "retry_in_secs": cfg.reconnect_secs,
            }));
        }
        thread::sleep(Duration::from_secs(cfg.reconnect_secs));
    }
}

fn main() {
    let mut raw: Vec<String> = env::args().skip(1).collect();
    if raw.is_empty() || raw.iter().any(|arg| arg == "--gui") {
        diag::init("gui");
        if let Err(error) = gui::run() {
            eprintln!("{error}");
            diag::error(&format!("GUI terminated with an error: {error}"));
            process::exit(1);
        }
        diag::info("GUI closed normally");
        return;
    }
    diag::init("engine");

    let json_events = raw.iter().any(|arg| arg == "--json-events");
    let password_stdin = raw.iter().any(|arg| arg == "--password-stdin");
    let parent_pipe_watchdog = raw.iter().any(|arg| arg == "--parent-pipe-watchdog");
    let unavailable_scan_confirmed = raw.iter().any(|arg| arg == "--confirm-randomx-memory");
    raw.retain(|arg| {
        !matches!(
            arg.as_str(),
            "--headless"
                | "--json-events"
                | "--password-stdin"
                | "--parent-pipe-watchdog"
                | "--confirm-randomx-memory"
        )
    });
    let mut cfg = parse_args_from(raw);
    if password_stdin {
        let mut password = String::new();
        io::stdin()
            .take(4_097)
            .read_to_string(&mut password)
            .unwrap_or_else(|error| {
                eprintln!("error: cannot read password from stdin: {error}");
                process::exit(2);
            });
        if password.len() > 4_096 {
            eprintln!("error: password from stdin exceeds 4096 bytes");
            process::exit(2);
        }
        cfg.password = password;
    }
    if let Err(error) = run_headless(
        cfg,
        json_events,
        parent_pipe_watchdog,
        unavailable_scan_confirmed,
    ) {
        eprintln!("fatal mining engine error: {error}");
        diag::error(&format!("fatal mining engine error: {error}"));
        process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_randomx_recovery_is_explicit_and_off_by_default() {
        assert!(!Config::default().randomx_light);

        let light = parse_args_from(["--randomx-light".to_owned()]);
        assert!(light.randomx_light);
    }

    fn valid_job() -> Value {
        json!({
            "job_id": "job-1",
            "blob": "00".repeat(64),
            "target_full": "ff".repeat(32),
            "algo": "sha256d",
            "height": 42,
            "seed_hash": "",
            "nonce_offset": 56,
            "nonce_size": 8,
        })
    }

    fn valid_rpc_template() -> Value {
        let header = BlockHeaderWire {
            height: 43,
            _prev_hash: [0; 32],
            _tx_root: [0; 32],
            _receipts_root: [0; 32],
            _state_root: [0; 32],
            timestamp_ms: 1_700_000_000_000_u64,
            proposer: "a35755d38a626de1b25820913aadbe8299b6ff6a8d0338ddef5295c7444c1e24".into(),
            // SOV 0.1.99's mainnet preset signals tx-domain + fee-auction.
            version_bits: 0b11,
            bits: 506_433_601,
            _nonce: 0,
        };
        let blob = borsh::to_vec(&header).unwrap();
        json!({
            "templateId": template_id_for_blob(&blob),
            "blob": hex::encode(&blob),
            "target": "00002f9041000000000000000000000000000000000000000000000000000000",
            "powAlgo": "RandomX",
            "powKey": "cb0272ff88e64c18cde0257f7fae1c8236b02651f10cc7a02456fd682ee2e72d",
            "height": 43,
            "timestampMs": 1_700_000_000_000_u64,
            "minTimestampMs": 1_699_999_999_999_u64,
            "nonceOffset": blob.len() - 8,
            "proposer": "a35755d38a626de1b25820913aadbe8299b6ff6a8d0338ddef5295c7444c1e24",
            "prevHash": "00".repeat(32),
            "txRoot": "00".repeat(32),
            "receiptsRoot": "00".repeat(32),
            "stateRoot": "00".repeat(32),
            "versionBits": 3,
            "bits": 506_433_601,
        })
    }

    #[test]
    fn direct_rpc_response_reader_is_strictly_bounded() {
        let exact = vec![b'x'; MAX_RPC_RESPONSE_BYTES];
        assert_eq!(
            read_bounded_rpc_response(&mut io::Cursor::new(exact))
                .unwrap()
                .len(),
            MAX_RPC_RESPONSE_BYTES
        );

        let oversized = vec![b'x'; MAX_RPC_RESPONSE_BYTES + 1];
        let error = read_bounded_rpc_response(&mut io::Cursor::new(oversized)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("safety limit"));
    }

    #[test]
    fn headless_randomx_memory_preflight_matches_gui_safety_policy() {
        let total = gib_to_bytes(16.0);
        let reserve = memory_headroom_bytes(total);
        assert_eq!(reserve, total / 10);
        assert_eq!(memory_headroom_bytes(gib_to_bytes(8.0)), gib_to_bytes(1.5));
        assert_eq!(
            memory_headroom_bytes(gib_to_bytes(128.0)),
            gib_to_bytes(4.0)
        );

        let required = randomx_memory_bytes(3) + reserve;
        assert_eq!(
            randomx_memory_bytes(3) - randomx_memory_bytes(1),
            mib_to_bytes(RANDOMX_WORKER_MIB * 2.0),
            "workers must share one full dataset instead of allocating one each"
        );
        let preflight = resolve_headless_memory(3, false, required, total, false).unwrap();
        assert_eq!(preflight.engine, randomx_memory_bytes(3));
        assert_eq!(preflight.reserve, reserve);
        assert_eq!(preflight.available, Some(required));
        assert_eq!(preflight.total, Some(total));
        assert!(resolve_headless_memory(3, false, required - 1, total, true)
            .unwrap_err()
            .contains("refusing to start"));
        assert!(resolve_headless_memory(1, false, 1, 0, false)
            .unwrap_err()
            .contains("explicit memory confirmation"));
        let confirmed = resolve_headless_memory(1, false, 1, 0, true).unwrap();
        assert_eq!(confirmed.available, None);
        assert_eq!(confirmed.total, None);
        assert_eq!(confirmed.reserve, gib_to_bytes(1.5));
        assert!(resolve_headless_memory(1, false, total + 1, total, false).is_err());

        let light_required = randomx_memory_bytes_for(1, true) + reserve;
        assert!(
            resolve_headless_memory(1, false, light_required, total, false).is_err(),
            "full-dataset mode must remain blocked at light-only capacity"
        );
        let light = resolve_headless_memory(1, true, light_required, total, false).unwrap();
        assert_eq!(light.engine, randomx_memory_bytes_for(1, true));
        assert!(light.engine < randomx_memory_bytes(1));
    }

    #[test]
    fn macos_memory_pressure_parser_is_bounded_and_strict() {
        let output = "\
The system has 25769803776 (1572864 pages with a page size of 16384).
System-wide memory free percentage: 62%
";
        assert_eq!(
            parse_macos_memory_pressure(output).unwrap(),
            (15_977_278_341, 25_769_803_776)
        );
        assert!(parse_macos_memory_pressure(
            "The system has 25769803776 pages\nSystem-wide memory free percentage: 101%\n"
        )
        .unwrap_err()
        .contains("free percentage"));
        assert!(parse_macos_memory_pressure(
            "The system has 0 pages\nSystem-wide memory free percentage: 50%\n"
        )
        .unwrap_err()
        .contains("physical-memory total"));
        assert!(parse_macos_memory_pressure("unrecognized output")
            .unwrap_err()
            .contains("physical-memory total"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_memory_probe_repeats_without_entering_application_ffi() {
        // The probe's contract is to repeat safely (no application FFI) and
        // return a self-consistent, STABLE reading — NOT to prove a host RAM
        // size. GitHub's hosted macOS runners have ~7 GiB, legitimately below a
        // developer machine, so a hard ">= 8 GiB" floor is a false failure. A
        // low floor still catches a gross misparse, and requiring the total to
        // be identical across calls catches an unstable/racing one.
        let (_, first_total) = query_memory_counters().unwrap();
        assert!(
            first_total >= gib_to_bytes(2.0),
            "implausible total physical memory from memory_pressure: {first_total} bytes"
        );
        for _ in 0..32 {
            let (available, total) = query_memory_counters().unwrap();
            assert!(
                available <= total,
                "available {available} exceeded total {total}"
            );
            assert_eq!(
                total, first_total,
                "total physical memory drifted between probes"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_memory_probe_reader_never_retains_unbounded_output() {
        let oversized = vec![b'x'; MAX_MACOS_MEMORY_PRESSURE_BYTES + 1];
        let error = read_bounded_command_stream(io::Cursor::new(oversized), "test").unwrap_err();
        assert!(error.contains("exceeded 4096 bytes"));
    }

    #[test]
    fn parses_complete_sov_job() {
        let job = Job::from_stratum(&valid_job()).unwrap();
        assert_eq!(job.id, "job-1");
        assert_eq!(job.height, 42);
        assert_eq!(job.nonce_offset, 56);
        assert_eq!(job.target, [0xff; 32]);
        assert_eq!(job.block_target, None);
        assert_eq!(job.algo, PowAlgo::Sha256d);
    }

    #[test]
    fn sov_0_1_99_rpc_template_contract_is_accepted_and_authenticates_coinbase() {
        let job = Job::from_rpc(&valid_rpc_template()).unwrap();
        let coinbase = "a35755d38a626de1b25820913aadbe8299b6ff6a8d0338ddef5295c7444c1e24";
        assert_eq!(job.coinbase.as_deref(), Some(coinbase));
        assert_eq!(job.algo, PowAlgo::RandomX);
        assert_eq!(job.pow_key.len(), 32);
        assert_eq!(
            BlockHeaderWire::try_from_slice(&job.blob)
                .unwrap()
                .version_bits,
            0b11
        );
        assert!(validate_rpc_coinbase(None, &job).is_ok());
        assert!(validate_rpc_coinbase(Some(coinbase), &job).is_ok());
    }

    #[test]
    fn rpc_template_refuses_missing_or_mismatched_coinbase() {
        let mut missing = valid_rpc_template();
        missing.as_object_mut().unwrap().remove("proposer");
        assert!(Job::from_rpc(&missing).unwrap_err().contains("proposer"));

        let mut dishonest = valid_rpc_template();
        dishonest["proposer"] = json!("different-account");
        assert!(Job::from_rpc(&dishonest)
            .unwrap_err()
            .contains("does not match encoded header coinbase"));

        let job = Job::from_rpc(&valid_rpc_template()).unwrap();
        let error = validate_rpc_coinbase(Some("different-account"), &job).unwrap_err();
        assert!(error.contains("refusing to hash"));
        assert!(error.contains("different-account"));
        assert!(error.contains("a35755d38a626de1b25820913aadbe8299b6ff6a8d0338ddef5295c7444c1e24"));
    }

    #[test]
    fn sov_0_1_99_compact_target_and_rpc_submission_are_verified_end_to_end() {
        let expected_target =
            hex::decode("00002f9041000000000000000000000000000000000000000000000000000000")
                .unwrap();
        assert_eq!(
            target_from_compact(506_433_601).unwrap().as_slice(),
            expected_target.as_slice()
        );
        assert_eq!(target_from_compact(0x1d80_ffff), None);

        let job = Job::from_rpc(&valid_rpc_template()).unwrap();
        let mut sealed_blob = job.blob.clone();
        sealed_blob[job.nonce_offset..].copy_from_slice(&7_u64.to_le_bytes());
        let accepted = json!({
            "accepted": true,
            "height": job.height,
            "hash": template_id_for_blob(&sealed_blob),
        });
        assert_eq!(
            validate_rpc_submission(&accepted, &job, &sealed_blob),
            Ok(())
        );

        let rejected = json!({
            "accepted": false,
            "height": job.height,
            "hash": template_id_for_blob(&sealed_blob),
            "error": "import rejected",
        });
        assert!(validate_rpc_submission(&rejected, &job, &sealed_blob)
            .unwrap_err()
            .contains("import rejected"));
        assert!(validate_rpc_submission(
            &json!({"height": job.height, "hash": template_id_for_blob(&sealed_blob)}),
            &job,
            &sealed_blob,
        )
        .unwrap_err()
        .contains("accepted"));
        assert!(validate_rpc_submission(
            &json!({"accepted": true, "height": job.height + 1, "hash": template_id_for_blob(&sealed_blob)}),
            &job,
            &sealed_blob,
        )
        .unwrap_err()
        .contains("height"));
        assert!(validate_rpc_submission(
            &json!({"accepted": true, "height": job.height, "hash": "00".repeat(32)}),
            &job,
            &sealed_blob,
        )
        .unwrap_err()
        .contains("locally sealed block"));

        let state = State::new(false);
        state.install_job(job.clone());
        let completed = state.job.read().expect("job lock").clone().unwrap();
        assert!(state.clear_job_if_current(&completed));
        assert!(state.job.read().expect("job lock").is_none());

        state.install_job(job);
        let replacement = state.job.read().expect("job lock").clone().unwrap();
        assert!(!state.clear_job_if_current(&completed));
        assert!(state
            .job
            .read()
            .expect("job lock")
            .as_ref()
            .is_some_and(|current| Arc::ptr_eq(current, &replacement)));
    }

    #[test]
    fn target_to_expected_hashes_uses_inclusive_big_endian_probability() {
        assert!((expected_hashes_for_target(&[0xff; 32]) - 1.0).abs() < f64::EPSILON);

        let mut one_in_256 = [0xff; 32];
        one_in_256[0] = 0;
        assert!((expected_hashes_for_target(&one_in_256) - 256.0).abs() < 1e-10);

        let impossible_except_zero = [0; 32];
        assert_eq!(
            expected_hashes_for_target(&impossible_except_zero),
            2.0_f64.powi(256)
        );
    }

    #[test]
    fn round_probability_segments_target_changes_and_rejects_stale_height_work() {
        let state = State::new(false);
        let mut first = Job::from_rpc(&valid_rpc_template()).unwrap();
        first.target = [0xff; 32];
        first.block_target = Some([0xff; 32]);
        state.install_job(first.clone());
        state.record_round_hashes(&first, 1);
        assert_eq!(
            state.round_snapshot(),
            (Some(first.height), 1, 1.0 - (-1.0_f64).exp())
        );

        let mut harder_refresh = first.clone();
        harder_refresh.block_target = Some({
            let mut target = [0xff; 32];
            target[0] = 0;
            target
        });
        state.install_job(harder_refresh.clone());
        state.record_round_hashes(&harder_refresh, 256);
        let (height, hashes, probability) = state.round_snapshot();
        assert_eq!(height, Some(first.height));
        assert_eq!(hashes, 257);
        assert!((probability - (1.0 - (-2.0_f64).exp())).abs() < 1e-12);

        let mut next_height = harder_refresh.clone();
        next_height.height += 1;
        let next_height_value = next_height.height;
        state.install_job(next_height);
        state.record_round_hashes(&harder_refresh, 256);
        assert_eq!(state.round_snapshot(), (Some(next_height_value), 0, 0.0));
    }

    #[test]
    fn refuses_layout_target_algorithm_and_seed_drift() {
        let mut value = valid_job();
        value["nonce_offset"] = json!(55);
        assert!(Job::from_stratum(&value).unwrap_err().contains("trailing"));

        let mut value = valid_job();
        value["nonce_size"] = json!(4);
        assert!(Job::from_stratum(&value)
            .unwrap_err()
            .contains("nonce_size"));

        let mut value = valid_job();
        value["target_full"] = json!("ff".repeat(31));
        assert!(Job::from_stratum(&value).unwrap_err().contains("want 32"));

        let mut value = valid_job();
        value["algo"] = json!("rx/wow");
        assert!(Job::from_stratum(&value)
            .unwrap_err()
            .contains("unsupported"));

        let mut value = valid_job();
        value["algo"] = json!("rx/0");
        assert!(Job::from_stratum(&value)
            .unwrap_err()
            .contains("cannot be empty"));

        let mut value = valid_rpc_template();
        value["powKey"] = json!("00");
        assert!(Job::from_rpc(&value).unwrap_err().contains("requires 32"));

        let mut value = valid_rpc_template();
        value["target"] = json!("ff".repeat(32));
        assert!(Job::from_rpc(&value)
            .unwrap_err()
            .contains("compact header difficulty"));

        let mut value = valid_rpc_template();
        let mut header =
            BlockHeaderWire::try_from_slice(&hex::decode(value["blob"].as_str().unwrap()).unwrap())
                .unwrap();
        header.bits = 0x1d80_ffff;
        let blob = borsh::to_vec(&header).unwrap();
        value["blob"] = json!(hex::encode(&blob));
        value["templateId"] = json!(template_id_for_blob(&blob));
        value["bits"] = json!(header.bits);
        assert!(Job::from_rpc(&value)
            .unwrap_err()
            .contains("invalid compact difficulty"));
    }

    #[test]
    fn nonce_wire_is_full_little_endian_u64() {
        assert_eq!(nonce_wire(1), "0100000000000000");
        assert_eq!(nonce_wire(0x0123_4567_89ab_cdef), "efcdab8967452301");
    }

    #[test]
    fn sha_job_hashes_the_exact_spliced_blob() {
        let job = Job::from_stratum(&valid_job()).unwrap();
        let nonce: u64 = 0x0123_4567_89ab_cdef;
        let mut blob = job.blob.clone();
        blob[job.nonce_offset..].copy_from_slice(&nonce.to_le_bytes());
        assert_eq!(
            job.seal_blob(&blob, || {}).unwrap().digest,
            crate::pow::sha256d(&blob)
        );
    }

    #[test]
    #[ignore = "allocates the full ~2 GiB RandomX mining dataset"]
    fn randomx_mainnet_job_uses_the_exact_sov_mining_seal() {
        let mut value = valid_job();
        value["algo"] = json!("rx/0");
        value["seed_hash"] =
            json!("cb0272ff88e64c18cde0257f7fae1c8236b02651f10cc7a02456fd682ee2e72d");
        let job = Job::from_stratum(&value).unwrap();
        let nonce: u64 = 0xfeed_face_cafe_beef;
        let mut blob = job.blob.clone();
        blob[job.nonce_offset..].copy_from_slice(&nonce.to_le_bytes());
        let fast = job.seal_blob(&blob, || {}).unwrap();
        assert_eq!(
            fast.mode,
            MiningMode::RandomXFastShared,
            "release validation must not pass through light-memory fallback"
        );
        let light = crate::pow::pow_hash(PowAlgo::RandomX, &job.pow_key, &blob).unwrap();
        assert_eq!(fast.digest, light);
    }

    #[test]
    fn peer_count_accepts_only_the_authenticated_peer_field() {
        assert_eq!(
            peer_count_from_info(&json!({
                "peers": 4,
                "tcpLinks": 9,
                "connectedPeers": ["one", "two"],
            })),
            Some(4)
        );
        assert_eq!(peer_count_from_info(&json!({"peers": 0})), Some(0));
        assert_eq!(
            peer_count_from_info(&json!({"connectedPeers": ["one", "two", "three"]})),
            None
        );
        assert_eq!(peer_count_from_info(&json!({"tcpLinks": 4})), None);
        assert_eq!(peer_count_from_info(&json!({"peers": "4"})), None);
        assert_eq!(peer_count_from_info(&json!({"peers": -1})), None);
        assert_eq!(peer_count_from_info(&json!({"peers": 1.5})), None);
    }

    #[test]
    fn pool_url_prefixes_are_optional() {
        assert_eq!(pool_address("127.0.0.1:3333"), "127.0.0.1:3333");
        assert_eq!(pool_address("tcp://host:1"), "host:1");
        assert_eq!(pool_address("stratum+tcp://host:2"), "host:2");
    }
}
