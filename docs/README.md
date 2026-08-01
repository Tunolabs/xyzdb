# xyzDB — Documentation

Entry point for the in-tree, versioned documentation. Project-facing docs live here; private planning and session-handoff notes live in a workspace folder outside the repo.

## Start here

- **New to the project?** Read [`usage/quickstart.md`](usage/quickstart.md) first — 5 minutes, server up and three queries run. Then [`architecture.md`](architecture.md) for how it works.
- **Operating xyzDB?** [`usage/reference.md`](usage/reference.md) is the operator manual (CLI flags, durability, throttle profiles, tuning); the root [`../OPERATIONS.md`](../OPERATIONS.md) is the full runbook.
- **Writing queries?** [`xytalk-spec.md`](xytalk-spec.md) is the complete language specification.
- **Connecting an agent?** [`mcp-integration.md`](mcp-integration.md) covers the MCP server.
- **Building a client?** [`../PROTOCOL.md`](../PROTOCOL.md) is the wire-protocol specification — implement it in any language, under any license.

## Layout

```
docs/
├── README.md             ← you are here
│
├── architecture.md       ← how the engine is structured
├── wal-state-machine.md  ← WAL durability state machine (invariant D1)
├── xytalk-spec.md        ← query language specification
├── mcp-integration.md    ← MCP server: tools, resources, modes
├── benchmark-native.md   ← native cross-engine results (AWS)
├── benchmark-agentic.md  ← agentic memory benchmark results
├── license-change-dates.md  ← BUSL Change Date schedule per version
├── branch-tag-discipline.md  ← branch & tag conventions
│
├── usage/
│   ├── quickstart.md     ← 5-minute "run it"
│   └── reference.md      ← the operator manual
│
└── releases/             ← per-version release notes (the development record)
    ├── README.md         ← index of every version, newest first
    └── v1.0.0, v1.1.0 … ← one frozen note per public release
```

Root-level docs (outside `docs/`): [`../README.md`](../README.md) (project overview + release history), [`../CHANGELOG.md`](../CHANGELOG.md) (per-version summary), [`../OPERATIONS.md`](../OPERATIONS.md) (operator runbook), [`../PROTOCOL.md`](../PROTOCOL.md) (wire-protocol specification), [`../ROADMAP.md`](../ROADMAP.md) (intent — next / later / not doing), [`../CONTRIBUTING.md`](../CONTRIBUTING.md).

## Conventions

- **Living vs. historical.** The top-level files here (`architecture.md`, `xytalk-spec.md`, `mcp-integration.md`, `usage/*`) track current state and are updated each release. `releases/` holds per-version notes frozen in time — a v0.3 note stays a v0.3 note; those files are not maintained after the fact.
- **Release notes.** `CHANGELOG.md` at the repo root is the top-level per-version summary; each version's full narrative lives in `releases/<version>.md`, indexed by [`releases/README.md`](releases/README.md) — the project's development record.
- **Cross-references.** Prefer linking to another in-tree doc over duplicating content.
- **Commit messages** carry the authoritative rationale for *why*; the doc carries *current*.

## External artefacts

Not in this directory, but referenced:

- **Planning / strategy docs.** Design, spike, and forensic notes are kept in a private workspace outside the repo and are not published; they surface here only as updates to the living docs once a change is stable.
- **Benchmark reports.** Per-run JSON/CSV/MD outputs are written to `benchmarks/native/results/`, which is git-ignored — the raw reports are not published in the repo. The consolidated results are in [`benchmark-native.md`](benchmark-native.md).
- **Landing site.** The marketing site is a separate deployment at [xyzdb.bar](https://xyzdb.bar); it does not live in this repo.

## Contributing to these docs

Update the doc in the same commit as the behaviour change. For something new:

- `architecture.md` if it changes the internals or a design principle.
- `xytalk-spec.md` if it changes the language surface.
- `usage/reference.md` (or the root `OPERATIONS.md`) if an operator needs to know about it.
- A new `releases/<version>.md` when a release cuts.

Keep tone terse and specific. Every sentence earns its place.
