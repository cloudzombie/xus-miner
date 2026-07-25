use borsh::BorshSerialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const COINBASE: &str = "integration.coinbase.sov";
const HEIGHT: u64 = 43;
// This fixed timestamp makes nonce zero's SHA-256d seal begin `008915…`.
// It therefore meets BITS below deterministically on the worker's first hash.
const TIMESTAMP_MS: u64 = 1_700_000_000_232;
const VERSION_BITS: u32 = 0b11;
// Canonical compact form of 00_ff_ff_00...00.
const BITS: u32 = 0x2000_ffff;
const POW_KEY: &str = "cb0272ff88e64c18cde0257f7fae1c8236b02651f10cc7a02456fd682ee2e72d";
const PROCESS_LOG_TAIL_BYTES: usize = 64 * 1024;
const TELEMETRY_QUEUE_CAPACITY: usize = 256;

#[derive(BorshSerialize)]
struct BlockHeaderFixture {
    height: u64,
    prev_hash: [u8; 32],
    tx_root: [u8; 32],
    receipts_root: [u8; 32],
    state_root: [u8; 32],
    timestamp_ms: u64,
    proposer: String,
    version_bits: u32,
    bits: u32,
    nonce: u64,
}

#[derive(Clone)]
struct TemplateFixture {
    blob: Vec<u8>,
    template_id: String,
    target: [u8; 32],
    response: Value,
    /// Serve the pre-upgrade node's shapes: no template txCount/txIds, no
    /// header.hash, no estimateFee floorGrains, and no
    /// sov_getMempoolHistogram method at all. The miner must degrade to
    /// honest placeholders against this node, never crash or invent values.
    legacy: bool,
    /// The mock chain tip served by `sov_health`. Tests advance it to drive
    /// the network block feed; blocks exist for every height at or below it.
    tip: Arc<AtomicU64>,
    /// The sealed-block hash of the last accepted `sov_submitBlock`; the
    /// upgraded mock discloses it as block HEIGHT's `header.hash` so the
    /// feed's exact-hash YOURS matching is exercised end to end.
    accepted_hash: Arc<Mutex<Option<String>>>,
    /// Every tip height `sov_health` actually served, in order. Lets a test
    /// wait deterministically (condition + deadline, never a bare sleep)
    /// until the miner has been given a specific tip value to observe.
    served_tips: Arc<Mutex<Vec<u64>>>,
    /// Per-height `header.hash` overrides for `sov_getBlockByHeight`:
    /// swapping the hash a height discloses drives a REAL reorg through the
    /// production feed path.
    hash_overrides: Arc<Mutex<BTreeMap<u64, String>>>,
    /// When set, `sov_getBlockByHeight` answers every call with a JSON-RPC
    /// error: the per-block detail fetch fails while the node itself still
    /// discloses hashes — the transient-failure case that must never be
    /// conflated with "node disclosed no hash".
    fail_block_detail: Arc<AtomicBool>,
}

/// Miner-declared header timestamp the mock serves for `height`: the fixed
/// template timestamp offset by 150 s per block around HEIGHT.
fn mock_timestamp_ms(height: u64) -> u64 {
    let offset = (i128::from(height) - i128::from(HEIGHT)) * 150_000;
    u64::try_from(i128::from(TIMESTAMP_MS) + offset).expect("mock timestamp fits u64")
}

impl TemplateFixture {
    fn sov_0_1_99() -> Self {
        let header = BlockHeaderFixture {
            height: HEIGHT,
            prev_hash: [0x11; 32],
            tx_root: [0x22; 32],
            receipts_root: [0x33; 32],
            state_root: [0x44; 32],
            timestamp_ms: TIMESTAMP_MS,
            proposer: COINBASE.to_owned(),
            version_bits: VERSION_BITS,
            bits: BITS,
            nonce: 0,
        };
        let blob = borsh::to_vec(&header).expect("serialize SOV header fixture");
        assert_eq!(
            hex::encode(sha256d(&blob)),
            "00891519a548c92ad3cd3334becc3fab58d88dbfe5c08cfac2e0a83d5adb21c7",
            "fixed template must keep its deterministic nonce-zero seal"
        );
        let template_id = hex::encode(blake3::hash(&blob).as_bytes());
        let mut target = [0u8; 32];
        target[1] = 0xff;
        target[2] = 0xff;
        let response = json!({
            "templateId": template_id,
            "height": HEIGHT,
            "prevHash": "11".repeat(32),
            "txRoot": "22".repeat(32),
            "receiptsRoot": "33".repeat(32),
            "stateRoot": "44".repeat(32),
            "timestampMs": TIMESTAMP_MS,
            "minTimestampMs": TIMESTAMP_MS - 1,
            "bits": BITS,
            "target": hex::encode(target),
            "powAlgo": "Sha256d",
            "powKey": POW_KEY,
            "proposer": COINBASE,
            "versionBits": VERSION_BITS,
            "blob": hex::encode(&blob),
            "nonceOffset": blob.len() - 8,
            // The upgraded node's template additionally discloses its real
            // transaction count and ids; these feed the forming tile.
            "txCount": 1,
            "txIds": ["ab".repeat(32)],
        });
        Self {
            blob,
            template_id,
            target,
            response,
            legacy: false,
            tip: Arc::new(AtomicU64::new(HEIGHT - 1)),
            accepted_hash: Arc::new(Mutex::new(None)),
            served_tips: Arc::new(Mutex::new(Vec::new())),
            hash_overrides: Arc::new(Mutex::new(BTreeMap::new())),
            fail_block_detail: Arc::new(AtomicBool::new(false)),
        }
    }

    fn sov_0_1_99_randomx() -> Self {
        let mut fixture = Self::sov_0_1_99();
        fixture.response["powAlgo"] = json!("RandomX");
        fixture
    }

    /// The pre-upgrade (live today) node: the same consensus contract but
    /// none of the additive v0.1.99 parity RPC fields.
    fn sov_pre_upgrade_legacy() -> Self {
        let mut fixture = Self::sov_0_1_99();
        fixture.legacy = true;
        let template = fixture
            .response
            .as_object_mut()
            .expect("template fixture object");
        // NOTE: the pre-upgrade sov_getBlockTemplate reply carries NO
        // transaction list or count — only the txRoot commitment — so the
        // forming-block tile's tx count must stay a placeholder.
        template.remove("txCount");
        template.remove("txIds");
        fixture
    }
}

#[derive(Debug)]
enum ServerEvent {
    Request(Value),
    Failure(String),
}

struct MockNode {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
}

impl MockNode {
    fn start(fixture: TemplateFixture) -> (Self, mpsc::Receiver<ServerEvent>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock SOV RPC");
        let address = listener.local_addr().expect("mock SOV RPC address");
        let stop = Arc::new(AtomicBool::new(false));
        let server_stop = Arc::clone(&stop);
        let (events_tx, events_rx) = mpsc::channel();
        let join = thread::spawn(move || {
            while !server_stop.load(Ordering::Acquire) {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) => {
                        let _ = events_tx.send(ServerEvent::Failure(format!(
                            "mock RPC accept failed: {error}"
                        )));
                        break;
                    }
                };
                if server_stop.load(Ordering::Acquire) {
                    break;
                }
                if let Err(error) = handle_rpc_connection(&mut stream, &fixture, &events_tx) {
                    let _ = events_tx.send(ServerEvent::Failure(error));
                }
            }
        });
        (
            Self {
                address,
                stop,
                join: Some(join),
            },
            events_rx,
        )
    }
}

