# xyTalk — Language Specification

**State:** current as of `1.0` (2026-07-30) — refreshed §-by-§ against the parser for the **xyTalk v1** alignment. One form per concept, with the previous spelling kept as a live alias where the change was cosmetic. The v1 surface changes, all reflected below: `TAKE` is the canonical top-N / truncate step (`TOP` is a deprecated alias); `NEAREST` is canonical with a phrase form (`NEAREST k BY field TO q [USING metric]`) and the function call as an alias — **`ORBIT` is removed**; `PURGE "lobe"` is the explicit total-delete verb and `DELETE` now requires a `WHERE`; `SET` / `DELETE` / `LINK` / `SCAN GHOST` accept the full `OR`/`NOT`/`IN` WHERE tree; `CREATE GHOST` accepts the query pipeline form; `IN [...]` is canonical (`IN (...)` an alias); `count(*)` is accepted as an alias of `count()`; `FIND` rejects `OR`/`NOT` at parse with an error that points to `SCAN`; `SCAN ORDER BY` requires `LIMIT`. The parser is the source of truth in every conflict. The engine is single-tier.
**Status:** Implemented and tested (`cargo test --workspace` green).

> xyTalk is the current name of the language surface (renamed from `xyzQL` in v0.2.5.1). The rename is purely cosmetic — semantics, on-disk format, and wire protocol are unchanged.

---

## Overview

xyTalk is the query language for xyzDB, a context database built on top of `turba-engine` — an in-house LSM-tree storage layer that replaced the earlier Fjall-backed storage in v0.2.0-alpha. The language is designed for graph traversal, analytical scans, and materialised views over schemaless records.

**Key characteristics:**
- Schemaless: records have dynamic fields, no predefined schema
- Co-location by design: related records physically stored together via `*gravity` fields
- Pipeline syntax for composing operations (`FIND ... | PULL | SET`)
- Ghost Lobes: materialized views with automatic projection optimization

---

## 1. DATA MODEL

### Values

xyTalk supports 11 data types:

| Type | Literal Syntax | Rust Type | Examples |
|------|---------------|-----------|---------|
| Text | `"string"` | String | `"Acme Corp"`, `"hello world"` |
| Int | `123` or `-5000` | i64 | `0`, `42`, `-100` |
| Float | `3.14` or `-0.5` | f64 | `1.0`, `-273.15` |
| Bool | `true` or `false` | bool | `true`, `false` |
| Null | `null` | — | `null` |
| Timestamp | `@"2026-04-01"` | i64 (microseconds) | `@"2026-01-15"` |
| LID | `LID("NNNN:LLLL:...")` | u128 | `LID("0000:0001:0001A2B3C4D5:00000001:0000")` |
| List | `[val, val, ...]` | Vec\<Value\> | `["tech", "saas"]`, `[1, 2, 3]` |
| Map | `{key: val, ...}` | BTreeMap\<String, Value\> | `{bureau: 685, risk: "low"}` |
| Vector | `[0.1, -0.4, ...]` (float list, len ≥ 64) | Vec\<f32\> | Dense f32 embedding; see §2.20 |
| Bytes | (binary only) | Vec\<u8\> | No literal syntax; binary protocol only |

**Type inference:** Determined by literal syntax. `"123"` is Text, `123` is Int, `123.0` is Float.

**Null:** `null` is a first-class value. A field can be explicitly set to null, which is different from a missing field. See Null Semantics below.

**Escape sequences in Text:** `\"` (double quote), `\\` (backslash).

**Negative numbers:** Supported for both Int and Float: `-5000`, `-3.14`.

**List/Map nesting:** Lists and Maps can be nested up to 16 levels deep:
```text
PUT {tags: ["tech", "saas"], scoring: {bureau: 685, risk: "low"}} IN "clients"
PUT {matrix: [[1, 2], [3, 4]], config: {rules: [{name: "r1", weight: 0.5}]}} IN "data"
```

### Null Semantics

**IMPORTANT DESIGN DECISION:** In xyzDB, `Null = Null` evaluates to **true**. This differs from SQL standard where `NULL = NULL` is UNKNOWN. xyzDB does not implement three-valued logic. This choice prioritizes predictability over SQL compatibility.

| Expression | Result | Notes |
|------------|--------|-------|
| `WHERE field = null` | true if field is `Null` | Only matches explicit Null, NOT missing field |
| `WHERE field != null` | true if field exists and is NOT Null | |
| `WHERE field IS NULL` | true if field is missing OR Null | Broader than `= null` |
| `WHERE field IS NOT NULL` | true if field exists and is NOT Null | |
| `WHERE field > null` | false | Null is incomparable with other types |
| `sum(field)` with Null | Null skipped | Same behavior as non-numeric values |
| `ORDER BY field` with Null | Null goes last | NULLS LAST behavior |
| `Null = Null` | **true** | Differs from SQL; conscious design choice |

### Records

A record is an unordered set of named fields with typed values:

```
{name: "Acme Corp", employees: 5000, active: true, founded: @"2010-06-15"}
```

Every record automatically receives:
- **LID**: A globally unique 128-bit identifier (auto-generated)
- **_type**: Defaults to the lobe name if not explicitly set
- **created_at / updated_at**: Microsecond timestamps (auto-managed)

### Lobes

A lobe is a named collection of records (analogous to a table). Lobes are auto-created on first insert or can be pre-declared via the `LOBE` statement (§2.1).

**Heterogeneous by design.** Unlike a relational table (one type per table), a lobe admits records with multiple `_type` values that share a domain. The `creditos` lobe in the fintech reference dataset, for instance, holds `Credit + Installment + Payment + Collection + CollectionAction` — the entire credit lifecycle for a client, co-located by `*rfc`. A `PULL depth=N` over a single seed retrieves the whole subtree in one range scan; the relational equivalent is a multi-table JOIN. See `docs/architecture.md` §2.1 for the trade-off rationale.

### Gravity Fields

Fields prefixed with `*` are gravity fields. Records sharing the same gravity value are physically co-located on disk, enabling efficient graph traversal:

```text
PUT {*company: "Acme", _type: "Invoice", amount: 5000} IN "fintech"
PUT {*company: "Acme", _type: "Payment", amount: 2000} IN "fintech"
-- Both records co-located; PULL retrieves them in one sequential read
```

The `*` prefix is per-record sugar for the lobe's *gravity keel*. Once the keel is registered — either by the first `*`-marked `PUT` or by an explicit `GRAVITY BY … IN "lobe"` statement (§2.2.1) — later records co-locate correctly even when the gravity field is written as a plain field without the `*`.

### Anchors

Anchors declare field uniqueness within a lobe and integrate the constraint with an O(1) dictionary lookup. An anchor is **not just a unique index**: it is constraint + lookup integrated as a single primitive. `FIND "clients" WHERE email = "X"` resolves through the dictionary keyspace once the anchor is populated, without consulting the primary spatial keyspace.

The language separates the declarative declaration from the operational population step:

- `ANCHOR "field" UNIQUE IN "lobe"` — declarative, registers the constraint. See §2.2.
- `AUTOANCHOR APPLY "field" IN "lobe"` — operational, populates the dictionary for existing records. See §2.17.

---

## 2. STATEMENTS

xyTalk statements are organized by **usage tier** so a casual reader meets the basics first and discovers operational depth on demand. The set of statements has not changed; the grouping reflects the level of mental model each one requires.

**Tier categorization criterion** (rule for future additions):

- **Tier 1 — Quickstart** — primitive with ≥1 deterministic fast path AND zero operational knowledge required. The verbs every user starts with.
- **Tier 2 — Common** — primitive a user discovers on demand (relate, cache, introspect, aggregate).
- **Tier 3 — Power user** — requires understanding of one of: ghost lifecycle, anchor population semantics, manual pinning, override of automatic routing.
- **Tier 4 — Operator** — administrative operation that should not appear in application code paths; deprecated as language statements in v0.2.5.1+ and exposed via `xyzdb-cli admin <verb>`.

---

### Tier 1 — Quickstart

The 8 verbs every user starts with: declare a space, declare identity, write, read, update, delete.

#### 2.1 LOBE — Declare a Lobe

```
LOBE "lobe_name" [HINT="description"]
```

**Examples:**

```text
-- Declare an empty lobe
LOBE "clients"

-- With descriptive hint
LOBE "clients" HINT="Customer records"

-- Declare a lobe before bulk-loading
LOBE "creditos" HINT="Credit lifecycle: Credit + Installment + Payment + Collection + CollectionAction"
```

**Behavior:**
- Registers the lobe in the lobe registry. Idempotent: declaring the same lobe twice is a no-op.
- A lobe is **also auto-created** on the first `PUT` that targets it, so explicit declaration is optional. Use `LOBE` when you want to attach a `HINT` or want the lobe to exist before any record is written (typical for migration scripts).
- See §1 Lobes for the conceptual model (heterogeneous lobes, co-location by gravity).

#### 2.2 ANCHOR — Declare Uniqueness

```
ANCHOR "field" UNIQUE IN "lobe"
```

**Examples:**

```text
ANCHOR "email" UNIQUE IN "clients"
ANCHOR "rfc"   UNIQUE IN "clientes"
```

**Behavior:**
- **Declarative.** Registers the uniqueness constraint and creates an empty entry in the dictionary keyspace. Subsequent inserts populate the dictionary on write; existing records are NOT indexed by this statement.
- After declaration, `FIND "lobe" WHERE field = X` resolves through the dictionary keyspace in O(1).
- Duplicate declarations of the same `(lobe, field)` are an error.
- For retroactive population over already-loaded records, see `AUTOANCHOR APPLY` (§2.17).

**Common ordering — declare before bulk load:**

```text
ANCHOR "rfc" UNIQUE IN "clientes"
PUT BATCH IN "clientes" [...]            -- 1.5 M rows in ≤10K chunks (§2.4); each PUT also writes the anchor entry
```

If you must declare the anchor *after* the bulk load, use `AUTOANCHOR APPLY` to populate the dictionary for the already-written records (§2.17).

#### 2.2.1 GRAVITY BY — Declare the Gravity Keel (v0.8)

```
GRAVITY BY <expr> IN "lobe"
```

