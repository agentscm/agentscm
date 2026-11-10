# Hub store consolidation

Status: proposed. Nothing in this document is implemented.

The Hub keeps its state in SQLite or PostgreSQL through one `Store` trait (`src/hub/store.rs:206`, 213 methods). Two
hand-written implementations sit behind it: `src/hub/store_sqlite.rs` (16,848 lines) and `src/hub/store_pg.rs` (15,325
lines), each with three small submodules. Every Hub feature that stores anything is written twice, reviewed twice and
conflicts twice: a Hub change that stores anything edits both files. This document measures how much of the
two files is really different, compares ways to stop writing it twice, and proposes an order of small steps that each land
green on their own.

## 1. Inventory

### How it was measured

On `main` at `a4ccedec`, a script listed every function in each file outside `#[cfg(test)]`. It paired functions by name,
and paired the 13 helpers that carry a `sqlite_`/`pg_` prefix by their stem. For each pair it compared the bodies after
removing differences that are pure dialect: placeholders (`?1` against `$1`), `params![...]` against `&[...]`, the client
calls (`query_row`/`query_opt`/`query_one`), `::bigint`/`::text` casts, and whitespace. A text similarity of 0.9 or more
means the bodies are the same apart from that; 0.6 to 0.9 means the same steps with more dialect noise or a different
layout (PostgreSQL writes several value-metrics functions on a few long lines); under 0.6 means the code is shaped
differently. It is a rough signal, used here only to size the problem; the examples below were read by hand.

### Totals

| | SQLite | PostgreSQL |
|---|---:|---:|
| Lines outside tests | 16,646 | 15,326 |
| Functions | 364 | 375 |
| Functions with a twin | 358 | 358 |
| Lines in twinned functions | 16,425 | 14,874 |
| Lines with no twin | about 80 | about 310 |
| SQL string literals | 570 | 593 |

Of the 16,425 SQLite lines that have a PostgreSQL twin, by similarity of the twin:

| Similarity | SQLite lines | Share |
|---|---:|---:|
| 0.9 or more (same logic, dialect only) | 7,849 | 48% |
| 0.6 to 0.9 (same steps, more dialect or layout) | 8,021 | 49% |
| under 0.6 (shaped differently) | 555 | 3% |

So about 97% of each file is the same behaviour written twice. What truly differs is small and concentrated:

- **Connection, locking and retry.** PostgreSQL connects lazily, retries serialization failures (`run`,
  `store_pg.rs:212`) and serializes concurrent Hub processes with advisory locks: migrations, event order, intent
  announces, publication creation, briefing delivery and repository capacity (`store_pg.rs:169`–`181`, 21 lock
  statements). SQLite has one connection behind a mutex (`store_sqlite.rs:218`), WAL mode (`store_sqlite.rs:276`) and 80
  `BEGIN IMMEDIATE` transactions. The two give the same answers for one Hub process; only PostgreSQL is built for several.
- **JSON access.** `json_extract(col, '$.x')` (32 lines) against `col::jsonb->>'x'` (62 lines).
- **Planner hints.** SQLite pins indexes in two queries (`INDEXED BY`, `store_sqlite.rs:184` and `:16492`);
  PostgreSQL has no equivalent.
- **Text ordering.** PostgreSQL adds `COLLATE "C"` in 16 places so ordering matches SQLite's byte order. Queries that sort
  text without it depend on the database's default collation (see drift below).
- **TLS and connection settings** (PostgreSQL only, about 50 lines).

### By area

Twinned functions grouped by what they serve (SQLite lines; the PostgreSQL lines are within 20%):

| Area | Functions | Lines | 0.9+ | 0.6–0.9 | under 0.6 |
|---|---:|---:|---:|---:|---:|
| Work: start, checkpoint, transition, recovery, reconciliation, insights, usage, value metrics | 89 | 5,057 | 2,849 | 1,710 | 498 |
| Coordination: sessions, intents, holds, hand-offs, requests | 58 | 3,076 | 1,358 | 1,718 | 0 |
| Skills and promises | 28 | 1,546 | 839 | 707 | 0 |
| Publication and forge | 35 | 922 | 289 | 633 | 0 |
| Change records and evidence | 32 | 815 | 428 | 387 | 0 |
| Catch-up and briefing (plus 13 prefixed helpers, 1,133 lines) | 13 | 669 | 270 | 399 | 0 |
| Accounts, tokens, devices, settings | 23 | 554 | 304 | 250 | 0 |
| Everything else | 67 | 2,653 | 1,327 | 1,269 | 57 |