impl Drop for MockNode {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        let _ = TcpStream::connect(self.address);
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn append_bounded_log_tail(tail: &mut String, line: &str) {
    tail.push_str(line);
    tail.push('\n');
    if tail.len() > PROCESS_LOG_TAIL_BYTES {
        let mut start = tail.len() - PROCESS_LOG_TAIL_BYTES;
        while !tail.is_char_boundary(start) {
            start += 1;
        }
        tail.drain(..start);
    }
}

fn process_log_tails(stdout_tail: &Arc<Mutex<String>>, stderr_tail: &Arc<Mutex<String>>) -> String {
    let stdout = stdout_tail
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    let stderr = stderr_tail
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    format!(
        "miner stdout tail:\n{}\nminer stderr tail:\n{}",
        if stdout.is_empty() {
            "<empty>"
        } else {
            &stdout
        },
        if stderr.is_empty() {
            "<empty>"
        } else {
            &stderr
        }
    )
}

fn observe_randomx_telemetry(
    line: &str,
    fast_shared_ready: &mut [bool],
    saw_positive_hashrate: &mut bool,
) -> Result<(), String> {
    let Ok(event) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    match event.get("event").and_then(Value::as_str) {
        Some("worker_ready") => {
            let worker = event["worker"].as_u64().ok_or_else(|| {
                format!("RandomX worker_ready telemetry omitted a numeric worker: {event}")
            })?;
            if worker >= fast_shared_ready.len() as u64 {
                return Err(format!(
                    "RandomX worker_ready telemetry reported unexpected worker {worker}"
                ));
            }
            let expected_mode = if env::var_os("XUS_TEST_RANDOMX_LIGHT").is_some() {
                "light-recovery"
            } else {
                "fast-shared"
            };
            if event["mode"].as_str() != Some(expected_mode) {
                return Err(format!(
                    "RandomX worker {worker} reported mode={} instead of mode=\"{expected_mode}\"",
                    event["mode"],
                ));
            }
            fast_shared_ready[worker as usize] = true;
        }
        Some("metrics") if event["hashrate"].as_f64().is_some_and(|rate| rate > 0.0) => {
            *saw_positive_hashrate = true;
        }
        Some("worker_error") => {
            return Err(format!(
                "RandomX worker {} reported an engine error: {}",
                event["worker"], event["message"]
            ));
        }
        Some("session_error") => {
            return Err(format!("RandomX RPC session failed: {}", event["message"]));
        }
        Some("engine_fatal") => {
            return Err(format!(
                "RandomX engine reported a fatal error: {}",
                event["message"]
            ));
        }
        _ => {}
    }
    Ok(())
}

fn receive_randomx_telemetry(
    telemetry_rx: &mpsc::Receiver<String>,
    timeout: Duration,
    fast_shared_ready: &mut [bool],
    saw_positive_hashrate: &mut bool,
) -> Result<(), String> {
    let first = match telemetry_rx.recv_timeout(timeout) {
        Ok(line) => line,
        Err(mpsc::RecvTimeoutError::Timeout) => return Ok(()),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("RandomX telemetry stream disconnected".into());
        }
    };
    observe_randomx_telemetry(&first, fast_shared_ready, saw_positive_hashrate)?;
    for _ in 0..TELEMETRY_QUEUE_CAPACITY {
        match telemetry_rx.try_recv() {
            Ok(line) => {
                observe_randomx_telemetry(&line, fast_shared_ready, saw_positive_hashrate)?;
            }
            Err(mpsc::TryRecvError::Empty) => return Ok(()),
            Err(mpsc::TryRecvError::Disconnected) => {
                return Err("RandomX telemetry stream disconnected".into());
            }
        }
    }
    Ok(())
}

fn sha256d(input: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(input);
    Sha256::digest(first).into()
}

fn read_http_json(stream: &mut TcpStream) -> Result<Value, String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set mock RPC read timeout: {error}"))?;
    let mut reader = BufReader::new(
        stream
            .try_clone()
            .map_err(|error| format!("clone mock RPC stream: {error}"))?,
    );
    let mut request_line = String::new();
    reader
        .read_line(&mut request_line)
        .map_err(|error| format!("read HTTP request line: {error}"))?;
    if !request_line.starts_with("POST / HTTP/1.1") {
        return Err(format!("unexpected HTTP request line: {request_line:?}"));
    }

    let mut content_length = None;
    let mut header_bytes = request_line.len();
    loop {
        let mut line = String::new();
        reader
            .read_line(&mut line)
            .map_err(|error| format!("read HTTP header: {error}"))?;
        if line.is_empty() {
            return Err("HTTP request ended before its header terminator".into());
        }
        header_bytes += line.len();
        if header_bytes > 16 * 1024 {
            return Err("HTTP request headers exceed 16 KiB".into());
        }
        if line == "\r\n" {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                content_length = Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .map_err(|error| format!("invalid Content-Length: {error}"))?,
                );
            }
        }
    }
    let content_length =
        content_length.ok_or_else(|| "HTTP request has no Content-Length".to_string())?;
    if content_length > 1024 * 1024 {
        return Err("HTTP request body exceeds 1 MiB".into());
    }
    let mut body = vec![0u8; content_length];
    reader
        .read_exact(&mut body)
        .map_err(|error| format!("read HTTP request body: {error}"))?;
    serde_json::from_slice(&body).map_err(|error| format!("parse JSON-RPC request: {error}"))
}

fn write_http_json(stream: &mut TcpStream, body: &Value) -> Result<(), String> {
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|error| format!("set mock RPC write timeout: {error}"))?;
    let body = body.to_string();
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
    .map_err(|error| format!("write JSON-RPC response: {error}"))?;
    stream
        .flush()
        .map_err(|error| format!("flush JSON-RPC response: {error}"))
}

