# Web Research: Database Management Workspace

Date: 2026-11-17. Scope: three topics; official primary documentation only.
Basis: [spec](../spec.md), [research decision](../research-decision.md).
Method: official pages opened; Tauri v2 documentation also queried through Context7 `/tauri-apps/tauri-docs`.
Status: research and proposed design constraints; no implementation or live-server verification.
Labels: **fact** is documented behavior; **design inference** is the proposed RestorePlane contract.

## 1. Transactions, cancellation, read-only and optimistic editing

### Engine guarantees and their limits

| Engine | Documented behavior | Design inference |
|---|---|---|
| PostgreSQL | Default READ COMMITTED takes a new statement snapshot; REPEATABLE READ retains a transaction snapshot; SERIALIZABLE may abort conflicting work. | Display effective isolation; do not promise one browsing snapshot across independent page transactions. |
| MySQL InnoDB | Default REPEATABLE READ keeps the first consistent-read snapshot; READ COMMITTED takes a fresh snapshot for each consistent read; locking reads differ. | Detect storage engine and effective isolation; transactional editing requires transactional tables. |
| MariaDB InnoDB | Default REPEATABLE READ; session/next-transaction settings differ and can be configured. | Probe its own version/capabilities rather than treating it as a MySQL version alias. |
| SQLite | Separate connections normally see committed changes; one writer at a time; WAL readers observe a read snapshot. | Bound read-transaction lifetime; handle BUSY and BUSY_SNAPSHOT explicitly. |