The largest functions are near-identical: `start_in` (642 and 648 lines, 0.98; `store_sqlite.rs:4336`, `store_pg.rs:4192`),
`work_checkpoint_in` (291 and 287, 0.96; `:7111`, `:6939`), `create_coordination_request` (174 and 168, 0.94; `:3781`,
`:3641`), `touch_in` (189 and 213, 0.90; `:6024`, `:5849`). The low-similarity Work functions are mostly the same logic
split differently. For example, `work_who` is one function in SQLite (`store_sqlite.rs:480`) and two in PostgreSQL
(`store_pg.rs:587` and `work_who_in` at `:609`). `work_catchup` keeps 378 lines inline in SQLite against 123 lines plus
prefixed helpers in PostgreSQL.

Row mapping is duplicated with the queries: each pair turns the same columns into the same structs by hand. For example,
`sqlite_usage_summary` (`store_sqlite.rs:1527`) and `pg_usage_summary` (`store_pg.rs:1632`) build the same `TokenGroup` and
`CostGroup` values from the same column lists.

### What the conformance suite covers

26 of 161 integration test files run their store cases on both backends, about 297 cases, through a `conformance!` macro.
`tests/integration/hub_store_conformance.rs` is the largest (64 cases). PostgreSQL cases skip unless
`AGENTSCM_TEST_DATABASE_URL` is set. Linux CI and the signed local policy set it, so the PostgreSQL half runs before
merge but not in a contributor's default local run.

197 of the 213 trait methods are called directly by a test that runs on both stores. These 16 are not (some may be reached
indirectly through Hub flows; that was not traced): `coordination_list_scoped`, `get_or_create_setting`, `get_setting`,
`live_intents_scoped`, `local_ci_device_events`, `observation_retained`, `repository_row`, `repository_usage`,
`repository_work_overview`, `set_setting`, `user_for_identity`, `value_scope`, `work_get_scoped`,
`work_pr_target_scoped`, `work_reconciliation_get_scoped`, `work_recovery_read`.

### Drift

What was found, and how sure it is:

- **Text ordering (risk, not shown).** SQLite compares text byte by byte. PostgreSQL uses the database collation unless
  told otherwise. 16 PostgreSQL queries say `COLLATE "C"`. Any other `ORDER BY` on a text column can return a different
  order on a non-C database for names with non-ASCII or mixed-case text. No test runs PostgreSQL with a non-C collation,
  so whether any query actually differs is unknown.
- **Shape drift (shown).** The same function is split differently (`work_who`, `work_catchup`, `usage_summary`) or
  formatted differently (the PostgreSQL value-metrics code is on a few long lines). Each split was correct when it was
  written, but a fix to one side no longer maps line by line to the other, which is how behaviour drift starts.
- **Untested methods (shown).** The 16 methods above can drift without a failing test.
- **Concurrency (by design).** Advisory locks exist only on PostgreSQL because only PostgreSQL serves several Hub
  processes. That difference must stay.

No case was found where the two stores return different results for the same calls in one process. The suite would
catch most such cases for the 197 covered methods.

## 2. Options

Each option is scored on what it costs to reach, what it risks while getting there, and what it saves per future
feature.

### A. One SQL layer behind a dialect trait

Write each query once and give the store a small `Dialect` trait. The trait renders placeholders, JSON access,
collation and upserts, and supplies the connection, transaction and lock steps. Both stores become one implementation
parameterized by the dialect.

- Cost: high if done at once (all 213 methods), low per method if done by area.
- Risk: the transaction and lock model differs (one mutex against advisory locks and retries), so the trait must carry
  transaction scope, not just SQL text. Done in one change it would conflict with every open pull request.
- Saves: almost all of the double writing, once finished.

### B. Lift shared row mapping and pure logic above the stores

Move what is not SQL into shared code that both stores call. That covers turning rows into structs, validation,
decisions such as `guard` (already shared in `session.rs`), capacity rules and JSON shaping. The stores keep their own
SQL.

- Cost: low, function by function.
- Risk: low. Each move is a pure refactor that the existing conformance cases check.
- Saves: a third to a half of each pair (rough estimate from the largest pairs, which are mostly control flow around a
  few queries), but the SQL is still written twice.

### C. A query builder or shared query text

Use a query builder crate, or keep each query as one string written with `$N` placeholders and translate it to SQLite's
`?N` at prepare time. SQLite numbers `?NNN` explicitly, so the rewrite is exact.

- Cost: a builder is a new dependency and a rewrite of 570 queries into its API; shared text is a few dozen lines of
  translation plus moving strings.
- Risk: a builder hides the SQL that reviewers read today. Shared text keeps it visible, but cannot express the JSON,
  hint and collation differences without small dialect hooks, which is option A again.
- Saves: the duplicated SQL text, not the duplicated Rust around it.

### D. Generate one store from the other

Treat SQLite as the source and generate the PostgreSQL file (or the reverse) with a script.

- Cost: a translator for Rust plus SQL, which has to understand `rusqlite` against `postgres` client calls.
- Risk: high. Generated code is hard to review and debug, and the real differences (locks, retries, hints) must be
  written by hand as exceptions inside generated output.
