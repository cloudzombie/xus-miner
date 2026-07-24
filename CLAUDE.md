# Claude project notes

Before working in this repository, read these root documents in order:

1. `AGENTS.md` — mandatory repository and chaincode boundary.
2. `BUILDING.md` — toolchains, platform packages, commands, and memory costs.
3. `SOV_COMPATIBILITY.md` — the exact network contract implemented independently
   by the miner; no SOV crate or source tree is consumed.
4. `RELEASING.md` — mandatory single-source version and immutable-tag procedure.

This is the standalone XUS Miner repository. Do not edit or invoke write tools
against `cloudzombie/sov`, a local SOV checkout, chain state, or wallet data from
this project. If compatibility work requires a node or consensus change, stop
and request a separately authorized task in the SOV repository.

Never add a SOV Git, path, submodule, or workspace dependency. Run
`python3 scripts/check_chaincode_boundary.py` before and after dependency
changes, then execute the complete validation sequence in `AGENTS.md`.

Do not add a second unsafe Rust surface. The only exception to the crate-wide
unsafe-code denial is `src/randomx_native.rs`, whose null checks, ownership
lifetimes, and synchronous full-dataset initialization are part of the Windows
crash-safety contract.

macOS builds must not link `sysinfo` or call `host_statistics64` for RAM
telemetry. Keep the bounded, deadline-limited absolute-path
`/usr/bin/memory_pressure -Q` probe, background periodic refresh, and the CI
dependency-tree guard intact.

Do not weaken the process-supervision contract: failed cleanup quarantines the
exact child, operator Stop suppresses every queued recovery, automatic recovery
is capped at five attempts, and Windows native execution faults use the tested
one-worker portable light-memory RandomX child backend without changing the
saved preference or normal optimized backend. Unknown failures receive one
one-worker optimized retry before that light tier. Preserve the flushed
GUI-parent pipe watchdog so an unexpectedly terminated GUI cannot leave its
engine mining invisibly.