Sources: [PG isolation](https://www.postgresql.org/docs/current/transaction-iso.html),
[MySQL isolation](https://dev.mysql.com/doc/refman/8.4/en/innodb-transaction-isolation-levels.html),
[MariaDB isolation](https://mariadb.com/docs/server/reference/sql-statements/administrative-sql-statements/set-commands/set-transaction),
[SQLite isolation](https://www.sqlite.org/isolation.html).

- **Fact:** MySQL DDL and several administrative statements implicitly commit; even some temporary-table operations cannot be rolled back.
- **Design inference:** A user script containing DDL, procedures, transaction controls or nontransactional tables has no generic all-or-nothing guarantee.
- Generated grid edits use their own bounded transaction; they do not join an arbitrary editor session's open transaction.
- A multi-target operation is several independent target transactions; partial completion is a first-class result.
- No initial cross-target XA/2PC coordinator is promised; backup, restore and SQL operations do not become one distributed transaction.
- Source: [MySQL implicit commits](https://dev.mysql.com/doc/refman/8.4/en/implicit-commit.html).

### Outcome and retry semantics

- **Fact:** PostgreSQL defines `08007 transaction_resolution_unknown` and `40003 statement_completion_unknown`; error codes are preferable to localized text.
- Source: [PG SQLSTATE catalog](https://www.postgresql.org/docs/current/errcodes-appendix.html).
- **Design inference:** Track `accepted -> running -> commit_requested -> committed | rolled_back | outcome_unknown` for writes.
- `committed` requires positive target commit evidence; a local success record alone is insufficient.
- A lost socket/worker after dispatch or COMMIT, without conclusive target evidence, becomes `outcome_unknown`.
- Timeout, cancellation requested, connection closed, application crash and UI disconnect do not prove rollback.
- A rollback acknowledgment proves only the transaction it actually covered; earlier autocommitted script statements can remain committed.
- Never automatically resend arbitrary SQL after a timeout, transport retry, service restart or unknown commit outcome.
- Arbitrary SQL has no generic exactly-once guarantee: caller request IDs cannot atomically enlist an unrelated target database.
- Even identical final values do not prove one execution: triggers, sequences and external procedure effects may already have occurred.
- A local request ledger deduplicates command admission and reports the prior operation identity; it does not prove target execution count.
- Same idempotency key with a different payload hash is rejected, scoped by workspace/target/operation kind.
- A local transaction may atomically persist operation state plus outbox events in the Operational Store.
- Outbox delivery is at least once; consumers deduplicate event IDs and enforce per-operation sequence/version.
- A target commit followed by a local ledger crash remains a reconciliation gap; outbox does not close this dual-write gap.
- An outcome-unknown write is parked for inspection/reconciliation; it is never relabeled failed-and-safe-to-retry automatically.
- Retryable serialization/deadlock rollback may be offered as a new attempt only when rollback is proven and no external effects are assumed.

### Cancellation and read-only

- **Fact:** Successful PostgreSQL cancel dispatch does not guarantee cancellation; final result must be consumed.
- **Fact:** MySQL/MariaDB KILL QUERY preserves the connection; MariaDB explicitly distinguishes it from rolling back a killed connection's active transaction.
- **Fact:** SQLite interruption can lose a race with completion; interrupted DML inside an explicit transaction automatically rolls that transaction back.
- Sources: [PG cancellation](https://www.postgresql.org/docs/current/libpq-cancel.html),
  [MySQL KILL](https://dev.mysql.com/doc/refman/8.4/en/kill.html),
  [MariaDB KILL](https://mariadb.com/docs/server/reference/sql-statements/administrative-sql-statements/kill),
  [SQLite interrupt](https://www.sqlite.org/c3ref/interrupt.html).
- **Design inference:** Show `cancel_requested` separately from observed cancellation; retain query identity and connection generation to prevent cancelling a later query.
- After interruption, drain protocol results and inspect transaction state before reuse; discard a session whose state cannot be established.
- UI read-only is an accident-prevention policy; least-privilege DB roles/file access provide the actual authority boundary.
- PostgreSQL READ ONLY transaction restrictions have exceptions; SQLite `query_only` prevents data changes but does not make every operation read-only.
- SQLite read-only sessions should use verified read-only open flags, with attachment/extension/pragma capabilities separately constrained.
- Unknown syntax/functions, session-setting changes and parser/server disagreement must fail closed under the application's read-only gate.
- Sources: [PG transaction access mode](https://www.postgresql.org/docs/current/sql-set-transaction.html),
  [SQLite query_only](https://www.sqlite.org/pragma.html#pragma_query_only).

### Safe optimistic editing

- **Design inference:** Enable a generated UPDATE/DELETE only for a proven base-table row with a complete nonnullable unique identity and supported lossless types.
- A display string, positional row index, nullable unique index or unproven SELECT-expression provenance is not row identity.
- Capture the original key plus an application version token, or an exact original-value predicate for the declared conflict policy.
- Use null-safe typed comparisons; collation, timestamp precision, unsigned integers, decimals, binary and floating-point semantics must be engine-specific.
- Read a matching row under an appropriate row lock inside the edit transaction, compare the original state, then issue the conditional write.
- SQLite uses its own bounded write-transaction protocol rather than a nonexistent `SELECT FOR UPDATE`.
- The adapter must prove the lock/isolation behavior; unsupported engines/types/table storage remain read-only for grid editing.
- Zero matches means missing/conflict; more than one match rolls back. Never silently widen the predicate or fall back to a table scan write.
- **Fact:** MySQL UPDATE counts changed rows by default; `CLIENT_FOUND_ROWS` changes it to matched rows. MariaDB also documents unchanged assignments affecting zero rows.
- Therefore `affected_rows == 0` alone cannot distinguish conflict from a no-op assignment.
- **Design inference:** Drop unchanged cells, verify row state under lock, and explicitly certify the driver's matched-row setting/count interpretation.
- Do not use REPLACE/upsert as an editing shortcut; their insert/delete/trigger semantics differ from a conditional update.
- Conflict detection is not historical-change detection: comparing current original values cannot detect ABA changes unless a real monotonic version token is present.
- Sources: [MySQL affected rows](https://dev.mysql.com/doc/c-api/8.4/en/mysql-affected-rows.html),
  [MariaDB affected rows](https://mariadb.com/docs/connectors/mariadb-connector-c/api-functions/mysql_affected_rows).

## 2. Values, identifiers and user-owned SQL

- **Fact:** Prepared-statement parameters represent values; MySQL/MariaDB parameter markers cannot substitute SQL keywords or identifiers.
- Sources: [PG PREPARE](https://www.postgresql.org/docs/current/sql-prepare.html),
  [MySQL PREPARE](https://dev.mysql.com/doc/refman/8.4/en/prepare.html),
  [MariaDB PREPARE](https://mariadb.com/docs/server/reference/sql-statements/prepared-statements/prepare-statement),
  [SQLite binding](https://www.sqlite.org/c3ref/bind_blob.html).
- **Design inference:** All generated filter/edit values use native driver binding; no escaping helper is a replacement for value binding.
- Schema/table/column names come from scoped catalog identities and are quoted per dialect, each qualified component separately.
- Sort directions, operators, null ordering and SQL structural fragments come from closed enums, never raw frontend strings.
- A catalog name may legitimately contain quotes or Unicode; validate identity and encode it rather than applying an ASCII-only name rule.
- Generated statements pass an independent structural gate and row-identity check before execution.
- User-owned SQL is submitted exactly as explicitly approved: no silent LIMIT injection, predicate change, projection or automatic transaction wrapper.
- User SQL can intentionally be destructive; classification/confirmation is separate from injection prevention in generated SQL.
- A query parser is not an authorization boundary and may not recognize every server extension; unsupported constructs receive an explicit restrictive policy.
- Bound result row/byte/time/resource consumption without claiming the user's SQL was transformed into a cheaper server query.
- `EXPLAIN ANALYZE` can execute work; do not classify it as a harmless plan-only read without inspecting the underlying statement.
- Preserve a typed wire envelope: null, bool, signed/unsigned integer decimal text, exact decimal, float special values, text, bytes and temporal values with metadata.
- JavaScript Number is not the canonical representation of 64-bit keys/decimals; formatted/truncated display text never becomes an edit parameter.
- Unsupported round-trip types are inspectable but not editable; invalid/oversize cells are explicit, never silently truncated then written back.
- Keyset browsing requires a stable total order with a unique tie-breaker and an opaque cursor bound to target/schema/filter/order identity.
- Keyset pagination alone is not a transaction snapshot: concurrent key updates/deletes/inserts can change visibility across pages.
- Initial live browsing does not promise snapshot-consistent pages, total counts or exhaustive export of a changing dataset.
- Snapshot-consistent export/paging requires an implemented engine-specific snapshot lifetime/resource contract; otherwise it is `unsupported`, not an implied guarantee.

## 3. Bounded Tauri streaming and reconnect semantics

- **Fact:** Tauri v2 events target small messages and are not intended for high-throughput result data; Channel is designed for ordered streaming.
- Sources: [Tauri frontend communication](https://v2.tauri.app/develop/calling-frontend/),
  [Tauri Channel API](https://docs.rs/tauri/latest/tauri/ipc/struct.Channel.html).
- **Design inference:** Keep full driver results in the Rust/service side; send bounded page/batch envelopes through a command-owned Channel.
- Channel ordering is a transport property, not durable delivery, application acknowledgment or a documented memory-credit guarantee.
- Add bounded driver-to-service and service-to-Tauri queues, maximum encoded frame/cell bytes, and an explicit per-stream byte/row credit window.
- Frontend acknowledgment returns credit after applying/releasing a batch; stopping consumption must eventually stop fetching or expire/cancel the stream.
- A synchronous `send` return is not proof the React grid rendered or retained the batch.
- Envelope: workspace/target/session/query IDs, connection generation, schema version, stream generation, batch sequence, row range, byte count, result-set ID.
- Reject late generations and wrong owners; ignore duplicate applied sequences; detect gaps and request a bounded restart/replay policy.
- A Channel handle is not authority: validate workspace/target ownership in every command and scope listeners to the initiating window.
- UI detach/disconnect cancels or leases out a read stream; it never automatically resubmits write SQL.
- Reconnect retrieves operation state from the service ledger; an exhausted/expired read cursor starts a new explicitly labeled live read.
- Restarted streams have a new generation; never concatenate a new query's rows into an old result snapshot silently.
- Terminal stream state distinguishes complete, capped, cancelled, errored and outcome-unknown operation state.
- Resource limits include backend queued bytes, frontend retained bytes, visible rows, cached pages, active sessions and cursor lifetime.
- Rendering virtualization bounds DOM work only; it does not bound retained JS arrays, driver allocations or server work.
- Verification gates: slow consumer, no acknowledgments, oversized cell, duplicate/out-of-order batch, window close, service restart, cancel/commit race.

## Verification boundary

Official behavior above was checked against the linked pages on 2026-11-17; version/driver compatibility still needs implementation-time probes.
NOT RUN: live PostgreSQL/MySQL/MariaDB/SQLite isolation, row-lock, matched-row, cancel/commit fault injection and Tauri memory-bound tests.
The design must preserve `unsupported` and `outcome_unknown` where those checks cannot establish the claimed capability.
