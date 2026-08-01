# Branch & tag discipline

xyzDB ships from two long-lived branches. The rule is small enough to hold in your
head, and it exists so that whoever clones the repo gets exactly what was tagged and
tested — never a work in progress.

## `main` — always the latest release

`main` is the most recent released commit and nothing else. It moves only when a
release is cut, always by fast-forward, always carrying that release's tag. Clone the
repo and you have precisely the tagged, tested code. `main` is protected against
force-push.

## `dev` — where work happens

Every commit lands on `dev`, and CI runs for real on `dev`. A release is `dev`
fast-forwarded onto `main`.

Code contributions are closed until the contribution agreement is in place; when
they open they will target `dev`. See [`CONTRIBUTING.md`](../CONTRIBUTING.md).

## `release/<minor>.x` — not yet

A `release/<minor>.x` branch opens **only** when the next minor's development starts
on `dev` while the previous line still needs patches in parallel. Until that day
`dev` *is* the current minor line — the 1.1.x line today — and a separate branch is
ceremony with no function.

## Tags — `vMAJOR.MINOR.PATCH`

One per release, annotated and signed. A tag is immutable: never moved, never
rewritten. `v1.0.0`, then `v1.0.1`, then `v1.1.0`, and so on.

## What each level means for this project

This is the part a user with data on disk needs: the version number tells you what an
upgrade is allowed to do to you.

| Level | Example | On-disk format | Public surface — xyTalk, wire protocol, MCP tools, CLI flags, HTTP endpoints | License |
|---|---|---|---|---|
| **PATCH** | `1.0.0` → `1.0.1` | **Never changes.** A patch always applies without thinking. | Additive only. Adding is allowed; removing or renaming is not. | **Untouched.** Inherits its minor line's Change Date — all of 1.0.x converts on the same day as 1.0.0. |
| **MINOR** | `1.0` → `1.1` | May change, with a documented `MANIFEST_VERSION` and a written upgrade path. | May add; never removes. | Re-stamped: new `Licensed Work: xyzDB Version 1.1` and a new Change Date (publication + 3 years), plus a new row in [`license-change-dates.md`](license-change-dates.md). |
| **MAJOR** | `1.x` → `2.0` | May change (as a minor). | May remove or incompatibly change public surface. | Re-stamped, exactly as a minor. |

## Release cycle

Four steps, in this order:

1. **On `dev`:** bump `version` in the root [`Cargo.toml`](../Cargo.toml) (all crates
   inherit it) and update [`CHANGELOG.md`](../CHANGELOG.md). If the release is a minor
   or major, re-stamp [`LICENSE`](../LICENSE), [`NOTICE`](../NOTICE), and
   [`license-change-dates.md`](license-change-dates.md).
2. **Parity gate:** `bash .ci/license-version-parity.sh` must be green — it validates
   that [`LICENSE`](../LICENSE), [`Cargo.toml`](../Cargo.toml), the change-dates table,
   and [`NOTICE`](../NOTICE) all say the same thing. If it fails, the release does not
   ship.
3. **Cut:** fast-forward `dev` onto `main`, then a signed tag on `main`. Push the tag
   **after** verifying the push of `main`.
4. **Document:** a release note under [`releases/`](releases/README.md) named
   `<version>.md`, following the format of the previous ones, plus a new row in
   [`releases/README.md`](releases/README.md).

## The version must match the tag

The workspace version has to equal the tag. A tag whose binary reports a different
version is a release bug — and `.ci/license-version-parity.sh` (step 2) catches it, so
the mismatch never reaches `main`.
