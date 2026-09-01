# RED TEAM Review — Ransomware-Resilient RestorePlane

Reviewer role: adversarial security / red team. Scope: envelope encryption, ransomware
kill chain, public-repo secret handling, config encryption, supply chain, restore trust.
Verdict: the availability story (Object Lock + append-only) is sound on paper but is
unenforced and has reachable bypasses; the confidentiality story has a structural,
unrecoverable flaw (single static DEK over immutable storage); and the restore path is an
unauthenticated code-execution sink.

Severity legend: Critical / High / Med

---

## C1 (Critical) — DPAPI Machine-scope + public-repo binary = any local process (or transient malware) recovers the DEK and every secret

The attack. wrap#3 stores the KEK under DPAPI Machine scope. Machine-scope DPAPI, by
definition, encrypts data that ANY process on the machine can decrypt (your own
rust-stack.md section 3). The only isolation is pOptionalEntropy, but the entropy is
compiled into a binary whose source is in a PUBLIC GitHub repo, so the entropy is public.
An attacker who gets any code execution on the backup host (even unprivileged, even for one
minute) does: read the DPAPI-wrapped KEK, CryptUnprotectData with the public entropy,
recover KEK, fetch the KEK-wrapped DEK (also in the public repo), recover the DEK, decrypt
every backup and every enc: config secret (DB passwords, S3 keys, SSH creds).

Why it works. The unattended requirement forces the key to be available to the machine with
no human in the loop. "Available to the machine unattended" is identical to "available to
anything running on the machine." App entropy only stops off-box theft, not another process
on the same box (the doc admits this). Putting the entropy in a public binary removes even
that partial protection.

