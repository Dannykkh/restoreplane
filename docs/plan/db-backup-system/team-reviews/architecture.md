# Architecture Review — Ransomware-Resilient DB Backup System

Reviewer role: architecture / trait-boundary / concurrency. Scope reviewed: `spec.md`,
`research.md`, `research/rust-stack.md`, `research/ransomware-resilience.md`.

Verdict up front: the stack picks are sound and the envelope/immutability strategy is
right. The **weakest parts are three structural seams** that will leak if not fixed
before coding: (1) the pipeline assumes every source is a single stdout stream (false),
(2) "dump-once fan-out in one pipe" couples DB-snapshot duration to network speed and to
per-destination failure, and (3) the catalog is the single restore-time source of truth
but lives on the very host that ransomware wipes. All three have concrete fixes below.

## Priority legend
- **P0** — silent data loss / undecryptable-or-incomplete backups, or blocks the core promise (P1 ransomware resilience). Fix before/at MVP.
- **P1** — forces a painful re-architecture later if not seamed correctly now.
- **P2** — completeness / ergonomics; can follow.

## P0 findings

### P0-1. The pipeline treats a truncated dump as a successful backup
`dump stdout -> zstd -> age -> writer` reads the child stdout to EOF. `pg_dump`/`mysqldump`
that fail mid-run still close stdout cleanly (they error on stderr and exit nonzero). The
pipeline then calls `finish()` on every stage and produces a cryptographically valid,
decompressible, but incomplete archive — the worst kind of dead backup: it passes header
verification and even a naive checksum, but restore is short rows/tables.

Recommendation:
- Capture the dump child stderr and exit status. Treat the backup as failed if exit != 0
  or stderr matches known fatal patterns, even if the stream reached EOF.
- Do the exit-code check after stream copy but before committing the object (see P0-3).
  Never write the catalog success row until the child has reaped with status 0.
- For `pg_dump` custom/directory format, also validate structurally (the format has a
  trailer); for plain SQL rely on exit code.

### P0-2. finish() cannot be guaranteed by RAII/Drop in an async chain
The spec mitigation "RAII wrapper to force finish" does not hold in Rust: `Drop` is sync
and cannot `.await`, and cannot propagate the error that `finish()`/`close()` returns.
`zstd` Encoder, `age` StreamWriter, and `opendal` writer all need an explicit awaited
finish; skipping any yields truncation (the final auth tag / frame / multipart-complete is
written there).

Recommendation:
- Do not rely on Drop to finish. Use an explicit do/finally shape: run the copy, capture
  any error into a variable, then in all paths call finishes inner->outer
  (`zstd.finish()?` -> `age.finish()?` -> `writer.close()`), then decide commit/abort.
- Guard the footgun with a debug-only "finished?" flag whose Drop panics/logs loudly if
  dropped un-finished — it catches the mistake in tests without pretending Drop does the
  real work.
- Critically: do not `?`-early-return between "stream copied" and "object committed" — an
  early return there is exactly what produces a committed-but-truncated object.

### P0-3. No atomic commit boundary — partial objects can look real
"clean up partial object on failure" is stated but not designed. Object stores have no
cheap rename. Without an explicit commit protocol, a crash after some bytes are uploaded
leaves a visible partial key that a later catalog/restore might trust.

Recommendation (prefer A, and do B on top):
- **A. Multipart-only-commit:** stream via opendal multipart, and only call `close()`
  (CompleteMultipartUpload) on full success (after P0-1 exit check). On any error call the
  writer abort/discard so no object becomes visible. `[확인 필요]`: confirm opendal 0.58
  aborts the multipart upload on writer drop-without-close (vs leaving orphan parts — if
  orphaned, add an S3 lifecycle rule AbortIncompleteMultipartUpload on the bucket).
- **B. Staging + manifest commit:** write ciphertext to `pending/<id>`, then write a tiny
  `manifest/<id>.json` only after success; restore/catalog trust only backups that have a
  manifest. This also solves P0-6 (catalog rebuild) — the manifest is the durable record.

### P0-4. Fan-out-vs-redump: decide dump-once -> encrypt-once -> spool -> upload N (option C)
This is the pivotal concurrency decision the task asks to make explicitly. Three options:

| Option | DB load | Encrypt cost | Failure isolation | Snapshot held during | Resumable |
|---|---|---|---|---|---|
| A. Re-dump per destination | N x (unacceptable) | N x | perfect | each upload | per-dump |
| B. Dump once -> in-pipe tee to N dests | 1 x | 1 x | coupled (slowest dest backpressures dump; one stall holds the DB snapshot) | slowest upload | no |
| **C. Dump once -> zstd -> age -> local staging file, then upload the file to each dest independently** | **1 x** | **1 x** | **per-destination independent** | **only the dump** | **yes (file is re-readable)** |

**Recommendation: C.** Reasoning that decides it:
- B holds the DB snapshot for the duration of the slowest network upload. With
  `mysqldump --single-transaction` (REPEATABLE-READ) or `pg_dump` snapshot, a stalled S3
  upload keeps that transaction open -> MySQL undo/history-list bloat, PostgreSQL table
  bloat + blocked vacuum. This violates the research pull/minimize-impact posture. C
  releases the DB the instant the local write completes.
- The LocalDest is already a required destination (minimum defense line). So the staging
  file IS the local backup — option C extra disk cost is essentially free; it is disk you
  were going to write anyway.
- C gives identical ciphertext and identical checksum to all destinations (encrypt once),
  independent retry/resume per destination (re-read the file), and natural partial-success
  (spec "one destination fails -> partial success report") falls out for free.
- Cost/limits: needs free disk >= encrypted backup size, and total wall-clock =
  dump + slowest upload (but DB impact = dump only). Add a pre-flight disk-space guard and
  a config `spool_dir`. If a deployment truly cannot spool (disk < backup), fall back to B
  for that job only, documented as "couples DB snapshot to upload".
- Across DBs/jobs: bound parallel dumps with a semaphore (`max_parallel_dumps`, default 1)
  to cap DB load and disk; uploads within a backup run in parallel via a `JoinSet`,
  collecting a per-(backup x destination) result vector.

### P0-5. age "multi-recipient" wrapping vs an Argon2id symmetric KEK is a category error
The spec says both "Argon2id KEK" and "age multi-recipient wrapping" wrap the DEK. But age
recipient stanzas are X25519 (asymmetric) or scrypt (age own KDF) — an Argon2id-derived
symmetric key is not an age recipient. As written, SecretManager conflates two incompatible
mechanisms. If you use age passphrase/scrypt path you get scrypt, not Argon2id, and the
"3 wraps as age recipients" story does not cleanly include a raw symmetric KEK or a DPAPI
blob.

Recommendation — make SecretManager own DEK wrap/unwrap distinctly from payload encryption:
- Payload: encrypt the dump with a random 32-byte DEK via either age recipient mode using
  one X25519 identity derived from the DEK, or `chacha20poly1305` STREAM keyed by the DEK.
  Keep KEK-wrapping out of age recipient list.
- Wrap the DEK three ways as explicit, self-describing stanzas managed by SecretManager:
  (a) `chacha20poly1305(DEK)` under Argon2id-KEK(password) + stored salt/params;
  (b) `chacha20poly1305(DEK)` under a KEK derived from the recovery code;
  (c) DPAPI-Machine-scope-protected copy of the DEK (or KEK) with app entropy.
- This keeps the re-wrap-without-re-encrypting-the-dump property the design wants, makes
  `rotate-key` (P2-2) implementable, and removes the asymmetric/symmetric muddle.
- Restore must unwrap from any single path given only the object-store artifact + the
  corresponding secret — verify end-to-end (see P0-6).

### P0-6. The catalog is on the host ransomware wipes, yet it is the restore index
SQLite catalog holds checksums/keys/history on the backup host. That host is exactly what
an attacker/ransomware compromises. If it is gone, you have a bucket of opaque encrypted
blobs and no map. This undercuts the whole recovery story.

Recommendation:
- Every backup writes a self-describing manifest object next to its ciphertext in the
  destination (job, source, engine, server version, dump-tool version, plaintext hash,
  cipher size, timestamps, the wrapped-DEK stanzas or a reference, schema version). The age
  header already embeds key-agreement material; the manifest adds catalog-level metadata.
- Add `catalog rebuild --from <destination>` that reconstructs SQLite purely from manifests.
  Treat SQLite as a fast local cache/index, not the source of truth. Restore must be
  provable from the object store alone (test this as an explicit scenario — the real drill).

