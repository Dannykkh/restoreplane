# Program-owned backup engine

## Ownership boundary

DbBackup registers one `DbBackupService` process so backups continue after the Console window is
closed and after Windows restarts. The Windows Service Control Manager owns only that process
lifecycle. It does not own a task per Plan and it does not contain Plan configuration.

The Operational Store owns Workspaces, Connections, Database Targets, immutable Backup Scopes,
published Plan revisions, schedules, scheduler cursors, durable Work Items, resource permits,
Recovery Points, and retention evidence. The Console changes those records through authenticated
local IPC. Closing the Console therefore does not stop scheduled work.

Legacy `schtasks` and direct CLI execution remain migration compatibility surfaces only while no
`DbBackupService` entry exists. If the service entry is installed, legacy `run` and `schedule`
execution is blocked regardless of whether the service is running or stopped. The program-owned
production path never creates a Task Scheduler entry per Plan and never lets the UI execute a dump
directly.

## Runtime root boundary

`InstallRoot` is the directory that contains `dbbackup-service.exe`, `dbbackup.exe`, and bundled
native tools. `DataRoot` contains the Operational Store, service identity and secrets, spool,
control backups, update journals, support bundles, and telemetry archives. The service derives
`InstallRoot` from its own executable and receives one absolute `DataRoot` through
`--data-root <path>`. A no-argument fallback to `%ProgramData%\DbBackup` remains for legacy service
definitions; signed install, repair, and update always persist the explicit argument.

The two roots must be distinct and neither may contain the other. This keeps binary replacement or
loss from crossing the durable data boundary. Repair repopulates `InstallRoot` while preserving
`DataRoot`. Normal uninstall unregisters the service and preserves both roots. If the service was
unregistered, reinstall uses the same `DataRoot`; `repair` is only for an existing service entry.

The production service resolves PostgreSQL, MySQL, and MariaDB executables only from versioned
vendor bundles below `InstallRoot/tools`. A missing bundle fails closed. Ambient `PATH` lookup and
bare command names remain available only to the explicitly invoked legacy CLI path after the
service entry has been removed; the service never passes database credentials to those fallbacks.
Before each native backup or MySQL-family metadata operation, the service canonicalizes the
resolved executable, re-hashes its current bytes, and requires an exact active package-attested
path/SHA-256 identity from the Operational Store.

## Execution flow

1. The singleton engine reads enabled, published Plans and advances a durable scheduler cursor.
2. Due times are materialized idempotently as Plan Occurrences, Backup Runs, and Work Items.
3. Overlap policy `coalesce_one` prevents a slow Plan from accumulating duplicate active backups.
4. A Backup worker leases one Work Item with a fencing token and acquires durable global, source,
   destination, and spool capacity.
5. The selected provider executes one database dump process per Backup Scope. PostgreSQL explicit
   scopes omit the cluster-globals part. MySQL/MariaDB explicit scopes include selected table
   triggers but deliberately exclude database-wide routines and events.
6. The streaming encoder writes bounded chunks to the managed spool. A signed Set Manifest labels
   whole-database versus partial-table coverage, the immutable selector hash, and the provider
   dependency policy.
7. Each required Destination publishes payloads first and reads them back in full. It then publishes
   the signed Set Manifest as the last logical commit marker and reads that bounded document back.
   Only verified required replicas can participate in the Recovery Point commit.
8. The retention runtime evaluates GFS policy, holds, locks, active recovery sources, and the last
   compliant replica before it creates tombstone/delete work.

## Capacity defaults

The native path defaults to four global Backup permits, one active dump per Connection Profile,
two uploads per Destination, a 512 MiB admission reservation per attempt, a 2 GiB free-space
floor, and a six-hour pre-commit execution deadline. Every native part performs a disk preflight;
the running encoder checks the deadline and free-space floor every five seconds. The same hard
deadline encloses source planning, database connection and metadata/version probes, dump execution,
encoding, and pre-commit publication.

These controls bound simultaneous work and memory buffers; they do not predict the final dump
size. Operators must provision spool and destination capacity for the largest expected database.
Retries before commit restart the native dump. Destination uploads are not resumable across a
process crash in the current release, and destination verification reads the committed object
back in full.

