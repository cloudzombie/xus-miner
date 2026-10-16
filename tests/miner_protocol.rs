use ed25519_dalek::{Signature, SigningKey, VerifyingKey};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

struct ChildGuard(Child);

fn sha256d(input: &[u8]) -> [u8; 32] {
    let first = Sha256::digest(input);
    Sha256::digest(first).into()
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn gui_parent_pipe_watchdog_prevents_an_orphan_engine() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--parent-pipe-watchdog",
            "--pool",
            "127.0.0.1:1",
            "--user",
            "watchdog-test",
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn supervised xus-miner");
    let stdout = child.stdout.take().expect("miner supervision pipe");
    let mut reader = BufReader::new(stdout);
    let mut startup_line = String::new();
    reader
        .read_line(&mut startup_line)
        .expect("read miner startup event");
    let startup: Value = serde_json::from_str(&startup_line).expect("valid startup JSON");
    assert_eq!(startup["event"], json!("startup"));

    // Closing the GUI's read end must make the next watchdog heartbeat fail
    // and terminate the engine instead of leaving a background miner behind.
    drop(reader);
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        match child.try_wait().expect("poll watchdog child") {
            Some(status) => {
                assert_eq!(
                    status.code(),
                    Some(70),
                    "broken supervision pipe must use the dedicated orphan-prevention exit code"
                );
                break;
            }
            None if Instant::now() < deadline => thread::sleep(Duration::from_millis(25)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("engine did not exit after its GUI supervision pipe closed");
            }
        }
    }
}

fn read_message(reader: &mut BufReader<std::net::TcpStream>) -> Value {
    let mut line = String::new();
    reader.read_line(&mut line).expect("read miner message");
    assert!(!line.is_empty(), "miner disconnected before sending JSON");
    serde_json::from_str(&line).expect("miner sent valid JSON")
}

fn read_submit_for(reader: &mut BufReader<std::net::TcpStream>, job_id: &str) -> Value {
    for _ in 0..10_000 {
        let message = read_message(reader);
        if message["method"] == json!("submit") && message["params"]["job_id"] == json!(job_id) {
            return message;
        }
    }
    panic!("miner never submitted a share for job {job_id}");
}

fn assert_valid_submit(submit: &Value, job_id: &str, blob: &[u8]) {
    assert_eq!(submit["params"]["job_id"], json!(job_id));
    let nonce_bytes = hex::decode(submit["params"]["nonce"].as_str().unwrap()).unwrap();
    assert_eq!(nonce_bytes.len(), 8);
    let nonce = u64::from_le_bytes(nonce_bytes.try_into().unwrap());
    let mut expected_blob = blob.to_vec();
    expected_blob[56..].copy_from_slice(&nonce.to_le_bytes());
    let expected = sha256d(&expected_blob);
    assert_eq!(submit["params"]["result"], json!(hex::encode(expected)));
    assert!(
        expected
            <= [
                0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
                0xff, 0xff, 0xff, 0xff
            ]
    );
}

fn accept_share(stream: &mut std::net::TcpStream, submit: &Value) {
    writeln!(
        stream,
        "{}",
        json!({
            "id": submit["id"],
            "jsonrpc": "2.0",
            "error": Value::Null,
            "result": {"status": "OK"},
        })
    )
    .unwrap();
    stream.flush().unwrap();
}

fn job(job_id: &str, height: u64, blob: &[u8]) -> Value {
    json!({
        "job_id": job_id,
        "blob": hex::encode(blob),
        "target": "ffff0000",
        // Roughly one share per 2^16 SHA-256d hashes: quick in a test without
        // flooding the socket with a difficulty-one share for every nonce.
        "target_full": format!("0000{}", "ff".repeat(30)),
        "algo": "sha256d",
        "height": height,
        "seed_hash": "",
        "nonce_offset": 56,
        "nonce_size": 8,
    })
}