The explicit form of the co-location declaration. Where the `*` prefix (§1 Gravity Fields) is per-record sugar, `GRAVITY BY` registers, once and up front, how a lobe derives every record's `gravity_hash` — the *keel* the write path and the `SCAN` / `FIND` fast path both resolve through.

**`<expr>` forms:**

| Form | Syntax | Meaning |
|------|--------|---------|
| Raw | `GRAVITY BY rfc IN "creditos"` | Co-locate by the field's value as-is |
| Normalized | `GRAVITY BY lower(email) IN "clients"` / `GRAVITY BY trim(name) IN "clients"` | Co-locate by an identity-safe fold (`lower` / `trim`) of the value |
| Composite | `GRAVITY BY (tenant, region) IN "events"` | Co-locate by the tuple of two or more fields |

**Behavior** (verified against `ops/put.rs`):

- **Co-location requires a registered gravity spec for the lobe.** A spec is registered in one of two ways:
  1. explicitly, by a `GRAVITY BY …` statement, or
  2. implicitly, auto-registered as `Raw(field)` on the **first** `PUT` that carries a single `*` marker.
- **Once a spec exists, the `*` marker is optional per write.** A record whose gravity field is written as a *plain* field (no `*`) still co-locates: placement routes through the registered spec, so write-side placement and query-side bucket resolution never diverge. You do **not** have to repeat `*field` on every record — a record missing the marker still lands in the correct bucket as long as the field is present.
- **A single `*` marker with no declared spec** auto-registers `Raw` on that field (backward-compatible with pre-v0.8 sugar).
- **Two or more `*` markers on one record** require a declared `Composite` spec covering them; otherwise the `PUT` is rejected (placement would otherwise silently collapse to the first marker). Declare `GRAVITY BY (a, b) IN "lobe"` first, then write the record.
- Declared before the first write; persisted in the dictionary keyspace, survives restart.

```text
-- Explicit keel, then plain-field writes co-locate without the * marker
GRAVITY BY rfc IN "creditos"
PUT {rfc: "ACME-001", _type: "Credit", monto: 50000} IN "creditos"     -- co-located by rfc
PUT {rfc: "ACME-001", _type: "Payment", amount: 2000} IN "creditos"    -- same bucket

-- Composite keel required before a record marks two gravity fields
GRAVITY BY (tenant, region) IN "events"
PUT {*tenant: "acme", *region: "eu", _type: "Login"} IN "events"
```

#### 2.2.2 SATELLITE BY — Declare the Sub-Gravity Axis

```
SATELLITE BY <field> IN "lobe"
```

A third foundational axis, sibling to gravity (placement) and vector (search). Where gravity decides *which bucket* a record lands in, the satellite axis decides *how one bucket is sub-divided*: it names the single field whose value maps (via a 16-bit hash) to the `sat` axis of the record's spatial key, so a large gravity bucket splits into ordered sub-buckets. A query that filters on **both** the gravity field and the satellite field then scans one satellite sub-range instead of the whole parent bucket. This bounds `SCAN … WHERE gravity = g AND kind = k`, the same shape feeding `AGGREGATE count()` / `GROUP BY`, and — importantly — `SCAN … WHERE gravity = g AND kind = k | NEAREST(…)`: with an equality on the satellite field the candidate set *is* the satellite, so NEAREST scores only that sub-range and returns the exact top-k of the filtered set, instead of scoring the whole gravity bucket.

**Rules:**

- **One axis per lobe.** A lobe has at most one satellite field; re-declaring the same field is a no-op, declaring a different one is rejected. (The `sat` axis is a single `u16`; two fields cannot share it — a two-level split is a deferred design.)
- **Declared on an empty lobe.** `SATELLITE BY` is refused if the lobe already holds records: declaring the axis over existing data would leave those records in the default sub-bucket, unreachable by a bounded per-satellite query. Declare it before the first write. (Re-packing existing data under a newly declared axis is a later, explicitly-justified path.)
- **Leaving is free.** Retracting the axis never loses data or correctness: the parent-bucket scan already covers every satellite, so reads stay exact — only the bounded-scan speed-up is given up.
- **Transparent optimisation.** The bounded per-satellite scan is a pure optimisation: it returns exactly the same rows, in exactly the same order, as the full parent-bucket scan would. The 16-bit hash collides by design, so the read path always re-applies the field predicate as a residual — a record that hashed into the same satellite but does not truly match is dropped. Correctness never depends on the hash being collision-free.
- **`SET` moves a changed record; `PUT … ON CONFLICT UPDATE` (upsert) does NOT.** A `SET` that changes the satellite field re-places the record into its new satellite (mirroring re-gravitation), so a bounded query on the new value finds it. **An upsert updates in place and does not re-place** — the same contract it has for the gravity field. **Declared consequence, read before choosing the axis:** if the satellite field can change through an upsert, the record stays in its *old* satellite; a bounded query on the new value will not find it, and a bounded `count` over the new value will be **silently short**. Choose an axis field that is immutable once written (identifiers that name a role are a natural fit), or mutate it with `SET`, never with a keyed upsert.
- Persisted in the dictionary keyspace; survives restart.

> **When it pays.** The axis only speeds things up when the field is present on **most** records of the lobe. Records missing the field (and any value whose 16-bit hash is 0) share the default satellite 0; if many records lack the field, satellite 0 becomes the large bucket and the bounded scan saves nothing. Choose a field that is near-universal in the lobe.

```text
-- Declare the sub-gravity axis on an empty lobe, up front
GRAVITY BY scope IN "events"
SATELLITE BY kind IN "events"
PUT {scope: "s1", kind: "click", n: 1} IN "events"
-- Bounded to the "click" satellite of the "s1" gravity bucket:
SCAN "events" WHERE scope = "s1" AND kind = "click"
SCAN "events" WHERE scope = "s1" AND kind = "click" | AGGREGATE count()
```

#### 2.3 PUT — Insert Record

```
PUT {field: value [, ...]} IN "lobe"
    [LINK TO target [WHERE filters] AS "relation"]
    [ON CONFLICT UPDATE]
```

**Fields block:** Comma-separated `name: value` pairs inside `{}`. Optional `*` prefix for gravity.

**Examples:**

```text
-- Simple insert
PUT {name: "Acme Corp", industry: "Tech", employees: 5000} IN "clients"

-- With gravity field (co-location)
PUT {*company: "Acme", _type: "Invoice", amount: 15000.50, status: "pending"} IN "fintech"

-- With explicit link to another record
PUT {*project: "Alpha", title: "Design phase", hours: 40} IN "tasks"
    LINK TO "projects" WHERE name="Alpha" AS "task_of"

-- Upsert: update if anchor conflicts
PUT {email: "ceo@acme.com", name: "New Name", role: "CEO"} IN "clients" ON CONFLICT UPDATE

-- With null value (V4)
PUT {name: "Pending", score: null} IN "data"

-- With List and Map (V4)
PUT {name: "Acme", tags: ["tech", "saas", "b2b"], scoring: {bureau: 685, risk: "medium"}} IN "clients"

-- Nested structures (V4)
PUT {config: {rules: [{name: "r1", weight: 0.5}, {name: "r2", weight: 1.0}]}} IN "settings"
```

**Behavior:**
- Auto-creates lobe if it doesn't exist
- Auto-injects `_type` field with lobe name if not provided
- `ON CONFLICT UPDATE`: requires an ANCHOR on the conflicting field; merges new fields into existing record
- Gravity fields (`*campo`) determine `gravity_hash` for co-location
- Returns the LID of the inserted record

#### 2.4 PUT BATCH — Insert Multiple Records

```
PUT BATCH IN "lobe" [{...}, {...}, ...]
    [LINK TO target [WHERE filters] AS "relation"]
    [ON CONFLICT UPDATE]
```

**Examples:**

```text
PUT BATCH IN "fintech" [
    {*company: "Acme", _type: "Installment", amount: 1000, status: "overdue", due_date: @"2026-01-15"},
    {*company: "Acme", _type: "Installment", amount: 1500, status: "active", due_date: @"2026-02-15"},
    {*company: "Acme", _type: "Installment", amount: 2000, status: "active", due_date: @"2026-03-15"}
]

-- Batch with link (all records inherit the relation)
PUT BATCH IN "tasks" [
    {title: "Design", *project: "Alpha", hours: 40},
    {title: "Build", *project: "Alpha", hours: 120}
] LINK TO "projects" WHERE name="Alpha" AS "task_of"
```

**Behavior:**
- Atomic: all records inserted in a single batch or none
- **Max 10,000 records per batch.** A larger batch is rejected whole with an
  `InvalidQuery` error — no partial insert. To load more, split into chunks of
  ≤ 10,000; each chunk commits as its own atomic batch (atomicity is per-chunk,
  not across chunks).
- `ON CONFLICT UPDATE` in batch: conflicting records are silently skipped
- Returns first LID, last LID, and count

#### 2.5 FIND — Lookup Records

```
FIND "lobe" [WHERE filter [AND filter ...]] [LIMIT n] [CURSOR "<token>"]
FIND LID("lid_string")
```

**Examples:**

```text
-- By field value (uses anchor -> gravity -> full scan automatically)
FIND "clients" WHERE name="Acme Corp"

-- Multiple filters
FIND "fintech" WHERE status="overdue" AND amount>=5000

-- Direct LID lookup
FIND LID("0000:0001:0001A2B3C4D5:00000001:0000")

-- Comparison operators
FIND "fintech" WHERE amount>10000 AND status!="cancelled"

-- Paginated FIND on a gravity bucket (v0.2.5.2)
FIND "creditos" WHERE rfc = "ACME-001" LIMIT 1000
-- → records (1000), cursor = "AQEAAQ...", has_more = true

FIND "creditos" WHERE rfc = "ACME-001" LIMIT 1000 CURSOR "AQEAAQ..."
-- → records (next 1000), cursor = "...", has_more = true | false
```

**AND-only by design.** `FIND` takes `WHERE filter [AND filter …]` — a conjunction, never `OR`/`NOT`. Its speed is a cost contract: the anchor/gravity fast path resolves a conjunction in O(1)/bounded, which `OR`/`NOT` cannot. An `OR`/`NOT` in a `FIND` is rejected **at parse** with a message that teaches the fix — *"FIND is AND-only (anchor/gravity fast path); OR/NOT needs a traversal — use SCAN"* — rather than silently dropping the rest of the predicate. Use `SCAN` for the boolean tree (§2.6).