fn handle_rpc_connection(
    stream: &mut TcpStream,
    fixture: &TemplateFixture,
    events: &mpsc::Sender<ServerEvent>,
) -> Result<(), String> {
    let request = read_http_json(stream)?;
    events
        .send(ServerEvent::Request(request.clone()))
        .map_err(|_| "test stopped receiving mock RPC events".to_string())?;
    let id = request.get("id").cloned().unwrap_or(Value::Null);
    let method = request
        .get("method")
        .and_then(Value::as_str)
        .ok_or_else(|| "JSON-RPC request has no method".to_string())?;
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));

    let response = match method {
        "sov_health" => {
            let tip = fixture.tip.load(Ordering::Acquire);
            fixture
                .served_tips
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(tip);
            json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "ok": true,
                    "chainId": "sov-mainnet",
                    "height": tip,
                    "mempool": 2,
                }
            })
        }
        "sov_getBlockTemplate" => {
            if params.get("coinbaseAccount").and_then(Value::as_str) != Some(COINBASE) {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": {
                        "code": -32602,
                        "message": "coinbaseAccount did not match the integration account",
                    }
                })
            } else {
                json!({"jsonrpc": "2.0", "id": id, "result": fixture.response})
            }
        }
        "sov_getDifficulty" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "sha256d": "1",
                "algo": "Sha256d",
                "hashrate": 1234.0,
                "targetBlockMs": 150000,
            }
        }),
        "sov_getPeerInfo" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "chainId": "sov-mainnet",
                "version": "v0.1.99",
                "genesisHash": POW_KEY,
                "height": HEIGHT - 1,
                "p2pEnabled": true,
                "listenAddr": "127.0.0.1:9650",
                "tcpLinks": 4,
                "peers": 4,
                "connectedPeers": [
                    "127.0.0.1:9651",
                    "127.0.0.1:9652",
                    "127.0.0.1:9653",
                    "127.0.0.1:9654",
                ],
                "peerVersions": [
                    {"addr": "127.0.0.1:9651", "protocol": 2, "agent": "sov/v0.1.99"},
                    {"addr": "127.0.0.1:9652", "protocol": 2, "agent": "sov/v0.1.99"},
                    {"addr": "127.0.0.1:9653", "protocol": 2, "agent": "sov/v0.1.99"},
                    {"addr": "127.0.0.1:9654", "protocol": 2, "agent": "sov/v0.1.99"},
                ],
                "protocolVersion": 2,
                "bestPeerHeight": HEIGHT - 1,
                "behindBlocks": 0,
                "syncing": false,
            }
        }),
        // Live-verified shape: a bare integer count.
        "sov_getMempoolSize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": 2,
        }),
        // Live-verified shape: feeGrains is a decimal string in grains. The
        // upgraded node additionally returns the auction floor fields
        // floorGrains + tipFloorGrains (also decimal strings).
        "sov_estimateFee" => {
            let mut result = json!({
                "feeGrains": "1651760",
                "gasPriceGrains": "10",
                "gasUsed": 165_176,
                "kind": "transfer",
            });
            if !fixture.legacy {
                result["floorGrains"] = json!("7000");
                result["tipFloorGrains"] = json!("0");
            }
            json!({"jsonrpc": "2.0", "id": id, "result": result})
        }
        // The upgraded node's mempool fee histogram (exact live PR shape:
        // grains fields are decimal strings, buckets high→low effective
        // tip). A pre-upgrade node does not know the method at all.
        "sov_getMempoolHistogram" if !fixture.legacy => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "txCount": 2,
                "floorGrains": "7000",
                "poolFloorGrains": "0",
                "maxBlockTxs": 1,
                "buckets": [
                    {"feeRateGrains": "7000", "txCount": 1, "totalBytes": 305},
                    {"feeRateGrains": "0", "txCount": 1, "totalBytes": 262},
                ],
            },
        }),
        // Live-verified shape: {"header": {...}, "transactions": [...]} with
        // no txIds/txCount fields, and the miner-declared header timestamp
        // as `timestamp_ms` in milliseconds (verified against the live
        // mainnet node; disclosed by pre-upgrade nodes too). The upgraded
        // node additionally discloses the block's own hash inside `header`.
        // A forced detail-fetch failure: the node is healthy and discloses
        // hashes, but this specific per-block call fails this cycle.
        "sov_getBlockByHeight" if fixture.fail_block_detail.load(Ordering::Acquire) => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32000,
                "message": "mock node forced a transient block-detail failure",
            }
        }),
        "sov_getBlockByHeight" => match params.get("height").and_then(Value::as_u64) {
            Some(height) if height <= fixture.tip.load(Ordering::Acquire) => {
                let mut header = json!({
                    "height": height,
                    "prevHash": "11".repeat(32),
                    "txRoot": "22".repeat(32),
                    "timestamp_ms": mock_timestamp_ms(height),
                    "proposer": COINBASE,
                });
                if !fixture.legacy {
                    // A test-installed override (reorg drills) wins; else
                    // block HEIGHT is the one this miner seals in the feed
                    // test: once a submission was accepted, the chain block
                    // at that height IS the accepted block, so its disclosed
                    // hash is the accepted sealed-header hash.
                    let overridden = fixture
                        .hash_overrides
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .get(&height)
                        .cloned();
                    let accepted = (height == HEIGHT)
                        .then(|| {
                            fixture
                                .accepted_hash
                                .lock()
                                .unwrap_or_else(|poisoned| poisoned.into_inner())
                                .clone()
                        })
                        .flatten();
                    header["hash"] = json!(overridden
                        .or(accepted)
                        .unwrap_or_else(|| format!("{height:064x}")));
                }
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "header": header,
                        "transactions": [
                            {"kind": "transfer", "nonce": 1},
                            {"kind": "transfer", "nonce": 2},
                        ],
                    }
                })
            }
            _ => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32602,
                    "message": "unknown or unconfirmed height",
                }
            }),
        },
        "sov_submitBlock" => {
            let is_sha_fixture =
                fixture.response.get("powAlgo").and_then(Value::as_str) == Some("Sha256d");
            let template_matches = params.get("templateId").and_then(Value::as_str)
                == Some(fixture.template_id.as_str());
            let timestamp_matches =
                params.get("timestampMs").and_then(Value::as_u64) == Some(TIMESTAMP_MS);
            let nonce = params.get("nonce").and_then(Value::as_u64);
            let (seal_matches, hash) = nonce.map_or((false, String::new()), |nonce| {
                let mut sealed_blob = fixture.blob.clone();
                let offset = sealed_blob.len() - 8;
                sealed_blob[offset..].copy_from_slice(&nonce.to_le_bytes());
                (
                    sha256d(&sealed_blob) <= fixture.target,
                    hex::encode(blake3::hash(&sealed_blob).as_bytes()),
                )
            });
            if is_sha_fixture && template_matches && timestamp_matches && seal_matches {
                *fixture
                    .accepted_hash
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hash.clone());
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "accepted": true,
                        "hash": hash,
                        "height": HEIGHT,
                    }
                })
            } else {
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "accepted": false,
                        "hash": hash,
                        "height": HEIGHT,
                        "error": "mock node rejected invalid submission",
                    }
                })
            }
        }
        other => json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("method not found: {other}"),
            }
        }),
    };
    write_http_json(stream, &response)
}