## Retention defaults

Retention maintenance runs every 15 minutes and evaluates at most 1,024 candidates per scan. A
tombstone has a 24-hour grace period before eligible deletion, and the delete worker polls for work
every 250 milliseconds. These are fixed values in the current release and are not operator-tunable
through the Console or Operational Store.

The scan bound controls maintenance work per pass, not the total number of Recovery Points that can
be retained. Holds, locks, active recovery sources, and the last compliant Recovery Point still
override age-based deletion.

## Crash and shutdown behavior

Before publishing, the worker atomically persists the intended Recovery Point ID and signed
Manifest object key while moving the fenced claim to `committing`. If the process stops after
payload or Manifest publication, startup uses that persisted intent rather than inventing a new
identity.

Reconciliation runs once before readiness and then every five seconds while the service is active,
so a claim whose 30-second lease expires shortly after a fast restart is not stranded. Each cycle
scans at most 1,000 expired claims and creates a fresh recovery source. All commit-evidence checks
within that cycle share one 20-second budget; the budget is reset per cycle, never per claim. For a
`committing` claim, every required Destination must expose identical canonical signed Manifest
bytes. The service checks the trusted signature, occurrence, Run, Scope, engine, Recovery Point ID,
Manifest key, and current Destination bindings. It then performs metadata-only `stat` calls for
each payload and requires the exact signed ciphertext byte count. It does not read or hash a
database-sized payload during reconciliation. The bounded signed Manifest remains the last commit
marker.

Only evidence that passes the same immutable commit validator as the normal path is converted to a
Recovery Point. Missing, mismatched, timed-out, or unavailable evidence leaves the claim for
operator Attention instead of guessing it complete. The metadata check proves bounded commit
settlement evidence; it does not replace later L1 content verification.

Native dump startup records the PID, OS creation time, service boot ID, absolute executable path,
and executable SHA-256 before output is consumed. Reconciliation re-reads the live process path and
hash. Pre-commit work whose process is confirmed exited, missing, or PID-reused is requeued with a
new fence. A live process that is not owned by a recoverable Job Object is retained for operator
attention until a later cycle confirms it is gone. Active spool reservations are preserved and
only service-shaped, unreferenced attempt directories are removed.

Generic configuration mutations use a durable idempotency ledger. If the service crashes after a
mutation commits but before the ledger result commits, an expired replay is finalized as
`APPLICATION_OUTCOME_UNKNOWN`; the handler is not executed again. The Console retains the attempt
key in memory, blocks blind repetition, and directs the operator to refresh durable state and
Activity. Dashboard inventory follows bounded cursor pages; a cursor loop, page cap, or count
mismatch is shown as partial evidence and disables Scope/Plan mutation instead of presenting a
global total.

Service stop disables new claims first. A dump still in the pre-commit pipeline receives a clean
shutdown cancellation. The absolute six-hour deadline also bounds Manifest-last publication. If
timeout, shutdown, or a publish error occurs after the claim entered `committing`, the worker does
not mark it failed, retry it, or delete its spool. It returns `AwaitingReconciliation` and preserves
the recovery intent until a periodic pass can validate signed evidence; incomplete evidence remains
Attention. Normal uninstall stops and unregisters the singleton service but does not delete the
Operational Store, spool, backup artifacts, control backups, or recovery material.

Retention deletes the signed Set Manifest first. Payload, part-manifest, and verification objects
are deleted only after that marker deletion succeeds. A locked, retryable, or terminal marker
failure produces explicit `not_attempted` observations for every payload and never calls their
delete operators, so a trusted marker cannot be left pointing at payloads removed by the same job.

## Backup Scope truth

- `whole_database` means the provider's supported logical database surface.
- `explicit_tables` is an exact immutable set of discovered eligible base tables.
- A partial Recovery Point is grouped by its Scope identity, not presented as whole-database
  protection.
- PostgreSQL dependent objects are not automatically included by `--table`; MySQL/MariaDB routines
  and events are excluded from explicit scopes. These limitations are signed into the Manifest.
- SQLite remains whole-database only.
