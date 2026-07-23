# Chaincode boundary

XUS Miner is an external client, not a component of the SOV source tree.

The repository enforces that separation in five ways:

1. It is a distinct Git repository and is not a SOV Cargo workspace member.
2. It has no SOV source dependency: no Git dependency, local path, symlink,
   submodule, or workspace relationship. Compatibility is checked with fixed
   wire/cryptographic vectors captured from the public node protocol.
3. Every source checkout and build job receives `contents: read` and disables
   persisted checkout credentials. Only the final tag-release publishing job
   receives `contents: write`, scoped by GitHub to `cloudzombie/xus-miner`; it
   has no source checkout and cannot write the SOV repository.
4. The running miner communicates with a node only through JSON-RPC or Stratum.
   It writes its own GUI preferences under the user's profile and never opens a
   chain source or node-data path.
5. Rust unsafe code is denied by default and mechanically confined to one small
   RandomX C-API ownership module. The boundary check rejects a second unsafe
   Rust surface.

The boundary check in `scripts/check_chaincode_boundary.py` rejects local SOV
paths, floating SOV revisions, repository symlinks, submodules, and write-level
workflow permissions.

Repository separation is not an operating-system sandbox: any deliberately
malicious program run under a user account inherits that account's filesystem
permissions. For a machine-level guarantee, run the released miner under a
separate unprivileged OS account or application sandbox that cannot write the
SOV checkout. Official release binaries are compiled on ephemeral GitHub-hosted
runners where no SOV checkout exists; local Cargo builds do not provide that
machine boundary because third-party build scripts run with the invoking user's
permissions. The maintained miner itself contains no chaincode write path.