#[test]
fn headless_miner_round_trips_the_sov_0_1_99_direct_rpc_contract() {
    let fixture = TemplateFixture::sov_0_1_99();
    let (mock_node, request_rx) = MockNode::start(fixture.clone());
    let endpoint = format!("rpc://{}", mock_node.address);

    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--pool",
            &endpoint,
            "--user",
            COINBASE,
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless xus-miner");

    let stdout = child.stdout.take().expect("miner telemetry stdout");
    let (telemetry_tx, telemetry_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if telemetry_tx.send(value).is_err() {
                    break;
                }
            }
        }
    });

    let stderr = child.stderr.take().expect("miner diagnostic stderr");
    let stderr_log = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_log);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut log = stderr_sink.lock().expect("stderr log lock");
            if log.len() < 64 * 1024 {
                log.push_str(&line);
                log.push('\n');
            }
        }
    });
    let _child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_job = false;
    let mut saw_peers = false;
    let mut saw_accepted_share = false;
    let mut saw_block_flow = false;
    let mut saw_chain_baseline = false;
    while Instant::now() < deadline
        && !(saw_job && saw_peers && saw_accepted_share && saw_block_flow && saw_chain_baseline)
    {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match telemetry_rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "miner telemetry ended unexpectedly; miner stderr:\n{}",
                stderr_log.lock().expect("stderr log lock")
            ),
        };
        match event.get("event").and_then(Value::as_str) {
            Some("job") => {
                assert_eq!(event["height"], json!(HEIGHT));
                assert_eq!(event["algorithm"], json!("SHA-256d"));
                assert_eq!(event["coinbase"], json!(COINBASE));
                saw_job = true;
            }
            Some("peers") => {
                assert_eq!(event["count"], json!(4));
                saw_peers = true;
            }
            Some("share") if event["accepted"].as_u64().unwrap_or_default() >= 1 => {
                assert_eq!(event["rejected"], json!(0));
                saw_accepted_share = true;
            }
            Some("block_flow") => {
                // Every strip value must be exactly what the mock node served
                // in the live-verified upgraded shapes: the template's own
                // txCount fills the forming tile, the auction floorGrains is
                // preferred over the flat feeGrains estimate, the histogram
                // packs to one projected pending block (2 pending txs, 1-tx
                // capacity, first block is the forming one), and each sealed
                // tile counts its `transactions` array.
                assert_eq!(event.pointer("/template/height"), Some(&json!(HEIGHT)));
                assert_eq!(event.pointer("/template/tx_count"), Some(&json!(1)));
                assert_eq!(event["tip_height"], json!(HEIGHT - 1));
                assert_eq!(event["fee_grains"], json!(7_000));
                assert_eq!(
                    event["projected"],
                    json!([{"tx_count": 1, "fee_min_grains": 0, "fee_max_grains": 0}])
                );
                let recent = event["recent"].as_array().expect("recent tiles");
                assert!(!recent.is_empty());
                assert_eq!(recent[0]["height"], json!(HEIGHT - 1));
                assert_eq!(recent[0]["tx_count"], json!(2));
                // The block-time graph's substrate: the exact miner-declared
                // header timestamp the node disclosed, in milliseconds.
                assert_eq!(
                    recent[0]["timestamp_ms"],
                    json!(mock_timestamp_ms(HEIGHT - 1))
                );
                // header.hash is disclosed and none of these are ours, so
                // the exact hash comparison keeps the highlight off.
                assert_eq!(recent[0]["mine"], json!(false));
                saw_block_flow = true;
            }
            Some("chain_baseline") => {
                // First successful poll: a single baseline at the tip.
                assert_eq!(event["height"], json!(HEIGHT - 1));
                saw_chain_baseline = true;
            }
            Some("chain_block") => panic!(
                "a static tip must never produce a block-found event (history \
                 replay): {event}"
            ),
            _ => {}
        }
    }
    assert!(
        saw_job && saw_peers && saw_accepted_share && saw_block_flow && saw_chain_baseline,
        "missing required telemetry (job={saw_job}, peers={saw_peers}, share={saw_accepted_share}, block_flow={saw_block_flow}, chain_baseline={saw_chain_baseline}); miner stderr:\n{}",
        stderr_log.lock().expect("stderr log lock")
    );

    let request_deadline = Instant::now() + Duration::from_secs(5);
    let mut requests = Vec::new();
    while Instant::now() < request_deadline {
        match request_rx.recv_timeout(Duration::from_millis(100)) {
            Ok(ServerEvent::Request(request)) => {
                requests.push(request);
                let methods: Vec<_> = requests
                    .iter()
                    .filter_map(|request| request.get("method").and_then(Value::as_str))
                    .collect();
                if [
                    "sov_health",
                    "sov_getBlockTemplate",
                    "sov_getDifficulty",
                    "sov_getPeerInfo",
                    "sov_getMempoolSize",
                    "sov_estimateFee",
                    "sov_getMempoolHistogram",
                    "sov_getBlockByHeight",
                    "sov_submitBlock",
                ]
                .iter()
                .all(|required| methods.contains(required))
                {
                    break;
                }
            }
            Ok(ServerEvent::Failure(error)) => panic!("mock SOV RPC failed: {error}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                panic!("mock SOV RPC stopped before all requests were observed")
            }
        }
    }

    let find_request = |method: &str| {
        requests
            .iter()
            .find(|request| request.get("method").and_then(Value::as_str) == Some(method))
            .unwrap_or_else(|| {
                panic!("miner never called {method}; observed requests: {requests:?}")
            })
    };
    let template_request = find_request("sov_getBlockTemplate");
    assert_eq!(
        template_request.pointer("/params/coinbaseAccount"),
        Some(&json!(COINBASE))
    );

    let submit = find_request("sov_submitBlock");
    assert_eq!(
        submit.pointer("/params/templateId"),
        Some(&json!(fixture.template_id))
    );
    assert_eq!(
        submit.pointer("/params/timestampMs"),
        Some(&json!(TIMESTAMP_MS))
    );
    let nonce = submit
        .pointer("/params/nonce")
        .and_then(Value::as_u64)
        .expect("submit carries a u64 nonce");
    assert_eq!(
        nonce, 0,
        "the fixed v0.1.99 fixture must win on the worker's first nonce"
    );
    let mut sealed_blob = fixture.blob.clone();
    let nonce_offset = sealed_blob.len() - 8;
    sealed_blob[nonce_offset..].copy_from_slice(&nonce.to_le_bytes());
    assert!(
        sha256d(&sealed_blob) <= fixture.target,
        "submitted nonce must meet the full big-endian SOV target"
    );
}

