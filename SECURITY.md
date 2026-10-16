# Security policy

The supported code is the latest commit on `main`. Direct solo mining needs no
wallet seed. Pool payout signing can use a miner-local seed (`--seed-hex` or
`--seed-file`); that seed is not sent to the pool and is not written to logs.
XUS Miner never needs a node private key, SOV repository token, node database,
or writable SOV checkout. Do not include a payout seed when reporting an issue.

Report suspected vulnerabilities through GitHub's private vulnerability
reporting for `cloudzombie/xus-miner`. Include the platform, miner version,
connection mode, and minimal reproduction. Remove public account identifiers,
passwords, LAN addresses, and logs unrelated to the issue.

Security-sensitive changes include PoW sealing, header wire parsing, target
comparison, nonce placement, submission, credential handling, dependency pins,
and GitHub workflows. They must pass the chaincode-boundary check, RustSec audit,
native platform matrix, protocol integration test, and release build before
merge.

Release tags are immutable and must exactly equal `v` plus the application
version embedded from `Cargo.toml`. Release binaries are rebuilt on hosted
Linux, macOS, and Windows runners, checksummed, and accompanied by GitHub build
provenance. See `RELEASING.md` for the mandatory procedure.

The miner communicates over plaintext HTTP/TCP today. Use it only on a trusted
LAN or through an authenticated encrypted tunnel; do not expose its RPC or
Stratum connection directly to the public Internet.
