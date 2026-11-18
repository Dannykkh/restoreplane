# QA Scenarios — Workbench

These are the52 implementation acceptance scenarios. Current scenario dispositions and their exact execution scope are recorded below and in [QA execution evidence](qa-execution.md); unresolved subcases are explicitly partial. The40 acceptance decisions remain in [checklist-status](checklist-status.md). Fixture definitions T1-T10 and metrics M1-M3/W1-W2 remain canonical in [spec](spec.md) and section01. Supported engine/version/settings combinations are scoped independently. Browser fixtures, actual native database tests and OS authorization E2E remain distinct.

## Acceptance mapping

| ID | Preconditions / environment | Action / injection | Expected observation | Coverage | Status |
|---|---|---|---|---|---|
| QA-001 | T2; Windows/WebView W1/W2 | 고정 W1/W2 환경에서 cold/warm 두 실행의 200개 입력·frame·100ms process-tree 샘플과 버전/hash manifest를 수집한다 | 고정된 Windows/WebView2/hardware/build/dataset/sample manifest를 사용해 200개 입력과 frame·idle/peak process-tree memory baseline의 원시 샘플·버전·해시를 재현 가능한 실행 artifact로 남긴다 — 근거 | AC-01-1 | PARTIAL |
| QA-002 | T2; Windows/WebView W1/W2 | 실제 Rust synthetic producer로 100k×100 결과를 스크롤하며 메모리·DOM·outstanding counters를 샘플링한다 | 실제 native producer의 100k×100 workload를 탐색하면 outstanding 8MiB·frontend retained 32MiB·native result buffers 64MiB 및 viewport+overscan DOM 상한을 넘지 않고 전체 결과를 적재하지 않는다 — 근거 | AC-01-2 | PARTIAL |
| QA-003 | T2; Windows/WebView W1/W2 | ACK 중단·중복·미래 ACK·다른 window ACK·oversize cell을 주입하고 Stop을 누른다 | slow/noACK·잘못된 ACK·oversize cell/frame을 주입하면 producer가 중지/대기/거부하고 byte 예약이 중복 해제되지 않으며 독립 Stop과 정직한 terminal이 응답하고 그 지연 원시값이 기록된다 — 근거 | AC-01-3 | PARTIAL |
| QA-004 | T2; Windows/WebView W1/W2 | 원시 artifact 하나를 누락시키고 정상/경계초과 실행 각각의 M1/M2/M3 판정을 생성한다 | W1/W2가 원시 artifact에서 M1 input p95·M2 incremental process-tree working set·M3 boundary violations와 tripwire 판정을 산출해 implementation ledger에 연결하며 미측정·누락·실패를 PASS로 표시하지 않는다 — 근거 | AC-01-4 | PARTIAL |
| QA-005 | T1,T3,T8,T10; Windows/native+4 engines | 기존 backup profile을 선택해 연결하고 service secret 접근 canary와 별도 interactive credential 입력을 관찰한다 | 허용된 Target의 redacted metadata만 바인딩하고 service secret을 사용자 연결에 자동 제공하지 않는다 — 근거 | AC-02-1 | PASS_NATIVE |
| QA-006 | T1,T3,T8,T10; Windows/native+4 engines | 다른 owner/window/Workspace와 과거 revision/generation handle을 각각 보낸다 | 다른 owner/window/workspace 또는 오래된 generation/revision handle을 데이터 전송 전에 거부한다 — 근거 | AC-02-2 | PASS_NATIVE |
| QA-007 | T1,T3,T8,T10; Windows/native+4 engines | 잘못된 hostname/root CA 및 미인증 driver/server capability 조합으로 연결한다 | 인증서 검증을 충족 못하면 거부하고 DB별 미지원 capability를 활성화하지 않는다 — 근거 | AC-02-3 | PASS_NATIVE |
| QA-008 | T1,T3,T8,T10; Windows/native+4 engines | 실행 transaction 중 reconnect·창닫기·lease 만료·service restart를 각각 발생시킨다 | 재접속/창닫기/권한만료 때 세션을 무효화하며 진행 transaction을 새 connection으로 복원하지 않는다 — 근거 | AC-02-4 | PASS_NATIVE |
| QA-009 | T1,T3,T8,T10; Windows/native+4 engines | DB handshake와 service 응답을 지연시키며 UI 입력 및 기존 backup worker를 동시에 실행한다 | DB·service 접속을 기다리는 동안 UI thread와 별도 backup worker permit을 점유하지 않는다 — 근거 | AC-02-5 | PARTIAL |
| QA-010 | T5,T6,T7; local SQLite fault harness | 동일 scoped attempt를 중복·다른 payload로 보내고 재시작 후 오래된 token을 다시 보낸다 | 같은 scoped key+payload는 같은 operation을 반환하고 다른 payload는 IDEMPOTENCY_CONFLICT로 거부한다 — 근거 | AC-03-1 | PASS_NATIVE |
| QA-011 | T5,T6,T7; local SQLite fault harness | 중복/역순/누락 operation hint를 보내고 재연결 후 authoritative journal을 조회한다 | operation state와 version을 함께 commit하고 duplicate/old hint를 무시하며 gap/reconnect는 journal 상태로 복구한다 — 근거 | AC-03-2 | PASS_NATIVE |
| QA-012 | T5,T6,T7; local SQLite fault harness | dispatch intent 전후와 COMMIT 응답 사이에서 프로세스/연결을 끊고 다시 시작한다 | dispatch/COMMIT 후 crash·응답유실을 unknown으로 복구하고 target SQL을 자동 재실행하지 않는다 — 근거 | AC-03-3 | PASS_NATIVE |
| QA-013 | T5,T6,T7; local SQLite fault harness | admission/COMMIT 상태 기록 시 disk-full·손상·알 수 없는 schema version을 주입한다 | journal 손상·disk-full이면 새 쓰기를 거부하고 기존 operation의 불확실성을 숨기지 않는다 — 근거 | AC-03-4 | PASS_NATIVE |
| QA-014 | T5,T6,T7; local SQLite fault harness | SQL·password·typed row에 canary를 넣고 DB/WAL/log/diagnostics를 검사한다 | raw SQL·credentials·row values를 default journal/diagnostics에 영속 저장하지 않는다 — 근거 | AC-03-5 | PASS_NATIVE |
| QA-015 | T1,T2,T3; real engines+render | 10k catalog 항목과 넓은 table을 탐색·검색·페이지 이동하며 DOM/캐시를 샘플링한다 | 10k metadata 목록을 lazy pages로 탐색하며 화면 DOM과 retained pages를 상한 안에 유지한다 — 근거 | AC-04-1 | PASS_NATIVE |
| QA-016 | T1,T2,T3; real engines+render | 필터/order/schema/target을 바꾼 뒤 과거 cursor를 재사용하고 keyless table을 연다 | cursor의 target/filter/order/schema mismatch를 거부하고 keyless preview의 한계를 표시한다 — 근거 | AC-04-2 | PASS_NATIVE |
| QA-017 | T1,T2,T3; real engines+render | NULL/empty/i64/u64/decimal/bytes/timezone 및 oversize/unsupported 값을 조회·복사한다 | NULL/empty/u64/decimal/binary/temporal 값을 lossless로 구분하고 truncated/unsupported 값은 편집하지 않는다 — 근거 | AC-04-3 | PASS_NATIVE |
| QA-018 | T1,T2,T3; real engines+render | 다른 connection에서 삽입/삭제하며 live pages와 인증된 snapshot을 각각 연다 | 페이지 일관성·count 정확도·cap 상태를 표시하며 live pages를 snapshot으로 주장하지 않는다 — 근거 | AC-04-4 | PASS_NATIVE |
| QA-019 | T1,T2,T3; real engines+render | page eviction·가상화·FK 이동·metadata refresh 중 키보드/selection/ARIA를 검사한다 | 가상화·page eviction 후 키보드 focus/selection/ARIA 좌표를 보존하고 FK/diagram을 같은 metadata version으로 표시한다 — 근거 | AC-04-5 | PASS_RENDER |
| QA-020 | T3,T6,T7,T8; actual driver+render | selection·literal semicolon·procedural block을 포함한 SQL 원문을 실행해 서버 수신 내용을 대조한다 | 사용자 SQL을 승인한 원문대로 실행하고 generated preview와 섞거나 LIMIT을 주입하지 않는다 — 근거 | AC-05-1 | PASS_NATIVE |
| QA-021 | T3,T6,T7,T8; actual driver+render | readonly/read-write/Full에서 unknown/DDL/EXPLAIN ANALYZE를 실행하고 service IPC를 관찰한다 | 모드·unknown/DDL/ANALYZE 정책을 Rust에서 검사하고 privileged backup service로 SQL을 넘기지 않는다 — 근거 | AC-05-2 | PASS_NATIVE |
| QA-022 | T3,T6,T7,T8; actual driver+render | run A 종료·cancel A·run B admission을 의도적으로 교차시키고 cancel lane을 포화한다 | 포화된 작업 중에도 정확한 query generation만 취소하고 취소 요청을 rollback 확인으로 표시하지 않는다 — 근거 | AC-05-3 | PASS_NATIVE |
| QA-023 | T3,T6,T7,T8; actual driver+render | manual transaction의 COMMIT 응답을 유실시키고 reconnect·후속 run·재실행을 시도한다 | manual transaction을 같은 connection에 고정하며 reconnect/unknown commit 때 격리하고 자동 replay하지 않는다 — 근거 | AC-05-4 | PASS_NATIVE |
| QA-024 | T3,T6,T7,T8; actual driver+render | duplicate/gap/stale result frame과 future ACK를 주입하고 retention expiry 후 attach한다 | duplicate/gap/stale batches를 정확히 처리하고 credit/cache 상한 및 terminal status를 유지한다 — 근거 | AC-05-5 | PASS_NATIVE |
| QA-025 | T3,T6,T7,T8; actual driver+render | 자동 history 없이 실행한 뒤 명시 saved query만 저장하고 DPAPI 이전 SQLite/WAL 유출을 검사한다 | 자동 raw history/result 저장 없이 명시적 named query만 사용자 암호화 저장하고 secret canary를 로그에 남기지 않는다 — 근거 | AC-05-6 | PASS_NATIVE |
| QA-026 | T3,T4,T6; certified engine matrix | join/view/keyless/nullable key/unsupported codec/nontransactional table에서 insert/update/delete draft를 시도한다 | join/expression/keyless/unsupported type 또는 nontransactional 대상의 편집을 구체적 read-only 이유와 함께 Rust·UI 양쪽에서 거부한다 — 근거 | AC-06-1 | PASS_NATIVE |
| QA-027 | T3,T4,T6; certified engine matrix | 값·이름에 SQL metacharacter를 넣고 preview→review 변조→apply를 시도한다 | 값은 bind하고 식별자는 scoped catalog와 dialect quoting으로 생성하며 독립 구조 gate 및 실제 대상·SQL·매개변수 review를 통과한 변경만 실행한다 — 근거 | AC-06-2 | PASS_NATIVE |
| QA-028 | T3,T4,T6; certified engine matrix | 다른 connection에서 원본/schema를 변경하거나 multi-match를 만들고 원자 batch를 apply한다 | 원본 행·schema 충돌이나 multi-match 시 지원 transactional effects의 전체 batch를 rollback하고 draft를 보존하며 불명 rollback을 성공으로 표시하지 않는다 — 근거 | AC-06-3 | PASS_NATIVE |
| QA-029 | T3,T4,T6; certified engine matrix | MySQL/MariaDB no-op update와 NULL·decimal·typed key를 actual driver/server에서 왕복시킨다 | MySQL/MariaDB no-op의 matched·changed 차이와 NULL·정밀 decimal 등 typed key/value를 검증하며 미지원 codec은 read-only로 유지한다 — 근거 | AC-06-4 | PASS_NATIVE |
| QA-030 | T3,T4,T6; certified engine matrix | COMMIT reply를 유실시킨 뒤 재apply·같은 token·새 token·operator acknowledgment를 시도한다 | COMMIT 결과 불명은 새 apply·resend를 차단하고 자동 재실행하지 않으며 사용자 acknowledgment를 commit·rollback 증거로 바꾸지 않는다 — 근거 | AC-06-5 | PASS_NATIVE |
| QA-031 | T2,T5,T9; streaming I/O+engines | 메모리보다 큰 파일과 넓은 결과를 import/export하고 slow consumer·oversize batch를 주입한다 | 대량 파일·결과를 이동할 때 JS/native에 전체 파일을 적재하지 않고 parser·batch·credit·retained memory 상한을 지킨다 — 근거 | AC-07-1 | PASS_NATIVE |
| QA-032 | T2,T5,T9; streaming I/O+engines | batch0 commit 뒤 batch1 rollback/unknown을 각각 발생시키고 후속 batch 및 restart를 관찰한다 | 검토한 import만 bound INSERT의 단일 child batch transaction으로 실행하며 confirmed commit/rollback·partial·unknown을 구분하고 unknown 뒤 후속 batch를 중단한다 — 근거 | AC-07-2 | PARTIAL |
| QA-033 | T2,T5,T9; streaming I/O+engines | shown export와 export-all을 실행하고 원본 query의 volatile/side-effect 재실행을 탐지한다 | shown export는 기존 결과를 재실행하지 않고 export-all은 명시적 새 읽기의 실제 target identity·time·consistency를 공개하며 volatile/unproven SQL을 자동 재조회하지 않는다 — 근거 | AC-07-3 | PASS_NATIVE |
| QA-034 | T2,T5,T9; streaming I/O+engines | 출력 중 취소·disk-full·flush 실패·rename 실패를 각각 주입한다 | 파일은 complete terminal·flush/sync·atomic finalize 성공 뒤에만 complete로 표시하며 취소·disk-full·finalize 실패는 partial로 남는다 — 근거 | AC-07-4 | PASS_NATIVE |
| QA-035 | T2,T5,T9; streaming I/O+engines | 동일 child 요청 재전송·재시작 및 formula/NULL/empty/u64/binary 값 매핑을 검증한다 | 중복 요청·child·재시작에도 전송된 batch를 자동 재실행하지 않고 formula·NULL/empty·정밀숫자·binary 변환은 검토된 정책과 정확한 타입 계약을 따른다 — 근거 | AC-07-5 | PASS_NATIVE |
| QA-036 | T1,T10; existing backup/restore integration | Workbench의 선택 table IDs를 기존 Scope로 넘기고 archive/revision/owner 변경 후 다시 시도한다 | 선택 table IDs를 기존 Scope API에 전달하고 service가 Workspace/target/schema identity를 다시 검사한다 — 근거 | AC-08-1 | PASS_NATIVE |
| QA-037 | T1,T10; existing backup/restore integration | backup-before-change 완료/실패 이벤트와 중복 이벤트를 보내고 SQL dispatch를 관찰한다 | backup-before-change를 별도 dependency로 표시하고 backup 완료 이벤트만으로 SQL을 실행하지 않는다 — 근거 | AC-08-2 | PASS_RENDER |
| QA-038 | T1,T10; existing backup/restore integration | 정상 restore evidence와 실제 target proof를 통해 restored DB를 연다 | 기존 restore preflight/confirmation/evidence를 유지하며 일치하는 restored target만 새 readonly session으로 연다 — 근거 | AC-08-3 | PASS_NATIVE |
| QA-039 | T1,T10; existing backup/restore integration | restored endpoint 불일치·권한거부를 주입하고 source endpoint/privileged credential 사용을 감시한다 | 복원 대상 불일치·권한거부 때 원본 fallback이나 더 높은 service credentials 사용을 하지 않는다 — 근거 | AC-08-4 | PASS_NATIVE |
| QA-040 | T1,T10; existing backup/restore integration | 긴 query/import/slow UI와 기존 backup/restore/cancel/service drain을 함께 실행한다 | 긴 query/import/느린 consumer와 백업 동시 실행에서 기존 service lifecycle·scheduling·cancel 회귀가 없다 — 근거 | AC-08-5 | PARTIAL |
| QA-041 | T6; manual transaction | BEGIN/COMMIT/rollback 응답을 각각 끊고 cancel이 늦게 도착하게 한다 | opening/failed_needs_rollback/implicit commit/unknown을 실제 증거로 표시하며 다음 run은 settlement 전 socket을 재사용하지 않는다 | AC-05-3,AC-05-4 | PASS_NATIVE |
| QA-042 | T6; epoch recovery | unknown operation을 보존한 채 journal 복구 후 header epoch를 회전시키고 old token을 보낸다 | 과거 epoch/unknown/key/digest가 보존되고 old token은 신규 admission되지 않는다 | AC-03-1,AC-03-3,AC-03-4 | PASS_NATIVE |
| QA-043 | T1; Windows integrity | 관리자 권한으로 앱을 열고 Workbench를 진입한 뒤 사용자 상태/credential 경로를 조사한다 | Workbench는 파일/DPAPI 초기화 전에 거부되며 기존 backup 콘솔 흐름과 구분된다 | AC-02-1,AC-02-2 | PARTIAL |
| QA-044 | T3; server readonly lease | 서버 readonly를 입증 못한 session에서 cached lease로 분류상 read SQL을 실행한다 | parser 판단만으로 lease를 허용하지 않고 fresh authority 또는 capability 거부가 적용된다 | AC-02-2,AC-05-2 | PASS_NATIVE |
| QA-045 | T4; insert/default | insert에서 generated key/omitted default/explicit NULL을 구분하고 update DEFAULT를 요청한다 | insert review에는 omitted defaults가 보이며 미인증 UPDATE DEFAULT는 거부한다; 임의 SQL로 인코딩하지 않는다 | AC-06-1,AC-06-2,AC-06-4 | PASS_NATIVE |
| QA-046 | T6; partial script | autocommit UPDATE 성공→DDL implicit commit→다음 statement 실패를 순차 실행한다 | 각 statement 증거와 overall partial/unknown을 보존하며 앞선 commit을 rolled_back으로 덮지 않는다 | AC-05-1,AC-05-4,AC-03-3 | PASS_NATIVE |
| QA-047 | T7; byte protocol | ACK를 재전송/역순/미래 sequence로 보내고 동일 retained frame을 replay한다 | credit는 실제 보낸 contiguous byte charge만 한 번 해제되며 replay는 SQL 재실행이나 double credit을 만들지 않는다 | AC-01-3,AC-05-5 | PASS_NATIVE |
| QA-048 | T9; import identity | child batch digest/range를 바꾸어 같은 parent+index로 재전송하고 unknown 뒤 continue한다 | 동일 batch 중복은 기존 상태를 반환하고 다른 payload는 충돌; unknown 뒤 후속 전송/자동 resume가 없다 | AC-03-1,AC-07-2,AC-07-5 | PASS_NATIVE |
| QA-049 | T9; export finalize | complete row stream 뒤 flush 또는 atomic rename을 실패시키고 UI를 재연결한다 | 파일은 partial/실패로 남고 status pull에서도 complete로 표시되지 않는다 | AC-07-3,AC-07-4 | PASS_NATIVE |
| QA-050 | T10; target conflict | authority read 직후 외부 client가 write/backup을 시작하는 경쟁을 만든다 | observed conflict는 admission을 차단하지만 기존 receipt를 분산 mutex/전체 DB QoS로 주장하지 않는다 | AC-02-5,AC-08-5 | PASS_NATIVE |
| QA-051 | T3; saved content privacy | 명명 SQL/label canary를 저장한 뒤 SQLite main/WAL/temp와 기본 diagnostics를 검사한다 | 원문은 DPAPI 이전 bind되지 않으며 redacted error도 값/secret을 포함하지 않는다 | AC-03-5,AC-05-6 | PASS_NATIVE |
| QA-052 | T2,T4; UI task | 한국어 IME 조합 중 Enter, focus-row eviction, theme/zoom/reduced motion을 실제 렌더로 확인한다 | IME Enter로 apply되지 않고 target/review가 명확하며 논리 focus/ARIA/contrast가 유지된다 | AC-04-5,AC-06-2 | PASS_RENDER |

## Execution and evidence

- Unit/property: typed codecs, canonical digest/IDs, generated SQL structure and state transitions; assertions must target behavior or boundary failures.
- Native integration: actual driver sessions, generation/cancel barriers, journal crash/disk faults, matching-row semantics, readonly/isolation/TLS. Mock-only success does not certify live engines.
- Render/E2E: Windows Tauri/WebView real native producer, keyboard/IME/ARIA/theme/font/zoom, paged/virtualized tables, stream/drop/reconnect and target identity.
- Existing regression: run repository-required Rust/console/service gates and existing backup/restore scenarios. Test implementation order follows [manifest](sections/index.md).
- Record build/runtime versions, fixture hashes, raw fault timing/state vectors and measured boundaries. Redact payloads/secrets; no production database access is required or authorized by this plan.
- A failed/blocked matrix cell is NOT RUN/FAIL with reason, not omitted. No benchmark or business acceptance is established by the document integrity checks.

## Operational journeys

[Operation scenarios](operation-scenarios.md) OP-01→QA001–040, OP-02→QA036–037, OP-03→QA038–039, OP-04→QA022–023/041, OP-05→QA032/048, OP-06→QA003/024/047. Exact AC IDs in the table are authoritative if QA order changes.