/// The v0.1.99 parity RPCs are not yet deployed to the live network: a
/// pre-upgrade node has no sov_getMempoolHistogram (method not found), no
/// template txCount/txIds, no estimateFee floorGrains, and no header.hash.
/// The miner must keep mining and degrade every consumer to its honest
/// placeholder/fallback — no crash, no invented value.
#[test]
fn headless_miner_degrades_cleanly_on_a_pre_upgrade_node() {
    let fixture = TemplateFixture::sov_pre_upgrade_legacy();
    let (mock_node, _request_rx) = MockNode::start(fixture.clone());
    let endpoint = format!("rpc://{}", mock_node.address);

    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--pool",
            &endpoint,
            "--user",
            COINBASE,
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless xus-miner");

    let stdout = child.stdout.take().expect("miner telemetry stdout");
    let (telemetry_tx, telemetry_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if telemetry_tx.send(value).is_err() {
                    break;
                }
            }
        }
    });

    let stderr = child.stderr.take().expect("miner diagnostic stderr");
    let stderr_log = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_log);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut log = stderr_sink.lock().expect("stderr log lock");
            if log.len() < 64 * 1024 {
                log.push_str(&line);
                log.push('\n');
            }
        }
    });
    let mut child = ChildGuard(child);

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut saw_degraded_block_flow = false;
    let mut saw_accepted_share = false;
    while Instant::now() < deadline && !(saw_degraded_block_flow && saw_accepted_share) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = match telemetry_rx.recv_timeout(remaining.min(Duration::from_millis(500))) {
            Ok(event) => event,
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                "miner telemetry ended unexpectedly against the legacy node; miner stderr:\n{}",
                stderr_log.lock().expect("stderr log lock")
            ),
        };
        match event.get("event").and_then(Value::as_str) {
            Some("block_flow") => {
                // Honest degraded state, field by field: no template count →
                // null (renders "—"); no floorGrains → the flat feeGrains
                // fallback; no histogram method → NO projected tiles; the
                // legacy header discloses no hash yet real sealed tiles
                // still carry their `transactions` counts.
                assert_eq!(event.pointer("/template/height"), Some(&json!(HEIGHT)));
                assert_eq!(event.pointer("/template/tx_count"), Some(&Value::Null));
                assert_eq!(event["fee_grains"], json!(1_651_760));
                assert_eq!(event["projected"], json!([]));
                let recent = event["recent"].as_array().expect("recent tiles");
                assert!(!recent.is_empty());
                assert_eq!(recent[0]["height"], json!(HEIGHT - 1));
                assert_eq!(recent[0]["tx_count"], json!(2));
                // The live pre-upgrade node DOES disclose header
                // timestamp_ms (verified on mainnet), so the block-time
                // graph substrate is present even against a legacy node.
                assert_eq!(
                    recent[0]["timestamp_ms"],
                    json!(mock_timestamp_ms(HEIGHT - 1))
                );
                saw_degraded_block_flow = true;
            }
            Some("share") if event["accepted"].as_u64().unwrap_or_default() >= 1 => {
                // The missing parity RPCs never block real work: the miner
                // still seals and submits against the legacy node.
                assert_eq!(event["rejected"], json!(0));
                saw_accepted_share = true;
            }
            Some("engine_fatal") | Some("session_error") => panic!(
                "miner failed against the legacy node instead of degrading: {event}; stderr:\n{}",
                stderr_log.lock().expect("stderr log lock")
            ),
            _ => {}
        }
    }
    assert!(
        saw_degraded_block_flow && saw_accepted_share,
        "missing degraded telemetry (block_flow={saw_degraded_block_flow}, share={saw_accepted_share}); miner stderr:\n{}",
        stderr_log.lock().expect("stderr log lock")
    );
    assert!(
        child.0.try_wait().expect("poll miner child").is_none(),
        "miner must stay alive against a pre-upgrade node"
    );
}

/// End-to-end network block feed: baseline once, YOUR accepted block
/// announced with mine=true on exact hash proof, a large tip jump reported
/// bounded with an explicit omitted count, and no height ever reported
/// twice.
#[test]
fn network_block_feed_reports_yours_gap_and_idempotent_network_blocks() {
    let fixture = TemplateFixture::sov_0_1_99();
    let tip = Arc::clone(&fixture.tip);
    let accepted_hash = Arc::clone(&fixture.accepted_hash);
    let (mock_node, _request_rx) = MockNode::start(fixture.clone());
    let endpoint = format!("rpc://{}", mock_node.address);

    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--pool",
            &endpoint,
            "--user",
            COINBASE,
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless xus-miner");

    let stdout = child.stdout.take().expect("miner telemetry stdout");
    let (telemetry_tx, telemetry_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if telemetry_tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let stderr = child.stderr.take().expect("miner diagnostic stderr");
    let stderr_log = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_log);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut log = stderr_sink.lock().expect("stderr log lock");
            if log.len() < 64 * 1024 {
                log.push_str(&line);
                log.push('\n');
            }
        }
    });
    let _child = ChildGuard(child);

    let fail = |message: &str, stderr_log: &Arc<Mutex<String>>| -> ! {
        panic!(
            "{message}; miner stderr:\n{}",
            stderr_log.lock().expect("stderr log lock")
        )
    };

    // Phase 1 — static tip: exactly one baseline, an accepted submission,
    // and NO block-found events (no history replay).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_baseline = false;
    let mut saw_accepted_share = false;
    let mut chain_blocks: Vec<Value> = Vec::new();
    while Instant::now() < deadline && !(saw_baseline && saw_accepted_share) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = telemetry_rx.recv_timeout(remaining.min(Duration::from_millis(500))) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_baseline") => {
                assert_eq!(event["height"], json!(HEIGHT - 1));
                assert!(!saw_baseline, "baseline must be announced exactly once");
                saw_baseline = true;
            }
            Some("chain_block") => chain_blocks.push(event),
            Some("share") if event["accepted"].as_u64().unwrap_or_default() >= 1 => {
                saw_accepted_share = true;
            }
            _ => {}
        }
    }
    if !(saw_baseline && saw_accepted_share) {
        fail(
            "no baseline + accepted submission on the static tip",
            &stderr_log,
        );
    }
    assert!(
        chain_blocks.is_empty(),
        "a static tip produced block-found events (history replay): {chain_blocks:?}"
    );
    let sealed_hash = accepted_hash
        .lock()
        .expect("accepted hash lock")
        .clone()
        .expect("mock recorded the accepted sealed hash");

    // Phase 2 — the accepted block confirms as the new tip: exactly one
    // block-found event, marked YOURS on the exact disclosed-hash proof.
    tip.store(HEIGHT, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut yours: Option<Value> = None;
    while Instant::now() < deadline && yours.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = telemetry_rx.recv_timeout(remaining.min(Duration::from_millis(500))) else {
            continue;
        };
        if event.get("event").and_then(Value::as_str) == Some("chain_block") {
            chain_blocks.push(event.clone());
            yours = Some(event);
        }
    }
    let Some(yours) = yours else {
        fail("the confirmed own block was never announced", &stderr_log);
    };
    assert_eq!(yours["height"], json!(HEIGHT));
    assert_eq!(yours["hash"], json!(sealed_hash));
    assert_eq!(yours["mine"], json!(true), "exact hash proof marks YOURS");
    assert_eq!(yours["tx_count"], json!(2));
    assert_eq!(yours["timestamp_ms"], json!(mock_timestamp_ms(HEIGHT)));

    // Phase 3 — a tip jump of 8: the newest 4 blocks are reported
    // individually (network, exact mock hashes) plus one explicit
    // "... and 4 more" gap event; nothing is reported twice.
    let jump_tip = HEIGHT + 8;
    tip.store(jump_tip, Ordering::Release);
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_gap = false;
    let mut reported_heights: Vec<u64> = Vec::new();
    while Instant::now() < deadline && !(saw_gap && reported_heights.len() >= 4) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = telemetry_rx.recv_timeout(remaining.min(Duration::from_millis(500))) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_gap") => {
                assert_eq!(event["omitted"], json!(4));
                assert_eq!(event["from"], json!(HEIGHT + 1));
                assert_eq!(event["to"], json!(HEIGHT + 4));
                assert!(!saw_gap, "the gap summary must be announced once");
                saw_gap = true;
            }
            Some("chain_block") => {
                let height = event["height"].as_u64().expect("chain_block height");
                assert!(
                    height > HEIGHT + 4 && height <= jump_tip,
                    "omitted gap heights must not be individually reported: {event}"
                );
                assert_eq!(event["mine"], json!(false));
                assert_eq!(event["hash"], json!(format!("{height:064x}")));
                reported_heights.push(height);
                chain_blocks.push(event);
            }
            _ => {}
        }
    }
    if !(saw_gap && reported_heights.len() >= 4) {
        fail("bounded gap reporting incomplete", &stderr_log);
    }
    reported_heights.sort_unstable();
    assert_eq!(
        reported_heights,
        vec![HEIGHT + 5, HEIGHT + 6, HEIGHT + 7, HEIGHT + 8],
        "exactly the newest MAX_GAP_BLOCKS heights are reported"
    );

    // Idempotence: drain a further grace window; no height may repeat.
    let drain_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < drain_deadline {
        let Ok(event) = telemetry_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        if event.get("event").and_then(Value::as_str) == Some("chain_block") {
            chain_blocks.push(event);
        }
    }
    let mut all_heights: Vec<u64> = chain_blocks
        .iter()
        .filter_map(|event| event["height"].as_u64())
        .collect();
    all_heights.sort_unstable();
    let deduplicated = all_heights.len();
    all_heights.dedup();
    assert_eq!(
        all_heights.len(),
        deduplicated,
        "a height+hash was reported twice: {chain_blocks:?}"
    );
}

