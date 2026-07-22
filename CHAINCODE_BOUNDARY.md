# Chaincode boundary

XUS Miner is an external client, not a component of the SOV source tree.

The repository enforces that separation in four ways:

1. It is a distinct Git repository and is not a SOV Cargo workspace member.
2. It has no SOV source dependency: no Git dependency, local path, symlink,
   submodule, or workspace relationship. Compatibility is checked with fixed
   wire/cryptographic vectors captured from the public node protocol.
3. GitHub Actions receives `contents: read`, disables persisted checkout
   credentials, and therefore has no repository token capable of writing SOV.
4. The running miner communicates with a node only through JSON-RPC or Stratum.
   It writes its own GUI preferences under the user's profile and never opens a
   chain source or node-data path.

The boundary check in `scripts/check_chaincode_boundary.py` rejects local SOV
paths, floating SOV revisions, repository symlinks, submodules, and write-level
workflow permissions.

Repository separation is not an operating-system sandbox: any deliberately
malicious program run under a user account inherits that account's filesystem
permissions. For a machine-level guarantee, run the released miner under a
separate unprivileged OS account or application sandbox that cannot write the
SOV checkout. The maintained miner itself contains no such write path.
