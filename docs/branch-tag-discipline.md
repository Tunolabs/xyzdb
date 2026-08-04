# Branch & tag discipline

xyzDB ships from two long-lived branches. The rule is small enough to hold in your
head, and it exists so that whoever clones the repo gets exactly what was tagged and
tested — never a work in progress.

## `main` — always the latest release

`main` is the most recent released commit and nothing else. It moves only when a
release is cut, always by fast-forward, always carrying that release's tag. Clone the
repo and you have precisely the tagged, tested code. `main` is protected against
force-push.

A fast-forward never rewrites or drops history: it moves the `main` pointer ahead to
the commit `dev` already reached, and every intermediate commit becomes part of
`main`'s history as-is. Cutting with `--ff-only` makes this a guarantee — if `main`
ever diverged, the merge aborts instead of creating a merge commit.

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

The signing key (GPG or SSH) must be registered in the maintainer's GitHub account
so tags and release commits show as **Verified** — for a database people trust with
their data, the badge is part of the release.

## What each level means for this project

This is the part a user with data on disk needs: the version number tells you what an
upgrade is allowed to do to you.

| Level | Example | On-disk format | Public surface — xyTalk, wire protocol, MCP tools, CLI flags, HTTP endpoints | License |
|---|---|---|---|---|
| **PATCH** | `1.0.0` → `1.0.1` | **Never changes.** A patch always applies without thinking. | Additive only. Adding is allowed; removing or renaming is not. | **Untouched.** Inherits its minor line's Change Date — all of 1.0.x converts on the same day as 1.0.0. |
| **MINOR** | `1.0` → `1.1` | May change, with a documented `MANIFEST_VERSION` and a written upgrade path. | May add; never removes. | Re-stamped: new `Licensed Work: xyzDB Version 1.1` and a new Change Date (publication + 3 years), plus a new row in [`license-change-dates.md`](license-change-dates.md). |
| **MAJOR** | `1.x` → `2.0` | May change (as a minor). | May remove or incompatibly change public surface. | Re-stamped, exactly as a minor. |

## Release cycle

Five steps, in this order:

1. **On `dev`:** bump `version` in the root [`Cargo.toml`](../Cargo.toml) (all crates
   inherit it) and update [`CHANGELOG.md`](../CHANGELOG.md). If the release is a minor
   or major, re-stamp [`LICENSE`](../LICENSE), [`NOTICE`](../NOTICE), and
   [`license-change-dates.md`](license-change-dates.md).
2. **Parity gates — both of them:**

   ```bash
   bash .ci/license-version-parity.sh    # licence surfaces
   bash .ci/release-version-parity.sh    # release surfaces
   ```

   The first validates that [`LICENSE`](../LICENSE), [`Cargo.toml`](../Cargo.toml), the
   change-dates table and [`NOTICE`](../NOTICE) all say the same thing. The second
   covers the surfaces that state the release version elsewhere:
   [`server.json`](../server.json)'s `version` and every `xyzdb-mcp:<tag>` image
   identifier in it, plus an assertion that `xyzdb-server --version` and the MCP
   handshake still DERIVE their version from the manifest instead of carrying a
   literal. It exists because that drift already shipped: at the 1.0.1 cut the manifest
   said 1.0.0 while the docs said 1.0.1 and the published binary reported 1.0.0, so the
   docs, `--version` and the registry disagreed about which release you had.

   If either fails, the release does not ship. A tag is superseded, never corrected.
3. **Cut:** fast-forward `dev` onto `main` locally — the GitHub UI cannot do a pure
   fast-forward, its merge buttons always create a merge or rewrite commits:

   ```bash
   git checkout main
   git merge --ff-only dev
   git push origin main
   git tag -s v<version> -m "xyzDB v<version>"
   git push origin v<version>
   ```

   Push the tag **after** verifying the push of `main`.
4. **Document:** a release note under [`releases/`](releases/README.md) named
   `<version>.md`, following the format of the previous ones, plus a new row in
   [`releases/README.md`](releases/README.md).
5. **Publish:** a GitHub Release on the tag, so watchers are notified and the
   release appears in the repo sidebar and feeds:

   ```bash
   gh release create v<version> --verify-tag \
     --title "xyzDB v<version>" \
     --notes-file docs/releases/v<version>.md
   ```

   `--verify-tag` refuses to publish if the tag does not exist on the remote — the
   release can never point at nothing.

## The version must match the tag

The workspace version has to equal the tag. A tag whose binary reports a different
version is a release bug — and `.ci/license-version-parity.sh` (step 2) catches it, so
the mismatch never reaches `main`.