/// A spawned headless miner with its parsed JSON telemetry stream and a
/// bounded stderr capture — shared plumbing for the feed drill tests below.
struct HeadlessMiner {
    _child: ChildGuard,
    telemetry_rx: mpsc::Receiver<Value>,
    stderr_log: Arc<Mutex<String>>,
}

fn spawn_headless_miner(endpoint: &str) -> HeadlessMiner {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--pool",
            endpoint,
            "--user",
            COINBASE,
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "1",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless xus-miner");

    let stdout = child.stdout.take().expect("miner telemetry stdout");
    let (telemetry_tx, telemetry_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Ok(value) = serde_json::from_str::<Value>(&line) {
                if telemetry_tx.send(value).is_err() {
                    break;
                }
            }
        }
    });
    let stderr = child.stderr.take().expect("miner diagnostic stderr");
    let stderr_log = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_log);
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let mut log = stderr_sink.lock().expect("stderr log lock");
            if log.len() < 64 * 1024 {
                log.push_str(&line);
                log.push('\n');
            }
        }
    });
    HeadlessMiner {
        _child: ChildGuard(child),
        telemetry_rx,
        stderr_log,
    }
}

fn stderr_snapshot(stderr_log: &Arc<Mutex<String>>) -> String {
    stderr_log
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// Wait (condition + deadline, never a bare sleep) until the miner's stderr
/// contains `needle`; returns the full captured stderr.
fn wait_for_stderr(stderr_log: &Arc<Mutex<String>>, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let log = stderr_snapshot(stderr_log);
        if log.contains(needle) {
            return log;
        }
        assert!(
            Instant::now() < deadline,
            "stderr never contained {needle:?}; miner stderr:\n{log}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// Wait until the mock has SERVED a `sov_health` reply carrying `tip` at or
/// after index `from`. Because the mock answers health polls serially and
/// the miner processes each reply before issuing its next poll, any tip
/// change made after this returns is observed by the miner strictly after
/// the awaited value.
fn wait_for_served_tip(served_tips: &Arc<Mutex<Vec<u64>>>, from: usize, tip: u64) {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        {
            let served = served_tips
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if served.iter().skip(from).any(|&served| served == tip) {
                return;
            }
        }
        assert!(
            Instant::now() < deadline,
            "mock never served sov_health tip {tip} after index {from}"
        );
        thread::sleep(Duration::from_millis(50));
    }
}

/// F2 regression (audit v0.1.6): a FAILED per-block detail fetch is not the
/// same thing as a node that discloses no hash. When the fetch fails, the
/// feed must (a) never claim "hash not disclosed by node", and (b) never
/// print an unqualified ★ YOURS from the height fallback the failure made
/// unverifiable — on an upgraded node a same-height reorg replacement could
/// otherwise be mislabeled as this miner's block exactly when the node is
/// busy. The event must carry the distinction additively
/// (`detail_fetch_failed`, `mine_unverified`) without touching any existing
/// field.
#[test]
fn detail_fetch_failure_is_reported_distinctly_and_never_upgrades_to_yours() {
    let fixture = TemplateFixture::sov_0_1_99();
    let tip = Arc::clone(&fixture.tip);
    let accepted_hash = Arc::clone(&fixture.accepted_hash);
    let fail_block_detail = Arc::clone(&fixture.fail_block_detail);
    let (mock_node, _request_rx) = MockNode::start(fixture.clone());
    let miner = spawn_headless_miner(&format!("rpc://{}", mock_node.address));

    // Phase 1 — static tip: baseline announced and the deterministic
    // nonce-zero submission accepted (the miner now has an accepted
    // submission at HEIGHT on record).
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_baseline = false;
    let mut saw_accepted_share = false;
    while Instant::now() < deadline && !(saw_baseline && saw_accepted_share) {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_baseline") => saw_baseline = true,
            Some("share") if event["accepted"].as_u64().unwrap_or_default() >= 1 => {
                saw_accepted_share = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_baseline && saw_accepted_share,
        "no baseline + accepted submission; miner stderr:\n{}",
        stderr_snapshot(&miner.stderr_log)
    );
    assert!(
        accepted_hash.lock().expect("accepted hash lock").is_some(),
        "mock recorded no accepted sealed hash"
    );

    // Phase 2 — the accepted block confirms as the new tip, but every
    // per-block detail fetch fails. The node DOES disclose hashes; the
    // miner just could not retrieve this one.
    fail_block_detail.store(true, Ordering::Release);
    tip.store(HEIGHT, Ordering::Release);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut announced: Option<Value> = None;
    while Instant::now() < deadline && announced.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        if event.get("event").and_then(Value::as_str) == Some("chain_block")
            && event["height"] == json!(HEIGHT)
        {
            announced = Some(event);
        }
    }
    let Some(announced) = announced else {
        panic!(
            "the confirmed block was never announced; miner stderr:\n{}",
            stderr_snapshot(&miner.stderr_log)
        );
    };
    // The hash is unknown BECAUSE THE FETCH FAILED: the event must say so
    // additively, must not assert a proven YOURS, and must keep every
    // pre-existing field in place (height/hash/tx_count/timestamp_ms/mine).
    assert_eq!(announced["hash"], Value::Null);
    assert_eq!(announced["tx_count"], Value::Null);
    assert_eq!(announced["timestamp_ms"], Value::Null);
    assert_eq!(
        announced["detail_fetch_failed"],
        json!(true),
        "the event must distinguish a failed fetch from an undisclosed hash: {announced}"
    );
    assert_eq!(
        announced["mine"],
        json!(false),
        "an uncorroborated height match must not be asserted as a proven win: {announced}"
    );
    assert_eq!(
        announced["mine_unverified"],
        json!(true),
        "the height match must still be reported as unverified: {announced}"
    );

    // The stderr line for that block must state the failure and must not
    // claim the node withheld the hash, nor print an unqualified YOURS.
    let log = wait_for_stderr(
        &miner.stderr_log,
        &format!("height {HEIGHT} | detail fetch failed"),
        Duration::from_secs(10),
    );
    assert!(
        !log.contains("hash not disclosed by node"),
        "a failed fetch must never be reported as the node withholding the hash:\n{log}"
    );
    assert!(
        !log.contains("★ BLOCK FOUND (YOURS)"),
        "a failed fetch must never print an unqualified YOURS:\n{log}"
    );
    assert!(
        log.contains("possibly YOURS — unverified"),
        "the qualified height-match wording is missing:\n{log}"
    );

    // The strip/graph substrate must carry the same honesty: the unfetched
    // tile at HEIGHT is only an unverified height match, never `mine: true`.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut flow_tile: Option<Value> = None;
    while Instant::now() < deadline && flow_tile.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        if event.get("event").and_then(Value::as_str) == Some("block_flow")
            && event["tip_height"] == json!(HEIGHT)
        {
            let recent = event["recent"].as_array().expect("recent tiles").clone();
            assert_eq!(recent[0]["height"], json!(HEIGHT));
            flow_tile = recent.first().cloned();
        }
    }
    let Some(flow_tile) = flow_tile else {
        panic!(
            "no block_flow at the new tip; miner stderr:\n{}",
            stderr_snapshot(&miner.stderr_log)
        );
    };
    assert_eq!(
        flow_tile["mine"],
        json!(false),
        "an unfetched tile must not claim a proven YOURS: {flow_tile}"
    );
    assert_eq!(
        flow_tile["mine_basis"],
        json!("unverified"),
        "the tile must carry the unverified height-match basis: {flow_tile}"
    );
}

/// F5 (audit v0.1.6): drive a REAL reorg end to end through the production
/// feed — tip regresses, the same height reappears with a different
/// node-disclosed hash — and assert the `chain_reorg` event and log line
/// fire exactly once, and that the replacement is never re-announced as a
/// duplicate `chain_block`. Deterministic: every step waits on an observed
/// condition (served health tips / received events), never on raced sleeps.
#[test]
fn network_block_feed_reports_a_real_reorg_and_never_a_duplicate_block() {
    let fixture = TemplateFixture::sov_0_1_99();
    let tip = Arc::clone(&fixture.tip);
    let served_tips = Arc::clone(&fixture.served_tips);
    let hash_overrides = Arc::clone(&fixture.hash_overrides);
    let (mock_node, _request_rx) = MockNode::start(fixture.clone());
    let miner = spawn_headless_miner(&format!("rpc://{}", mock_node.address));

    let reorg_height = HEIGHT + 1;
    let original_hash = format!("{reorg_height:064x}");
    let replacement_hash = format!("bb{:062x}", reorg_height);

    // Phase 1 — baseline, then advance the tip past HEIGHT so the reorg
    // height is announced with the mock's original disclosed hash.
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_baseline = false;
    while Instant::now() < deadline && !saw_baseline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        if event.get("event").and_then(Value::as_str) == Some("chain_baseline") {
            saw_baseline = true;
        }
    }
    assert!(
        saw_baseline,
        "no baseline; miner stderr:\n{}",
        stderr_snapshot(&miner.stderr_log)
    );
    tip.store(reorg_height, Ordering::Release);

    let mut chain_blocks: Vec<Value> = Vec::new();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut saw_reorg_height = false;
    while Instant::now() < deadline && !saw_reorg_height {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_block") => {
                if event["height"] == json!(reorg_height) {
                    assert_eq!(
                        event["hash"],
                        json!(original_hash),
                        "the original disclosed hash must be reported: {event}"
                    );
                    saw_reorg_height = true;
                }
                chain_blocks.push(event);
            }
            Some("chain_reorg") => {
                panic!("no reorg happened yet: {event}")
            }
            _ => {}
        }
    }
    assert!(
        saw_reorg_height,
        "height {reorg_height} was never announced; miner stderr:\n{}",
        stderr_snapshot(&miner.stderr_log)
    );

    // Phase 2 — the tip walks BACK below the reorg height. Wait until the
    // mock has served that regressed tip, so the miner has observed it
    // before the replacement branch re-advances.
    let served_before_regression = served_tips
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .len();
    tip.store(HEIGHT, Ordering::Release);
    wait_for_served_tip(&served_tips, served_before_regression, HEIGHT);

    // Phase 3 — the replacement branch: the same height reappears with a
    // DIFFERENT node-disclosed hash.
    hash_overrides
        .lock()
        .expect("hash override lock")
        .insert(reorg_height, replacement_hash.clone());
    tip.store(reorg_height, Ordering::Release);

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut reorg: Option<Value> = None;
    while Instant::now() < deadline && reorg.is_none() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(event) = miner
            .telemetry_rx
            .recv_timeout(remaining.min(Duration::from_millis(500)))
        else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_reorg") => reorg = Some(event),
            Some("chain_block") => chain_blocks.push(event),
            _ => {}
        }
    }
    let Some(reorg) = reorg else {
        panic!(
            "the reorg was never announced; miner stderr:\n{}",
            stderr_snapshot(&miner.stderr_log)
        );
    };
    assert_eq!(reorg["height"], json!(reorg_height));
    assert_eq!(reorg["old_hash"], json!(original_hash));
    assert_eq!(reorg["new_hash"], json!(replacement_hash));

    // The distinctly worded REORG log line reached stderr.
    wait_for_stderr(
        &miner.stderr_log,
        &format!("REORG at height {reorg_height}"),
        Duration::from_secs(10),
    );

    // Idempotence: drain a grace window; the replacement block must never
    // surface as a duplicate block-found announcement, and no height may
    // ever be announced twice.
    let drain_deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < drain_deadline {
        let Ok(event) = miner.telemetry_rx.recv_timeout(Duration::from_millis(250)) else {
            continue;
        };
        match event.get("event").and_then(Value::as_str) {
            Some("chain_block") => chain_blocks.push(event),
            Some("chain_reorg") => panic!("the reorg must be announced exactly once: {event}"),
            _ => {}
        }
    }
    assert!(
        !chain_blocks
            .iter()
            .any(|event| event["hash"] == json!(replacement_hash)),
        "the reorg replacement was re-announced as a new block: {chain_blocks:?}"
    );
    let mut announced_heights: Vec<u64> = chain_blocks
        .iter()
        .filter_map(|event| event["height"].as_u64())
        .collect();
    announced_heights.sort_unstable();
    let total = announced_heights.len();
    announced_heights.dedup();
    assert_eq!(
        announced_heights.len(),
        total,
        "a height was announced twice: {chain_blocks:?}"
    );
}

