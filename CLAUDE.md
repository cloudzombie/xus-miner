# Claude project notes

Before working in this repository, read these root documents in order:

1. `AGENTS.md` — mandatory repository and chaincode boundary.
2. `BUILDING.md` — toolchains, platform packages, commands, and memory costs.
3. `SOV_COMPATIBILITY.md` — the exact SOV crates and network contracts consumed
   by the miner.

This is the standalone XUS Miner repository. Do not edit or invoke write tools
against `cloudzombie/sov`, a local SOV checkout, chain state, or wallet data from
this project. If compatibility work requires a node or consensus change, stop
and request a separately authorized task in the SOV repository.

Never add a SOV Git, path, submodule, or workspace dependency. Run
`python3 scripts/check_chaincode_boundary.py` before and after dependency
changes, then execute the complete validation sequence in `AGENTS.md`.
