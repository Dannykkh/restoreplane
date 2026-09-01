# Flowchart Index — Ransomware-Resilient RestorePlane

## Diagrams

| Diagram | File | Description |
|---------|------|-------------|
| **Initialization & Key Management** | `init-keymgmt.mmd` | Master password → Argon2id KEK → random DEK → 3-path wrapping (password/recovery-code/DPAPI) → ed25519 signing key generation → recovery code display → config roundtrip test. |
| **Backup Run Pipeline** | `backup-run.mmd` | Option-C spool-then-fanout: plan (globals+dbs/engine scan) → version recheck (hard fail) → dump (Stream/Directory→tar) → blake3 tee → zstd → age(per-backup DEK) → local spool → commit protocol → sign manifest → parallel upload (S3/SSH/Local) → auto verify L1/L2 with partial-success handling. |
| **Immutability Self-Test** | `immutability-selftest.mmd` | Every run: attempt DeleteObject/PutLifecycle on probe key with writer creds → assert DENIED → assert versioning ON + default retention → refuse/warn if over-privileged or bucket not locked. |
| **Restore & Verification** | `restore-verify.mmd` | Fetch object+manifest → verify ed25519 signature → unwrap DEK (password/recovery/dpapi path) → age decrypt → zstd decode → untar → restore in SANDBOX (ephemeral, no outbound, non-privileged, local_infile=0, COPY PROGRAM blocked) → verify L1(plaintext blake3) / L2(pg_restore --list) / L3(full restore + schema hash + row counts). |
| **Scheduling & Automation** | `scheduling.mmd` | Schtasks XML registration with StartWhenAvailable → scheduled trigger → overlap guard (named mutex/runs unique index) → catch-up after reboot → failure notification via webhook/email. |

## Notes

- All diagrams use Mermaid `flowchart` syntax and are compatible with GitHub Markdown rendering.
- Node IDs follow Mermaid reserved-word safety rules (no bare words like `if`, `end`, `return`).
- Labels with special characters are quoted for valid parsing.
- No emojis used; status indicated by color coding: blue (start), green (success), red (failure), yellow (warning), orange (partial).
