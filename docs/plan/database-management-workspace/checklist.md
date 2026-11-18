# Acceptance Checklist

Canonical union of the eight implementation sections. Criteria are unchecked until implementation evidence satisfies them. This file is not a claim that any application feature is implemented. Preserve the original wording/owner/basis; revisions require updating the owning section and all mappings together. Q1 scope remains inferred as recorded in [interview](interview.md).

| ID | 섹션 | 완료 기준 | 근거 | 검증 |
|---|---|---|---|---|
| AC-01-1 | section-01-bounded-performance | 고정된 Windows/WebView2/hardware/build/dataset/sample manifest를 사용해 200개 입력과 frame·idle/peak process-tree memory baseline의 원시 샘플·버전·해시를 재현 가능한 실행 artifact로 남긴다 | P2 | bench workbench.baseline, T2; QA-001 |
| AC-01-2 | section-01-bounded-performance | 실제 native producer의 100k×100 workload를 탐색하면 outstanding 8MiB·frontend retained 32MiB·native result buffers 64MiB 및 viewport+overscan DOM 상한을 넘지 않고 전체 결과를 적재하지 않는다 | P2 | bench workbench.bounds, T2; QA-002 |
| AC-01-3 | section-01-bounded-performance | slow/noACK·잘못된 ACK·oversize cell/frame을 주입하면 producer가 중지/대기/거부하고 byte 예약이 중복 해제되지 않으며 독립 Stop과 정직한 terminal이 응답하고 그 지연 원시값이 기록된다 | P2 | bench workbench.slow_consumer, T2; QA-003 |
| AC-01-4 | section-01-bounded-performance | W1/W2가 원시 artifact에서 M1 input p95·M2 incremental process-tree working set·M3 boundary violations와 tripwire 판정을 산출해 implementation ledger에 연결하며 미측정·누락·실패를 PASS로 표시하지 않는다 | P2 | bench workbench.metrics, T2, W1/W2; QA-004 |
| AC-02-1 | section-02-sessions-and-credentials | 허용된 Target의 redacted metadata만 바인딩하고 service secret을 사용자 연결에 자동 제공하지 않는다 | P1,P3,P6 | session.separate_credentials, T1; QA-005 |
| AC-02-2 | section-02-sessions-and-credentials | 다른 owner/window/workspace 또는 오래된 generation/revision handle을 데이터 전송 전에 거부한다 | P1,P3,P6 | session.handle_scope, T1; QA-006 |
| AC-02-3 | section-02-sessions-and-credentials | 인증서 검증을 충족 못하면 거부하고 DB별 미지원 capability를 활성화하지 않는다 | P1,P3,P6 | driver.tls_capability, T3; QA-007 |
| AC-02-4 | section-02-sessions-and-credentials | 재접속/창닫기/권한만료 때 세션을 무효화하며 진행 transaction을 새 connection으로 복원하지 않는다 | P1,P3,P6 | session.lifecycle, T8; QA-008 |
| AC-02-5 | section-02-sessions-and-credentials | DB·service 접속을 기다리는 동안 UI thread와 별도 backup worker permit을 점유하지 않는다 | P1,P3,P6 | session.worker_isolation, T10; QA-009 |
| AC-03-1 | section-03-operation-journal | 같은 scoped key+payload는 같은 operation을 반환하고 다른 payload는 IDEMPOTENCY_CONFLICT로 거부한다 | P5,P6 | journal.idempotency, T5; QA-010 |
| AC-03-2 | section-03-operation-journal | operation state와 version을 함께 commit하고 duplicate/old hint를 무시하며 gap/reconnect는 journal 상태로 복구한다 | P5,P6 | journal.version_resync, T7; QA-011 |
| AC-03-3 | section-03-operation-journal | dispatch/COMMIT 후 crash·응답유실을 unknown으로 복구하고 target SQL을 자동 재실행하지 않는다 | P5,P6 | journal.commit_loss, T6; QA-012 |
| AC-03-4 | section-03-operation-journal | journal 손상·disk-full이면 새 쓰기를 거부하고 기존 operation의 불확실성을 숨기지 않는다 | P5,P6 | journal.disk_failure, T6; QA-013 |
| AC-03-5 | section-03-operation-journal | raw SQL·credentials·row values를 default journal/diagnostics에 영속 저장하지 않는다 | P5,P6 | journal.secret_canary, T3; QA-014 |
| AC-04-1 | section-04-schema-explorer | 10k metadata 목록을 lazy pages로 탐색하며 화면 DOM과 retained pages를 상한 안에 유지한다 | P2,P4,P7 | browse.metadata_bounds, T2; QA-015 |
| AC-04-2 | section-04-schema-explorer | cursor의 target/filter/order/schema mismatch를 거부하고 keyless preview의 한계를 표시한다 | P2,P4,P7 | browse.cursor_identity, T1; QA-016 |
| AC-04-3 | section-04-schema-explorer | NULL/empty/u64/decimal/binary/temporal 값을 lossless로 구분하고 truncated/unsupported 값은 편집하지 않는다 | P2,P4,P7 | types.roundtrip, T3; QA-017 |
| AC-04-4 | section-04-schema-explorer | 페이지 일관성·count 정확도·cap 상태를 표시하며 live pages를 snapshot으로 주장하지 않는다 | P2,P4,P7 | browse.consistency, T2; QA-018 |
| AC-04-5 | section-04-schema-explorer | 가상화·page eviction 후 키보드 focus/selection/ARIA 좌표를 보존하고 FK/diagram을 같은 metadata version으로 표시한다 | P2,P4,P7 | grid.keyboard_diagram, T2; QA-019 |
| AC-05-1 | section-05-sql-workspace | 사용자 SQL을 승인한 원문대로 실행하고 generated preview와 섞거나 LIMIT을 주입하지 않는다 | P2,P3,P5 | query.exact_sql, T3; QA-020 |
| AC-05-2 | section-05-sql-workspace | 모드·unknown/DDL/ANALYZE 정책을 Rust에서 검사하고 privileged backup service로 SQL을 넘기지 않는다 | P2,P3,P5 | query.execution_policy, T3; QA-021 |
| AC-05-3 | section-05-sql-workspace | 포화된 작업 중에도 정확한 query generation만 취소하고 취소 요청을 rollback 확인으로 표시하지 않는다 | P2,P3,P5 | query.cancel_race, T8; QA-022 |
| AC-05-4 | section-05-sql-workspace | manual transaction을 같은 connection에 고정하며 reconnect/unknown commit 때 격리하고 자동 replay하지 않는다 | P2,P3,P5 | query.transaction_loss, T6; QA-023 |
| AC-05-5 | section-05-sql-workspace | duplicate/gap/stale batches를 정확히 처리하고 credit/cache 상한 및 terminal status를 유지한다 | P2,P3,P5 | query.stream_protocol, T7; QA-024 |
| AC-05-6 | section-05-sql-workspace | 자동 raw history/result 저장 없이 명시적 named query만 사용자 암호화 저장하고 secret canary를 로그에 남기지 않는다 | P2,P3,P5 | query.saved_privacy, T3; QA-025 |
| AC-06-1 | section-06-verified-editing | join/expression/keyless/unsupported type 또는 nontransactional 대상의 편집을 구체적 read-only 이유와 함께 Rust·UI 양쪽에서 거부한다 | P3,P4,P5 | edit.capability_gate, T4; QA-026 |
| AC-06-2 | section-06-verified-editing | 값은 bind하고 식별자는 scoped catalog와 dialect quoting으로 생성하며 독립 구조 gate 및 실제 대상·SQL·매개변수 review를 통과한 변경만 실행한다 | P3,P4,P5 | edit.injection_gate, T3; QA-027 |
| AC-06-3 | section-06-verified-editing | 원본 행·schema 충돌이나 multi-match 시 지원 transactional effects의 전체 batch를 rollback하고 draft를 보존하며 불명 rollback을 성공으로 표시하지 않는다 | P3,P4,P5 | edit.optimistic_atomic, T4; QA-028 |
| AC-06-4 | section-06-verified-editing | MySQL/MariaDB no-op의 matched·changed 차이와 NULL·정밀 decimal 등 typed key/value를 검증하며 미지원 codec은 read-only로 유지한다 | P3,P4,P5 | edit.noop_types, T4; QA-029 |
| AC-06-5 | section-06-verified-editing | COMMIT 결과 불명은 새 apply·resend를 차단하고 자동 재실행하지 않으며 사용자 acknowledgment를 commit·rollback 증거로 바꾸지 않는다 | P3,P4,P5 | edit.unknown_quarantine, T6; QA-030 |
| AC-07-1 | section-07-data-movement | 대량 파일·결과를 이동할 때 JS/native에 전체 파일을 적재하지 않고 parser·batch·credit·retained memory 상한을 지킨다 | P2,P5,P7 | movement.memory_bounds, T2; QA-031 |
| AC-07-2 | section-07-data-movement | 검토한 import만 bound INSERT의 단일 child batch transaction으로 실행하며 confirmed commit/rollback·partial·unknown을 구분하고 unknown 뒤 후속 batch를 중단한다 | P2,P5,P7 | movement.import_consistency, T9; QA-032 |
| AC-07-3 | section-07-data-movement | shown export는 기존 결과를 재실행하지 않고 export-all은 명시적 새 읽기의 실제 target identity·time·consistency를 공개하며 volatile/unproven SQL을 자동 재조회하지 않는다 | P2,P5,P7 | movement.export_identity, T9; QA-033 |
| AC-07-4 | section-07-data-movement | 파일은 complete terminal·flush/sync·atomic finalize 성공 뒤에만 complete로 표시하며 취소·disk-full·finalize 실패는 partial로 남는다 | P2,P5,P7 | movement.partial_file, T9; QA-034 |
| AC-07-5 | section-07-data-movement | 중복 요청·child·재시작에도 전송된 batch를 자동 재실행하지 않고 formula·NULL/empty·정밀숫자·binary 변환은 검토된 정책과 정확한 타입 계약을 따른다 | P2,P5,P7 | movement.duplicate_types, T5; QA-035 |
| AC-08-1 | section-08-protection-links | 선택 table IDs를 기존 Scope API에 전달하고 service가 Workspace/target/schema identity를 다시 검사한다 | P1,P6,P8 | protection.scope_handoff, T1; QA-036 |
| AC-08-2 | section-08-protection-links | backup-before-change를 별도 dependency로 표시하고 backup 완료 이벤트만으로 SQL을 실행하지 않는다 | P1,P6,P8 | protection.no_global_tx, T10; QA-037 |
| AC-08-3 | section-08-protection-links | 기존 restore preflight/confirmation/evidence를 유지하며 일치하는 restored target만 새 readonly session으로 연다 | P1,P6,P8 | protection.restore_identity, T10; QA-038 |
| AC-08-4 | section-08-protection-links | 복원 대상 불일치·권한거부 때 원본 fallback이나 더 높은 service credentials 사용을 하지 않는다 | P1,P6,P8 | protection.no_fallback, T10; QA-039 |
| AC-08-5 | section-08-protection-links | 긴 query/import/느린 consumer와 백업 동시 실행에서 기존 service lifecycle·scheduling·cancel 회귀가 없다 | P1,P6,P8 | protection.backup_regression, T10; QA-040 |

## Release disposition

Design artifact checks are recorded in [verification](verification.md). That document preserves the original planning checkpoint separately from current implementation evidence in [checklist-status](checklist-status.md). Actual scoped native/engine, Windows performance and browser checks have run; remaining integration and review gates stay open until their recorded execution. Original numerical budgets remain unchanged, including the256MiB process-tree target and its measured early signal. Production signed deployment retains its existing parked gate.
