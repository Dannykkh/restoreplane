# Ransomware-Resistant Backup Architecture — Research & Guidance

Scope: a solo-operated DB backup tool that dumps MySQL/PostgreSQL and stores copies
offsite. Goal: survive an attacker who compromises the DB host, the backup host, *or*
the backup-destination credentials — and still restore clean data.

Verified against current (2025–2026) vendor docs and practitioner sources; see Sources
at the end. Treat every number/claim as sourced, not remembered.

---

## TL;DR — the decisions that matter most

1. **Immutability is the single highest-leverage control.** Put backups in
   object storage with **Object Lock (WORM)** so nothing — not stolen creds, not the
   root account (in compliance mode) — can delete them inside the retention window.
2. **The backup writer must use append-only / write-only credentials.** A stolen
   upload key must not be able to `DeleteObject`/`DeleteObjectVersion` or overwrite.
   Object Lock + versioning is the enforcement; IAM is the first fence.
3. **Prefer a pull topology** (backup host reaches into DB servers) so a compromised
   DB host has no path to the backup store, and keep pruning/lifecycle on a *separate*
   well-secured identity — never the backup-writer identity.
4. **An untested backup is not a backup.** Automate periodic *restore* tests, not just
   integrity checks. 30–40% of orgs that never test discover failures during a real
   disaster.
5. **Encryption at rest protects confidentiality, NOT availability.** It is not
   ransomware protection. Immutability is.

**Biggest failure modes (in order):**
- Backup-writer credential can also delete/overwrite → attacker wipes backups. (Fix: append-only + Object Lock.)
- No immutability → any admin/root/cred compromise deletes everything. (Fix: Object Lock compliance mode with sane retention.)
- Prune/forget identity is reachable from the same compromised host → append-only protection is void. (Fix: isolate the pruning identity.)
- Backups never test-restored → silent corruption discovered mid-incident. (Fix: scheduled restore drills.)
- Single copy / single provider / same cloud account as the DB → shared blast radius.

---

## 1. The 3-2-1 rule and its evolution to 3-2-1-1-0

**Classic 3-2-1:**
- **3** copies of the data (1 primary + 2 backups).
- **2** different media/storage types.
- **1** copy offsite.

The 3-2-1 rule predates ransomware that *actively hunts and deletes backups*
(shadow copies, backup repos, backup-admin creds). Veeam extended it to
**3-2-1-1-0**:

- **+1** = one copy that is **immutable OR air-gapped** (offline). "Offline/immutable"
  means an attacker who owns your live infrastructure still cannot alter or delete this
  copy. Immutable (WORM object storage) is the practical cloud equivalent of an air gap.
- **+0** = **zero recovery errors**: backups are regularly *verified by test restore*,
  so you never learn a backup is corrupt during an incident.

For a solo DB tool, a realistic mapping:
- Copy 1: primary DB (production).
- Copy 2: local/nearby dump or replica (fast restore, different media).
- Copy 3: offsite object storage, **with Object Lock** (this is the "+1" immutable copy).
- "+0": automated restore verification job (Section 6).

---

## 2. Immutability: S3 Object Lock and equivalents

Object Lock enforces **WORM** (write once, read many): a locked object version cannot
be overwritten or deleted until its retention expires. It requires **versioning
enabled** on the bucket.

### Governance mode vs Compliance mode (AWS S3)

| | **Governance mode** | **Compliance mode** |
|---|---|---|
| Who can delete/shorten before expiry | Users/roles holding `s3:BypassGovernanceRetention` | **No one — including the AWS root user** |
| Retention can be shortened | Yes (with bypass permission) | **Never** (can only be *extended*) |
| Retention mode can be changed | Yes (with bypass) | No |
| Use case | Protect against accidents & stolen non-privileged creds; keep an escape hatch | Regulatory WORM (SEC 17a-4, FINRA, CFTC via Cohasset assessment); true ransomware hard-stop |