**Resolution order:**
1. Anchor lookup (O(1) dictionary)
2. Gravity dictionary lookup (O(1))
3. Full lobe scan with filter (O(n))

For a casual reader's view of how the engine picks the path automatically, see the *How xyTalk routes queries* section in `usage/quickstart.md`.

**Cursor pagination on FIND (v0.2.5.2):**

`CURSOR` extends `FIND` for the gravity-bounded fast path only — the one shape where a single bucket can hold many records (`Credit + Installment + Payment + ...` co-located by `*rfc`). The token format and filter-checksum binding are identical to `SCAN`'s (§2.6); cursors are version-bound the same way.

| Path | Cursor accepted | Rationale |
|---|---|---|
| `FIND WHERE gravity_field = X [LIMIT n]` | **Yes** | Bounded range scan over the gravity bucket; pages emit cursor when the bucket overflows the active LIMIT |
| `FIND WHERE anchor_field = X` | No | Single record; cursor not applicable |
| `FIND LID("...")` | No | Single record; cursor not applicable |
| `FIND WHERE other_field = X` | No | No fast path; use SCAN, or declare ANCHOR / register gravity |

`LIMIT` without `CURSOR` on a gravity-eligible predicate triggers the first-page paginated path automatically: the response is `PaginatedRecords` with a fresh cursor and `has_more`. Subsequent calls pass the cursor back unchanged. Anchor and FIND-LID shapes ignore `LIMIT` (single record returned regardless).

A `NEAREST` whose bounded hydration was cut by the latency airbag (`--nearest-budget-ms`) also returns `PaginatedRecords` with `has_more = true` but **no cursor** — its scoring pass is not resumable. In that one case the frame additionally carries `budget_stop: { examined, candidates, found }` — the counts at the cut, defined and interpreted in §2.20. The field is **absent on every other** `PaginatedRecords` (cursor pages, SCAN caps), so those frames stay byte-identical and clients that key off `has_more` are unaffected.

#### 2.5.1 FETCH — Multi-lobe co-located read (v1)

```
FETCH "lobe1", "lobe2", … WHERE filter [AS {name1, name2, …}]
```

Read several co-located lobes in **one call**. `FETCH` resolves the shared `WHERE` against each lobe and returns a single record whose fields are one **named section** per lobe — a list of that lobe's matching records. It is the grain that closes a multi-entity lookup (a customer *and* their credits *and* their operations, all keyed by `rfc`) that used to take one round-trip per lobe.

```text
-- Customer 360 in one call: the client plus their credits plus their operations
FETCH "clientes", "creditos", "operaciones" WHERE rfc = "ACME-001"
    AS {cliente, creditos, operaciones}
```

**Behavior:**
- `WHERE` is **required** — the shared key that selects the co-located records across the lobes. Because the lobes co-locate by that key, each resolve rides the anchor/gravity fast path.
- Returns `Records` with exactly one envelope record. Each field is a section: `name → [ {record fields}, … ]`. A section is byte-for-byte what `SCAN "lobe" WHERE filter` would return on its own — `FETCH` is packaging, not composition.
- `AS {…}` renames the sections positionally and must give one name per lobe (else a parse error); omitted, each section is named by its lobe.
- The envelope is a transport shape, not a stored record (nil LID, no timestamps).

#### 2.6 SCAN — Filtered Scan

```
SCAN "lobe" [WHERE filter_expression]
             [ORDER BY field [ASC|DESC]]
             [LIMIT n]
             [CURSOR "<token>"]
```

**Filter expressions** support AND, OR, NOT, and parentheses with standard precedence (NOT > AND > OR):

```text
-- Basic scan
SCAN "fintech" WHERE status="overdue"

-- With limit (early termination)
SCAN "fintech" WHERE status="active" LIMIT 100

-- Ordered (requires LIMIT)
SCAN "fintech" WHERE status="overdue" ORDER BY due_date ASC LIMIT 10

-- OR filter (V4)
SCAN "fintech" WHERE status = "overdue" OR status = "defaulted"

-- NOT filter (V4)
SCAN "fintech" WHERE NOT status = "cancelled"

-- Combined with parentheses (V4)
SCAN "fintech" WHERE (status = "overdue" OR status = "defaulted") AND amount > 5000

-- NOT with parenthesized OR
SCAN "fintech" WHERE NOT (status = "cancelled" OR status = "archived")

-- Dot notation for nested fields (V4)
SCAN "clients" WHERE scoring.bureau > 650
SCAN "data" WHERE config.rules.0.weight >= 0.5

-- CONTAINS for list membership (V4)
SCAN "clients" WHERE tags CONTAINS "tech"

-- IS NULL / IS NOT NULL (V4)
SCAN "data" WHERE score IS NULL
SCAN "data" WHERE score IS NOT NULL

-- Without WHERE
SCAN "fintech" LIMIT 50
```

**Behavior:**
- Automatically routed to Ghost Lobe if a relevant one exists (transparent to the user)
- Ghost routing only works with pure AND expressions; OR/NOT queries scan primary
- `ORDER BY` **requires** `LIMIT` — error without it
- `ORDER BY` supports single field only, `ASC` (default) or `DESC`
- ORDER BY uses a bounded min-heap: O(n) scan, O(k) memory where k = LIMIT
- Records scan telemetry; may trigger auto-ghost creation after 5+ slow patterns (AND-only)

For the user-facing view of how `SCAN` picks between Primary, Ghost, and GhostPreComputed routes, see *How xyTalk routes queries* in `usage/quickstart.md`. Full routing semantics with PreComputed precondition rules are in §5.

**Safety net (v0.2.5.1):**
- **Default LIMIT 1000** — a plain `SCAN` that omits both `LIMIT` and `ORDER BY` is capped at `SCAN_LIMIT_DEFAULT = 1000` records and the server emits a `tracing::warn`. Aggregate paths (`SCAN | AGGREGATE`, `SCAN | GROUP BY | AGGREGATE`) are not affected — the SCAN collapses to a single row regardless of input size.
- **Hard ceiling** — explicit `LIMIT N > 10000` (`SCAN_LIMIT_HARD_MAX`) is rejected. Use `CURSOR` pagination for larger result sets.

**Cursor pagination (v0.2.5.1):**

```text
-- First page.
SCAN "creditos" WHERE rfc = "X" LIMIT 1000
-- → records (1000), cursor = "AQEAAQ...", has_more = true

-- Next page: pass the token back unchanged.
SCAN "creditos" WHERE rfc = "X" LIMIT 1000 CURSOR "AQEAAQ..."
-- → records (next 1000), cursor = "...", has_more = true | false
```

- The `CURSOR` token is opaque — postcard-encoded `CursorPayload { format_ver, lobe_id, last_spatial_key, filter_checksum }` wrapped in URL-safe base64 with no padding. Round-trip the token unchanged.
- Reusing a cursor under a different `WHERE` clause errors with `cursor invalid: WHERE clause does not match the cursor's binding` (filter checksum is `xxh3_64` of the AST `Debug` form).
- Result variant: `QueryResult::PaginatedRecords { records, cursor, has_more }`. Plain `Records` is preserved for SCANs that fit completely under the active `LIMIT`.
- **Constraints**: `CURSOR` + `ORDER BY` rejected; `CURSOR` + ghost routing rejected (engine forces `ScanSource::Primary`). Both are post-v0.6 grammar scope (not in the v0.5.x mini-cycles or v0.6.0-pre format bump; deferred until a richer cursor payload is designed).
- Cursor tokens are version-bound: an AST `Debug` change in a future release invalidates in-flight cursors. Treat them as ephemeral pagination state, not as durable handles.

#### 2.7 SET — Update Fields

```
-- Pipeline (update records found by FIND)
FIND ... | SET field = value [, field = value ...]

-- Standalone
SET "lobe" field = value [, field = value ...] WHERE filters
```

**Examples:**

```text
-- Update via pipeline
FIND "clients" WHERE name="Acme Corp" | SET status = "inactive", updated_by = "admin"

-- Multiple field updates
FIND "fintech" WHERE invoice_id="INV-001" | SET status = "paid", paid_at = @"2026-04-01"
```

**Behavior:**
- Updates `updated_at` timestamp automatically
- Does NOT recalculate `gravity_hash` — if a `*gravity` field is changed, the record stays in its original location until compaction
- The standalone `WHERE` accepts the full `OR`/`NOT`/`IN` tree (§3), not just AND: an AND-pure predicate takes the anchor/gravity fast path, `OR`/`NOT` scans the target and filters. Ghosts stay exact either way (the update fires the ghost `notify_write` hook).

#### 2.8 DELETE — Remove Records

```
-- Pipeline (delete the records the upstream step produced)
FIND ... | DELETE

-- Standalone (WHERE is REQUIRED)
DELETE "lobe" WHERE filters
```

**Examples:**

```text
FIND "fintech" WHERE status="cancelled" | DELETE
DELETE "clients" WHERE name="Old Corp"
DELETE "fintech" WHERE status = "cancelled" OR status = "archived"
```

**Behavior:**
- **`WHERE` is required on the standalone form.** A WHERE-less `DELETE "lobe"` used to empty the whole lobe silently — a footgun. It now errors with *"DELETE requires WHERE — add a predicate, or use `PURGE \"lobe\"` to empty a whole lobe"*. To empty a lobe on purpose, use `PURGE` (§2.8.1).
- The standalone `WHERE` accepts the full `OR`/`NOT`/`IN` tree (§3): AND-pure takes the anchor/gravity fast path, `OR`/`NOT` scans + filters. Ghosts stay exact either way (each removal fires `notify_write`).
- Removes the record from spatial, identity, and dictionary (anchor entries).
- Deletes only the matched record(s); there is no cascade to linked or
  co-located records.
- The pipeline form (`FIND … | DELETE`) needs no `WHERE`: the upstream records are the selection.

#### 2.8.1 PURGE — Empty a Lobe

```
PURGE "lobe"
```

The explicit, hard-to-typo verb for total deletion — what a WHERE-less `DELETE` used to do by accident. It now can only happen on purpose.

```text
PURGE "scratch"     -- remove every record in "scratch"
```

