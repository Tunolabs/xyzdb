# Change Dates by version

The Business Source License 1.1 applies **separately to each version** of
xyzDB. Each version carries its own Change Date, fixed at the moment that
version is first published and never changed afterwards. On its Change Date,
that version — and only that version — becomes available under the Change
License. Later versions are unaffected and continue under the BSL until their
own Change Dates arrive.

| Version | First published | Change Date | Change License |
|---|---|---|---|
| 1.1 (and the 1.1.x line) | 2026-08-01 | 2029-09-01 | Apache License, Version 2.0 |
| 1.0 (and the 1.0.x line) | 2026-07-30 | 2029-08-01 | Apache License, Version 2.0 |

## Rules we follow

**Patch releases inherit the Change Date of their minor line.** Everything in
the 1.0.x line converts on the same day as 1.0.0. A patch release does not
restart the clock, so this table has one row per minor version, not per release.

**A version's Change Date is set when it is published and is never extended.**
The parameters in a published LICENSE file are final. If we ever shorten the
window for a future version, that is a change to that version's LICENSE only,
and it appears as a new row here.

**The maximum window is four years.** The Business Source License requires the
Change Date to fall no later than four years after a version is first made
publicly available. We use three years, rounded as below.

**How the date is computed — the rounding is defined, including its edge.**
Take the publication date, add three years, then move to **the first day of the
following month**. Always the following month, even when the three-year date
already falls on a 1st. Worked both ways:

| Version | Published | + 3 years | Change Date |
|---|---|---|---|
| 1.0 | 2026-07-30 | 2029-07-30 | 2029-08-01 |
| 1.1 | 2026-08-01 | 2029-08-01 (already a 1st) | 2029-09-01 |

The edge is real and was decided deliberately: a version sealed on a 1st gains a
whole extra month over the three-year floor, where 1.0 gained two days. The
alternative rule — "the first 1st **on or after** three years" — would have given
1.1 a Change Date of 2029-08-01, exactly three years. Both are defensible; the
longer one wins for one reason, and it is the rule directly above: **a Change
Date is never extended, only ever shortened.** Erring long keeps the choice open,
erring short spends it permanently. The window is a floor of "no earlier than
three years", not a target of "exactly three years".

**Check the LICENSE file shipped with the release you are using.** That file is
the authoritative statement for that version. This table is a convenience.
