# Threat model

This document records the current M6 hardening claims and their limits. It does
not expand the local-first trust model: the service has no cloud account,
relay, or inbound internet API.

## Assets and boundaries

- Folder capabilities, their contents, and destination paths remain local
  authority. Agents receive named capabilities, never arbitrary paths.
- Pairing secrets and local bearer tokens are authentication material. Discovery
  records and manual addresses are only untrusted endpoint hints.
- The loopback automation API is separate from the paired-peer listener. The
  paired-peer listener accepts only identities with persisted confirmed trust.
- Audit records are metadata, not an authorization source. Dynamic actor, peer,
  capability, and transfer identifiers are stored as deterministic redacted
  digests; unknown operation/result values are normalized rather than logged.

## Controls verified by automated tests

“Verified” here means covered by deterministic Rust workspace tests, not an
external security assessment.

- Relative path validation rejects traversal, absolute paths, NUL-containing
  paths, and symlink escapes; its fixed generated component matrix exercises
  traversal combinations.
- Peer frames are authenticated and encrypted before transfer policy sees their
  plaintext. Deterministic ciphertext mutations and malformed length-delimited
  frame corpus tests fail closed.
- Each encrypted socket frame is capped at 64 KiB of file payload. Transfers
  reject declared totals above 4 GiB, more than 10,000 files/entries, a file
  larger than its remaining manifest total, non-monotonic offsets, and hash or
  byte-count mismatches.
- Outgoing and accepted peer sockets set a two-second connect or per-I/O
  timeout. The timeout configuration is unit-tested; it is not a promise about
  end-to-end delivery latency.
- Audit tests verify that request-controlled identifiers and unknown
  operations/results do not appear verbatim in serialized audit entries.

## Important unverified claims and residual risks

- This is not fuzzing or a cryptographic review. The deterministic malformed
  corpus and path matrix do not prove parser, filesystem, or protocol safety
  for all inputs.
- Limits and timeouts reduce single-transfer resource exhaustion but do not
  provide global rate limiting, disk quotas, scheduler fairness, or protection
  from a trusted peer repeatedly reconnecting.
- Path checks mitigate traversal and known symlink escapes, but filesystem
  time-of-check/time-of-use behavior and platform-specific link semantics need
  dedicated review.
- Pairing secrets and token hashes are currently stored by local-file adapters;
  the daemon does not encrypt pairing secrets at rest or use an OS credential
  store.
- mDNS, firewall, sleep/wake, network-interface changes, and cross-platform
  behavior have not been verified on every target OS. Discovery never grants
  trust, even when these components fail.
- No signed release, update mechanism, SBOM, vulnerability review, accessibility
  review, or localization verification is implemented by this pass.
