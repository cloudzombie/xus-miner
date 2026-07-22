# Building XUS Miner

## Common requirements

- Git
- Rust `1.97.0` through rustup (pinned by `rust-toolchain.toml`)
- CMake
- A native C/C++ compiler toolchain for the bundled RandomX implementation
- Internet access during the first dependency fetch; third-party crates are
  checksummed and frozen by exact direct versions plus `Cargo.lock`
- Approximately 4 GiB free disk space for a clean debug plus release build
- At least 3 GiB free RAM per full-dataset RandomX worker
- Maintainer validation only: Python 3.11+ and Ruby with its standard Psych YAML
  parser. Neither is linked into or required to run the miner binary.

The miner has no SOV source dependency, does not require a SOV checkout, and
must not be built as part of one.

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
cargo audit --deny warnings
cargo build --locked --release
```

The full-dataset RandomX equivalence test is intentionally ignored in ordinary
CI because it allocates roughly 2.3 GiB:

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