**Retention period**: a fixed duration (days/years) applied per object version, set at
write time (or via a bucket default). In compliance mode it can only be lengthened.

**Legal hold**: an on/off flag, **independent of any retention period and with no
expiry**. If *either* a retention period *or* a legal hold is active, deletion is
blocked. Legal hold is for "keep indefinitely until I explicitly release it."

### Which mode for a solo operator (ransomware AND lock-out fear)

There is real tension: **compliance mode also locks *you* out** — a fat-fingered
99-year retention is unrecoverable and you pay storage for it forever, even root can't
fix it. Recommended posture:

- Use **compliance mode with a *modest* retention** (e.g. 14–35 days) matched to your
  RPO/retention needs. Short enough that a mistake self-heals; long enough to outlast
  the dwell time before you detect an incident. **Test the whole flow in a throwaway
  bucket first.**
- Alternatively, **governance mode** where the `s3:BypassGovernanceRetention`
  permission lives *only* on a separate, MFA-protected break-glass identity that your
  backup automation never holds. This gives ransomware resistance (the backup writer
  can't bypass) while keeping a controlled escape hatch. For a solo operator worried
  about self-lockout, governance-mode-with-isolated-bypass is often the pragmatic
  sweet spot; move to compliance mode once the process is proven.
- Keep retention **short-ish + versioned + lifecycle-expired** rather than long +
  immutable-forever.

### Backblaze B2 and MinIO equivalents

Both are **S3-API-compatible Object Lock** and behave like AWS:
- **Backblaze B2**: Governance mode (overridable with app-key capability) and
  Compliance mode (not removable by anyone, but the retention date *can be extended*),
  plus Legal Hold (no expiry). Marketed explicitly for immutable/ransomware backups.
- **MinIO** (self-hosted / AIStor): API-compatible WORM object locking, both
  duration-based retention and indefinite legal hold. Useful if you want an on-prem
  immutable target. **Caveat: self-hosted means *you* secure the host — root on the
  MinIO box defeats it (see Section 4).** Prefer a managed provider or a
  separately-owned server for true isolation.

---

## 3. Credential hygiene for the backup destination

**Principle: the identity that writes backups must not be able to destroy backups.**
If the DB host or backup host is popped, the attacker gets whatever that host's
credentials can do. If those creds can `DeleteObject`, all backups die.

**Layered defense:**

1. **Append-only / write-only IAM for the backup writer.** Grant `s3:PutObject` (and
   `s3:ListBucket` if needed), and **explicitly `Deny`** `s3:DeleteObject`,
   `s3:DeleteObjectVersion`, and `s3:PutLifecycleConfiguration`. All three are needed:
   objects die via the DELETE API *or* via a malicious lifecycle rule. An explicit
   `Deny` always beats any `Allow`.

   ```json
   {
     "Version": "2012-10-17",
     "Statement": [
       { "Sid": "AllowWrite",
         "Effect": "Allow",
         "Action": ["s3:PutObject", "s3:ListBucket", "s3:GetBucketLocation"],
         "Resource": ["arn:aws:s3:::my-db-backups",
                      "arn:aws:s3:::my-db-backups/*"] },
       { "Sid": "DenyDestroy",
         "Effect": "Deny",
         "Action": ["s3:DeleteObject", "s3:DeleteObjectVersion",
                    "s3:PutLifecycleConfiguration", "s3:PutBucketVersioning",
                    "s3:BypassGovernanceRetention",
                    "s3:PutObjectRetention", "s3:PutObjectLegalHold"],
         "Resource": ["arn:aws:s3:::my-db-backups",
                      "arn:aws:s3:::my-db-backups/*"] }
     ]
   }
   ```
   (Deny `PutObjectRetention/LegalHold` too, so a stolen key can't set an absurd
   retention as a *denial-of-service* against you. Set retention via bucket default
   instead of client-side.)

2. **Versioning as a safety net.** With versioning on, an "overwrite" from ransomware
   just creates a *new* version; the clean prior version remains. Object Lock then
   makes those versions undeletable. Without versioning, Object Lock has nothing to
   protect and overwrite-in-place can still hurt you.

3. **Separate identities for separate jobs.** Writer = append-only. Pruning/lifecycle/
   forget = a *different*, tightly-held identity that never touches an
   internet-facing/DB host. Reader/restore = read-only. Break-glass (bypass/delete) =
   MFA, offline, used by hand.

4. **Scope tightly**: per-bucket (ideally per-prefix) resources, short-lived creds
   where possible, rotate keys, and never reuse the production DB app's cloud role for
   backups.

---

## 4. Pull vs Push topology and blast radius

**Push** = DB servers (or a job on them) hold destination credentials and send
backups out. **Pull** = a dedicated backup host reaches *into* the DB servers (read
replica creds / SSH), pulls the data, and writes to the store; the DB servers know
nothing about the backup store and hold no credentials to it.

**Pull is generally safer.** Blast-radius reasoning:

- **Compromised DB host, pull model:** the DB host has no credentials for and no
  knowledge of the backup destination → attacker cannot reach or delete backups.
  Damage stops at the production data (which you restore).
- **Compromised DB host, push model:** the host holds destination creds →
  append-only + Object Lock are what save you (hence Section 3 is non-negotiable if you
  push).
- **Compromised *backup host* (either model):** this host by design can read all DB
  servers and write the store — it is the crown jewels. Its store credential must still
  be **append-only**, and pruning must be a *separate* identity, so even owning the
  backup host cannot delete existing immutable backups.

**Key nuance from the research:** "blast radius follows the dependency graph, not the
network topology." Whoever holds delete/prune rights is your true single point of
failure. Append-only client mode (borg/restic, Section 8) protects against a
compromised *client*, but is **void if the attacker reaches the identity/host that runs
`prune`/`forget`**, and neither protects against **root on the storage server itself**.
So: managed immutable object storage > self-hosted storage you also administer from the
same blast zone.

**Practical solo setup:** backup host pulls from DBs → writes to managed object storage
with append-only key + Object Lock. Retention/lifecycle enforced by the *bucket* (server
side), not by a delete-capable client. Break-glass identity offline.

---

## 5. Encryption at rest — and what it does NOT do

- **Do** encrypt backups at rest (and in transit): SSE-S3/SSE-KMS server-side, or
  client-side encryption (restic/borg do this by default) so the provider never sees
  plaintext. Protects **confidentiality** if the bucket/credentials/media leak.
- **Encryption is not ransomware protection.** Ransomware's weapon against backups is
  **deletion/overwrite**, not reading. An encrypted-but-deletable backup is still gone.
  Availability is protected by **immutability + versioning + isolated delete rights**,
  which are orthogonal to encryption.
- **Config/credential encryption is a separate concern again.** Encrypting your
  backup-tool config (DB passwords, cloud keys) reduces the chance creds *leak*, but
  once the tool is running those creds are decrypted in memory and usable — so it does
  nothing against an attacker who already controls the host. It complements, but never
  replaces, append-only creds + immutable storage.
- **Guard the encryption keys.** Client-side encryption means *losing the key = losing
  the backups*. Store the key/passphrase separately from the backups and from the
  backup host (e.g. a secrets manager + an offline copy). A lost key is self-inflicted
  data loss.

---

## 6. Restore verification — "an untested backup isn't a backup"

You do not have backups until you have *restored* from them. ~30–40% of orgs that never
test discover corruption/incompleteness only during a real disaster. Integrity
checksums are necessary but **not sufficient** — they catch bit-rot, not logically
valid but semantically wrong dumps, missing objects, or broken restore *procedures*.

**Tiered verification (automate the lower tiers, schedule the higher ones):**

| Level | Check | Frequency |
|---|---|---|
| 1 | Existence: expected file(s) present, plausible size/count | every backup |
| 2 | Integrity: checksums valid, archive not corrupt | every backup |
| 3 | Partial restore: restore to a scratch instance, `pg_dumpall --schema-only` / `SHOW TABLES`, row-count spot checks | weekly |
| 4 | Full restore into a throwaway DB, WAL replay / PITR to a target time | monthly |
| 5 | Application connects and business logic works against the restored DB | monthly/quarterly |

**Automation pattern:** after each backup, run integrity check + Level-1/2. On a
schedule, spin up an ephemeral DB container, restore the latest backup into it, run
`pg_restore`/`mysql` import, assert schema + key row counts, emit a metric, tear down.
Alert on *failure to verify*, not just on backup-job failure. Track restore *time* too
(RTO regressions are a silent failure). AWS Backup, pgbackrest_auto, and similar tools
formalize this loop.

**How the mature tools verify integrity:**
- **restic**: content-addressed chunks; `restic check` verifies structure and (with
  `--read-data`) re-hashes every pack. Everything is checksum-addressed, so corruption
  is detectable.
- **borg**: `borg check` verifies repository and archive consistency via chunk hashes.
- **pgBackRest**: stores a per-file **checksum in the manifest**; validates PostgreSQL
  **page checksums** during backup (warns on bad pages *before* the good backup
  expires), and `pgbackrest verify` validates a repo without a full restore; delta
  restore re-checksums existing files.
- **barman**: checksum verification on backup creation and restore;
  `barman verify-backup` / `barman check`.

---

## 7. Retention / GFS (grandfather-father-son) rotation

GFS keeps more granularity for recent data, less for old, controlling cost while
preserving long recovery windows:

- **Son** = frequent short-term (e.g. daily, kept ~7–14 days).
- **Father** = weekly, kept ~4–8 weeks.
- **Grandfather** = monthly (optionally yearly), kept ~6–12+ months.

Practical rules:
- Match retention to **RPO** (how much data you can lose = backup frequency) and
  **RTO** (how fast you must restore = restore-tested procedure), plus any compliance
  window.
- **Object-Lock retention should cover at least your incident dwell time** (attackers
  often sit undetected for days/weeks) so a locked-but-short backup isn't already
  expired when you need it.
- Enforce expiry via **S3 lifecycle rules on the bucket** (server-side), executed by a
  privileged/lifecycle identity — *not* by giving the backup writer delete rights.
- Beware: Object Lock retention and lifecycle expiration interact — an object can't be
  lifecycle-deleted until its lock expires. Keep lock retention ≤ your longest GFS tier
  you intend to actually delete.

---

## 8. Transferable lessons from existing tools

**restic** — content-addressed, chunk-level dedup; **client-side encryption by
default**; `rest-server --append-only` refuses DELETE over the REST API, so a
compromised client that knows the repo password still can't delete data. *Lesson:*
dedup + encryption + a server that enforces append-only server-side. *Caveat:*
`forget`/`prune` need full delete access — run them from a separate, well-secured
client, never the append-only one.

**borgbackup** — dedup + compression + encryption; canonical
**SSH + `borg serve --append-only --restrict-to-repository`** model: a host pushes
archives but the server-side SSH `command=` restriction stops it deleting prior
archives even if fully compromised. *Lesson:* enforce append-only at the *server/SSH*
layer, not by trusting the client.

**pgBackRest** — purpose-built for Postgres: parallel full/diff/incremental, WAL
archiving + **PITR**, per-file manifest checksums, page-checksum validation during
backup, `verify` without restore, delta restore. *Lesson:* verify integrity
continuously and *before* good backups expire; make PITR and restore-verification
first-class.

**barman** — Postgres backup + WAL management, streaming/rsync backups, PITR, retention
policies, `verify-backup`/`check`. *Lesson:* built-in retention policy engine +
integrity checks as routine operations.

**Common threads to steal for this tool:**
- Dedup + compression to make offsite/immutable storage affordable (immutability keeps
  *every* retained version, so size matters).
- Client-side encryption by default; key stored separately.
- **Append-only enforced by the destination/server**, not the client.
- WAL archiving / incremental for tight RPO with PITR.
- Integrity verification (checksums/page-checksums) baked into every backup, plus
  scheduled *restore* verification for the "+0".
- Immutable repo/target as the default, not an add-on.

---

## Concrete checklist for the solo developer

- [ ] Object storage bucket with **versioning ON** + **Object Lock ON** (start
      governance-mode w/ isolated bypass, or compliance-mode w/ 14–35d retention after
      testing).
- [ ] **Append-only** IAM key for the backup writer (explicit Deny on Delete*,
      lifecycle, retention/legal-hold, versioning). Test that a delete actually fails.
- [ ] **Separate identities**: writer (append-only) / reader (restore) / lifecycle+prune
      (privileged, off-host) / break-glass (MFA, offline).
- [ ] **Pull topology**: backup host pulls from DBs; DB hosts hold no destination creds.
- [ ] **Client-side or SSE encryption**; encryption key/passphrase stored separately
      from backups and backup host.
- [ ] **Retention via bucket lifecycle** (GFS tiers), lock retention ≥ expected
      detection/dwell time.
- [ ] **Integrity check every backup** + **automated restore test** (ephemeral DB) on a
      schedule; alert on *failure to verify*, track restore time.
- [ ] Backups in a **different cloud account/provider** than production; monitoring +
      alerting on missed/failed jobs.

---

## Sources

3-2-1 / 3-2-1-1-0:
- https://www.veeam.com/blog/321-backup-rule.html
- https://www.zmanda.com/blog/understanding-the-3-2-1-1-0-backup-rule/
- https://objectfirst.com/guides/data-backup/3-2-1-backup-rule-and-strategy/
- https://www.avepoint.com/blog/backup/what-is-the-3-2-1-backup-rule

S3 Object Lock (governance/compliance/legal hold):
- https://aws.amazon.com/s3/features/object-lock/
- https://docs.aws.amazon.com/AmazonS3/latest/userguide/object-lock.html
- https://cloudian.com/blog/s3-object-lock-protecting-data-for-ransomware-threats-and-compliance/
- https://objectfirst.com/guides/immutability/s3-object-lock-for-ransomware-protection/

IAM append-only / deny delete:
- https://docs.aws.amazon.com/AmazonS3/latest/userguide/example-bucket-policies.html
- https://docs.aws.amazon.com/AmazonS3/latest/userguide/example-policies-s3.html
- https://repost.aws/questions/QUrnYsBQrfTvCmHTef5qO4XQ/permissions-to-create-but-not-delete-or-update

Backblaze B2 / MinIO Object Lock:
- https://www.backblaze.com/docs/cloud-storage-object-lock
- https://www.backblaze.com/blog/digging-deeper-into-object-lock/
- https://docs.min.io/aistor/administration/object-locking-and-immutability/

Pull vs push / blast radius:
- https://www.rack2cloud.com/backup-blast-radius/
- https://localhake.com/content/pbs-321-part-2-pbs-to-pbs-pull-sync
- https://nimbus.rdem-systems.com/en/blog/pbs-replication-proxmox-backup-server/

restic / borg append-only:
- https://marcusb.org/posts/2024/07/ransomware-resistant-backups-with-borg-and-restic/
- https://restic.readthedocs.io/en/latest/060_forget.html
- https://forum.restic.net/t/is-a-safe-forget-command-possible-when-the-append-only-client-was-compromised/4543
- https://computingforgeeks.com/immutable-backups-ransomware-proof-linux-minio-restic-borg/

pgBackRest / barman / restore verification:
- https://pgbackrest.org/user-guide.html
- https://github.com/vitabaks/pgbackrest_auto
- https://oneuptime.com/blog/post/2026-01-21-postgresql-backup-testing/view
- https://oneuptime.com/blog/post/2026-01-30-backup-verification-testing/view
- https://aws.amazon.com/blogs/storage/implementing-restore-testing-for-recovery-validation-using-aws-backup/
