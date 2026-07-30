# Security Policy

## Reporting a vulnerability

**Do not open a public issue for a security vulnerability.** Public issues,
pull requests, and discussions disclose the problem before a fix exists — they
are the wrong channel.

Report it privately by email to **licensing@tuno.bar**. Include the affected
version, a description of the issue, and, where possible, steps to reproduce it.

## What to expect

We will acknowledge receipt of your report and tell you how we intend to handle
it. We do not promise a fixed resolution timeline, but we treat security
reports seriously and will keep you informed as we work through one.

## Supported versions

Security fixes are provided for the **1.0.x** release line.

## Threat model

xyzDB assumes a **trusted network**. As of 1.0 the server binds to loopback
(`127.0.0.1`) by default, and binding a non-loopback address with no
`--auth-token` is refused at startup. When a token is configured,
authentication applies to every request except the `/health` and `/ready`
liveness probes.

Within that trusted-network posture, a few operations are intentionally
unbounded and can exhaust resources: `FIND` and `PULL` issued without a `LIMIT`
materialize their entire result set (unlike `SCAN`, capped at
`SCAN_LIMIT_DEFAULT = 1000`, and `NEAREST`, bounded by `--nearest-budget-ms`),
so one query over a large lobe can drive a large memory/CPU spike. This is a
resource-exhaustion **design limit under the trusted-network model** — not an
authentication or memory-safety defect: with `--auth-token` set these verbs are
authenticated, and a default row cap plus a per-query budget are tracked for a
1.0.x release. If you are about to report this, it is already understood; the
operator mitigations are in `OPERATIONS.md`.

Operating xyzDB is the operator's responsibility: backups and their
verification, encryption at rest, and the access layer in front of the server
are outside the engine's scope.
