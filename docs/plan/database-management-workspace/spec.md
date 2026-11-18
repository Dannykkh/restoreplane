# RestorePlane Database Management Workspace

Date: 2026-11-17. Status: design baseline; Q1 scope inferred/pending. Application implementation NOT RUN.
Existing: Windows service / Operational Store schema v4 / Tauri + React. Additive upgrade.

## Context Map

### 공유 전제
- 목표: 같은 RestorePlane에서 DB를 탐색·조회·편집하고 기존 백업·검증·복원으로 연결한다.
- 산업: database developer/operations tooling. Windows first; Tauri/React + Rust 유지.
- 성공 기준: 기존 백업 기능을 보존하며 PostgreSQL/MySQL/MariaDB/SQLite의 조회·SQL·지원 타입 편집을 수행하고 대량 결과·충돌·중복·재시작·commit 불명 상태를 정확히 처리한다.
- 기한·인력·실제 기기/데이터 규모: 미확인. 수치는 실측이 아닌 가정으로 표시한다.
- [explicit] paging, injection, idempotency, distributed workflows, duplicate messages, data consistency는 기본 계약이다.
- [inferred] Q1 응답 전 기본은 조회·SQL·verified editing·bounded import/export·보호 연계다.
- 만들지 않을 것: GPUI 전환(Tauri 유지); 신규 모든 DB/ODBC(네 엔진 인증 뒤 재검토); MCP/AI SQL(명시 사례·권한계약 뒤); 사용자/역할관리·시각적 DDL designer(별도 요구 뒤); cross-DB XA/2PC와 임의 SQL exactly-once(서버 참여 없이는 보장 불가); default raw SQL/result disk cache(민감값 잔류); DBDelve GPL 소스 복사(기능 참고); production 서명 배포(기존 parked gate).

### 이해관계자
| 역할 | 과업 | 관련 섹션 |
|---|---|---|
| 요청자/제품 소유자 | 통합 제품 범위·채택 | section-01, section-08 |
| DB 개발자 | 탐색·SQL·데이터 편집 | section-02, section-04, section-05, section-06, section-07 |
| 운영자 | 보호·복구 증거와 실제 대상 확인 | section-03, section-08 |

### 시스템 역할 (Role Inventory)
NOT APPLICABLE: single role — 새 human RBAC를 만들지 않는다. desktop_owner는 현재 Windows 사용자이며 기존 Workspace 권한을 따라야 한다. backup_service는 기존 OS 실행 주체다. Read-only/Read-write/Full은 연결별 실행 모드이며 DB 권한을 부여하지 않는다.

### 에코시스템 맵
| 시스템 | 연동 | 관련 섹션 |
|---|---|---|
| Tauri Console | 기존 App route + typed Workbench composition | section-01, section-02, section-04, section-05 |
| User-owned Rust Workbench | Tauri managed state와 bounded workers, 임의 SQL은 privileged service 밖에서 실행 | section-02, section-03, section-05, section-06, section-07 |
| 기존 backup_service | service_request로 Workspace/Target metadata·권한 검증과 보호 연계 | section-02, section-08 |
| PostgreSQL/MySQL/MariaDB/SQLite | 전용 interactive driver/session | section-02, section-04, section-05, section-06, section-07 |
| 사용자 로컬 상태 | SQLite journal + user ACL + 내용 user DPAPI | section-02, section-03, section-05 |
| 기존 S3/SSH/Local 목적지 | 기존 backup_service 소유 | section-08 |

### 기존 환경
research/codebase.md 정본. console_query는 운영 카탈로그 조회이며 일반 SQL 실행기가 아니다. native dump를 interactive driver로 재사용하지 않는다. 현재 service secret은 machine DPAPI이므로 사용자 SQL credentials와 자동 혼용하지 않는다.

## Problem Statement
| ID | 문제 | 영향 | 우선순위 | 해결 섹션 |
|---|---|---|---|---|
| P1 | DB 관리와 보호 흐름 분리 | 대상 혼동·맥락 손실 | 필수 | section-02, section-08 |
| P2 | 전체 로드·DOM 렌더 | 결과·메모리 누적 | 필수 | section-01, section-04, section-05, section-07 |
| P3 | 일반 SQL session 계약 없음 | 과도권한·취소/transaction 혼선 | 필수 | section-02, section-05 |
| P4 | 편집 identity·타입·동시성 미정 | 잘못된 행/덮어쓰기/정밀도 손실 | 필수 | section-04, section-06 |
| P5 | 원격 commit과 local journal 이중 쓰기 | 중복·거짓 실패·불명 결과 | 필수 | section-03, section-05, section-06 |
| P6 | backup service와 사용자 secret 혼용 | 권한 확대·비밀 노출 | 필수 | section-02, section-03 |
| P7 | 대량 이동·조회 일관성 미정 | OOM·부분 파일·시점 혼합 | 필수 | section-04, section-07 |
| P8 | 복원 후 잘못된 대상 접속 | 원본 쓰기·잘못된 검증 주장 | 필수 | section-08 |

