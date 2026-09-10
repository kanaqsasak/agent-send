# Contributing

Read [PLAN.md](PLAN.md) and [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) before making changes.

## Local checks

Install stable Rust and run:

```sh
cargo fmt --all -- --check
cargo test --workspace
```

The Tauri desktop shell is not part of the current workspace yet. When it is
added, contributors will also need Node.js and the platform prerequisites listed
in the Tauri documentation.

## Change boundaries

Keep the daemon, transport, filesystem policy, desktop shell, CLI, and agent API
as separate layers. Add tests with behavior changes and update `PLAN.md` only
when a milestone exit criterion is met. Do not claim a platform is supported
without testing it or marking it unverified.

## Security expectations

Treat peer discovery as untrusted metadata until pairing succeeds. Never expose
arbitrary filesystem paths to an agent. Shared folders are explicit capabilities;
path resolution must reject traversal, absolute escapes, symlink escapes, and
writes outside the configured root. Do not add an inbound internet listener.

When reporting a transfer, include enough context to audit the actor, peer,
capability, result, and timestamp without logging file contents or secrets.
Audit metadata must redact request-controlled identifiers and normalize unknown
operations rather than preserve arbitrary input. Review [the threat model](docs/THREAT_MODEL.md)
and [release readiness](docs/RELEASE_READINESS.md) when changing a security or
release claim.

## Pull requests

Keep pull requests focused on one plan milestone or bounded foundation. Explain
security and cross-platform implications, include the validation commands run,
and call out anything that remains unverified.