#[test]
#[ignore = "long soak for RandomX worker startup and engine stability"]
fn headless_randomx_direct_rpc_engine_stays_alive_for_stability_window() {
    let fixture = TemplateFixture::sov_0_1_99_randomx();
    // Keep the event receiver alive: the mock deliberately refuses to answer
    // after its observer disconnects.
    let (mock_node, request_rx) = MockNode::start(fixture.clone());
    let endpoint = format!("rpc://{}", mock_node.address);

    let light_recovery = env::var_os("XUS_TEST_RANDOMX_LIGHT").is_some();
    let one_worker_recovery =
        light_recovery || env::var_os("XUS_TEST_RANDOMX_ONE_WORKER").is_some();
    let worker_count = if one_worker_recovery { 1_u64 } else { 2_u64 };
    let worker_count_text = worker_count.to_string();
    let mut command = Command::new(env!("CARGO_BIN_EXE_xus-miner"));
    command.args([
        "--headless",
        "--json-events",
        "--pool",
        &endpoint,
        "--user",
        COINBASE,
        "--workers",
        &worker_count_text,
        "--reconnect-secs",
        "1",
        "--report-secs",
        "1",
    ]);
    if light_recovery {
        command.arg("--randomx-light");
    }
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn headless xus-miner");

    let stdout = child.stdout.take().expect("miner telemetry stdout");
    let (telemetry_tx, telemetry_rx) = mpsc::sync_channel(TELEMETRY_QUEUE_CAPACITY);
    let telemetry_overflowed = Arc::new(AtomicBool::new(false));
    let overflow_sink = Arc::clone(&telemetry_overflowed);
    let stdout_tail = Arc::new(Mutex::new(String::new()));
    let stdout_sink = Arc::clone(&stdout_tail);
    let stdout_reader = thread::spawn(move || {
        for result in BufReader::new(stdout).lines() {
            match result {
                Ok(line) => {
                    append_bounded_log_tail(
                        &mut stdout_sink.lock().expect("stdout tail lock"),
                        &line,
                    );
                    match telemetry_tx.try_send(line) {
                        Ok(()) => {}
                        Err(mpsc::TrySendError::Full(_)) => {
                            overflow_sink.store(true, Ordering::Release);
                        }
                        Err(mpsc::TrySendError::Disconnected(_)) => break,
                    }
                }
                Err(error) => {
                    append_bounded_log_tail(
                        &mut stdout_sink.lock().expect("stdout tail lock"),
                        &format!("failed to read miner stdout: {error}"),
                    );
                    break;
                }
            }
        }
    });

    let stderr = child.stderr.take().expect("miner diagnostic stderr");
    let stderr_tail = Arc::new(Mutex::new(String::new()));
    let stderr_sink = Arc::clone(&stderr_tail);
    let stderr_reader = thread::spawn(move || {
        for result in BufReader::new(stderr).lines() {
            match result {
                Ok(line) => append_bounded_log_tail(
                    &mut stderr_sink.lock().expect("stderr tail lock"),
                    &line,
                ),
                Err(error) => {
                    append_bounded_log_tail(
                        &mut stderr_sink.lock().expect("stderr tail lock"),
                        &format!("failed to read miner stderr: {error}"),
                    );
                    break;
                }
            }
        }
    });
    let mut child = ChildGuard(child);

    let mut fast_shared_ready = vec![false; worker_count as usize];
    let mut saw_positive_hashrate = false;
    let mut failure = None;
    let startup_deadline = Instant::now() + Duration::from_secs(240);
    while Instant::now() < startup_deadline
        && !(fast_shared_ready.iter().all(|value| *value) && saw_positive_hashrate)
    {
        if telemetry_overflowed.load(Ordering::Acquire) {
            failure = Some(format!(
                "RandomX telemetry exceeded the bounded queue capacity of \
                 {TELEMETRY_QUEUE_CAPACITY}"
            ));
            break;
        }
        match child.0.try_wait() {
            Ok(Some(status)) => {
                failure = Some(format!("RandomX child exited during startup: {status}"));
                break;
            }
            Ok(None) => {}
            Err(error) => {
                failure = Some(format!(
                    "failed to poll RandomX child during startup: {error}"
                ));
                break;
            }
        }
        let remaining = startup_deadline.saturating_duration_since(Instant::now());
        if let Err(error) = receive_randomx_telemetry(
            &telemetry_rx,
            remaining.min(Duration::from_millis(500)),
            &mut fast_shared_ready,
            &mut saw_positive_hashrate,
        ) {
            failure = Some(error);
            break;
        }
    }
    if failure.is_none() && !fast_shared_ready.iter().all(|value| *value) {
        failure = Some("every RandomX worker must explicitly report its expected mode".to_owned());
    }
    if failure.is_none() && !saw_positive_hashrate {
        failure = Some("RandomX engine never reported positive hashrate".to_owned());
    }

    let mut saw_stability_hashrate = false;
    if failure.is_none() {
        // Clear any startup telemetry before requiring a fresh stability sample.
        if let Err(error) = receive_randomx_telemetry(
            &telemetry_rx,
            Duration::ZERO,
            &mut fast_shared_ready,
            &mut saw_positive_hashrate,
        ) {
            failure = Some(error);
        }
    }
    if failure.is_none() {
        let stability_deadline = Instant::now() + Duration::from_secs(90);
        while Instant::now() < stability_deadline {
            if telemetry_overflowed.load(Ordering::Acquire) {
                failure = Some(format!(
                    "RandomX telemetry exceeded the bounded queue capacity of \
                     {TELEMETRY_QUEUE_CAPACITY} during the stability window"
                ));
                break;
            }
            match child.0.try_wait() {
                Ok(Some(status)) => {
                    failure = Some(format!(
                        "RandomX child terminated during stability window: {status}"
                    ));
                    break;
                }
                Ok(None) => {}
                Err(error) => {
                    failure = Some(format!(
                        "failed to poll RandomX child during stability window: {error}"
                    ));
                    break;
                }
            }
            let remaining = stability_deadline.saturating_duration_since(Instant::now());
            if let Err(error) = receive_randomx_telemetry(
                &telemetry_rx,
                remaining.min(Duration::from_millis(250)),
                &mut fast_shared_ready,
                &mut saw_stability_hashrate,
            ) {
                failure = Some(error);
                break;
            }
        }
    }

    let _ = child.0.kill();
    if let Err(error) = child.0.wait() {
        failure.get_or_insert_with(|| format!("failed to reap RandomX child: {error}"));
    }
    if stdout_reader.join().is_err() {
        failure.get_or_insert_with(|| "miner stdout reader thread panicked".to_owned());
    }
    if stderr_reader.join().is_err() {
        failure.get_or_insert_with(|| "miner stderr reader thread panicked".to_owned());
    }
    drop(request_rx);

    for line in telemetry_rx.try_iter() {
        if let Err(error) =
            observe_randomx_telemetry(&line, &mut fast_shared_ready, &mut saw_stability_hashrate)
        {
            failure.get_or_insert(error);
        }
    }
    if telemetry_overflowed.load(Ordering::Acquire) {
        failure.get_or_insert_with(|| {
            format!(
                "RandomX telemetry exceeded the bounded queue capacity of \
                 {TELEMETRY_QUEUE_CAPACITY}"
            )
        });
    }
    if failure.is_none() && !saw_stability_hashrate {
        failure = Some("RandomX engine reported no positive hashrate during stability".into());
    }
    if failure.is_none() {
        let output = stdout_tail
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let expected_backend = if light_recovery {
            r#""randomx_backend":"light-recovery""#
        } else {
            r#""randomx_backend":"optimized""#
        };
        if !output.contains(expected_backend) {
            failure = Some("RandomX soak did not confirm the requested backend".into());
        }
    }
    if let Some(error) = failure {
        panic!("{error}\n{}", process_log_tails(&stdout_tail, &stderr_tail));
    }
}