Mitigation.
- Treat the DEK as confidentiality-only, never as a ransomware control (it isn't).
- Do NOT commit the DPAPI-wrapped blob (machine-local and meaningless elsewhere; see H4).
- Derive DPAPI entropy from a non-committed local keyfile (NTFS ACL to SYSTEM + task
  account), not a hardcoded constant, so public binary + copy of the blob is insufficient.
- Run the task as a dedicated least-privilege service account, not SYSTEM; lock install dir
  and keyfile ACLs so arbitrary users cannot invoke the decrypt path.
- Consider TPM-backed or CNG non-exportable machine key so key material cannot be copied
  off-box at all.

---

## C2 (Critical) — Single static DEK over WORM storage = an unrotatable, unrecoverable confidentiality breach

The attack. One random 32-byte DEK encrypts all dumps, forever. If ANY of the three wraps is
ever compromised (password brute-forced C4, recovery code leaked, DPAPI/entropy exposed C1),
the attacker unwraps the DEK once and can decrypt ALL past and future backups. Rotating the
compromised wrap does nothing: the DEK value is already in the attacker's hands. The only
real remediation is generate a new DEK and re-encrypt all history. But backups live in Object
Lock (WORM) immutable storage: by design you cannot overwrite or delete them inside the
retention window. So the historical ciphertext, readable with the leaked DEK, is permanently
exposed and permanently un-remediable. The two headline features (immutable storage + single
reusable DEK) are in direct conflict for confidentiality.

Why it works. Multi-recipient wrapping lets you re-wrap without re-encrypting, but that only
helps against losing access, not against disclosure. Against disclosure, wrap rotation is
useless because the wrapped secret (DEK) is the same value, and immutability forbids the one
action (re-encrypt) that would fix it.

Mitigation.
- Per-backup (or per-epoch) DEKs. Fresh DEK per run/epoch, wrapped to the recipients. A
  leaked DEK then exposes only one backup/epoch; future backups are safe without touching
  immutable history. age multi-recipient supports this naturally.
- Rotating KEK generation so a compromised KEK is retired for future epochs.
- Document that historical confidentiality cannot be recovered after DEK loss, so retention
  windows are chosen with that blast radius in mind.

---

## C3 (Critical) — Auto-restore of an attacker-supplied dump = remote code execution, and backups have no origin authentication

The attack (two layers).
1. Poisoned restore -> RCE. Restoring a logical dump feeds attacker-influenced SQL to
   psql/pg_restore/mysql. These are code-execution engines, not passive data:
   - PostgreSQL: COPY ... FROM PROGRAM 'cmd', CREATE FUNCTION ... LANGUAGE plpythonu/plperlu,
     and the psql meta-command backslash-bang all run OS commands on the restore host.
   - MySQL: LOAD DATA LOCAL INFILE, INTO OUTFILE, and stored-program payloads.
   The Verifier auto-runs on a schedule and restores "the latest backup" into an instance, so
   a malicious dump yields automated RCE on the verifier host with no operator present.
2. No authenticity, only integrity. age STREAM (ChaCha20-Poly1305) gives integrity against
   bit-flips, but it is SYMMETRIC: anyone holding the DEK (C1/C2) can produce a perfectly
   valid, authenticated malicious backup. Append-only IAM allows PutObject, so an on-box
   attacker can upload a new version/object that is a valid poisoned dump. Object Lock
   protects old versions from deletion but does nothing to stop a new malicious object being
   added and then selected as "latest."

Why it works. The design equates "decrypts cleanly" with "trustworthy." It never verifies who
produced the backup, and the restore consumer (SQL client) is a powerful interpreter. The
auto-verify loop turns a write into an exec.

Mitigation.
- Sign every backup manifest with an asymmetric key whose private half is OFFLINE (age /
  minisign / ed25519, kept off the backup host). Restore/verify must reject any object
  without a valid signature. A DEK-holding, host-owning attacker then still cannot forge a
  trusted backup, closing both forgery and substitution paths.
- Restore into a hardened sandbox: ephemeral container/VM, no outbound network, no host
  mounts, non-privileged DB; disable dangerous features (local_infile=0; restrict/deny
  untrusted PLs and COPY PROGRAM; pg_restore with --no-privileges, never as superuser; never
  pipe untrusted dumps through interactive psql).
- Monotonic, signed catalog/manifest so "latest" cannot be silently substituted/rolled back.
- Verify against a signed expected-checksum from the manifest, not local SQLite alone.

---

## C4 (Critical) — Offline brute force of the password-wrapped DEK straight from the public repo

The attack. The KEK-wrapped DEK, the Argon2id salt, and the exact Argon2 params are all
committed to the PUBLIC repo (spec: repo holds the 3 wrapped DEKs + encrypted config). An
attacker clones the repo and brute-forces the master password OFFLINE, at their own hardware
scale, with no rate limiting and unlimited attempts. Argon2id m=64MiB/t=3 is respectable
(~100 ms/guess/core) but is not a substitute for password strength: a weak or human-memorable
master password falls to a GPU/ASIC dictionary+rules attack. Success yields the DEK -> all
backups + all config secrets (compounds C2).

Why it works. Publishing the wrap removes the only thing that normally throttles guessing (a
live server / hardware possession). Argon2 raises cost per guess but the attacker controls
budget and parallelism.

Mitigation.
- Do NOT commit the password-wrap to a public repo. Strongly reconsider the public-repo
  premise; use a private repo (H3). If public is truly required, keep the wrapped-DEK bundle
  out of the repo (local + offline copy only) so there is no offline target at all.
- Enforce a strong master password at init (length >= 12-16, zxcvbn-style gate, reject the
  weak-password case the spec already lists); prefer a generated high-entropy passphrase.
- Raise memory hardness where RAM allows (m=256 MiB) since login latency is irrelevant here.
- The recovery-code wrap must meet the same bar (>=128-bit entropy): it too is a committed,
  offline-attackable wrap.

---

## H1 (High) — Where does prune/bypass run? If automated on the backup host, append-only + Object Lock are void

The attack. Append-only only holds while the delete-capable identity is unreachable from the
compromised host. Your research is explicit: blast radius follows the dependency graph, and
append-only is void if the attacker reaches the identity/host that runs prune/forget. The spec
says prune uses a "separate isolated identity" but never says WHERE it runs. If retention/prune
is a scheduled task on the same backup host (the natural, lazy implementation), its privileged
creds sit on the box; an attacker who owns the box gets delete/bypass rights and
deletes/shortens everything. Governance mode makes this worse: its entire premise is that
s3:BypassGovernanceRetention CAN delete inside the window.

Why it works. Convenience pushes prune onto the always-on backup host, silently collapsing the
two identities into one blast zone.

Mitigation.
- Prune/lifecycle must NEVER run on the backup host. Enforce retention with server-side S3
  bucket lifecycle rules (executed by S3 itself), or a separate off-host MFA-gated identity.
  The backup tool itself should have no delete/bypass path.
- Keep the break-glass/bypass identity offline + MFA, used only by hand.
- Default to Compliance mode with modest (14-35 day) retention once proven; Governance's
  bypass is a standing hole an account-level compromise can walk through.

---

## H2 (High) — Append-only IAM is an unenforced manual template; the tool never self-verifies it, and Governance is the weak default

The attack. The append-only guarantee rests entirely on the operator correctly applying the
IAM template. Nothing in the tool enforces or checks it. A solo operator under pressure pastes
a broad s3:* key "to make it work," and every immutability guarantee silently evaporates with
no warning. Additionally, if the bucket lacks versioning or a default retention, PutObject
creates freely deletable/overwritable objects and Object Lock protects nothing.

Mitigation.
- Self-test at init/run: with the writer creds, attempt DeleteObject /
  PutLifecycleConfiguration / PutObjectRetention against a probe key and assert they are
  DENIED; assert versioning=ON and a default retention is set. Refuse to run (or loud-warn) if
  creds are over-privileged or the bucket is not locked. (Your checklist says "test that a
  delete actually fails" — make the tool do it every run.)
- Ship the deny-list exactly as in research (Delete*, lifecycle, versioning, retention,
  legal-hold, bypass); document B2/MinIO equivalents.
- Prefer managed object storage over self-hosted MinIO (M3).

---

## H3 (High) — Committing even an "encrypted" config to a public repo leaks a reconnaissance goldmine (metadata)

The attack. Only enc: secret fields are encrypted; the rest of the TOML is plaintext in a
public repo. That exposes DB hostnames/IPs + ports, usernames, database names, the S3 bucket
name + endpoint/region, SSH/bastion host + user, schedule times, and retention config. This is
a targeted-attack map: the bucket name enables direct S3 enumeration/attacks; usernames are
half of every credential and enable focused brute force; schedule times tell the attacker
exactly when backups run (and when to strike); the bastion/SSH host is a labeled front door.
"Encrypted config" gives a false sense of safety.

Mitigation.
- Use a PRIVATE repo. The public-repo premise buys nothing here and costs metadata + provides
  the C4 offline-attack target. This single change neutralizes H3 and greatly reduces C4.
- If public is mandatory: encrypt the whole config (or all non-cosmetic fields) under the DEK,
  not just secret fields; keep plaintext-metadata config local/gitignored; commit only an
  opaque encrypted blob.
- Treat bucket names / hostnames as sensitive; keep them out of committed docs/README too.

---

## H4 (High) — The DPAPI-wrapped blob should never be committed (spec commits all 3 wraps)

The attack. Spec commits all three wraps, including the DPAPI one, to the public repo. The
DPAPI blob is machine-local (useless off-box) so committing it gains nothing, but it WIDENS
exposure: combined with the public binary's entropy (C1), any future on-box code-exec (or
anyone who images/exfiltrates the host) can decrypt it. It also advertises the exact
key-management structure to attackers.

Mitigation. Store the DPAPI wrap only on the host (outside the repo), ACL-restricted. Commit
at most the password-wrap and recovery-wrap, and per H3/C4 ideally not even those to a public
repo.

---

## H5 (High) — Local SQLite catalog is mutable and the restore path trusts it

The attack. The catalog (backup inventory, checksums, verified/unverified state) is a local
SQLite file with no immutability. An attacker on the backup host edits the catalog: repoint
"latest good" at a poisoned object, mark a malicious backup verified, or hide/rename real
backups so restore cannot find them. Restore/verify then trusts tampered metadata. If the
catalog is only local and is lost/wiped, you may not even know which immutable objects are
valid.

Mitigation.
- Anchor restore trust in a signed manifest stored immutably alongside the backups (Object
  Lock), not in local SQLite; the local catalog becomes a cache, not the source of truth.
- Include a monotonic sequence number + signature in each manifest to detect
  substitution/rollback.
- Back up the catalog itself immutably, and make restore able to reconstruct from the object
  store's signed manifests without the local DB.

---

## H6 (High) — Supply-chain risk in the chosen crates

- windows-dpapi (0.x): low version, small surface, wraps sensitive CryptProtectData. A
  low-maturity single-purpose crate protecting your KEK is a high-value supply-chain target.
  Consider calling windows/CNG directly (larger, Microsoft-aligned, auditable) or vendor+vet.
- russh 0.62.2: security-critical SSH implementation with documented API churn and a DIY
  host-key policy (research section 4). A wrong/naive client::Handler = accept-any-host-key =
  MITM of the DB tunnel (credential + data theft). A malicious minor bump could weaken this
  silently.
- age 0.12.1 (BETA / pre-1.0): reputable author, stable format, but pre-1.0 API; pin and track
  advisories.
- opendal 0.58: large transitive tree (HTTP stack, cloud SDK surface). Watch for openssl
  creeping in transitively (C-build + CVE surface on Windows); force rustls.
- Process/tooling:
  - Commit Cargo.lock (it is a binary: lockfile must be pinned) to prevent dependency
    substitution; pin exact versions (=x.y.z) for security-critical crates (russh, age, dpapi).
  - Add cargo-deny + cargo-audit to CI, failing the build on High/Critical advisories, yanked,
    unknown source, license issues.
  - Watch typosquatting on small crates (windows-dpapi and look-alikes); verify crate/author
    before pinning.
  - Bundled pg_dump/mysqldump binaries are a supply-chain vector too: pin versions, verify
    vendor signatures/hashes, store them where a local attacker cannot swap them (ACL +
    hash-check before exec).

---

## M1 (Med) — Recovery-code exposure at display time
"Show once, store offline" prints a root-of-trust secret to a terminal: it can land in shell
history, terminal scrollback logs, screen-recording/RDP capture, or the clipboard. Mitigation:
gate with a clear "write this down, it will not be shown again"; avoid clipboard; consider
writing to an operator-chosen offline file with restrictive ACLs rather than stdout; require
re-entry to confirm capture. Ensure >=128-bit entropy (C4).

## M2 (Med) — Plaintext dump temp files on disk
If any stage buffers the dump to a temp file (large dumps, mydumper's directory output which
breaks the pipe model), plaintext DB contents hit disk unencrypted and could be committed or
left behind. Mitigation: keep the strict streaming pipe (dump->zstd->age->writer) with no
plaintext spill; for mydumper, encrypt the output dir before it lands or write to an encrypted
volume; gitignore + secure-delete temp; use a dedicated temp dir with tight ACLs.

## M3 (Med) — Self-hosted MinIO destination is defeated by root on that box
Research sections 2/4: self-hosted WORM is only as strong as the host you also administer from
the same blast zone. Mitigation: prefer managed object storage in a separate cloud account
from production; if MinIO, run it on a separately-owned/hardened host you do not administer
from the backup host, and enable compliance-mode locking.

## M4 (Med) — Object Lock preconditions unverified; retention vs dwell time
If versioning is not ON or default retention is not set, Object Lock protects nothing; and a
lock window shorter than attacker dwell time means the immutable copy has already expired when
you need it. Mitigation: verify versioning + default-retention at runtime (H2); set lock
retention >= expected detection/dwell window (research section 7).

## M5 (Med) — Compliance-mode self-lockout
A fat-fingered long retention in Compliance mode is unrecoverable (even root cannot fix) and
you pay storage forever. Mitigation: cap/validate retention input, require explicit opt-in +
confirmation for Compliance, force a throwaway-bucket dry run first (research notes it; enforce
it in the tool).

## M6 (Med) — Secrets leaking into logs
DB/SSH/S3 errors often echo connection strings, hostnames, or tokens into logs, which may be
less protected than the config. Mitigation: structured logging with explicit secret redaction;
never log the DEK/KEK/recovery code/config secrets; scrub URLs/DSNs before logging.

## M7 (Med) — gitignore gaps (public repo)
Must be ignored/never committed: the DPAPI blob, the local *.sqlite catalog, any *.env,
recovery-code files, temp dump files/dirs, logs, and any local keyfile/entropy source. A single
miss = confidentiality loss. Mitigation: ship a strict .gitignore + a pre-commit secret scanner
(gitleaks/trufflehog) in CI that fails on any high-entropy/known-pattern match; document the
"never commit" list in the repo.

---

## Summary table

| ID | Sev | One-line |
|----|-----|----------|
| C1 | Critical | DPAPI machine-scope + public-binary entropy -> any local process/malware recovers DEK + all secrets |
| C2 | Critical | Single static DEK over WORM = unrotatable, unrecoverable confidentiality breach if any wrap leaks |
| C3 | Critical | Auto-restore of attacker dump = RCE; symmetric AEAD gives no origin auth (forgeable/substitutable) |
| C4 | Critical | Password-wrapped DEK in public repo -> unlimited offline brute force |
| H1 | High | Prune/bypass identity may live on the backup host -> append-only void |
| H2 | High | Append-only IAM unenforced/unverified; Governance weak default; versioning/retention unchecked |
| H3 | High | Public "encrypted" config leaks metadata (hosts, users, bucket, bastion, schedule) |
| H4 | High | DPAPI wrap should never be committed |
| H5 | High | Mutable local SQLite catalog is trusted by restore |
| H6 | High | Supply chain: windows-dpapi 0.x, russh host-key/churn, Cargo.lock + cargo-deny/audit missing |
| M1-M7 | Med | Recovery-code display, plaintext temp, self-hosted MinIO, lock preconditions, self-lockout, log leaks, gitignore |

## Top structural recommendations (highest leverage)
1. Sign backups with an OFFLINE private key (C3) and use per-backup DEKs (C2): together these
   turn "on-box compromise = total confidentiality loss + forgeable restores" into "on-box
   compromise = bounded, non-forgeable."
2. Use a PRIVATE repo and keep all wraps + metadata out of any public location (C4, H3, H4).
3. Enforce append-only at runtime (self-test deny) and never run prune on the backup host
   (H1, H2). The whole ransomware story depends on these, and both are currently unenforced.
4. Sandbox the auto-restore and treat every dump as hostile input (C3).

---

## Dictionary Updates

- ADD -- Confidentiality-Rotation Deadlock: the failure mode where immutable (WORM) storage
  forbids re-encryption, so a single reused data key (DEK) cannot be rotated for
  already-stored data; a key disclosure becomes permanently un-remediable. (From C2.)
- ADD -- Restore-as-Exec: logical DB restore is code execution (COPY FROM PROGRAM,
  psql backslash-bang, untrusted PLs, LOAD DATA LOCAL), so an auto-restore/verify loop over
  attacker-influenced dumps is an RCE sink. (From C3.)
- ADD -- Origin Authentication (vs Integrity): symmetric AEAD proves the ciphertext was not
  bit-flipped, not who produced it; any DEK holder can forge a valid backup. Distinguish from
  integrity throughout; add asymmetric signing for origin authenticity. (From C3.)
- ADD -- Unenforced-Control: a security guarantee that depends on an out-of-band manual step
  (IAM template, versioning toggle) with no runtime verification; treated as present but often
  absent. (From H2.)
- REFINE -- Append-only: clarify it protects existing objects from delete/overwrite only; it
  still permits PutObject of new poisoned versions, and is void if the prune/bypass identity is
  reachable from the compromised host. Not a standalone guarantee.
- REFINE -- DPAPI Machine-scope: means "decryptable by ANY process on the machine"; app entropy
  only blocks off-box theft and is worthless if the entropy ships in a public binary. Not an
  access-control boundary against local code.
- REFINE -- Encrypted config: encrypting only enc: secret fields still leaks all metadata
  (hostnames, usernames, bucket, bastion, schedule); reconnaissance value, not safe to publish.