#[test]
fn external_process_logs_in_hashes_and_submits_a_valid_share() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock Stratum pool");
    let address = listener.local_addr().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--password-stdin",
            "--pool",
            &address.to_string(),
            "--user",
            "integration-worker",
            "--workers",
            "1",
            "--reconnect-secs",
            "1",
            "--report-secs",
            "60",
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xus-miner");
    child
        .stdin
        .take()
        .expect("miner password stdin")
        .write_all(b"integration-secret")
        .expect("write miner password");
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
    let _child = ChildGuard(child);

    let (mut stream, _) = listener.accept().expect("accept miner");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());

    let login = read_message(&mut reader);
    assert_eq!(login["method"], json!("login"));
    assert_eq!(login["params"]["login"], json!("integration-worker"));
    assert_eq!(login["params"]["pass"], json!("integration-secret"));
    assert!(login["params"]["agent"]
        .as_str()
        .unwrap_or_default()
        .starts_with("xus-miner/"));

    let blob = vec![0u8; 64];
    let first_job = job("integration-job", 999, &blob);
    writeln!(
        stream,
        "{}",
        json!({
            "id": 1,
            "jsonrpc": "2.0",
            "error": Value::Null,
            "result": {"id": "session", "job": first_job, "status": "OK"}
        })
    )
    .unwrap();
    stream.flush().unwrap();

    let submit = read_submit_for(&mut reader, "integration-job");
    assert_valid_submit(&submit, "integration-job", &blob);

    let mut events = Vec::new();
    for _ in 0..5 {
        events.push(
            telemetry_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("GUI telemetry event"),
        );
    }
    let event_names: Vec<&str> = events
        .iter()
        .map(|event| event["event"].as_str().expect("telemetry event name"))
        .collect();
    // Pool mode degrades the network block feed EXPLICITLY: a pool discloses
    // shares, not network blocks, and no share is ever presented as one.
    assert_eq!(
        event_names,
        [
            "startup",
            "connecting",
            "pool_connected",
            "chain_feed_unavailable",
            "job"
        ]
    );
    let feed_unavailable = &events[3];
    assert_eq!(feed_unavailable["reason"], json!("stratum"));
    assert!(feed_unavailable["message"]
        .as_str()
        .expect("degradation message")
        .contains("pool discloses shares, not network blocks"));
    let worker_ready = loop {
        let event = telemetry_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("SHA-256d worker readiness telemetry");
        if event["event"] == "worker_ready" {
            break event;
        }
    };
    assert_eq!(worker_ready["worker"], json!(0));
    assert_eq!(worker_ready["mode"], json!("sha256d"));
    // Until the pool answers, the one-worker client must not queue a second
    // share. This is the backpressure that prevents stale-share floods.
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(200)))
        .unwrap();
    let mut unexpected = String::new();
    let error = reader
        .read_line(&mut unexpected)
        .expect_err("a second share was queued before the first response");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    accept_share(&mut stream, &submit);

    // A server-pushed replacement job must supersede the old work immediately.
    let pushed_blob = vec![0x5au8; 64];
    writeln!(
        stream,
        "{}",
        json!({
            "jsonrpc": "2.0",
            "method": "job",
            "params": job("pushed-job", 1000, &pushed_blob),
        })
    )
    .unwrap();
    stream.flush().unwrap();
    let pushed_submit = read_submit_for(&mut reader, "pushed-job");
    assert_valid_submit(&pushed_submit, "pushed-job", &pushed_blob);
    accept_share(&mut stream, &pushed_submit);

    // Closing the pool connection must make the long-running client reconnect
    // and log in again, rather than silently leaving its workers on stale work.
    drop(reader);
    drop(stream);
    let (mut stream, _) = listener.accept().expect("accept reconnecting miner");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let second_login = read_message(&mut reader);
    assert_eq!(second_login["method"], json!("login"));

    let reconnected_blob = vec![0xa5u8; 64];
    writeln!(
        stream,
        "{}",
        json!({
            "id": 1,
            "jsonrpc": "2.0",
            "error": Value::Null,
            "result": {
                "id": "session-2",
                "job": job("reconnected-job", 1001, &reconnected_blob),
                "status": "OK"
            }
        })
    )
    .unwrap();
    stream.flush().unwrap();
    let reconnected_submit = read_submit_for(&mut reader, "reconnected-job");
    assert_valid_submit(&reconnected_submit, "reconnected-job", &reconnected_blob);
    accept_share(&mut stream, &reconnected_submit);
}