- Saves: writing twice, but adds a third thing to maintain.

### E. Drop one backend for some profile

Run PostgreSQL only for hosted and team Hubs, and SQLite only for a single computer. Both already exist for exactly these
profiles, so this does not remove a backend; it only restricts which features each must support. Dropping SQLite entirely
would make the local Hub need a database server. Dropping PostgreSQL would lose multi-process serving.

- Cost: product decisions per feature; no code saved unless a feature is declared single-profile.
- Risk: features diverge by profile, which is the drift this document is trying to remove.
- Saves: little.

### Recommendation

Do **B, then A by area, using C's shared text as A's first form.** Lift pure logic and row mapping first: it is cheap,
safe and makes each remaining pair mostly SQL. Then move one area at a time onto shared query text with `$N`
placeholders and a small `Dialect` trait. The trait grows only as an area needs it (JSON access, collation, an index
hint) and owns the transaction and lock steps, so locking stays exactly as it is. Do not add a query builder, do not
generate code, and keep both backends for both profiles.

When finished, each Hub feature writes its queries once and its locking once per backend. A store pull request then
touches one shared file and, rarely, a dialect hook.

## 3. Migration order

Each step is one pull request that changes no behaviour, lands green on its own, and leaves both stores working. Steps in
different areas can land in any order and alongside feature work. A feature in flight in an area waits only for that
area's step, which is small.

**How each step is checked:**

1. Before moving an area, add conformance cases on both stores for every method in it that has none. Land them first,
   unchanged, so they pass against the old code.
2. The move itself changes no test. The full conformance suite runs on both stores, as the signed local policy and Linux
   CI already require.
3. For moved SQL, a unit test renders each shared statement for SQLite and compares it with the old SQLite text (after
   placeholder translation). Any intended change shows up as a reviewed diff in that test.
4. Ordering: the first area that moves a text `ORDER BY` adds a PostgreSQL case on a database created with a non-C
   collation, and the dialect renders `COLLATE "C"` (or SQLite's default) in one place.

**Step 1 (first, small): settings.** Add one conformance case for `get_setting`, `set_setting` and
`get_or_create_setting`. They are untested on PostgreSQL today, and their SQL differs only in placeholders
(`store_sqlite.rs:9054`–`9085`, `store_pg.rs:8935`–`8966`). Add `src/hub/store_sql.rs` with
`fn sqlite(sql: &str) -> Cow<'_, str>`, which rewrites `$N` outside string literals to `?N`, and a unit test for it.
Move the three statements into `store_sql.rs` as constants used by both stores. That is about 60 changed lines and does
not touch the lock code. Open pull requests 398, 413 and 429 call these methods without changing them, and 398 adds
new settings queries to `store_pg.rs`, so land step 1 after 398 or rebase it onto 398. It proves the translation and the
review shape on something trivial.

**Step 2: row mapping, one area at a time.** Move hand-written row-to-struct code into `from_row`-style functions that
take plain values, shared by both stores. Start with the usage and insight summaries (`sqlite_usage_summary` and
`pg_usage_summary`, `sqlite_insight_input` and `pg_insight_input`), then catch-up pages. Pure Rust, no SQL changes.

**Step 3: pure logic out of the transactions.** Where a pair has the same decision code between queries (capacity
checks, cursor and page bounds, JSON shaping), move it into the module that owns the concept, as `session::guard`
already is. Start with Work start and checkpoint (`start_in`, `work_checkpoint_in`), the largest and most similar pairs,
in pieces.

**Step 4: the `Dialect` trait, minimal.** Introduce it with only what step 5's first area needs: placeholder rendering
and JSON field access. Transactions and locks stay in each store, called around the shared queries.

**Step 5 onwards: one area per pull request onto shared queries.** Suggested order, smallest and best-covered first:

1. accounts, tokens and settings;
2. change records and evidence;
3. skills and promises;
4. publication and forge;
5. coordination;
6. catch-up and briefing;
7. Work.

Each area first gains conformance cases for any untested method, then moves. Work goes last because it has the
largest functions and the most open feature work.

**Last step: one store type.** Once every area uses shared queries, the two store types differ only in connection,
transaction, lock and retry code. Merge them into one `SqlStore<D: Dialect>` with the two dialects as the only
backend-specific code. Until then, nothing forces this.

**Not in scope:** changing the schema, the migration files (already paired per backend and generated into one list by
`build.rs`), the locking model, or which backend a profile uses.

## Open questions

- Should PostgreSQL databases be required to use the C collation (a setup check), instead of `COLLATE "C"` in queries?
  That would remove the ordering risk at its root, but the operator owns that setting.
- The PostgreSQL value-metrics code is written on a few long lines that `rustfmt` leaves alone. Reformatting it is a
  pure layout change that would make the twins diff cleanly. Should it be its own step before step 2?
