# Building XUS Miner

## Common requirements

- Git
- Rust `1.97.0` through rustup (pinned by `rust-toolchain.toml`)
- CMake
- A native C/C++ compiler toolchain for the bundled RandomX implementation
- Internet access during the first dependency fetch; third-party crates are
  checksummed and frozen by exact direct versions plus `Cargo.lock`
- Approximately 4 GiB free disk space for a clean debug plus release build
- Approximately 2.3 GiB available RAM for one shared full RandomX dataset, plus
  a conservative 32 MiB allowance per worker and the miner's OS/application
  reserve (at least 1.5 GiB)
- Maintainer validation only: Python 3.11+ and Ruby with its standard Psych YAML
  parser. Neither is linked into or required to run the miner binary.

The miner has no SOV source dependency, does not require a SOV checkout, and
must not be built as part of one.

The exact `randomx-rs` crate supplies the checksummed native RandomX library,
but XUS Miner does not use its unchecked VM constructor. All native ownership is
confined to `src/randomx_native.rs`: cache, dataset, and VM pointers are
null-checked, backing memory outlives every VM, output is written only into a
mutable 32-byte buffer, and dataset initialization completes synchronously
before the dataset can be shared. Rust unsafe code remains denied everywhere
else in the application.

At runtime the GUI refreshes only the operating system's RAM counters every ten
seconds and immediately before Start. The isolated/headless engine checks the
same counters again immediately before its first RandomX job. Windows and Linux
use the RAM-only `sysinfo` feature. macOS excludes `sysinfo` from the compiled
dependency tree and parses bounded output from Apple's absolute-path
`/usr/bin/memory_pressure -Q` utility with an empty environment; this avoids
placing `vm_statistics64` SDK/runtime ABI changes inside the miner process. The
helper has fixed-size stdout/stderr capture and a 500 ms deadline; periodic GUI
refreshes run in a background probe.
Neither path inventories processes, CPU, disks, network interfaces, users, or
swap, and neither performs GPU work.

For the strongest isolation, use an attested GitHub release built on an
ephemeral hosted runner where SOV is absent. A local Cargo build executes locked
third-party build scripts with your account's permissions; use a separate
unprivileged build account/container if you require an OS-enforced boundary.

## macOS

Install Xcode Command Line Tools and CMake:

```sh
xcode-select --install
brew install cmake
```

Build and launch:

```sh
cargo build --locked --release
./target/release/xus-miner
```

The primary GitHub macOS archive is `macos-apple-silicon-arm64`. Release CI
asserts a native arm64 runner, validates the full shared RandomX dataset, and
soaks two simultaneous direct-RPC workers. This is the archive intended for M3,
M4, and later Apple Silicon systems; it does not require Rosetta.

## Windows 10/11 (64-bit)

Install:

1. Git for Windows.
2. rustup using the `x86_64-pc-windows-msvc` toolchain.
3. CMake, available on `PATH`.
4. Visual Studio 2022 Build Tools with **Desktop development with C++**, the
   Windows SDK, and MSVC x64 tools selected.

From a Developer PowerShell:

```powershell
cargo build --locked --release
.\target\release\xus-miner.exe
```

The Windows release gate runs separate 90-second steady-hashing soaks for the
two-worker optimized backend, the one-worker optimized recovery backend, and the
one-worker portable light-memory backend. At runtime the GUI supervises the
isolated engine with at most five restart attempts. Every Windows recovery child
uses one effective worker without overwriting the saved worker preference. An
unclassified exit gets one optimized retry. A repeated exit, recognized native
execution fault, or native memory/commit-exhaustion status selects
`--randomx-light`, which budgets a 256 MiB cache and does not allocate the full
dataset. Ordinary launches retain CPU-specific acceleration and the selected
worker count.
Operator Stop always cancels the recovery latch. GUI-spawned Windows engines run
below normal process priority so the operator console stays responsive under
sustained hashing load. Their flushed parent-pipe heartbeat exits the child if
the GUI or telemetry reader disappears, preventing an orphan engine.

## Linux (X11)

On Debian/Ubuntu, install the usual native GUI and RandomX build dependencies:

```sh
sudo apt-get update
sudo apt-get install -y build-essential cmake pkg-config libx11-dev libxi-dev libgl1-mesa-dev
```

Then build with `cargo build --locked --release`.

## Required validation

```sh
python3 scripts/check_chaincode_boundary.py
python3 scripts/check_version.py
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --bin xus-miner
cargo test --locked --test miner_protocol
cargo test --locked --test rpc_0199_protocol
cargo audit --deny warnings
cargo build --locked --release
```

The ignored stability soak is exercised by CI in optimized mode on Windows and
Apple Silicon and again in both one-worker recovery configurations on Windows:

```sh
cargo test --locked --release --test rpc_0199_protocol \
  headless_randomx_direct_rpc_engine_stays_alive_for_stability_window \
  -- --ignored --test-threads=1
XUS_TEST_RANDOMX_ONE_WORKER=1 cargo test --locked --release \
  --test rpc_0199_protocol \
  headless_randomx_direct_rpc_engine_stays_alive_for_stability_window \
  -- --ignored --test-threads=1
XUS_TEST_RANDOMX_LIGHT=1 cargo test --locked --release \
  --test rpc_0199_protocol \
  headless_randomx_direct_rpc_engine_stays_alive_for_stability_window \
  -- --ignored --test-threads=1
```

The full-dataset RandomX equivalence test is ignored by an ordinary `cargo test`
because it allocates roughly 2.3 GiB; CI and Release exercise it explicitly:

```sh
cargo test --locked --release randomx_mainnet_job_uses_the_exact_sov_mining_seal \
  -- --ignored --nocapture --test-threads=1
```

## Dependency updates

Do not use `cargo update` casually. Preserve `Cargo.lock`, review every changed
package, rerun the complete validation sequence, and confirm native CI on Linux,
Apple Silicon macOS, Intel macOS, and Windows. There is no SOV revision or SOV
source dependency to update.
SOV compatibility changes update client protocol code and fixed fixtures here;
they never patch or import the SOV repository.