**Behavior:**
- Removes every record in the lobe through the same per-record delete path as a WHERE-matching `DELETE`, so ghosts and anchor indexes are maintained (each removal fires `notify_write`): after `PURGE`, a routed aggregate over the lobe returns empty, and an anchor `FIND` finds nothing — no stale derived state survives.
- Classified as a destructive statement by the MCP query policy (§ trust boundary), alongside `DELETE` and `DROP GHOST`.

---

### Tier 2 — Common

Discovered on demand. The user lands here when they need to relate records, keep something hot in cache, aggregate, or introspect what they have.

#### 2.9 LINK — Create Relationship

```
LINK "source_lobe" [WHERE source_filters] TO "target_lobe" [WHERE target_filters] AS "relation_name"
```

**Examples:**

```text
-- Link a single source record to a single target record
LINK "clients" WHERE name="Acme Corp" TO "fintech" WHERE invoice_id="INV-001" AS "invoiced_to"

-- Multi-record link: every source matching its WHERE links to every target matching its WHERE
LINK "clientes" WHERE rfc = "ACME-001" TO "creditos" WHERE _type = "Credit" AS "owner"
```

**Behavior:**
- Adds a `_link_{relation_name}` field to each source record with the target's LID as value
- Enables PULL traversal across linked records
- `WHERE` on either side is optional; omitting it links **all** records in that lobe (use with care)
- Each side's `WHERE` accepts the full `OR`/`NOT`/`IN` tree (§3): an AND-pure side takes the anchor/gravity fast path, `OR`/`NOT` scans + filters
- Standalone-form `WHERE` filters were added in v0.2.5.1 (previously only the `PUT … LINK TO` sub-clause was usable for filtered links)

#### 2.10 INCACHE / OUTCACHE — RecordCache control

```
INCACHE  ( "lobe" | lobe ) [WHERE filter_expression]
OUTCACHE ( "lobe" | lobe )
```

**INCACHE.** Loads matching records from a lobe into the in-memory `RecordCache`. The optional `WHERE` clause uses the same boolean expression grammar as `SCAN` (`AND` / `OR` / `NOT`, parentheses, dot notation, `IS NULL` / `CONTAINS`). Subsequent `FIND` / `SCAN` reads on the same lobe consult the cache before the spatial keyspace, trading memory for predictable single-digit-microsecond hot reads — the surface targeted as the Redis replacement for hot operational data.

```text
INCACHE "creditos" WHERE status = "active" OR status = "overdue"
INCACHE clientes
```

**OUTCACHE.** Evicts the lobe from the cache. The pre-cache state is recovered on the next read.

```text
OUTCACHE "creditos"
OUTCACHE clientes
```

**Operational notes:**

- Both forms accept quoted (`"creditos"`) and unquoted (`creditos`) lobe names.
- Server must be started with `--record-cache-size N` (MiB; `--hot-cache-size` is a deprecated alias) — without it both statements error with `RecordCache not enabled`.
- `INCACHE` against a missing lobe errors with `LobeNotFound`. The bare keywords `INCACHE` / `OUTCACHE` (no argument) are rejected at parse time.
- The cache is consulted by `FIND`, `SCAN`, `SET`, `DELETE`, and `PUT` (write-through). `SCAN` cursor pagination still seeks the spatial keyspace (cache contributes only to hit-on-read, not to range iteration).
- `SHOW CACHE` lists currently cached lobes and per-lobe entry counts (§2.12).

#### 2.11 AGGREGATE — Compute Statistics (Pipeline Only)

```
SCAN ... | AGGREGATE metric [, metric ...]
SCAN ... | GROUP BY field [, field ...] | AGGREGATE metric [, metric ...]

metric := func() [AS <alias>] [WHERE <predicate>]
```

**Functions:**

| Function | Syntax | Returns |
|----------|--------|---------|
| Count | `count()` | Int — total records |
| Sum | `sum(field)` | Float — sum of numeric values |
| Average | `avg(field)` | Float — average of numeric values |
| Minimum | `min(field)` | Float — smallest numeric value |
| Maximum | `max(field)` | Float — largest numeric value |

**Aggregate fields support dot notation:** `sum(scoring.bureau)`, `avg(config.weight)`.

**Per-metric filter + alias (v0.9):** each metric may carry its own `WHERE`
predicate and an `AS <alias>`. The metric folds only the records passing its
predicate (composed with the query/ghost header as `header AND metric`), and the
result column is named by the alias. This computes several conditional
aggregates in one pass — a "monthly close" with active/overdue/paid counts and
sums — instead of one query (or one ghost) per condition.

- The per-metric `WHERE` accepts the same predicate grammar as a top-level
  `WHERE` (`AND`/`OR`/`NOT`, `IN`, `CONTAINS`, comparisons).
- An alias is **required** when two metrics would otherwise resolve to the same
  column — notably a filtered `count()`, which must be aliased so it stays
  distinct from the group total `count`. Duplicate labels are a parse error.
- `AS` precedes `WHERE`: `func() AS <alias> WHERE <predicate>`.

**Examples:**

```text
-- Single aggregate
SCAN "fintech" WHERE status="overdue" | AGGREGATE count()

-- Multiple aggregates
SCAN "fintech" WHERE status="overdue" | AGGREGATE count(), sum(amount), avg(amount), min(amount), max(amount)

-- GROUP BY (V4)
SCAN "payments" | GROUP BY payment_month | AGGREGATE count(), sum(amount)

-- GROUP BY with filter
SCAN "fintech" WHERE status = "overdue" | GROUP BY credit_id | AGGREGATE count(), sum(amount)

-- GROUP BY with dot notation
SCAN "clients" | GROUP BY scoring.risk | AGGREGATE count(), avg(scoring.bureau)

-- Per-metric filter + alias: conditional aggregates in one pass (v0.9)
SCAN "credits" | GROUP BY empresa_id | AGGREGATE
    count()      AS total,
    count()      AS active         WHERE status = "active",
    sum(amount)  AS overdue_amount WHERE status = "overdue"
```

**Behavior:**
- `SCAN | AGGREGATE` is a special-cased pipeline: streaming O(1) memory
- `SCAN | GROUP BY | AGGREGATE` is also special-cased: O(num_groups) memory
- GROUP BY returns an array of groups, each with the group key field(s) + aggregate results
- GROUP BY uses deterministic canonical keys internally (not dependent on Rust Debug format)
- If the GROUP BY field is missing from a record, the record is grouped under a "null" key
- `count()` counts records and takes no field argument; `count(*)` is accepted as an alias of `count()`. `count(field)` and `count(DISTINCT …)` are not supported.
- Null and non-numeric values in `sum/avg/min/max` are silently skipped
- **Result column labels** are the same whether the query is answered live or by
  a PreComputed ghost: the `AS` alias if given, else the canonical `count`,
  `sum(field)`, `avg(field)`, `min(field)`, `max(field)`. A grouped result always
  carries a `count` (the group total) plus a column per metric.
- A PreComputed ghost answers a query only when it precomputes every requested
  metric (same op, field, per-metric filter, alias); otherwise the query is
  computed live — same numbers, just without the zero-scan short-circuit.
- `count(DISTINCT ...)` is NOT supported
- `HAVING` is NOT supported (filter groups in application code)

#### 2.11.1 TAKE — Top-N / Truncate (pipeline step)

```
... | AGGREGATE ... | TAKE n BY <metric> [DESC|ASC]   -- top-N over the grouped aggregate
... | TAKE n                                           -- truncate the stream to n (pipeline LIMIT)
```