## P1 findings

### P1-1. BackupSource -> stdout stream is a leaky abstraction (directory-output tools)
The pipeline trait presumes a single byte stream, but:
- `mydumper` outputs a directory of files (opt-in for large InnoDB).
- `pg_dump --format=directory` (needed for parallel `-j`) also outputs a directory.
- `pg_dumpall --globals-only` (roles/tablespaces) is a separate stream the current
  single-pipe model omits entirely — without it a Postgres restore has no roles/grants.

So there are two artifact shapes (stream vs directory) and multi-part sources (globals +
per-DB). Modeling BackupSource as "gives me one stdout" forces ugly special-casing.

Recommendation — put the seam at the artifact, not at "stdout":

    trait BackupSource {
        fn plan(&self) -> DumpPlan;            // 1..N parts + metadata (globals, per-db)
        fn spawn(&self, part) -> DumpArtifact;
    }
    enum DumpArtifact {
        Stream(impl AsyncRead),                // pg_dump -Fc -f -, mysqldump
        Directory(TempDir),                    // mydumper, pg_dump -Fd
    }

Normalize both into the pipeline by streaming-tar-ing a Directory artifact into the same
`zstd -> age -> writer` chain (one object per backup, still restorable). Directory tools
lose true streaming (must land on local disk first) — but under option C you spool to local
disk anyway, so this composes cleanly. Make globals a first-class part of the plan for PG.

### P1-2. Destination trait must expose an immutability capability, not assume it
Object Lock / retention / required-checksum is S3-only; LocalDest and SshDest cannot enforce
WORM. A flat Destination trait silently gives weaker guarantees on some targets — and
per-object retention / legal-hold / checksum-on-lock do not apply uniformly. ISP problem
(fat trait forcing meaningless methods on Local/Ssh).

Recommendation:

    trait Destination {
        async fn writer(&self, key) -> Writer;   // multipart/streamed
        fn capabilities(&self) -> DestCaps;       // { immutable, checksum_required, resumable }
    }
    enum ImmutabilityKind { None, ObjectLockGovernance, ObjectLockCompliance, ServerAppendOnly }

- Let the job assert a required immutability level and fail/warn loudly if a chosen
  destination cannot meet it (so nobody thinks Local-only satisfies P1).
- `checksum_required` drives the Object-Lock "S3 needs a checksum" gotcha: set
  `checksum_algorithm` when caps demand it. `[확인 필요]`: opendal 0.58 per-object retention
  header surface — until confirmed, rely on bucket default retention and record the
  effective retention in the manifest.

### P1-3. Checksum-while-streaming: hash the plaintext, tee at the source
The valuable checksum is over the plaintext dump (proves the thing you restore equals the
thing you took, and lets Verifier assert it end-to-end). A ciphertext/ETag checksum is also
needed for the Object-Lock requirement, but S3/opendal computes that for you.

Recommendation:
- Wrap the dump reader in a hashing reader (`blake3` — fast, streaming) that updates the
  digest as bytes flow into zstd. One pass, constant memory. Store `plaintext_blake3` +
  `plaintext_bytes` in catalog and manifest.
- Store the destination-side object checksum/ETag separately (per destination). Verifier
  (Level 3+) decrypts->decompresses->re-hashes and asserts equality with plaintext_blake3.
- The two checksums answer different questions: plaintext hash = "is the dump intact and
  identical", object checksum = "is the stored blob intact / satisfies Object Lock".

### P1-4. Verifier and restore share machinery — model a Restorer seam
Verifier = restore-to-throwaway + assertions. Do not build it twice. restore = fetch object
-> unwrap DEK -> age decrypt -> zstd decode -> (untar) -> feed to psql/mysql/pg_restore/
myloader. Verifier is that plus "target is ephemeral + assert schema/row-counts + record
result". Decide the ephemeral instance (Open Question): recommend a local Docker container
matched to server_version from the manifest; fall back to a scratch schema on a local engine
if Docker absent. Wire the tiered levels (existence / integrity / partial-restore weekly /
full-restore monthly) to config, and alert on failure-to-verify, not only on job failure.

