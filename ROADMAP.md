# Switchyard roadmap

Switchyard furnishes the box. It gives every parallel agent its own branch, port range, database, and env file with one command, and it stands at 100 of 100 closed rounds, with claim and list and release landed in Round 1 over one registry file, the Rust port and Python binding landed in Round 2, intent routing landed in Round 3, the scored wait queue landed in Round 4, furnishing failure traces landed in Round 5, and the volume and routed-wait measurement batteries completed through Round 8.

## Post-100 release arc for v1.0.0

Five post-100 engineering slices carry switchyard from its Round 100 baseline to the public v1.0.0 release while preserving every landed registry, routing log, and queue log contract.

Slice SY-S1 establishes multi-platform gate parity on Linux and macOS, enforces the empty waits rejection of obligation SY-18, bounds git pipe drains against orphaned grandchildren, adds sync_all crash durability to registry publication, migrates file locking to the Rust standard library File.lock primitive, and adds repeatable --judge-arg support with raw operating system path preservation.

Slice SY-S2 adds offline local repository attachment through init --from with detached HEAD worktree fanout and switchyard-config.json, non-destructive release workflows through --keep-branch and --safe, and stack health inspection and recovery through doctor and doctor --fix.

Slice SY-S3 adds IPv4 and IPv6 loopback host port probing through --probe-ports across both direct claims and release offers, enriched .env.switchyard convenience variables with protected --env-template overrides, and symlink-safe .worktreeinclude seed file copying verified by SHA-256 digest.

Slice SY-S4 adds quarantine-safe expiring claim leases through --ttl and renew that move dirty expired worktrees into expired/ with zero data loss and record events in switchyard-claims.jsonl, async-signal-safe blocking queue waits through --wait --block, structured --json output and agent status inspection, and an offline-testable reference judge script.

Slice SY-S5 isolates codepoint_scan into an unpublished workspace crate, finalises the switchyard-ai crates.io and PyPI packaging with root pyproject.toml and strict Python type stubs, and ships the v1.0.0 release workflow.

## Decision models and the offline transport boundary

Judgment enters switchyard at exactly two edges, and nowhere else.

An intent level request is the first edge. A plain or profiled claim is fully specified by its caller. A routed claim lets an agent describe the job in prose instead, and a decision model maps that description to one of the three declared allocation profiles, lean, standard, or roomy. The model returns a choice with per label probabilities, and the deterministic layer validates the reply and allocates under the settled profile or falls back to the caller's declared default profile.

Contention is the second edge. When the port pool cannot furnish a draw, the wait queue holds the waiting claim, and released ranges become the scarce resource. The model scores each waiting request once on arrival, and the deterministic offer rule orders scored waits ahead of scoreless waits by score descending and arrival order. A score is advice to an ordering rule, never an allocation.

Switchyard performs no network access and holds no API credentials. Every consultation crosses the offline JudgeTransport boundary, driven on the command line by an operator-supplied --judge-command subprocess with optional --judge-arg arguments and in Python by a caller-supplied transport callable. The pinned request and reply grammar targets calibrated System One classification under jev-1.13.0. An earlier roadmap note named an OpenAI decision model comparison route waiting on program-level wiring and a live call budget, and that unwired comparison route is retired in favour of the offline subprocess and callable transport boundary, where any external or local model adapter stands outside switchyard behind JudgeTransport.

What stays deterministic is everything that makes switchyard trustworthy. The registry and its validation, the file lock and its inode check, the port arithmetic, the git worktree lifecycle, and every refusal never pass through a model. Every model-influenced decision is recorded in switchyard-routing.jsonl or switchyard-queue.jsonl with the model name, the chosen label probability, and the deterministic fallback taken when the consultation abstained, fell below threshold, returned malformed output, or failed at the transport seam.

Tests stay completely offline. Recorded synthetic request and response fixtures live in the repository under tests/fixtures and replay deterministically through FixtureTransport, and no test in the workspace or continuous integration ever performs a network call.
