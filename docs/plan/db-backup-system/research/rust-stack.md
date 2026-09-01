# Rust Stack Research — Windows DB Backup System

Research date: 2026-07-16. Versions verified against docs.rs / crates.io. Facts checked via
web search, not memory. Items marked `[확인 필요]` need runtime confirmation in this project.

---

## 0. TL;DR recommended stack

| Concern | Pick | Version | Why |
|---|---|---|---|
| Envelope encryption | `age` (library API) | 0.12.1 | Streaming AEAD (STREAM), passphrase + recipient modes, well-audited format |
| KEK derivation | `argon2` (RustCrypto) | 0.5.3 | Argon2id, OWASP params. (age already does scrypt internally for passphrase mode) |
| OS secret store | `windows-dpapi` (Machine scope) primary; `keyring` 4.x optional | dpapi 0.x / keyring 4.1.5 | DPAPI Machine scope is the robust unattended path |
| SSH tunneling | `russh` | 0.62.2 | Pure Rust, no libssh2/OpenSSL build friction on Windows, native local-forward |
| Object storage | `opendal` | 0.58.0 | S3 + local + fs unified. NOTE: SFTP backend is Unix-only |
| Compression | `zstd` | 0.13.3 | Mature libzstd binding, streaming Encoder/Decoder |
| CLI / config | `clap` + `serde` + `toml` | clap 4.6.2 | Standard, nothing exotic |
| Dump tools | native `pg_dump` / `mysqldump` (or `mydumper`) | bundled per-DB | Client version >= server version |
| Scheduling | Windows Task Scheduler via `schtasks` | built-in | Simpler + more robust for "run at time, catch up after reboot" than a service |

---

## 1. Encryption — `age` vs RustCrypto AEAD

**`age` crate** — v0.12.1 (2026-07-14), status: BETA (pre-1.0, but the *format* v1 is stable and
widely deployed via the `rage` CLI; author str4d is an active, reputable cryptographer).

- **Streaming: yes.** age uses the STREAM construction (ChaCha20-Poly1305 over 64 KiB chunks) so
  large dumps encrypt/decrypt with constant memory. API: `Encryptor::...wrap_output()` returns a
  `StreamWriter` you write the plaintext into. **Gotcha:** you MUST call `StreamWriter::finish()` —
  skipping it produces a truncated, undecryptable file (the final auth tag is written in `finish`).
- **Modes:**
  - Recipient mode — `Encryptor::with_recipients(...)` + `age::x25519::Recipient` (asymmetric,
    public-key). Decrypt with `age::x25519::Identity`. Also supports `ssh-ed25519`/`ssh-rsa` recipients.
  - Passphrase mode — `Encryptor::with_user_passphrase(...)` which internally uses **scrypt** as the
    KDF (`age::scrypt::Recipient` / `Identity`).

**RustCrypto** — `aes-gcm` (0.10.x) and `chacha20poly1305` (0.10.x), both stable, actively
maintained, no_std-capable. These are raw AEAD primitives only: **no streaming framing, no key
wrapping, no file format.** You would hand-roll chunking, nonce management, and headers — easy to
get wrong.

**Envelope encryption recommendation:** The classic pattern is: generate a random DEK → encrypt the
dump with the DEK → wrap (encrypt) the DEK with a password-derived KEK.
- Cleanest: **let `age` be the whole envelope.** age's passphrase mode already *is* envelope
  encryption — it generates a random file key (DEK), streams the payload under it, and wraps the
  file key with a scrypt-derived KEK. One dependency, audited construction, streaming built-in.