### P1-5. Concurrency / overlap guard for scheduled runs
SQLite under Task Scheduler catch-up (StartWhenAvailable) can double-fire, and a long backup
may still run when the next fires. Two concerns: DB-file concurrency and job-level overlap.

Recommendation:
- SQLite: `PRAGMA journal_mode=WAL`, `busy_timeout=5000`, a single writer connection.
- Job overlap: an OS-level single-instance guard (named mutex) per job so a catch-up run
  cannot stack on a still-running one; or a `runs` table with a partial UNIQUE index on
  `(job, status='running')`. Log skipped/coalesced runs so missed windows are visible.

## P2 findings

### P2-1. Config (TOML) — gaps to close before it ossifies
- `[meta] schema_version` (config migrations are inevitable).
- `[[tunnels]]` host-key fingerprint pin — research (russh) explicitly warns against
  blind-accept; a tunnel without a pinned host key is a MITM hole. Same for SshDest.
- `[[destinations]]` S3: endpoint, region, bucket, prefix, object_lock.mode,
  object_lock.retention_days, checksum_algo; secrets as enc:...
- The enc: field must carry a key-id/version (which DEK/wrap generation encrypted it) so
  rotate-key can re-encrypt deterministically.
- `[[jobs]]` binding sources->destinations + required_immutability + retention/prune
  policy + verify policy + schedule ref.
- `[policy]` max_parallel_dumps, retry {attempts, backoff}, spool_dir, min_free_disk,
  per-source single_transaction toggle, dump_tool_version pin per source (version-matrix).
- `[notify]` channel (Open Question — webhook/email).
- Prune identity separation: the delete-capable credential must not live in the same
  profile the unattended writer uses (research: else append-only is void). Keep it in a
  separate prune.toml/profile the scheduled run never loads.
- Globals toggle for Postgres (dump_globals = true).

### P2-2. CLI surface — missing commands
Present: init | add-db | add-dest | run | schedule | restore | verify | catalog. Add:
- **test-connection** (P0-ish for unattended safety): probe DB reachability, open the
  tunnel, verify destination is writable, validate creds — before scheduling a headless
  job. Without it the first failure is at 02:00 unattended.
- **rotate-key / rekey**: change passphrase, regenerate recovery code, re-register DPAPI on
  a new PC — re-wrap DEK without re-encrypting dumps (the whole point of P0-5).
- **doctor / self-check**: dump-tool binaries discoverable + versions vs server versions
  (the pg_dump >= server risk), disk space for spool, DPAPI availability.
- **prune**: apply retention via the isolated identity; keep it a separate, hard-to-fire
  command (never part of run).
- **catalog rebuild --from <dest>** (P0-6 disaster recovery of the index).
- **restore** subshape: --backup <id>, --from-destination <name>, --to <target>,
  --decrypt-only (produce plaintext dump without importing), --with-recovery-code.
- **register-dpapi / unlock**: the new-PC re-register DPAPI cache flow (spec previous-PC).
- **keys status**: show which wrap paths (password / recovery / DPAPI) are active (recovery
  code is one-time display — do not re-print it).
- Minor: run --dry-run, edit-db/remove-db, edit-dest/remove-dest.

### P2-3. Catalog schema sketch

    backups(
      id TEXT PK, job TEXT, source TEXT, engine TEXT,
      started_at, finished_at, status TEXT,           -- running|success|failed|partial
      plaintext_blake3 TEXT, plaintext_bytes INT, cipher_bytes INT,
      server_version TEXT, dump_tool_version TEXT,
      dek_wrap_gen INT, schema_version INT
    );
    artifacts(                                          -- one row per (backup x destination)
      backup_id TEXT, dest_name TEXT, dest_kind TEXT, object_key TEXT,
      object_checksum TEXT, retention_until TEXT, immutability TEXT,
      status TEXT, uploaded_at,                          -- pending|committed|failed
      PRIMARY KEY(backup_id, dest_name)
    );
    verifications(
      id TEXT PK, backup_id TEXT, verified_at, level INT,
      result TEXT, restored_rows INT, notes TEXT
    );
    runs(job TEXT, started_at, status TEXT);            -- overlap guard + missed-run log
    meta(key TEXT PK, value TEXT);                       -- schema_version, argon params ref

artifacts as a child of backups is what makes the spec partial-success report first-class.

