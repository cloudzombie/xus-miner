use borsh::BorshSerialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::env;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
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
            // Real template transaction identifiers; the miner's block-flow
            // strip must report their count for the forming block.
            "txIds": ["aa".repeat(32), "bb".repeat(32), "cc".repeat(32)],
        });
        Self {
            blob,
            template_id,
            target,
            response,
        }
    }

    fn sov_0_1_99_randomx() -> Self {
        let mut fixture = Self::sov_0_1_99();
        fixture.response["powAlgo"] = json!("RandomX");
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
        "sov_health" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "ok": true,
                "chainId": "sov-mainnet",
                "height": HEIGHT - 1,
                "mempool": 2,
            }
        }),
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
        "sov_getMempoolSize" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": 2,
        }),
        "sov_estimateFee" => json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {"minTip": 1_000},
        }),
        "sov_getBlockByHeight" => match params.get("height").and_then(Value::as_u64) {
            Some(height) if height < HEIGHT => json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "height": height,
                    "hash": format!("{height:064x}"),
                    "txIds": ["dd".repeat(32), "ee".repeat(32)],
                }
            }),
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
    while Instant::now() < deadline
        && !(saw_job && saw_peers && saw_accepted_share && saw_block_flow)
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
                // Every strip value must be exactly what the mock node served:
                // template txIds count, per-block txIds counts, fee estimate.
                assert_eq!(event.pointer("/template/height"), Some(&json!(HEIGHT)));
                assert_eq!(event.pointer("/template/tx_count"), Some(&json!(3)));
                assert_eq!(event["tip_height"], json!(HEIGHT - 1));
                assert_eq!(event["fee_estimate"], json!(1_000));
                let recent = event["recent"].as_array().expect("recent tiles");
                assert!(!recent.is_empty());
                assert_eq!(recent[0]["height"], json!(HEIGHT - 1));
                assert_eq!(recent[0]["tx_count"], json!(2));
                assert_eq!(recent[0]["mine"], json!(false));
                saw_block_flow = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_job && saw_peers && saw_accepted_share && saw_block_flow,
        "missing required telemetry (job={saw_job}, peers={saw_peers}, share={saw_accepted_share}, block_flow={saw_block_flow}); miner stderr:\n{}",
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