- If you want an explicit password→KEK with **Argon2id** (stronger than age's scrypt) and/or a DEK
  you can re-wrap for multiple recipients without re-encrypting the dump: derive KEK with `argon2`,
  wrap a random 32-byte DEK with `chacha20poly1305`, and encrypt the dump with `age` recipient mode
  (DEK as an X25519-independent symmetric key) or with `chacha20poly1305` STREAM. Slightly more code.

**Verdict:** Use `age` for the streaming payload encryption. For the KEK, either accept age's scrypt
(simplest) or add `argon2` for an Argon2id KEK wrapping a random DEK (best-in-class). Do **not**
hand-roll streaming with bare `aes-gcm`.

---

## 2. KDF — `argon2` crate

- Version: **0.5.3** stable (2024-01-20); 0.6.0-rc.x exists (rc.8, 2026-03) but stick to 0.5.3 unless
  you need the RC. RustCrypto org, maintained, high trust.
- Default params in the crate already are **Argon2id, v0x13 (19)**, m=19 MiB, t=2, p=1 — this equals
  the OWASP *minimum* (19 MiB / 2 iters / 1 lane).
- **Recommended params for a backup tool** (login is not latency-critical, so go higher than the
  minimum): **Argon2id, m=64 MiB, t=3, p=1** (~100 ms/derivation on a modern core, current 2026
  OWASP interactive guidance). If the box has RAM headroom, m=256 MiB with t=2 is even stronger
  (memory hardness beats time against GPU/ASIC).
- Usage: `Argon2::new(Algorithm::Argon2id, Version::V0x13, Params::new(65536, 3, 1, Some(32))?)`
  then `hash_password_into(pw, salt, &mut kek)` to fill a 32-byte KEK. Store the salt + params
  alongside the ciphertext (age handles this automatically if you use its passphrase mode instead).
- **Gotcha:** memory cost is in KiB (65536 = 64 MiB). Persist the exact params used, or you can't
  re-derive after you bump defaults.

---

## 3. OS keystore on Windows — `keyring` + DPAPI

**`keyring` crate** — v**4.1.5** (2026-07-14). Actively maintained (open-source-cooperative).
**Major architectural change in 4.x:** the crate was split — the API now lives in `keyring-core`
and each credential store is a separate crate (e.g. `windows-native-keyring-store`). You now
**explicitly allocate and `set_default_store()` at startup** instead of selecting a backend by cargo
feature. If you follow old 3.x tutorials they will not compile against 4.x.

- Windows backend = **Windows Credential Manager** (generic credentials). Persistence types:
  Session / Local / **Enterprise** (default). Credentials are **per-user** and are stored encrypted
  under the user's DPAPI keys.
- **Unattended scheduled-task question:** Credential Manager entries are unlocked automatically *when
  that user is logged on / when a process runs in that user's security context.* A Task Scheduler
  task configured **"Run whether user is logged on or not"** under the same user account (with the
  password saved, so the profile/creds load) *can* read them. But this is the fragile part:
  - The user profile must be loadable; DPAPI user-scope secrets need the user's key material.
  - `keyring` docs warn the Windows backend is not reliably safe across concurrent threads on the
    same entry (a set immediately followed by a get on another thread can return `NoEntry`) — fine
    for a single-threaded backup run, just don't race it.

**DPAPI direct (`windows` crate / `windows-dpapi` crate)** — for a *truly unattended* service the
cleaner path is DPAPI **Machine scope**:
- `windows-dpapi` is a safe wrapper; `Scope::Machine` encrypts data that **any process on the
  machine can decrypt** (no user login required) — ideal for headless/scheduled tools. `Scope::User`
  ties it to the user. Add `pOptionalEntropy` as an app-specific salt so other machine processes
  can't trivially decrypt.
- Or call `CryptProtectData` / `CryptUnprotectData` directly via the `windows` crate
  (`windows::Win32::Security::Cryptography`, flag `CRYPTPROTECT_LOCAL_MACHINE`).

**Recommendation:** For an unattended scheduled backup, use **DPAPI Machine scope (`windows-dpapi`)
with app entropy** to protect the master passphrase/DEK at rest — it works with no interactive login
and no saved-password profile-load dependency. Use `keyring` only if you specifically want the
credential to appear in Credential Manager and the task always runs as a logged-capable user.
**Gotcha:** Machine-scope DPAPI protects against off-box theft, not against another admin/process on
the same box — entropy mitigates but does not fully isolate.

---

## 4. SSH — `russh` vs `ssh2`

| | `russh` 0.62.2 | `ssh2` 0.9.6 |
|---|---|---|
| Impl | Pure Rust (Tokio async), fork of Thrussh | Bindings to C `libssh2` |
| Maintenance | Very active (Eugeny) | Maintained but slow; libssh2 is old/stable |
| Local port forward (tunnel to DB behind bastion) | Yes — native `channel_open_direct_tcpip` + server-side local/reverse forwarding; used in production tunneling projects | libssh2 supports `direct-tcpip`, but the Rust `ssh2` binding historically lacked a clean local-forward API (open issue asking for `ssh_channel_open_forward` equivalent) |
| Windows build friction | **Low** — no C toolchain, no OpenSSL. Pure Rust + Tokio | **Higher** — needs libssh2 + OpenSSL/zlib; vendored build or vcpkg. Common source of Windows CI pain |
| Async | Native async/Tokio | Blocking (sync) |

**Recommendation:** Use **`russh`**. For reaching a DB behind a bastion, open a `direct-tcpip`
channel and bridge it to a local `TcpListener` (or feed the channel stream straight to your DB
client). Pure-Rust avoids the libssh2/OpenSSL Windows build headaches, which is the decisive factor
here. **Gotcha:** russh is lower-level — you implement `client::Handler` and must decide a host-key
verification policy yourself (do not blindly accept unknown keys). Also russh's API churns between
minor versions; pin the exact version.

---

## 5. Object storage — `opendal`

- Version: **0.58.0** (2026-07-10). Apache project, very active, high trust. Unified API over many
  backends: **S3** (and S3-compatible: MinIO, R2, DO Spaces, Tencent COS), **fs/local**, azblob,
  gcs, and **SFTP**.
- **Streaming large uploads: yes.** `Operator::writer()` returns a writer that does chunked /
  multipart uploads under the hood; you can stream a dump of arbitrary size without buffering it all.
- **S3 Object Lock / immutability:** OpenDAL writes into Object-Lock-enabled buckets, but it does
  **not expose per-object retention headers** (`x-amz-object-lock-retain-until-date`,
  legal-hold) as first-class write options. `[확인 필요]` for 0.58 exact surface. Practical approach:
  enable **Object Lock with a bucket-level default retention** (Compliance or Governance mode) on the
  bucket — every object OpenDAL writes then inherits immutability automatically, no per-request
  header needed. **Gotcha:** when Object Lock is on, S3 requires a checksum; OpenDAL needs the
  correct `checksum_algorithm` configured on the S3 service or writes will fail.
- **SFTP backend — big gotcha for this project:** OpenDAL's `sftp` service **only works on Unix.**
  It depends on `openssh` → `tokio-pipe`, which does not build on Windows. So on a Windows host you
  **cannot** use OpenDAL for SFTP. If you need SFTP as a destination from Windows, use `russh`
  (SFTP subsystem) or `ssh2`'s sftp directly instead, and reserve OpenDAL for S3 + local.

**Recommendation:** Use OpenDAL for **S3 (with bucket-level Object Lock default retention for
immutability) + local filesystem**. Do **not** rely on OpenDAL SFTP on Windows — route SFTP through
russh if required.

---

## 6. Compression — `zstd`

- Version: **0.13.3** (2025-02-20). Mature, widely used binding to Facebook's libzstd; stable, low
  churn. (Pure-Rust alternatives: `ruzstd` for decode + newer encode; not needed here.)
- Streaming: `zstd::stream::write::Encoder<W>` (wrap the output/upload writer, write plaintext in) and
  `zstd::stream::read::Decoder<R>`. Constant-memory, ideal for piping a dump: `pg_dump stdout →
  zstd Encoder → age StreamWriter → opendal writer`.
- **Gotcha:** call `Encoder::finish()` (or use `auto_finish()`) to flush the final frame — same
  truncation risk as age. Level 3 is the balanced default; 19+ is slow.

---

## 7. CLI + config — `clap`, `serde`, `toml`

- `clap` **4.6.2** (2026-07-15) — current, actively maintained. Use the derive API. Nothing exotic.
- `serde` 1.x — the standard; `toml` crate ~0.9.x for config parsing (serde-integrated).
- No concerns. Recommendation: derive `clap` for CLI, `serde`+`toml` for a config file describing
  jobs/targets/destinations.

---

## 8. Native dump tools

**PostgreSQL — `pg_dump` / `pg_dumpall`:**
- **Version rule: the client must be >= the server.** `pg_dump` is backward-compatible (a v17
  client dumps v13/14/15/16 servers) but **not forward-compatible** — an older `pg_dump` against a
  newer server aborts with *"aborting because of server version mismatch."* Ship/point at a
  `pg_dump` at least as new as the newest server you back up.
- Consistent dump flags: `pg_dump` is already MVCC-consistent (single snapshot) by default. Key
  flags: `--format=custom` (or `directory` for parallel `-j`), `--no-owner`/`--no-acl` as needed.
  `pg_dumpall` for globals (roles, tablespaces) — run `pg_dumpall --globals-only` plus per-DB
  `pg_dump`.
- Streams to stdout: yes (plain or custom format to stdout with `-f -`). Pipe directly into
  zstd/age.
- Windows: official EDB PostgreSQL installers ship `pg_dump.exe`; bundle the matching version.

**MySQL/MariaDB — `mysqldump` vs `mydumper`:**
- `mysqldump`: single-threaded, ships with the client. For a **consistent InnoDB snapshot** use
  `--single-transaction` (opens one REPEATABLE-READ transaction, no global lock, writers keep going)
  — combine with `--routines --triggers --events`, and `--source-data`/`--master-data` if you need
  binlog coords. **Gotcha:** `--single-transaction` gives consistency for InnoDB only; MyISAM tables
  are not transactional and can be inconsistent. Also `--single-transaction` + DDL during dump can
  break consistency (DDL isn't transactional in MySQL).
- `mydumper`: multi-threaded, **4–8x faster**, genuine cross-table consistency (brief global lock to
  record binlog position, then all workers share one snapshot), parallel restore via `myloader`.
  Best for large (>100 GB) DBs. Cost: separate binary to install/bundle, outputs a directory of
  files (not a single stdout stream).
- Streaming to stdout: `mysqldump` streams SQL to stdout naturally (pipe into zstd/age). `mydumper`
  writes to a directory, so it does **not** fit the single-pipe model — you'd compress/encrypt the
  output dir afterward.
- Windows: `mysqldump.exe` ships with MySQL/MariaDB client packages. `mydumper` has Windows builds
  but they are less turnkey.

**Recommendation:** Default to `mysqldump --single-transaction` (streams cleanly into the
zstd→age→opendal pipe). Offer `mydumper` as an opt-in for large InnoDB databases where speed matters,
accepting the directory-output handling. For Postgres, use `pg_dump` (custom/directory format) +
`pg_dumpall --globals-only`, with a client >= every server version.

---

## 9. Scheduling on Windows — `schtasks` vs Windows service

**Task Scheduler (`schtasks` / Task Scheduler XML):**
- Purpose-built for "run at a specific time." Robust, native, survives reboot.
- For "must still run if the machine was off at the scheduled time," set **"Run task as soon as
  possible after a scheduled start is missed"** (the `StartWhenAvailable` XML flag). Without it, a
  task whose time passed while the box was off is skipped until the next occurrence.
- Register from your installer with `schtasks /Create /XML ...` (XML gives full control:
  missed-run catch-up, run-whether-logged-on-or-not, highest privileges, retry-on-failure).

**Windows service (`windows-service` crate, v0.8.1, 2026-05-08, maintained):**
- A service is best for **always-on** background work (a resident daemon). For periodic backups
  you'd then have to build your own in-process scheduler/timer inside the service and handle
  catch-up logic yourself — reinventing what Task Scheduler already does. Services auto-start on
  boot (good) but "fire at 02:00 daily and catch up missed runs" is not what a service model gives
  you for free.

**Recommendation:** Use **Windows Task Scheduler** (register via `schtasks`/XML) with
`StartWhenAvailable=true` and "run whether user logged on or not." It is more robust and less code
for the "run at scheduled time, catch up after reboot" requirement than a custom service loop.
Consider the `windows-service` crate only if you later need a resident agent (e.g. continuous WAL
archiving or an on-demand API). **Gotcha:** a task set to run as a specific user "whether logged on
or not" needs the saved password and interacts with the DPAPI-user-scope concern in §3 — another
reason to prefer **DPAPI Machine scope** for the secret if the task may run under SYSTEM or without a
loaded user profile.

---

## Cross-cutting pipeline sketch

```
pg_dump/mysqldump (stdout)
   → zstd::stream::Encoder            (compress, .finish())
   → age StreamWriter                 (encrypt DEK-wrapped, .finish())
   → opendal Operator::writer()       (streamed multipart to S3 w/ Object Lock default retention)
```
Secret (master passphrase / DEK) protected at rest via DPAPI Machine scope (`windows-dpapi`).
SSH tunnel (if DB behind bastion) via `russh` direct-tcpip. Scheduled by Windows Task Scheduler.

## Open items to confirm at build time (`[확인 필요]`)
- Exact OpenDAL 0.58 S3 write-option surface for object-lock retention headers (vs relying on
  bucket-level default retention).
- keyring 4.x `windows-native-keyring-store` exact crate name/version pin.
- argon2 0.6 RC vs 0.5.3 stable decision.
- russh 0.62 client tunnel example against your bastion (host-key policy).