## Functional requirements
### F1. Sessions and credentials
Reuse redacted Workspace/Connection Profile/Database Target identity and existing service authorization; no generic SQL endpoint in backup_service. Tauri managed Workbench owns separate user DB credentials (user DPAPI), sessions, runtime and permits. Existing service secrets are not exported, automatically reused or silently downgraded. Single target handle binds owner SID/window/workspace/target/profile revision/source identity/generation. Reconnect/window close/archive/revision change invalidates prior handles. Every mutation/COMMIT/protection dispatch requires fresh authoritative revision/owner validation immediately before wire; only reads may use a short in-memory lease (proposed 30s revocation window). Already-sent SQL cannot be revoked by local state. Read-only default; DB role/file ACL is actual privilege boundary; TLS verify-full default and unsupported checks refuse.

### F2. Explorer, table preview, FK and ER diagram
Lazy schema/table/view/column/key/index metadata; server-side search and versioned metadata pages. Table browse uses stable unique tie-breaker and typed cursor bound to identity/filter/order/schema. No stable key => bounded preview with explicit unordered/limited state, not exhaustive paging. Count is unknown/estimate/exact, not compulsory COUNT per page. Basic consistency is live-per-page; an explicit bounded transaction snapshot is offered only after per-engine certification. Keyset alone is not a snapshot. ER diagram uses the same versioned metadata and bounded node/edge work; SVG/Mermaid export.

### F3. SQL workbench
SQL tabs, schema completion, explicit saved queries, plan-only Explain, explicit execution/selection, results/messages/errors. Execute approved user SQL unchanged: no injected LIMIT, ORDER BY or transaction wrapper. AST classification/confirmation is accident prevention, not authorization. Unknown/DDL/procedure/multi-statement requires restrictive Full policy. ANALYZE is an executing operation and inspects underlying statement. Per-session serialization, generation-bound query IDs and independent cancel lane. Manual transaction stays on one live connection, never resumes on reconnect; implicit commits and supported isolation are visible. Generated edits cannot join an arbitrary open manual transaction.

### F4. Typed results and resource bounds
Canonical Typed Cell separates NULL/empty, exact signed/unsigned ints, decimals, floats/specials, bytes and temporal values from display text. Unsupported/truncated types are not editable.
Proposed unmeasured budgets: browse 100/default, 200/max rows and 512KiB/page; 512 columns; 64KiB inline cell; 256KiB stream batch; 8MiB outstanding byte credit; 32MiB frontend page LRU; 64MiB native result buffers; 8 active sessions/global, 2/target; 4 running queries/global, 1/session. Validate in section-01/04/05, never call these measured throughput.
Tauri Channel sends only a bounded frames-available notice. Bounded invoke pulls carry workspace/target/session/connectionGeneration/query/resultSet/streamGeneration/sequence/encodedBytes/terminal; the full reply is at most256KiB and one pull per stream is active. Explicit credit+ACK bounds queues; actual pull advances the sent ledger, duplicate sequence applies once, gap resyncs, stale generation is rejected. Channel ordering is not durable delivery or bounded memory. Row+column virtualization bounds DOM; LRU/byte budgets bound memory. Large values use explicit bounded fetch/export. UI close stops reads; never replays writes. Separate interactive and backup permits do not guarantee remote server QoS.

### F5. Verified data editing
Only proven base table + complete nonnullable PK/unique identity + lossless editable type + transactional storage is editable. Keyless/join/expression/unsupported types show read-only reason.
Review draft shows target/generated SQL/parameters/original basis. Values bind; identifiers come from scoped catalog and dialect-component quoting; structural clauses are closed enums. Independently validate generated statement structure.
In one standalone target transaction: identity/schema recheck -> read+lock original -> typed compare -> conditional write -> certify matched-row semantics -> COMMIT. Conflict rolls back supported transactional effects; unproven trigger/nontransactional/external effects are outside the guarantee and may require refusal. MySQL/MariaDB changed/matched/no-op distinction is adapter-certified. Original-value compare cannot claim ABA/history detection. Unknown COMMIT quarantines session/operation and blocks further apply until explicit reconciliation; no automatic resend.

### F6. Data movement
Streaming CSV/TSV/JSON import with typed mapping/preview and generated bound INSERT only. Chunked atomic-per-batch import states partial completion; whole-job atomicity only where explicitly supported and bounded.
Export shown data consumes that result; export-all is an explicit safe requery with identity/isolation/time disclosure. Never auto-reexecute volatile/side-effect SQL. Stream to partial file and atomically finalize only after complete terminal+flush. Cancel/disk-full cannot leave a file labeled complete. Declare CSV spreadsheet formula policy.

### F7. Protection links
Pass exact selected table identity into existing Backup Scope workflow and revalidate in service. Backup-before-change is a visible optional dependency, not atomic rollback or distributed commit. Backup completion alone never automatically dispatches pending SQL. Restore uses existing preflight/confirmation/evidence path. Open restored DB only after actual target identity+completed restore proof; new Read-only session, no original-target fallback. Metadata diff is not row/semantic equality.

