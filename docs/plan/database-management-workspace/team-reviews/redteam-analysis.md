# Red Team Expert — Phase A

Date: 2026-11-17. Scope: spec/interview, research synthesis and source reports, parent dictionary v4, extension v1.
Selected architecture: current-user Workbench engine/credentials; backup_service validates existing ownership/protection actions and never executes arbitrary SQL.
Review mode: design stress test, not implementation vulnerability report. No new source execution or broad web search.
Findings: Critical 0 / High 6 / Medium 3. Open design contracts below must be resolved in sections/API/QA.

## Findings

### RT-01 — High: late cancellation can hit the next query on the same connection

- Evidence: `spec.md:60` serializes one session and adds generation-bound query IDs plus an independent cancel lane; `:108` requests a race test but not the dispatch barrier.
- Trigger: query A completes; B starts on the same physical connection/generation; A's delayed PostgreSQL backend cancel or MySQL KILL QUERY reaches B. An application query ID cannot be passed to these server cancellation primitives.
- Fix: serialize cancel targeting with run completion/next-run admission, retain a per-physical-connection execution epoch, and prevent B dispatch until A's cancel lane settles. If settlement is uncertain, discard the connection instead of reusing it.
- Acceptance: force A completion just before cancel socket dispatch and queue B; B must never execute on a cancellable old session or receive A's cancellation.

### RT-02 — High: one terminal outcome cannot explain partial explicit SQL scripts

- Evidence: `spec.md:60` admits explicit scripts/implicit commits; `:82` lists only committed/rolled_back/failed/unknown; `research/web.md:40` already warns earlier autocommits can survive rollback.
- Trigger: `UPDATE a ...; ALTER TABLE b ...; UPDATE c ...` commits early work, then fails/cancels. Calling the submission rolled_back or failed without an effect vector implies recoverability it lacks.
- Fix: record ordered per-statement dispatch/completion/outcome and an aggregate partial/unknown summary; an unparseable script uses conservative unknown effect scope. Never auto-wrap or replay the user's script.
- Acceptance: autocommit DML then syntax error, DDL implicit commit, and procedure with external effect all preserve earlier known effects and later uncertainty separately.

### RT-03 — High: immediate handle invalidation conflicts with a 30-second authorization lease

- Evidence: `spec.md:54` promises archive/revision invalidation while allowing fresh service validation OR a proposed 30s lease for new dispatch.
- Trigger: service archives/moves a Workspace or revises a profile after a user engine receives its lease; the old handle dispatches a write before expiry.
- Fix: choose explicit semantics. Writes/protection links require current authoritative revision/owner validation immediately before dispatch; cached read leases have a documented revocation window. Already-sent target SQL cannot be revoked by local invalidation.
- Acceptance: archive/profile revision races reject pending new writes; in-flight writes retain honest outcome and are not described as rolled back by authorization revocation.

### RT-04 — High: trigger effects exceed the claimed whole-batch rollback boundary

- Evidence: `spec.md:68-70` checks the edited table's transactional storage then promises rollback of an entire supported batch; `:83` acknowledges triggers/external effects only for replay uncertainty.
- Trigger: an InnoDB edited table has a trigger that writes a nontransactional table or invokes an external effect; the next draft conflicts and the batch rolls back only transactional mutations.
- Fix: narrow the guarantee to transactional effects of the certified target transaction. Record trigger/routine/nontransactional/external-effect limitations in review and capability evidence; do not certify global effect rollback from the base table's engine alone.
- Acceptance: a trigger counter/external test hook surviving rollback must not produce an all-effects-reverted message. Safe generated editing never implies reversible arbitrary server code.

### RT-05 — High: ACK/credit resynchronization lacks accounting rules

- Evidence: `spec.md:65` promises duplicate apply-once, gap recovery and byte credit but leaves ACK validation unspecified; dictionary defines Credit Window only in prose.
- Trigger: duplicate ACK doubles credit; a future sequence acknowledges unsent rows; a stale window acknowledges another generation; a gap recovery retransmits batches and charges/releases credit twice.
- Fix: engine owns encoded-byte accounting per full stream identity, a sent-sequence ledger and highest contiguous ACK. ACK may only advance within sent range; duplicate/stale ACK gives zero credit; gaps never trigger SQL re-execution. Reject negative/overflow credit and bound unacknowledged metadata too.
- Acceptance: duplicate/future/wrong-owner ACKs cannot increase available credit; no-ACK and dropped-terminal paths reach bounded expiry without generating a new query.

### RT-06 — High: encrypted user journal still needs a pre-persistence secret boundary

