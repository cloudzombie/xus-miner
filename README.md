# xus-miner

`xus-miner` is a standalone native desktop and headless CPU miner for SOV/XUS.
It connects directly to a SOV Station/node JSON-RPC endpoint (normally port
`8645`) or, optionally, to a `sov-stratum` pool/bridge. It does not run a node
or decide consensus. Direct solo mining does not use a wallet seed. Pool mode
can hold a payout seed locally so this miner signs the transfers for a block
it finds; that seed is never sent to the pool and never written to logs.

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

The primary macOS release archive is named
`macos-apple-silicon-arm64` and is built natively for Apple Silicon. Its release
gate runs the full RandomX vector and a two-worker direct-RPC soak on an arm64
runner. On macOS, RAM telemetry uses Apple's fixed
`/usr/bin/memory_pressure -Q` utility; `sysinfo` is excluded from the compiled
dependency tree so SDK/runtime changes to `vm_statistics64` cannot corrupt the
GUI process. Its output capture is fixed-size, the helper has a 500 ms deadline,
and periodic probes run off the GUI thread.

On Windows, the RandomX engine remains isolated from the GUI. If that engine
exits unexpectedly, the GUI retains the diagnostic tail and attempts up to five
supervised restarts with 2–32 second backoff. A recovery child uses one effective
worker without changing the saved worker setting. An unclassified exit gets one
optimized one-worker retry, matching the conservative configuration used to
stabilize affected systems. A repeated exit, recognized native execution fault,
or native memory/commit-exhaustion status goes directly to portable light mode
without a full-dataset allocation. The normal path stays optimized. A missing
runtime DLL is terminal instead of being retried. Stop cancels every queued
recovery, including an exit/drain race.

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
  RAM-only operating-system scan every ten seconds (Apple's safe external
  memory-pressure probe on macOS);
- honest worker-readiness telemetry that distinguishes the shared fast dataset
  from the lower-memory fallback, while keeping a live node link visibly
  connected if one worker enters a degraded state;
- selectable engine logs, copied diagnostics, and crash reports that scrub the
  password and endpoint userinfo; embedded endpoint credentials are rejected;
- bounded automatic engine recovery with logged one-worker optimized and
  portable light-memory Windows tiers;
- a template-confirmed Coinbase field in Active Work, diagnostics, and the
  startup engine log;
- validated pool, worker, thread, reconnect, and telemetry settings;
- automatic start preflight against live available RAM, with a conservative
  OS/application reserve and a non-persistent manual fallback only when the
  native memory probe is unavailable;
- settings persistence without ever saving the Stratum password or a payout seed
  (a seed-file path may be saved; the seed bytes are not);

The GUI passes the password to its child engine over standard input instead of
exposing it in process arguments. Closing the window requests child termination
and release of its RandomX memory. If the operating system cannot prove that
termination completed, the GUI keeps the child quarantined and restart disabled
instead of risking a duplicate engine. A flushed parent-pipe heartbeat also
forces a GUI-launched engine to exit if the GUI process or its telemetry reader
vanishes unexpectedly, preventing an invisible orphan miner.

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
- **External pool / contributor** pays through miner-signed transfers. Log in
  as your payout account (`account` or `account/worker`). When the pool pushes
  a `payout` notice, the miner signs those transfers locally and sends
  `payout_txs` before a block is sealed, because the header already commits to
  the transaction root. The pool checks the signatures and relays them. It
  never receives the seed. A worker label alone is not a payout address. An
  empty share window needs no transfers; the finder keeps the coinbase.

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
  --user <payout-account-or-account/worker> \
  --seed-file ~/.xus-miner/payout.seed \
  --workers 1
```

`--seed-file` is a mode-`0600` file containing the 32-byte Ed25519 seed as 64
hex characters. `--seed-hex` accepts the same seed on the command line; the
process list can show it, so prefer the file. With either flag and the default
user `xus-miner`, the Stratum login is the seed's implicit account. An explicit
implicit account must be that same id. The seed is used only to sign `payout`
transfers. It is not written to the socket, the diagnostic log, or stderr.

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
`--randomx-light` disables CPU-specific RandomX acceleration and avoids the full
dataset. It is the slower recovery tier used after a failed one-worker optimized
retry or immediately for recognized Windows native execution and
memory/commit-exhaustion statuses.

### Crash diagnostics

Both the GUI and the isolated engine append timestamped diagnostic logs to
`~/.xus-miner/logs/` (`%USERPROFILE%\.xus-miner\logs\` on Windows); the active
file path is printed on stderr at startup and the ten newest files are kept.
Every line names the miner version, operating system, and architecture, the
startup header records detected CPU features, and a panic writes its full
backtrace to the file before the process exits. Set `XUS_MINER_LOG=debug` for
verbose engine-lifecycle detail when reporting a problem; the default level is
`info`. Credentials are never written: the Stratum password stays on stdin, a payout
seed is scrubbed if it would otherwise reach a diagnostic line, and GUI log
lines are redacted before they are mirrored. The GUI can remember a seed-file
path. It does not store the seed.

## Current constraints

- The connection is plaintext TCP. Use a trusted network or a secured tunnel.
- Workers use independent, allocation-checked RandomX VMs backed by one shared
  read-only dataset (approximately 2.3 GiB total plus a conservative 32 MiB
  allowance per worker). The default is one worker. Normal GUI launches and
  manual headless launches keep the selected worker count. Supervised Windows
  recovery explicitly and visibly uses one effective worker while preserving
  the saved preference; its light tier budgets a 256 MiB cache instead of the
  full dataset. RandomX initialization is blocked when the live RAM preflight
  cannot preserve the larger of 1.5 GiB or 10% of installed RAM (capped at
  4 GiB).
- Pool payouts follow the non-custodial sharechain in
  [SOV PR #55](https://github.com/cloudzombie/sov/pull/55). The finder signs
  ordinary transfers for the rest of the PPLNS window. The pool does not hold
  a key. A block is payable only when those transfers are already in the
  template, so the miner signs a `payout` notice as soon as it arrives.
- This is a correctness-first reference miner, not yet an XMRig-performance
  replacement.
- No application can honestly guarantee that an operating system, native
  dependency, or failing hardware will never terminate a process. Windows
  physical-RAM preflight also cannot prove system-wide commit availability.
  XUS Miner therefore fails closed on low physical RAM, records Windows native
  status codes (including commit exhaustion), isolates the engine, falls back
  to low-memory mode when those statuses are observed, and bounds recovery
  attempts instead of entering an infinite restart loop.

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
RPC regressions also exercise trickled headers/bodies against an absolute call
deadline, complete replies on sockets that stay open, response size limits,
HTTP status and JSON-RPC ID validation, and real-engine reconnect after an HTTP
failure. The deadline covers socket operations; operating-system hostname
resolution remains blocking.