## Consistency and delivery invariants
- Local SQLite atomically admits request/digest + operation state/version. For current single-process UI, notifications are possibly duplicated hints; ignore duplicate/old versions and pull authoritative operation state on gap/reconnect. No new durable UI outbox/inbox tables. Existing service notification outbox stays intact; a future external consumer needs a separately reviewed durable delivery contract. Target SQL remains outside that local transaction.
- Idempotency key scope=(owner,workspace,target,kind,key); same payload returns existing operation/status, different payload rejects. Key does not make unrelated remote commit atomic.
- Operation: accepted -> dispatching -> running -> commit_requested -> committed|rolled_back|failed|outcome_unknown. Persist dispatch intent before wire. Lost worker/socket/COMMIT reply is unknown absent conclusive server evidence.
- A timeout/cancel request/session disconnect/local fence does not prove rollback. Unknown writes never automatically replay; same final values cannot prove no trigger/sequence/external effect happened.
- State versions never regress. Duplicate event is ignored; gaps refetch authoritative operation.get. Restart invalidates sessions/cursors and recovers dispatched/commit_requested writes as unknown.
- Journal corruption/disk failure/new incompatible local schema starts write-disabled diagnostics. Target and local durability are separate.

## Risk Assessment
| Risk | Impact | Probability (inferred) | Mitigation |
|---|---|---|---|
| commit reply lost | duplicate mutation | Medium | durable intent, unknown, no replay |
| backup credential reuse | privilege escalation | Medium | separate user engine/secret scope |
| wide results/LOB/slow UI | OOM/stall | High | credit/cache/cell/DOM budgets |
| stale schema/row/cursor | wrong write | High | bound identities, typed compare |
| persisted SQL/results | sensitive disk residue | Medium | opt-in encrypted saved query only |
| DDL/nontransactional storage | partial commit | High | capability refusal, explicit limits |
| new vulnerable drivers | untrusted input exposure | Medium | cargo audit/deny and engine matrix |

## Test Scenarios
| ID | Input | Expected | Problems |
|---|---|---|---|
| T1 | cross-workspace/session/cursor | reject without data | P1,P6 |
| T2 | 10k tables, 100k x 100 cells, slow/no ACK | bounded DOM/memory/queues and honest terminal | P2 |
| T3 | hostile value/quoted identifier/NULL/u64 max | structural safety and exact roundtrip | P3,P4 |
| T4 | concurrent edit/no-op/multirow conflict | conflict or exact no-op, batch rollback | P4 |
| T5 | same/different payload duplicate key | same operation/conflict | P5 |
| T6 | kill socket/process around COMMIT | unknown, zero automatic replays | P5 |
| T7 | duplicate/gap/stale generation events | one apply, resync, no regression | P2,P5 |
| T8 | saturated query queue+cancel race | correct query only; rollback not invented | P3 |
| T9 | import partial/export cancel/disk-full | per-batch evidence, partial file | P7 |
| T10 | restored identity mismatch+concurrent backup | reject target and preserve backup behavior | P1,P8 |

## Open Questions
Q1 scope preference pending and non-blocking. Hardware/data scale/deadline unconfirmed. Proposed performance budgets need implementation measurement. Drivers/types/isolation/cancel get certified before enabling capability; unsupported remains disabled.

## Reviewed execution contracts
- Workbench refuses elevated/high-integrity execution initially; existing backup admin UI remains usable. Native OS/window identity and one process/journal coordinator are authoritative.
- Expired authority blocks new dispatch but permits status/cancel/rollback/disconnect. COMMIT needs fresh authority; no automatic COMMIT on close.
- Physical Execution Epoch and cancel settlement barrier prevent the next run starting until old cancellation settles; uncertainty discards the connection.
- Preserve ordered Statement Outcomes and containing transaction outcome for partial autocommits/implicit commits/unknown effects.
- Reserve credit before enqueue. Highest valid contiguous sent-sequence ACK releases bytes once; duplicate/stale ACK grants zero; future/wrong-owner/overflow rejects. Gap recovery never reruns SQL.
- JournalEpoch/BootEpoch/native attempt expiry, single writer and retained uncertainty/tombstones prohibit redispatch across restarts. Metadata-only journal; encrypt explicitly saved content before SQLite/WAL bind.
- Operator Acknowledgment preserves unknown unless decisive target evidence exists. It never proves commit or enables automatic resend.
- Target Context Label; separate completeness/consistency/freshness; identity-based focus; explicit close disposition; IME Enter guards are required UX contracts.

## Scope simplifications after independent review
Reuse existing domain/application contracts and add a distinct CurrentUser DPAPI API in restoreplane-secrets without changing Machine protection defaults.
Operational Store remains v4: canonical descriptor revision is a hash derived from one existing configuration/ownership read transaction; service boot identity is authority epoch. Unsupported proof disables capability rather than inventing stored revision fields.
All SQL runs require fresh authorization unless the read session has a certified server readonly hold. Elevated mode rejects before any Workbench user files are created.
Use per-statement outcome and per-import-batch child operations; unknown halts later dispatch. Physical discard after uncertain cancel preserves unknown transaction effects.
Fresh authority observes target backup/restore conflicts and gates new mutating admission; no distributed target lock or retroactive revocation is claimed.