- Evidence: `spec.md:34,54` selects user DPAPI local state; `:80` persists digests/state/outbox; no canonical persisted field allowlist or DPAPI/WAL ordering is specified. `research/web.md:46,118,127` still uses earlier service/store wording.
- Trigger: serialize SQL/parameters/server errors to SQLite/WAL/outbox/log first and encrypt only an exported copy; a sensitive key, value or driver exception survives on disk or in crash/support diagnostics.
- Fix: one user-owned journal authority, metadata-only allowlist, sensitive payload encryption before any SQLite bind, no default raw SQL/result history. Define DPAPI failure/write-disable behavior and keep service secret IDs/interactive secret refs purpose-separated. Correct stale research proposals through an explicit disposition.
- Acceptance: secret canaries remain absent from journal/WAL/temp/outbox/log/support artifacts before and after crash; user DPAPI failure never falls back to plaintext or machine protection.

### RT-07 — Medium: reconciliation can silently promote user belief to commit evidence

- Evidence: `spec.md:70` blocks apply until explicit reconciliation, but does not define who may clear it or what evidence transitions unknown to committed; `:83` correctly rejects final-value equality as execution proof.
- Trigger: user sees expected values and clicks resolved; UI marks committed although triggers/sequence advances or an earlier actor could have produced them.
- Fix: distinguish server-proven resolution from operator-acknowledged uncertainty. An acknowledgment may unlock a new independent session/intent, but original outcome remains unknown absent conclusive evidence; never reuse the old idempotency key as a fresh attempt.
- Acceptance: acknowledge-and-continue cannot rewrite the original event history, claim rollback or automatically resend the unknown mutation.

### RT-08 — Medium: unique identity and no-op proof need a concrete adapter matrix

- Evidence: `spec.md:68-70` has the right restrictions, but defines neither eligible unique-index forms nor exact typed-comparison/matched-row proof.
- Trigger: partial/expression/nullable key mistaken for unconditional identity; collation-equal text masks changed bytes; MySQL zero changed rows is called conflict or success without certified CLIENT_FOUND_ROWS behavior.
- Fix: certify complete ordinary nonnullable unique keys with provenance, actual object identity and equality semantics; reject unresolved index forms. Specify version-token versus original-value conflict policy, no-op normalization, null/binary/decimal/time comparisons and locked read protocol for each engine.
- Acceptance: cover ABA explicitly as not detected without a true version token, collation-equivalent mutations, unsigned max/decimal scale, SQLite BUSY_SNAPSHOT and both changed/matched-row driver settings.

### RT-09 — Medium: process restart/idempotency retention can reopen dispatch uncertainty

- Evidence: `spec.md:81-85` scopes keys and recovers dispatched writes as unknown, but omits multi-instance executor ownership, dedup retention/tombstones and user-journal replacement epochs.
- Trigger: two windows/processes share a journal; one is paused after intent, another takes over after lease expiry and dispatches again; pruning an old key or restoring an old local DB admits it as new.
- Fix: atomic claim plus owner boot/journal epoch; dispatched writes never receive takeover execution on lease expiry. Retain bounded metadata tombstones for published retry horizons, reject expired-key reuse, and start write-disabled after journal restore/corruption until reconciliation establishes an epoch.
- Acceptance: pause/crash at each dispatch boundary, open a second instance, expire claims and replay old keys; no target write is automatically resent.

## Dictionary Updates

| Term | Proposed definition/change | Reason |
|---|---|---|
| Execution Epoch | Per-physical-session running-statement generation plus cancellation settlement boundary | Connection Generation alone does not stop late cancel hitting the next run. |
| Statement Outcome | One script statement's known dispatch/completion/effect status | Submission failure cannot erase earlier autocommits. |
| Authorization Lease | Scope/revision/expiry and explicitly bounded revocation semantics; no remote rollback power | Resolves immediate invalidation versus cached authorization. |
| Operator Acknowledgment | Human acceptance of unresolved uncertainty, not target commit proof | Prevents false promotion of Outcome Unknown. |
| Journal Epoch | User journal identity/boot ownership separating restore/recreation from prior admission history | Prevents stale local records from becoming fresh dispatch authority. |
| Verified Apply | Transactional certified effects only; trigger/external effects explicitly excluded unless certified | Avoids overclaiming whole-world rollback. |

## Preserved controls and boundary

Current-user credentials, TLS verify-full default, raw persistence opt-in, independent generated SQL checks, no arbitrary SQL exactly-once, keyset-not-snapshot and no automatic unknown-write replay are sound selected constraints; no finding proposes relaxing them.
All findings are design omissions/ambiguities, not observed production exploits. Live fault injection and driver certification remain NOT RUN.
