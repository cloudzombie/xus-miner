# xus-miner

`xus-miner` is a standalone native desktop and headless CPU miner for SOV/XUS.
It connects directly to a SOV Station/node JSON-RPC endpoint (normally port
`8645`) or, optionally, to a `sov-stratum` pool/bridge. It does not run a node,
hold a wallet seed, or decide consensus.

This is an independent repository. It is not part of the SOV workspace and has
no local path into chaincode. See [CHAINCODE_BOUNDARY.md](CHAINCODE_BOUNDARY.md)
for the enforced dependency and GitHub-permission boundary.

For maximum build isolation, use a GitHub release: its binaries are compiled on
ephemeral hosted runners with no SOV checkout, then checksummed and given GitHub
build-provenance attestations. Verify a downloaded asset with
`gh attestation verify <asset> --repo cloudzombie/xus-miner` before unpacking it.

For direct solo mining, enter the node endpoint and your **public account ID**:

```text
Endpoint: 192.168.0.244:8645
User:     <public SOV account ID receiving coinbase rewards>
Password: unused in direct mode
```

Leaving User as `xus-miner` requests no override and uses the SOV node's
configured miner account. Supplying a public account ID requests that account
explicitly. In both cases the miner reads the `proposer` encoded in the returned
block template and reports it as `COINBASE CONFIRMED`; if an explicit account
does not match the template, the miner refuses to hash.

When the node is on another computer, enable **Expose node RPC on LAN** in SOV
Station and restart the node. SOV Station's own mining switch may remain on or
off; it is independent of external mining.

The miner validates every job before hashing it, uses locked upstream
RandomX/SHA-256d primitives verified against fixed SOV compatibility vectors,
writes the trailing little-endian eight-byte nonce at the job-provided offset,
checks the full 256-bit SOV share target, and submits the resulting nonce and
seal to the bridge.

## Native GUI

Build and open the operator console:

```sh
git clone https://github.com/cloudzombie/xus-miner.git
cd xus-miner
cargo build --locked --release
./target/release/xus-miner
```

On 64-bit Windows, install Rust, CMake, and the Visual Studio C++ Build Tools,
then run `cargo build --locked --release`; the executable is
`target\release\xus-miner.exe`. Native macOS and Windows builds are exercised by
CI.

The GUI provides:

- visible connection, authentication, reconnecting, mining, and failure states;
- start/stop control with an isolated miner child process;
- live hashrate history, height, uptime, job, algorithm, and share health;
- a unified low-overhead PoW flight recorder with smoothed/raw hashrate,
  block-height markers, current-round probability, mempool activity cells,
  measured network hashrate, and consensus target cadence;
- an authenticated SOV node-peers chip and a permanent activity-cell legend:
  bright cyan is stronger sampled hashrate, dim cyan is lower sampled hashrate,
  and green means transactions were waiting in the mempool for that sample;
- next-block estimates calculated from the template's inclusive 256-bit target
  and the miner's smoothed local hashrate (both expected/mean and 50% window);
- an unmistakable green, pulsing `BLOCK FOUND` confirmation for 30 seconds when
  the direct SOV node accepts this miner's exact sealed block;
- bounded engine logs, an explicit shared-dataset RandomX memory estimate, and a
  RAM-only operating-system scan every ten seconds;
- honest worker-readiness telemetry that distinguishes the shared fast dataset
  from the lower-memory fallback, while keeping a live node link visibly
  connected if one worker enters a degraded state;
- selectable engine logs, copied diagnostics, and crash reports that scrub the
  password and endpoint userinfo; embedded endpoint credentials are rejected;
- a template-confirmed Coinbase field in Active Work, diagnostics, and the
  startup engine log;
- validated pool, worker, thread, reconnect, and telemetry settings;
- automatic start preflight against live available RAM, with a conservative
  OS/application reserve and a non-persistent manual fallback only when the
  native memory probe is unavailable;
- settings persistence without ever saving the Stratum password.

The GUI passes the password to its child engine over standard input instead of
exposing it in process arguments. Closing the window requests child termination
and release of its RandomX memory. If the operating system cannot prove that
termination completed, the GUI keeps the child quarantined and restart disabled
instead of risking a duplicate engine.