#[test]
fn seed_flag_errors_do_not_echo_the_seed() {
    let seed = "11".repeat(32);
    let conflict = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args(["--headless", "--seed-hex", &seed, "--seed-file", "also-set"])
        .output()
        .expect("spawn miner");
    assert!(!conflict.status.success());
    let conflict_err = String::from_utf8_lossy(&conflict.stderr);
    assert!(conflict_err.contains("only one of --seed-hex or --seed-file"));
    assert!(!conflict_err.contains(&seed), "{conflict_err}");

    let malformed = "ab".repeat(40);
    let rejected = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args(["--headless", "--seed-hex", &malformed])
        .output()
        .expect("spawn miner");
    assert!(!rejected.status.success());
    let rejected_err = String::from_utf8_lossy(&rejected.stderr);
    assert!(rejected_err.contains("64 hex characters"));
    assert!(!rejected_err.contains(&malformed), "{rejected_err}");
}

#[test]
fn seed_file_signs_a_payout_notice_without_leaking_the_seed() {
    let seed_hex = "11".repeat(32);
    let account = "8ce53d0a623b12aac68d7a18ba9c31af1b11f56352e73959c0b484d06d10dfb7";
    let directory = std::env::temp_dir().join(format!(
        "xus-miner-payout-{}-{}",
        std::process::id(),
        "protocol"
    ));
    let _cleanup = TempDir(directory.clone());
    fs::create_dir_all(&directory).expect("create seed directory");
    let seed_path = directory.join("seed");
    let seed_path_arg = seed_path.to_string_lossy().into_owned();
    {
        let mut file = fs::File::create(&seed_path).expect("create seed file");
        writeln!(file, "{seed_hex}").expect("write seed file");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&seed_path, fs::Permissions::from_mode(0o600)).expect("mode 0600");
    }

    let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock pool");
    let address = listener.local_addr().unwrap();
    let mut child = Command::new(env!("CARGO_BIN_EXE_xus-miner"))
        .args([
            "--headless",
            "--json-events",
            "--pool",
            &address.to_string(),
            "--user",
            account,
            "--seed-file",
            &seed_path_arg,
            "--workers",
            "1",
            "--reconnect-secs",
            "30",
            "--report-secs",
            "60",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn miner");
    let stdout = child.stdout.take().expect("stdout");
    let stderr = child.stderr.take().expect("stderr");
    let (out_tx, out_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if out_tx.send(line).is_err() {
                break;
            }
        }
    });
    let (err_tx, err_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            if err_tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut miner = ChildGuard(child);

    let (mut stream, _) = listener.accept().expect("accept miner");
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let login = read_message(&mut reader);
    let login_line = login.to_string();
    assert_eq!(login["method"], json!("login"));
    assert_eq!(login["params"]["login"], json!(account));
    assert!(!login_line.contains(&seed_hex), "{login_line}");

    let blob = vec![0u8; 64];
    writeln!(
        stream,
        "{}",
        json!({
            "id": 1,
            "jsonrpc": "2.0",
            "error": Value::Null,
            "result": {
                "id": "session",
                "job": job("payout-job", 2, &blob),
                "status": "OK"
            }
        })
    )
    .unwrap();
    let notice = json!({
        "jsonrpc": "2.0",
        "method": "payout",
        "params": {
            "account": account,
            "satisfied": false,
            "rewardGrains": "1250000000",
            "transfers": [{"to": "bob.sov", "amount": "42"}],
            "nextNonce": 7,
            "signingDomain": Value::Null
        }
    });
    writeln!(stream, "{notice}").unwrap();
    stream.flush().unwrap();

    let payout = read_method(&mut reader, "payout_txs");
    let payout_line = payout.to_string();
    assert!(!payout_line.contains(&seed_hex), "{payout_line}");
    assert_eq!(payout["params"]["txs"].as_array().unwrap().len(), 1);
    let tx = &payout["params"]["txs"][0];
    assert_eq!(tx["transaction"]["signer"], json!(account));
    assert_eq!(tx["transaction"]["nonce"], json!(7));
    assert_eq!(tx["transaction"]["action"]["type"], json!("transfer"));
    assert_eq!(tx["transaction"]["action"]["to"], json!("bob.sov"));
    assert_eq!(tx["transaction"]["action"]["amount"], json!("42"));

    let signing = SigningKey::from_bytes(&[0x11; 32]);
    let public = signing.verifying_key().to_bytes();
    assert_eq!(
        tx["transaction"]["public_key"],
        json!(format!("0x{}", hex::encode(public)))
    );
    let signature =
        hex::decode(tx["signature"].as_str().unwrap().trim_start_matches("0x")).unwrap();
    let preimage = pool_transfer_preimage(account, &public, 7, "bob.sov", 42);
    let parsed = Signature::from_slice(&signature).unwrap();
    VerifyingKey::from_bytes(&public)
        .unwrap()
        .verify_strict(&preimage, &parsed)
        .expect("payout signature must match the pool preimage");

    writeln!(stream, "{notice}").unwrap();
    stream.flush().unwrap();
    reader
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(400)))
        .unwrap();
    let deadline = Instant::now() + Duration::from_millis(400);
    while Instant::now() < deadline {
        let mut line = String::new();
        match reader.read_line(&mut line) {
            Ok(0) => break,
            Ok(_) => {
                if line.contains("payout_txs") {
                    panic!("duplicate payout notice was signed again: {line}");
                }
                assert!(!line.contains(&seed_hex), "{line}");
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                break;
            }
            Err(error) => panic!("read after duplicate notice: {error}"),
        }
    }

    drop(reader);
    drop(stream);
    let _ = miner.0.kill();
    let _ = miner.0.wait();
    let mut logs = String::new();
    let log_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < log_deadline {
        let mut progressed = false;
        if let Ok(line) = out_rx.try_recv() {
            logs.push_str(&line);
            logs.push('\n');
            progressed = true;
        }
        if let Ok(line) = err_rx.try_recv() {
            logs.push_str(&line);
            logs.push('\n');
            progressed = true;
        }
        if !progressed {
            thread::sleep(Duration::from_millis(20));
        }
    }
    assert!(
        logs.contains("payout_submitted") || logs.contains("signed 1 payout"),
        "miner did not report the signature: {logs}"
    );
    assert!(!logs.contains(&seed_hex), "{logs}");
}

fn read_method(reader: &mut BufReader<std::net::TcpStream>, method: &str) -> Value {
    for _ in 0..10_000 {
        let message = read_message(reader);
        if message["method"] == json!(method) {
            return message;
        }
    }
    panic!("miner never sent {method}");
}

/// The Borsh transfer body the pool verifies, built independently of the miner.
fn pool_transfer_preimage(
    signer: &str,
    public: &[u8; 32],
    nonce: u64,
    to: &str,
    amount: u128,
) -> Vec<u8> {
    let mut out = Vec::new();
    let put = |out: &mut Vec<u8>, value: &str| {
        let bytes = value.as_bytes();
        out.extend_from_slice(&(u32::try_from(bytes.len()).unwrap()).to_le_bytes());
        out.extend_from_slice(bytes);
    };
    put(&mut out, signer);
    out.push(0);
    out.extend_from_slice(public);
    out.extend_from_slice(&nonce.to_le_bytes());
    out.push(0);
    put(&mut out, to);
    out.extend_from_slice(&amount.to_le_bytes());
    out
}

struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}