`TAKE` is the canonical top-N and truncate step. `TOP` parses to the identical node and is a **deprecated alias** (kept live so existing queries and the native benchmark's `TOP n BY sum(monto)` keep working).

**Behavior:**
- `TAKE n BY <metric> [DESC|ASC]` keeps the `n` groups with the highest (`DESC`, the default) or lowest (`ASC`) value of one of the declared `AGGREGATE` metrics, server-side. Ordering is total (metric, then group key), so the result equals sort-all-then-truncate and ties at the N/N+1 cut are deterministic. `<metric>` is a function form (`sum(monto)`, `count()`) or an `AS` alias, and must name a metric the `AGGREGATE` clause produced.
- When a ghost declared the same metric order (§2.15), `TAKE n BY <metric>` reads the first N straight from the metric-ordered rollup — **O(N)** instead of O(M) — with a byte-identical result.
- `TAKE n` with no `BY` truncates the current stream to the first `n` items in order — the pipeline form of `LIMIT`. Valid on grouped rows and on a plain record stream (e.g. `SCAN "l" | TAKE 5`).

```text
SCAN "creditos" WHERE _type = "Credit" AND status IN ["active", "overdue"]
    | GROUP BY rfc
    | AGGREGATE sum(monto) AS exposicion, count()
    | TAKE 100 BY exposicion DESC
```

#### 2.11.2 SHAPE — Project fields (pipeline step)

```
... | SHAPE {field1, field2, …}
```

Project each record down to the named fields — the read-side mirror of `PUT {…}` (braces put a shape in, braces take a shape out). The braces hold bare field names, not assignments.

**Behavior:**
- Keeps only the listed fields on each record; every other field is dropped from the result.
- A projection, **not** a filter: a record that lacks a named field is still returned, just without that field.
- Structural identity (LID, lobe, timestamps) is untouched — `SHAPE` shapes the field set, not which records or their order. Compose it after `WHERE` / `ORDER BY` / `LIMIT` and those are unaffected.

```text
-- Return just the two fields the caller needs
SCAN "clients" WHERE tier = "gold" ORDER BY score DESC LIMIT 10
    | SHAPE {name, score}
```

#### 2.12 SHOW (introspection)

Discover what your database holds. For tuning info (profile, scan stats, throttle), see §2.19.

```
SHOW LOBES
SHOW ANCHORS IN "lobe"
SHOW GHOSTS
SHOW CACHE
```

**Behavior:**
- `SHOW LOBES` — list every registered lobe with its record count.
- `SHOW ANCHORS IN "lobe"` — list anchor fields declared on the lobe.
- `SHOW GHOSTS` — list materialised ghosts across the database with their source lobe, order-by field, record and filter counts. A ghost declared to keep a metric order (`| TAKE BY <metric>`, or the `ORDER BY <metric>` alias) also shows its metric-ordered rollup and freshness: `[metric-order sum(monto) DESC — emitted <age>s ago]`, or `[metric-order … — STALE, not emitted]` when the last emit failed/collided (a `TAKE` then falls back to the O(M) path). Health markers are appended when a ghost is no longer exact: `[aggregates stale — REFRESH to reconcile]` after a `min`/`max` member was deleted (they can't be decremented incrementally), and `[maintenance degraded — REFRESH to rebuild]` if an incremental maintenance write failed. `REFRESH GHOST "name"` rebuilds from the source lobe and clears them (and re-emits the metric order).
- `SHOW CACHE` — list lobes currently held in the RecordCache and their per-lobe entry counts. Useful after `INCACHE` / `OUTCACHE` (§2.10).

> **Note on `AUTOLINK` / `SHOW AUTOLINK` / `AUTOLINK APPLY` (status at v0.5.0).** These statements appear in some external surfaces — the server's write-classifier ([`xyzdb-server/src/connection.rs:289`](../crates/server/src/connection.rs#L289)), the Python SDK (`client.show_autolink()`), and the operational suite [`tools/validation/src/suites/s08_autodiscovery.rs`](../tools/validation/src/suites/s08_autodiscovery.rs) — but are **NOT implemented** in the xyTalk parser. The parser has `parse_autoanchor_apply` ([`xytalk-parser/src/parser.rs:1259`](../crates/xytalk-parser/src/parser.rs#L1259)) and no equivalent for AUTOLINK; both `SHOW AUTOLINK` and `AUTOLINK APPLY` fail at parse time with `Unknown command`. The scaffolding is pre-v1.0 work; cleanup or completion is deferred (no mini-cycle in the v0.5.x → v0.6.0-pre track addresses it). Validation suite s08 AUTOLINK tests are not expected to pass against current engine builds.

---

### Tier 3 — Power user

Tuning, ghost lifecycle, anchor population over already-loaded data, manual routing override. Each statement here assumes you understand at least one engine subsystem (ghost lifecycle, anchor dictionary semantics, projection pinning, scan telemetry).

#### 2.13 PULL — Graph Traversal

```
-- As pipeline step
FIND ... | PULL [depth=N] [only=Type]

-- Standalone
PULL FROM "lobe" [depth=N] [only=Type]
```

**Examples:**

```text
-- Traverse co-located records (1 level deep)
FIND "clients" WHERE name="Acme Corp" | PULL depth=1

-- Deep traversal: company -> projects -> tasks -> subtasks
FIND "clients" WHERE name="Acme Corp" | PULL depth=3

-- Filter by type
FIND "clients" WHERE name="Acme Corp" | PULL depth=2 only=Invoice
```

**Behavior:**
- Retrieves all records sharing the same `gravity_hash` (co-located via gravity)
- Follows `_link_*` fields recursively up to `depth` levels
- `only=Type` filters results to matching `_type` field
- Default depth: 1
- No hard limit on depth — recursion stops when no more `_link_*` fields found

#### 2.13.1 FOLLOW — Cross-Entity Expansion (pipeline step, v0.8)

```
-- Pipeline step only (no standalone form)
FIND ... | FOLLOW <field> TO "<lobe>" ON <target_field>
SCAN ... | FOLLOW <field> TO "<lobe>" ON <target_field>
```

The relational bridge `PULL` cannot cross. `PULL` stays inside one gravity bucket (co-located records of the same entity); `FOLLOW` jumps *across* buckets and lobes: for each record produced so far, it takes the value of `field` and resolves it as `target_field` in `lobe`, fetching the matching records. "Chat message → its cited document (a different entity)" becomes one pipeline step.

**Examples:**

```text
-- Each message cites a document by id; fetch those documents
FIND "messages" WHERE thread = "T-1"
    | FOLLOW cited_doc TO "documents" ON doc_id

-- Resolve each order's customer record
SCAN "orders" WHERE status = "open"
    | FOLLOW customer_ref TO "customers" ON customer_id
```

**Behavior** (verified against `ops/follow.rs`):

- Resolution uses the target lobe's `FIND` fast path (anchor / gravity lookup) on `target_field = <value>` — no full scan when the target field is an anchor or the lobe's gravity field. Otherwise it falls back to a full scan of the target lobe, run once per distinct source value, so the cost scales with (distinct source values × target lobe size); prefer following into an anchor or gravity field.
- The source `field` value must be **text**; a record whose `field` is missing or not a text value is skipped (not an error).
- Results are **deduplicated** twice: by reference value (each distinct `field` value is followed once) and by target LID (the same target record is emitted once).
- Output is the set of fetched target records — the source records are replaced, not merged.

#### 2.14 SCAN GHOST — Direct Ghost Scan

```
SCAN GHOST "name" [WHERE filters] [LIMIT n]
```

**Examples:**

```text
SCAN GHOST "overdue_by_date" LIMIT 1000
SCAN GHOST "overdue_by_date" | AGGREGATE count(), sum(amount)
```

**Note:** Regular `SCAN` is automatically routed to a ghost if relevant. `SCAN GHOST` forces a specific ghost — useful for diagnostics, benchmarks, or when you want to bypass the router for a one-off.

**`NEAREST` is not supported through `SCAN GHOST`.** Piping `SCAN GHOST "name" | NEAREST(...)` returns the ghost's index entries — null LIDs, only the embedded fields — not resolved records. Use a plain `SCAN "lobe" WHERE ... | NEAREST(...)`: the router still uses a matching ghost for the filter and falls back to primary point-reads for the vector, returning correct records.

The `WHERE` accepts the full `OR`/`NOT`/`IN` tree (§3). An AND-pure predicate pushes into the ghost's ordered read with early-out at the `LIMIT`; `OR`/`NOT` reads the ordered entries and filters them with the shared walker before truncating — same result set, so no silently-unfiltered rows escape.

#### 2.15 CREATE GHOST — Materialised view (Permanent ghost)

A ghost is a saved query, so it declares like one — the **canonical pipeline form**:

```
CREATE GHOST "name" FROM "lobe" [WHERE filter_expression]
    [| GROUP BY field [, field ...]]
    [| AGGREGATE func() [, func() ...]]
    | TAKE BY (field | metric) [DESC|ASC]
    [| EMBED field [, field ...]]
```

The classic **clause form** is a live alias (the same statement):

```
CREATE GHOST "name" FROM "lobe" [WHERE filter_expression]
    ORDER BY (field | metric) [ASC|DESC]
    [GROUP BY field [, field ...]]
    [AGGREGATE func() [, func() ...]]
    [EMBED field [, field ...]]
```

> **The order declaration is mandatory.** In the pipeline form it is the final `| TAKE BY <target>` step (no `n` — that is a query-time count, not part of the declaration); in the clause form it is `ORDER BY <target>`. A `<target>` that is a record field gives the classic covering-entry iteration order; a `<target>` that is a declared aggregate metric gives a metric-ordered rollup. Both forms fill the identical statement; the pipeline form is preferred because it reads exactly like the `SCAN` the ghost serves.

> **Metric-ordered rollup**: when the order target is a declared aggregate (e.g. `| TAKE BY sum(monto) DESC`) on a `GROUP BY … AGGREGATE …` ghost, the ghost keeps a second rollup ordered by that metric. `SCAN … | GROUP BY … | AGGREGATE … | TAKE n BY <same metric+direction>` then reads only the first N groups — **O(N)** — instead of materialising all M groups and quickselecting (O(M)). The metric must name one of the `AGGREGATE` metrics; direction is `DESC` (default) or `ASC`. It is rebuilt in full on every `CREATE`/`REFRESH` (blind-insert, no write-path cost); between rebuilds it serves an as-of-last-pass snapshot whose age is shown in `SHOW GHOSTS`. A `TAKE` by a different metric or direction transparently falls back to the O(M) path.

**Examples:**

```text
-- Permanent index on overdue installments, sorted by due_date (covering)
CREATE GHOST "overdue_by_date" FROM "fintech"
    WHERE _type="Installment" AND status="overdue"
    | TAKE BY due_date

-- TopN-friendly ghost with embedded fields (avoids hydrate from primary)
CREATE GHOST "active_by_monto" FROM "fintech"
    WHERE _type="Credit" AND status="active"
    | TAKE BY monto DESC
    | EMBED monto, rfc, status

-- Pre-computed group aggregate — PreComputed-routable (clause-form alias)
CREATE GHOST "credits_by_rfc" FROM "fintech"
    WHERE _type="Credit"
    GROUP BY rfc
    AGGREGATE count(), sum(monto)

-- Metric-ordered group aggregate — serves TAKE n BY sum(monto) in O(N)
CREATE GHOST "top_exposure_by_rfc" FROM "fintech"
    WHERE _type="Credit" AND status IN ["active","overdue"]
    | GROUP BY rfc
    | AGGREGATE count(), sum(monto)
    | TAKE BY sum(monto) DESC
```

**Behavior:**
- Scans the source lobe, applies the `WHERE` filter, writes matching records to the ghost keyspace.
- **Automatic projection** — only fields needed by the query pattern are stored. Redundant fields (Eq filter constants) are not stored; injected from metadata on read.
- **`EMBED`** — operator-supplied projection. Listed fields are stored on the ghost entry to avoid hydrating from the primary keyspace on read. Used for TopN queries where the projection-from-filter heuristic alone misses fields the response shape needs.
- **`PIN`ned fields** (via the `PIN` statement, §2.18) are also included in the projection automatically.
- **`GROUP BY` + `AGGREGATE`** — creates a ghost that persists per-group accumulator state (`group_summaries`) in its meta. The router can serve `SCAN ... | GROUP BY ... | AGGREGATE ...` queries directly from this state via `ScanSource::GhostPreComputed`, with **zero scan** of the primary keyspace. See §5 Ghost routing for the precondition.
- **Auto-update on writes — no `REFRESH` required.** Every `PUT` invokes the ghost's `notify_write` hook; matching records update the ghost's index and group accumulators incrementally. This is the structural difference from a relational materialised view: an analyst-defined `CREATE GHOST ... GROUP BY ...` does not need a periodic `REFRESH MATERIALIZED VIEW` job. Staleness window is bounded by the WAL group-commit interval (~1 ms in `Durable` mode), not by a refresh schedule.
- `| TAKE BY <target>` (or the `ORDER BY` alias) records the ghost's iteration order for efficient ordered retrieval.
- Returns record count and projection info.

**Ghost classes:** a ghost has a `GhostType` of:
- **Permanent** — created by `CREATE GHOST`. No TTL, not auto-evicted.
- **Ephemeral** — auto-created when scan telemetry detects a hot pattern (default thresholds: ≥ 5 hits within a 10-minute sliding window AND avg latency ≥ 20 ms; flat AND filters only). 24 h TTL. Max 20 per lobe. LRU-evicted at the cap.
- **Promoted** — an Ephemeral accessed on ≥ 7 distinct UTC days within 30 days. Renamed in place, TTL extended to 30 days. No data re-scan. Max 5 per lobe.

Default thresholds (5 / 20 ms) are tunable via the server CLI flags `--auto-ghost-min-hits` and `--auto-ghost-min-latency-ms`. Setting `--auto-ghost-min-latency-ms 1e9` effectively disables auto-ghost creation; existing manual ghosts still work and Promoted survivors continue to serve, but no new Ephemerals appear. See §5 and §6.

#### 2.16 REFRESH GHOST / DROP GHOST

```
REFRESH GHOST "name"    -- Drop and recreate with same filters
DROP GHOST "name"       -- Delete permanently
```

#### 2.17 AUTOANCHOR APPLY — Operational anchor population

```
AUTOANCHOR APPLY "field" IN "lobe"
```

**Behavior:**
- **Operational.** Iterates the existing primary records in `<lobe>` and indexes them into the dictionary. This is the entry point that retroactively populates the anchor for a lobe loaded before the constraint was declared (e.g. after a bulk import).
- If no anchor was previously declared on `<field>`, APPLY also registers it; if one was declared (via `ANCHOR ... UNIQUE IN`, §2.2), APPLY skips registration and proceeds straight to populate.
- After APPLY, `FIND "lobe" WHERE field = X` resolves through the dictionary keyspace in O(1), instead of falling through to scan + bloom prune on the primary spatial keyspace.

**Examples:**

```text
-- Common ordering: declare + bulk load + apply
ANCHOR "rfc" UNIQUE IN "clientes"
PUT BATCH IN "clientes" [...]            -- 1.5 M rows in ≤10K chunks (§2.4)
AUTOANCHOR APPLY "rfc" IN "clientes"     -- populates dictionary for all 1.5 M
```

**Idempotency contract** (post Finding 12):

| Sequence | Result |
|---|---|
| `ANCHOR ... UNIQUE` then `AUTOANCHOR APPLY` | OK — register skipped, populate runs |
| `ANCHOR ... UNIQUE` declared **twice** on same `(lobe, field)` | **Error** — declarative path keeps strict semantics |
| `AUTOANCHOR APPLY` with no prior `ANCHOR` declaration | OK — registers and populates in one operation |
| `AUTOANCHOR APPLY` run **twice** on same `(lobe, field)` | OK — both registration and populate are idempotent; second call re-walks primary, dictionary entries already present are detected as duplicates and skipped without error |

The fourth row matters operationally: a migration script that runs `AUTOANCHOR APPLY rfc IN clientes` repeatedly (idempotent re-run on retry, on staging-vs-prod copy, in CI fixtures) is safe. This is the failure mode the bench enhanced runs surfaced and that Finding 12 closed.

**Response shape.** APPLY returns a message reporting both new entries written and duplicates encountered:

```
Anchor 'rfc' applied in 'clientes': 1500000 indexed, 0 duplicates found
```

A non-zero `duplicates` count means the dictionary already held an entry for that value (typical when PUTs preceded APPLY — the PUT path also writes the anchor entry as a side effect). Duplicates are reported, not re-inserted; the contract is "no error", not "no duplicates".

#### 2.18 PIN / UNPIN — Field Pinning

```
PIN field1, field2, ... IN "lobe"
UNPIN field1, field2, ... IN "lobe"
```

**Examples:**

```text
-- Ensure category and region are always in ghost projections
PIN category, region IN "fintech"

-- Remove pin
UNPIN region IN "fintech"
```

**Behavior:**
- Pinned fields are included in ghost projections even if not in the query filter/ORDER BY
- Persisted in dictionary keyspace; survives restart
- Useful when a field is frequently read alongside the records returned by ghost-routed queries but does not appear in the filter or ORDER BY shape

#### 2.19 SHOW (tuning)

Inspect engine internals for tuning. For introspection of what data exists, see §2.12.

```
SHOW SCAN STATS
SHOW PROFILE "lobe"
SHOW THROTTLE
```

**Behavior:**
- `SHOW SCAN STATS` — recent scan patterns observed by `ScanTelemetry`, with hit counts, rolling avg latency, and `AutoGhostCandidate` markers. Used to inspect what auto-ghost is about to promote (see §5).
- `SHOW PROFILE "lobe"` — pinned fields, learned scan patterns for this lobe, active ghosts (with `[projected]` marker indicating which fields are stored on the ghost entry), and the searchable vector field: `Vector: <field> dim <n>` (or `dim unknown` before the first embedding fixes it), or `Vector: (none)` when the lobe has none.
- `SHOW THROTTLE` — current write throttle state across all lobes (Healthy / Degraded / Critical / Paused) and the active throttle profile (`balanced`, `transactional`, `analytical`, `bulk`, `maintenance` — see §6).

#### 2.20 VECTOR / NEAREST — Searchable embeddings (v0.8)

The searchable-vector surface. `VECTOR` declares which field is a lobe's searchable embedding; `NEAREST` returns the top-k records whose embedding is most similar to a query vector, scored **exactly** within a gravity bucket. `NEAREST` has a canonical **phrase form** and a function-call **alias** that parse to the identical node. **`ORBIT` is removed** — it was an unused synonym, and one name per concept is the rule.

```
-- Declare the lobe's searchable embedding field (one per lobe)
VECTOR <field> IN "<lobe>"

-- Semantic top-k, as a pipeline step over a (gravity-bounded) scan
... | NEAREST <k> BY <field> TO <query> [USING <metric>]   -- canonical phrase form
... | NEAREST(<field>, <query>, <k>, <metric>)             -- function alias
```

**`VECTOR <field> IN "<lobe>"`** names the single `Value::Vector` field hoisted to the record's on-disk vector prefix / vector column and swept by `NEAREST`. It is a foundational axis, **sibling to (not part of) `GRAVITY BY`**: gravity decides *placement* (which bucket a record lands in), the searchable vector decides *what is searched*. A record co-locates by (say) topic and searches by its embedding — orthogonal concerns. Declared before the first write; persisted in the dictionary, survives restart.

**`NEAREST k BY field TO query [USING metric]`** (phrase) / **`NEAREST(field, query, k, metric)`** (function alias) — keep the `k` records (over the records the pipeline produced) whose `field` embedding is most similar to `query` under `metric`:

- **`field`** — the lobe's declared `VECTOR` field.
- **`query`** — one of:
  - an inline list literal, e.g. `[0.1, -0.4, …]`;
  - a bound parameter `$q` (preferred — the vector travels out-of-band via the protocol, so a 256-/768-float literal never lands in the query string);
  - `REF "id"` — "more like this": use the embedding of the uniquely-matching scanned record whose field value equals `id`, and exclude that record from the results.
- **`k`** — number of records to return.
- **`metric`** — `cosine` (alias `cos`), `dot` (alias `inner`), or `l2` (alias `euclidean`), case-insensitive. In the phrase form `USING` is optional and defaults to `cosine`; the function form requires it.

**Examples:**

```text
-- Declare the searchable embedding
VECTOR embedding IN "memories"

-- Top-8 most similar within a gravity bucket, query bound out-of-band (phrase form)
FIND "memories" WHERE topic = "billing"
    | NEAREST 8 BY embedding TO $q USING cosine

-- USING omitted → defaults to cosine
SCAN "memories" WHERE topic = "billing"
    | NEAREST 8 BY embedding TO $q

-- More-like-this: nearest to a known record, excluding itself (function alias)
SCAN "memories" WHERE topic = "billing"
    | NEAREST(embedding, REF "mem-42", 5, cosine)
```

**Behavior:**
- **Gravity-bounded EXACT brute-force.** `NEAREST` scans the records the pipeline produced (typically one gravity bucket) and scores every candidate exactly. There is **no ANN / HNSW / IVF index** — results are exact within the bucket, not approximate.
- The engine **never embeds**: the caller supplies the query vector (`$q`, an inline list, or `REF "id"`), embedded with the same model the corpus used. No network call happens on any path.
- Internally a fused `Scan`+`Nearest` fast path ranks candidates from the cheap on-disk vector prefix / `vectors` column (not the full record blobs) and hydrates only the surviving top-k — bit-identical to the unfused path. See `docs/architecture.md` §3.7.
- **Result shape and the latency wall.** `NEAREST` returns plain `QueryResult::Records` for a complete answer — whether that is a full top-`k` or a complete-but-short set (fewer than `k` rows pass a residual filter). A wall-clock safety budget bounds how long a single query runs; it is a **latency** wall, never a recall wall. If it expires while hydrating a very selective residual (matches must be found by descending the bucket in score order), the engine does **not** fail — it returns the highest-scoring passers found so far as `QueryResult::PaginatedRecords { has_more: true, cursor: None }`. This partial is **prefix-correct when it was produced in score order** (an exact prefix of the true ranking, not an arbitrary sample) — the `strategy` field below says which order produced it, because the engine may walk candidates in key order instead when that is cheaper, and a partial cut there is the best of a key region rather than a prefix. It carries **no cursor** — unlike SCAN pagination, a `NEAREST` truncation is not resumable, since resuming would repeat the whole scoring pass. Read `has_more` as "these are the best found within budget; more, lower-scoring matches may exist".

That truncated frame additionally carries a `budget_stop` object describing the cut:

| Field | Meaning |
|---|---|
| `candidates` | The whole **scored** set in score order (the bucket), *before* the residual filter. NOT the number of filter matches. |
| `examined` | How many of those candidates had their residual filter **checked** (were hydrated) before the budget expired. Counts passers and non-passers alike. |
| `found` | How many **passed** — the survivors returned. Equal to `records.len()` by construction. |
| `strategy` | Which order the candidates were walked in: `"score_order"` (best-first ⇒ the rows are a true **prefix** of the answer) or `"key_order"` (sequential I/O ⇒ the rows are the best of a contiguous **key region**, and the unwalked part may hold better ones). It names the fact, not the implementation. |

Read them as one sentence: *"scored `candidates`, checked the filter on `examined` of them, `found` passed."* In the selective case that motivates the airbag, almost none of the scored candidates pass — e.g. `{ examined: 238000, candidates: 246000, found: 6 }` reads "scored 246k, checked 238k, 6 passed", **not** "246k matched".

The frame also carries `strategy`, and it changes what the counts license you to conclude. **Two different readings live in the trio, and only one of them survives both traversal orders — so keep them apart:**

| Reading | `strategy: "score_order"` | `strategy: "key_order"` |
|---|---|---|
| **How many remain** — `examined / candidates` is the fraction checked; `found / examined` is the filter's observed pass rate, so 6 passers in 238k checked extrapolates to a fraction of a row across the ~8k unchecked | **Valid** | **Valid** — a contiguous key region is not a score-biased sample, so the rate still estimates |
| **Whether what remains is worse** — i.e. reading the returned rows as the best ones and the rest as lower-scoring | **Valid**: candidates were walked best-first, so the rows are a true PREFIX of the answer and nothing unexamined scores better | **NOT valid**: candidates were walked in key order for sequential I/O, so the rows are the best of a contiguous KEY REGION and the unwalked part may hold **better** rows, not merely more |

So under `"score_order"` a client can upgrade "there may be more" to "almost certainly not". Under `"key_order"` it can still say "almost certainly few remain", but **not** "and they are worse". Both are inferences the client draws; the engine only reports which order produced the partial.

The object describes the **cut, not the set**. `examined` is what was checked, not what exists; and because there is no cursor, `candidates - examined` is **not** "the remainder, request it" — those candidates are unchecked, not pending, and there is no call that returns them. The only responses to a `budget_stop` are: raise `--nearest-budget-ms`, narrow the query scope, or accept the partial. `budget_stop` is present **only** on this truncated NEAREST frame; every other `PaginatedRecords` omits it, so ordinary pagination frames are byte-identical to before it existed.

---

### Tier 4 — Operator (deprecated as language)

Administrative operations that should not appear in application code paths. Deprecated as language statements in v0.2.5.1+; the engine accepts them for backward compatibility with existing drivers, benchmarks, and validation suites and emits a `tracing::warn` deprecation notice on every invocation. **They will be retired from the grammar in v0.3.0.**

The successor surface is the `xyzdb-cli admin` subcommand (a thin wrapper that builds the equivalent xyTalk string and ships it over the standard V1 protocol — no new wire shape).

#### 2.21 ADMIN — COMPACT / ANALYZE / BULKMODE / MIGRATE

**Deprecated language form** (still accepted in v0.2.5.x):

```
COMPACT
ANALYZE "lobe"
BULKMODE ON | OFF
MIGRATE "lobe"
MIGRATE
```

**Replacement — `xyzdb-cli admin`:**

```bash
xyzdb-cli admin compact                  # COMPACT (every keyspace)
xyzdb-cli admin analyze <lobe>           # ANALYZE "<lobe>"
xyzdb-cli admin bulkmode <on|off>        # BULKMODE ON / OFF
xyzdb-cli admin migrate <lobe>           # MIGRATE "<lobe>" (single lobe)
xyzdb-cli admin migrate --all            # MIGRATE (every lobe)
```

**Per-verb behavior:**
- `COMPACT` — runs `major_compact()` on all 5 keyspaces. Reduces SSTable count, bloom filter RAM, and improves point lookup performance.
- `ANALYZE` — samples up to 10,000 records of the lobe. Reports per-field cardinality (HIGH/MEDIUM/LOW/CONSTANT), dominant type, length range, and suggestions (ANCHOR candidates, gravity candidates, ghost filter candidates). Auto-creates dictionary encodings for low-cardinality TEXT fields (cardinality < 1000, lobe ≥ 1000 records).
- `BULKMODE ON | OFF` — toggles bulk-load mode (relaxed durability + write conversion to sequential).
- `MIGRATE "<lobe>" | (no arg)` — runs format migration on a single lobe or every lobe.

**Deprecation notice on every invocation** (server-side `tracing::warn`):

```
Statement {NAME} is deprecated as language statement;
use 'xyzdb-cli admin {name}' in v0.3+.
Will be removed from grammar in v0.3.0.
```

**Note**: The deprecation does **not** apply to `INCACHE` / `OUTCACHE` (§2.10) — those are operator-grade *workload tuning* expected to drive query routing from inside the language, not administrative housekeeping.

#### 2.22 SCRUB — On-Disk Integrity Verify

```
SCRUB
```

**Behavior:**
- **Read-only.** Verifies on-disk integrity across every keyspace — SST block checksums plus the `MANIFEST` — and reports any corruption it finds. It **alerts, never repairs**: SCRUB never mutates data. Safe to run on a live database.
- Not part of the v0.2.5.1 deprecation set (unlike `COMPACT` / `ANALYZE` / `BULKMODE` / `MIGRATE` above); it is a current operator-grade verb.

---

## 3. FILTER OPERATORS

### Comparison Operators

| Operator | Syntax | Description |
|----------|--------|-------------|
| Equal | `=` | Exact match |
| Not Equal | `!=` | Negation |
| Greater Than | `>` | Strict greater |
| Greater or Equal | `>=` | Greater or equal |
| Less Than | `<` | Strict less |
| Less or Equal | `<=` | Less or equal |
| Is Null | `IS NULL` | Field missing or explicitly Null |
| Is Not Null | `IS NOT NULL` | Field exists and is not Null |
| In | `IN [a, b, …]` | Field equals any listed value. `[...]` is canonical (`[]` is the list delimiter, §1); `IN (a, b, …)` is an accepted parenthesized alias |
| Contains | `CONTAINS` | List contains element (exact match) |

### Boolean Operators (V4)

| Operator | Syntax | Precedence |
|----------|--------|-----------|
| NOT | `NOT expr` | Highest (binds tightest) |
| AND | `expr AND expr` | Middle |
| OR | `expr OR expr` | Lowest |

Parentheses override precedence: `(expr OR expr) AND expr`.

**Examples:**
```text
WHERE status = "active" OR status = "overdue"
WHERE NOT status = "cancelled"
WHERE (status = "overdue" OR status = "defaulted") AND amount > 5000
WHERE NOT (status = "cancelled" OR status = "archived")
```

### Dot Notation (V4)

Field names support dot notation for nested Map/List access:

```text
WHERE scoring.bureau > 650        -- Map field access
WHERE items.0.price > 100         -- List index + Map field
WHERE config.rules.0.weight >= 0.5
```

If any segment of the path doesn't exist, the record is excluded.

### CONTAINS (V4)

Tests if a List contains an element (exact equality match):

```text
WHERE tags CONTAINS "tech"
WHERE scores CONTAINS 85
```

CONTAINS only works on List values. On non-List fields, returns false (no error).

### Cross-type Comparison

Int and Float are compared by converting both to f64. All other cross-type comparisons fail (record excluded). Null is incomparable with any other type (returns false for >, <, >=, <=).

### Missing Fields

If a filtered field doesn't exist in a record, the record is excluded — except for `IS NULL`, which matches both missing fields and explicit Null values.

---

## 4. PIPELINE SYNTAX

Pipelines chain operations with the `|` operator:

```
first_step | second_step [| third_step ...]
```

**First step must be:** `FIND`, `SCAN`, or `SCAN GHOST`

**Subsequent steps:** `PULL`, `SET`, `DELETE`, `AGGREGATE`, `GROUP BY`, `TAKE` (alias `TOP`), `SHAPE`, `NEAREST`, `FOLLOW`

**Valid combinations:**

| Pipeline | Result Type | Memory |
|----------|-------------|--------|
| `FIND \| PULL` | Records (graph) | O(n) |
| `FIND \| SET` | Ok (count updated) | O(n) |
| `FIND \| DELETE` | Ok (count deleted) | O(n) |
| `SCAN \| AGGREGATE` | Aggregation | **O(1)** streaming |
| `SCAN \| GROUP BY \| AGGREGATE` | Grouped Aggregation | **O(groups)** |
| `SCAN \| GROUP BY \| AGGREGATE \| TAKE n BY metric` | Grouped Aggregation (top-N) | O(N) on a metric-ordered ghost, else O(M) |
| `SCAN \| PULL` | Records | O(n) |
| `SCAN \| TAKE n` | Records (first n) | O(n) |
| `SCAN \| SHAPE {…}` | Records (projected) | O(n) |
| `SCAN GHOST \| AGGREGATE` | Aggregation | O(n) |
| `FIND \| NEAREST ...` | Records (top-k) | O(bucket) |
| `SCAN \| NEAREST ...` | Records (top-k) | O(bucket) |
| `FIND \| FOLLOW ... TO ...` | Records (cross-entity) | O(n) |
| `SCAN \| FOLLOW ... TO ...` | Records (cross-entity) | O(n) |

**Note:** `GROUP BY` is only valid as the middle step in a `SCAN | GROUP BY | AGGREGATE` pipeline. It cannot be used standalone or in other positions.

---

## 5. AUTO-OPTIMIZATION

### Auto-Ghost Creation

xyzDB tracks scan patterns automatically. When a pattern accumulates ≥ `min_hits` within a 10-minute sliding window AND its rolling average latency ≥ `min_latency_ms`, the next scan returns an `AutoGhostCandidate` and the engine spawns a background worker to materialise an Ephemeral ghost.

Defaults (in `scan_telemetry.rs`):

- `min_hits = 5`
- `min_latency_ms = 20.0`

Tunable via the server CLI (§6): `--auto-ghost-min-hits N`, `--auto-ghost-min-latency-ms F`.

The auto-ghost inherits the scan's filters and `ORDER BY`, plus aggregate specs and pinned fields for optimal projection. Only **flat AND filter expressions** are eligible — OR / NOT shapes are not auto-promoted.

Ephemeral ghosts have a 24 h TTL; Promoted ghosts (≥ 7 distinct daily access bits within 30 days) have 30 d. LRU-evicted at per-lobe caps. See §2.15 for the full lifecycle.

### Ghost Routing

When a `SCAN` (or pipeline starting with `SCAN`) is executed, `GhostRouter::plan_scan` returns one of three sources:

- **`Primary`** — scan the spatial keyspace. Default when no ghost matches.
- **`Ghost(name)`** — iterate the named ghost's keyspace and hydrate from primary if needed. Selected when a ghost's `filter_fields` are a subset of the query's flat AND predicates.
- **`GhostPreComputed(name)`** — serve aggregates directly from the ghost's persisted `group_summaries`, **zero scan**. Selected when the query is `SCAN ... | GROUP BY ... | AGGREGATE ...` against a ghost that was created with matching `GROUP BY + AGGREGATE`.

**PreComputed precondition** (post Finding 11): every predicate in the query's `WHERE` clause must be either:

- a ghost-constant filter (already present in `meta.filter_fields`), or
- an `Eq` predicate on a field listed in `meta.group_fields`.

Any other predicate disqualifies the ghost from PreComputed; the router falls back to `Primary` (or to `Ghost(name)` if a non-PreComputed match exists).

**Examples** — given `CREATE GHOST credits_by_rfc ... WHERE _type = "Credit" GROUP BY rfc AGGREGATE sum(monto), count()`:

| Query | Routes to | Why |
|---|---|---|
| `SCAN ... WHERE _type = "Credit" AND rfc = "X" \| GROUP BY rfc \| AGGREGATE sum(monto), count()` | `GhostPreComputed` | `_type` is ghost-constant, `rfc = "X"` is Eq on group key |
| `SCAN ... WHERE _type = "Credit" AND status = "active" \| GROUP BY rfc \| AGGREGATE sum(monto), count()` | `Primary` | `status` is neither ghost-constant nor a group key |
| `SCAN ... WHERE _type = "Credit" AND rfc > "M" \| GROUP BY rfc \| AGGREGATE sum(monto), count()` | `Primary` | range op on group key, not Eq |

This guarantees the pre-computed group entries returned to the caller satisfy every `WHERE` clause — not just those covered by the ghost definition. Range operators on group keys (`!=`, `<`, `<=`, `>`, `>=`, `IN`) are post-v0.6 scope (deferred until a richer PreComputed lookup design exists).

**Routing is transparent** — the query text is the same regardless of which source serves it. Operators inspect routing decisions via `SHOW SCAN STATS` (§2.19) and the `/stats` endpoint.

**Conservative routing for OR / NOT.** Ghost routing requires pure AND filter expressions. Queries using `OR` or `NOT` always scan the primary keyspace, even when a ghost's flat filter set would superficially match — this is a correctness guarantee. A ghost created for `WHERE status = "overdue"` cannot serve `WHERE status = "overdue" OR status = "defaulted"` because it lacks the "defaulted" records.

**Transparent fallback** (Finding 1). If the ghost the router selected has been evicted between `plan_scan` and the actual read, the scan path catches `Err(XyzError::GhostNotFound)`, unregisters the stale entry, and re-executes against `Primary`. Invisible to the caller.

### Dictionary Encoding

`xyzdb-cli admin analyze <lobe>` (§2.21) automatically detects TEXT fields with cardinality < 1,000 and creates bidirectional dictionary codecs. These are applied during ghost creation (Text → Int code) and reversed on read.

The cardinality count is **exact** — `xyzdb-engine/src/analyze.rs` uses a `HashSet<u64>` over xxh3-hashed values ([`analyze.rs:54-60`](../crates/engine/src/analyze.rs#L54)) and reports `value_hashes.len()`. There is no probabilistic estimator (HyperLogLog or otherwise) in the analyze path; the V3.2 design draft mentioning "AUTOANCHOR with HLL" was an aspiration, not the implementation.

---

## 6. SERVER FLAGS

```bash
xyzdb-server \
    --path ./data/xyzdb \
    --port 2505 \
    --bind 0.0.0.0 \
    --throttle-profile balanced \
    --memory-budget-mb 8192 \
    --storage-profile ssd \
    --durability durable \
    --batch-interval 100
```

| Flag | Values | Default | Description |
|------|--------|---------|-------------|
| `--throttle-profile` | transactional, analytical, balanced, maintenance, bulk | balanced | Query throttling |
| `--memory-budget-mb` | MB | cgroup limit, else 1024 | The single memory knob: block cache is derived from it, and ingest backpressure bounds memtable growth against it. (`--cache-size` is a deprecated, hidden override.) |
| `--storage-profile` | ssd, hdd | ssd | HDD: larger blocks, more bloom bits |
| `--durability` | durable, batched, async | durable | Write durability mode |
| `--batch-interval` | ms | 100 | Fsync interval for batched mode |
| `--auto-ghost-min-hits` | N | 5 | Hits per pattern within the 10-min window before auto-promotion |
| `--auto-ghost-min-latency-ms` | F | 20.0 | Avg latency threshold for promotion. `1e9` disables auto-ghost |

A side `/stats` HTTP-style endpoint on the same TCP port emits a JSON snapshot of engine internals (ghost counts, lifecycle states, sync-thread health, per-tree memtable / SST counts, process and cgroup memory). Operators can scrape it directly into Prometheus / Grafana without an add-on. See `docs/architecture.md` §10.

---

## 7. SYNTAX RULES

- **Keywords** are case-insensitive: `PUT`, `put`, `Put` all work
- **Lobe names** can be quoted (`"fintech"`) or unquoted (`fintech`) if alphanumeric
- **Field names** are unquoted identifiers: alphanumeric, `_`, `.`
- **Values** must use the appropriate literal syntax (quoted text, bare numbers, etc.)
- **Gravity fields** use `*` prefix without space: `*company`
- **Comments** use `--` (stripped before parsing)
- **Whitespace** is flexible between tokens
- **UTF-8** is fully supported in string values and filters

---

## 8. LIMITATIONS

| Limitation | Detail |
|------------|--------|
| No OFFSET | Use opaque `CURSOR` token instead (§2.6) |
| No HAVING | Cannot filter GROUP BY results (use application code) |
| No subqueries | No nesting of queries |
| No LIKE/regex | No pattern matching on text |
| No multi-field ORDER BY | Single field only |
| ORDER BY requires LIMIT | Full sort without limit is not supported |
| No transactions | No BEGIN/COMMIT; individual operations are atomic |
| count() only | No count(field), count(DISTINCT). `count(*)` is an accepted alias of `count()` |
| CONTAINS only on List | No substring search on Text, no key search on Map |
| Ghost routing AND-only | OR/NOT queries always scan primary keyspace |
| PreComputed group-key Eq only | Non-`Eq` operators on `GROUP BY` fields disqualify the ghost from PreComputed (router falls back to Primary). Range support is post-v0.6 grammar scope. |
| Cursor + ORDER BY rejected | Paginated sort needs a richer payload — post-v0.6 grammar scope. |
| Cursor + ghost routing rejected | Engine forces `ScanSource::Primary` when a cursor is present — post-v0.6 grammar scope. |
| Null = Null is TRUE | Differs from SQL standard (see Null Semantics section) |
| 256 MB max response | Frame size limit for monolithic TCP protocol |

### Resolved in V4 (no longer limitations)

| Was | Now |
|-----|-----|
| No NULL | `null` literal + `IS NULL` / `IS NOT NULL` operators |
| No OR/NOT | Full boolean expressions with AND/OR/NOT and parentheses |
| No GROUP BY | `SCAN \| GROUP BY field \| AGGREGATE funcs()` |
| No List/Map syntax | `[1, 2, 3]` and `{key: val}` literals with nesting |
| No nested field access | Dot notation: `scoring.bureau`, `items.0.price` |
| No streaming | Chunked TCP streaming for SCAN without ORDER BY |

### Resolved in V5 / v0.2.5.1

| Was | Now |
|-----|-----|
| No pagination on large SCAN | Opaque `CURSOR "<token>"` clause on `SCAN` (§2.6) |
| Unbounded SCAN walked entire lobe | Default `LIMIT 1000` cap + `LIMIT > 10000` rejected (§2.6) |
| `WHERE` rejected on standalone `SET` / `DELETE` / `LINK` | Accepted on all three forms (§2.7 / §2.8 / §2.9) |
| `INCACHE` / `OUTCACHE` undocumented + bare-keyword silently OK | Documented (§2.10); bare keywords rejected at parse time |
| `COMPACT` / `ANALYZE` / `BULKMODE` / `MIGRATE` mixed into the language | Deprecated; use `xyzdb-cli admin <verb>` (§2.21) |

### Resolved / changed in xyTalk v1

| Was | Now |
|-----|-----|
| `TOP n BY metric` the only spelling | `TAKE` canonical (§2.11.1); `TOP` a deprecated alias; `TAKE n` (no BY) truncates |
| `ORBIT` canonical, `NEAREST` an alias | `NEAREST` canonical with a phrase form (§2.20); `ORBIT` removed |
| Only `NEAREST(field, q, k, metric)` function form | Phrase form `NEAREST k BY field TO q [USING metric]` canonical, function an alias |
| `DELETE "lobe"` emptied the lobe silently | `DELETE` requires `WHERE`; `PURGE "lobe"` is the explicit total-delete verb (§2.8 / §2.8.1) |
| `SET` / `DELETE` / `LINK` / `SCAN GHOST` `WHERE` was AND-only | Full `OR`/`NOT`/`IN` tree on all four (§2.7 / §2.8 / §2.9 / §2.14) |
| `CREATE GHOST` only in clause form | Query pipeline form canonical (`… \| GROUP BY … \| AGGREGATE … \| TAKE BY …`); clause an alias (§2.15) |
| `IN (a, b)` only | `IN [a, b]` canonical, `IN (a, b)` an alias (§3) |
| `count(*)` rejected | Accepted as an alias of `count()` (§2.11) |
| `FIND WHERE a OR b` silently dropped the tail | `FIND` rejects `OR`/`NOT` at parse, teaching `SCAN` (§2.5) |
| No projection — a read returned whole records | `\| SHAPE {f1, f2}` projects each record to the named fields (§2.11.2) |
| A multi-lobe context took one call per lobe | `FETCH "a","b" WHERE key = X [AS {…}]` reads N co-located lobes in one call (§2.5.1) |