Mining is probabilistic: the displayed solo block ETA is `expected hashes ÷
smoothed local hashes/second`, not a guaranteed countdown. The round meter uses
`1 − exp(−Σ(hashes tried ÷ expected hashes at that target))` and resets only
when chain height advances; a routine template refresh at the same height does
not fake a reset or retroactively reprice earlier work.
For a Stratum pool, the ordinary job target is a variable-difficulty share
target, not the consensus block target, so block ETA remains unavailable unless
the bridge explicitly supplies `network_target_full`/`network_target`.

The GUI makes reward routing explicit:

- **My bridge / my coinbase** means the connected `sov-stratum` process must
  request templates using a public account controlled by your SOV Station
  wallet. The wallet retains the spending keys; neither the bridge nor miner
  needs them.
- **External pool / contributor** does not imply payment. The current bridge
  logs per-session shares but has no automatic payout implementation, and a
  Stratum worker label is not a payout address.

Direct mining requires an updated node supporting `sov_getBlockTemplate` and
`sov_submitBlock`. In direct mode the User field is sent as the public coinbase
account. XUS Miner validates the SOV 0.1.99 compact target commitment and only
counts a direct block as accepted when the node confirms `accepted: true` with
the matching height and locally computed sealed-block hash. A Stratum bridge
remains supported for pool operation.

## Headless/server mode

Passing `--headless` preserves the terminal and cloud-server workflow:

```sh
./target/release/xus-miner \
  --headless \
  --pool 127.0.0.1:3333 \
  --user <public-account-or-worker-label> \
  --workers 1
```

Start the existing bridge against a synchronized SOV node before starting the
miner:

```sh
./target/release/sov-stratum \
  --node 127.0.0.1:8645 \
  --bind 0.0.0.0:3333 \
  --coinbase <coinbase-account>
```

Use `--help` for all options. `tcp://` and `stratum+tcp://` pool prefixes are
accepted. Operators that automate the miner can use `--json-events` for
newline-delimited structured telemetry and `--password-stdin` to keep a
Stratum password out of the process list. The engine performs its own RAM
preflight immediately before accepting its first RandomX job, including when it
is launched without the GUI. `--confirm-randomx-memory` is accepted only as a
non-persistent fallback when the operating-system scan is unavailable; it
cannot override a valid low-memory reading.

## Current constraints

- The connection is plaintext TCP. Use a trusted network or a secured tunnel.
- Workers use independent, allocation-checked RandomX VMs backed by one shared
  read-only dataset (approximately 2.3 GiB total plus a conservative 32 MiB
  allowance per worker). The default is one worker. Neither the GUI nor
  headless engine silently changes the chosen worker count; RandomX
  initialization is blocked when the live RAM preflight cannot preserve the
  larger of 1.5 GiB or 10% of installed RAM (capped at 4 GiB).
- The current bridge records shares but has no PPLNS payout implementation. Its
  configured coinbase account receives the block. A public operator must not
  promise automatic or trustless payouts until that layer exists.
- This is a correctness-first reference miner, not yet an XMRig-performance
  replacement.

## Verification

```sh
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --bin xus-miner
cargo test --locked --test miner_protocol
cargo test --locked --test rpc_0199_protocol
python3 scripts/check_chaincode_boundary.py
python3 scripts/check_version.py
cargo audit --deny warnings
cargo build --locked --release
```

Application versions and GitHub releases follow the non-negotiable contract in
[RELEASING.md](RELEASING.md): application `X.Y.Z` is released only as immutable
tag `vX.Y.Z`, and CI rejects any mismatch before publishing.

The integration test launches the real headless engine used by the GUI against
a mock Stratum server. It verifies password delivery over stdin, the structured
GUI telemetry contract, login, full SOV-format job validation, a share whose
hash is independently recomputed from its nonce, one-in-flight-share
backpressure, pushed job replacement, and automatic reconnect.

A second integration test launches that same engine against a bounded local
SOV 0.1.99 JSON-RPC mock. It verifies the direct block-template, peer telemetry,
coinbase request, compact target, trailing nonce, accepted sealed-block hash,
and submission acknowledgement contracts without allocating RandomX memory.
