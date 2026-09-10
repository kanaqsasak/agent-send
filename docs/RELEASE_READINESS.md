# Release readiness

**Current status: not release-ready.** This bounded M6 pass hardens the existing
local transfer implementation; it does not claim a production desktop release.

## Verified in automated workspace tests

| Area | Claim |
| --- | --- |
| Trust boundary | Local automation binds to loopback; discovery/manual addresses remain untrusted until confirmed pairing. |
| Transfer input bounds | 64 KiB chunks, a 4 GiB declared-transfer limit, and a 10,000 file/entry limit are rejected deterministically. |
| Failure handling | Peer socket connection/I/O timeouts are configured to two seconds; malformed frame corpus and ciphertext-mutation tests fail closed. |
| Filesystem policy | Deterministic path matrix and symlink tests cover relative-path traversal and escape rejection. |
| Audit privacy | Dynamic audit identifiers are redacted and unknown operations/results are normalized. |
| Local measurement | `cargo run -p agent-send-daemon --example measure` reports loopback throughput and a 250 ms idle wall-clock lifecycle measurement without LAN traffic. |

The measurement harness reports one local run. It does **not** measure idle CPU,
memory, battery use, startup distribution, or cross-host throughput; no baseline
or release performance threshold has been verified.

## Explicitly unverified or incomplete

- Signed installers, reproducible builds, provenance, SBOMs, and an update
  strategy are not implemented or verified.
- macOS, Windows, and Linux packaging, code signing, firewall prompts,
  accessibility, localization, sleep/wake, and network transitions have not
  been verified by this repository's tests.
- There is no external security review, fuzzing campaign, penetration test, or
  interoperability validation with LocalSend/QUIC/HTTPS peers.
- The local-file pairing-secret store is not an OS credential store and does
  not encrypt secrets at rest.
- Transfer limits do not replace global rate limiting, quota management, or
  operational monitoring.

## Release gate before a production claim

1. Run the workspace formatter, tests, and diff check on the release commit.
2. Establish recorded performance baselines on each supported platform,
   including idle CPU/memory and LAN throughput under normal and lossy network
   conditions.
3. Complete independent security review and targeted fuzzing of frame and path
   parsing; resolve findings.
4. Implement and verify signing, update, rollback, provenance/SBOM, and secure
   platform credential storage.
5. Validate installers, firewall behavior, accessibility, localization, and
   lifecycle behavior on macOS, Windows, and Linux.

These gates preserve the existing local-first design: they do not require cloud
services, relays, or a broader network listener.