## Phasing — is Postgres->Local the right first cut?
Yes as engineering sequencing, but Local-only does NOT satisfy P1 (ransomware), so do not
call that "MVP-done". The thin vertical slice is correct; the risk is shaping the traits
around the two easiest implementations (Postgres + Local) so they ossify before the hard
ones (MySQL, S3-with-Object-Lock, tunnel) validate the seams. Recommended slices:

1. **Walking skeleton — Postgres -> Local, full envelope + catalog + restore + verify.**
   Proves the load-bearing risks cheaply: finish()/truncation (P0-1/2/3), DEK wrap/unwrap
   (P0-5), plaintext-hash tee (P1-3), and the +0 verify loop. Verify belongs in slice 1,
   not the end — it is what proves the pipeline at all.
2. **S3 destination WITH Object Lock (do this SECOND, not last).** The actual ransomware
   defense (P1) and highest external risk (opendal retention surface, checksum-on-lock
   gotcha). Deferring it hides the biggest unknown.
3. **MySQL source (second BackupSource impl).** Pull a second implementation of the trait
   forward to break Postgres-shaped assumptions before they harden (two impls before you
   trust an abstraction). Validates P1-1 (directory output / globals) too.
4. **Unattended path: DPAPI Machine-scope + schedule (schtasks XML).** Proves unattended run.
5. **SSH dest + russh tunnel, fan-out to 3 destinations (option C), prune/rotate-key.**

MVP-complete = at least PG+MySQL + Local + S3(Object Lock) + unattended + verify. SSH/tunnel
and mydumper are legitimate fast-follows. The requested "both DBs + 3 destinations" is the
MVP definition-of-done, reached by slice 5; Postgres->Local is the first increment, not the
shipped MVP.

## SOLID quick pass
- **DIP**: good — Pipeline depends on Destination/BackupSource traits, not opendal/russh
  concretes. Keep opendal fully behind Destination (no opendal types in pipeline signatures).
- **ISP**: fix Destination (P1-2) — immutability/retention as a capability query, not methods
  every impl must fake.
- **SRP**: split SecretManager (DEK lifecycle/wrap) from Pipeline (payload encryption) per
  P0-5; split Restorer (mechanics) from Verifier (assertions) per P1-4.
- **OCP**: DumpArtifact enum (P1-1) is a closed set — acceptable (adding a source shape is
  rare and wants a compile error to force pipeline handling), preferable to a leaky
  "everything is stdout" open abstraction.

## Top-5 to fix before writing code
1. Pipeline commit protocol: exit-code check + explicit finish order + multipart-only-commit + manifest (P0-1/2/3/6).
2. Adopt option C (dump-once -> encrypt-once -> spool -> independent uploads) and write the ADR (P0-4).
3. Separate SecretManager DEK-wrapping from age payload encryption; drop the symmetric-KEK-as-age-recipient muddle (P0-5).
4. Reseam BackupSource around DumpArtifact (stream|directory) + globals; Destination around an immutability capability (P1-1/2).
5. Add test-connection, rotate-key, doctor, catalog rebuild; pin host keys in config (P2-1/2).

## Dictionary Updates
- **DumpArtifact** — the normalized output shape of a BackupSource: Stream (stdout-pipeable, e.g. pg_dump -Fc -f -, mysqldump) or Directory (multi-file, e.g. mydumper, pg_dump -Fd), the latter streamed into the pipeline via tar. Chosen seam to stop the "everything is stdout" leak.
- **Spool-once fan-out (option C)** — dump once -> zstd -> age -> local staging file (which also serves as LocalDest), then upload the finished encrypted file to each remote destination independently. Contrast with in-pipe tee (option B) and re-dump-per-destination (option A).
- **Commit protocol** — a backup object is committed only after (a) dump child exits 0, (b) all stream stages finish() in inner->outer order, and (c) multipart close() / manifest write succeeds; any earlier failure aborts with no visible object.
- **Manifest object** — a self-describing metadata file stored beside each ciphertext in the destination, enabling catalog rebuild and restore from the object store alone (catalog-on-host is only a cache).
- **Immutability capability** — a Destination self-report (None | ObjectLockGovernance | ObjectLockCompliance | ServerAppendOnly) that a job asserts against, so weaker targets (Local/Ssh) cannot silently masquerade as ransomware-resilient.
