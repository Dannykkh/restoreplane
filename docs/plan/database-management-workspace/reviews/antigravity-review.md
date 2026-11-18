# Database Management Workspace 구현 계획 독립 아키텍처 리뷰

**결론 및 종합 판정:** Tauri 유지와 사용자 프로세스 소유 Workbench 분리(전략 A), 임의 SQL에 대한 비현실적 서버 `exactly-once` 배제는 타당하다. 그러나 **신규 거대 크레이트(`restoreplane-workbench`)의 과도한 책임 집중, Outbox/Inbox의 단일 프로세스 과설계, 권한/마이그레이션/취소 경계의 상충 계약 7건(Critical 3, High 4)**은 하위 산출물(`db-schema.md`, `ipc-spec.md`, `sections/*`)로 위임하기 전에 [plan.md](../plan.md)와 [spec.md](../spec.md)에서 먼저 교정해야 한다. 런타임·성능·장애 주입 테스트는 **NOT RUN**이다.

## 1. Critical / High 결함 및 구체적 수정안

### [Critical-1] 신규 모듈(`restoreplane-workbench`) 비대화 및 기존 조합 지점 우회
- **근거:** [plan.md](../plan.md), [spec.md](../spec.md), [codebase.md](../research/codebase.md)
- **문제 (Composition/Security):** `restoreplane-workbench` 단일 크레이트에 드라이버, 세션 코디네이터, 저널, 타입 값, 메타데이터, 쿼리, 편집, 데이터 이동, 보호 연계, user-DPAPI까지 모두 넣도록 계획했다. 이는 기존 `restoreplane-secrets`(DPAPI 추상화), `restoreplane-domain`/`restoreplane-application`(명령·검증 계약)과 중복되며, 단일 프로세스(Tauri Rust ↔ WebView) 내부 통신에 영속 `outbox_event`/`inbox_receipt` 테이블([plan.md](../plan.md), [domain-dictionary-database-management.md](../../../domain-dictionary-database-management.md))까지 강제해 SQLite 쓰기 증폭을 유발한다.
- **수정안:** ① user-scope DPAPI는 `restoreplane-secrets`의 스코프 분리(`MachineScope` vs `CurrentUserScope` 타입 분리)로 확장해 재사용한다. ② `restoreplane-workbench`는 세션/실행/저널 코어와 엔진 어댑터 경계만 소유하고, 스키마/명령 계약은 `restoreplane-application`에 둔다. ③ 로컬 Tauri 프로세스 내 UI 투영은 `Operation Journal` 버전 조회(`operation.get` + 단조 증가 `state_version`)로 단순화하고, 별도 영속 Outbox/Inbox 테이블을 제거한다.

### [Critical-2] Operational Store 스키마 v4 동결 선언과 `workbench.binding.authorize` 저장소 요구의 상충
- **근거:** [plan.md](../plan.md), [plan.md](../plan.md), [spec.md](../spec.md)
- **문제 (Consistency/Migration):** [plan.md](../plan.md)는 "Operational Store schema v4를 올리지 않는다"고 선언하면서 동시에 "additive profile revision/epoch control-plane storage가 필요할 수 있다"고 모순되게 기술한다. `workbench.binding.authorize`가 반환할 `profile_revision`/`authority_epoch`의 영속 원천이 기존 v4 테이블에 있는지, 스키마 마이그레이션(v5)이 필요한지 확정되지 않아 Wave 1(`section-02`)이 즉시 차단된다.
- **수정안:** [plan.md](../plan.md)에서 기존 v4 컬럼(`updated_at`/기존 revision·fingerprint)만으로 `authorize` 서명·검증이 가능한지 확정하거나, 필요한 경우 서비스 카탈로그의 명시적 additive migration 번호와 하위 호환 규칙을 계획에 명시한다.

### [Critical-3] 물리 연결 취소(Cancel)와 `Execution Epoch` 배리어의 프로토콜·트랜잭션 불일치
- **근거:** [plan.md](../plan.md), [plan.md](../plan.md), [spec.md](../spec.md), [spec.md](../spec.md)
- **문제 (Cancel/Consistency):** PG(`CancelToken` 별도 소켓), MySQL/MariaDB(별도 제어 연결 `KILL QUERY <conn_id>`), SQLite(`sqlite3_interrupt` 핸들)는 취소 완료 시점의 보장 방식이 다르다. 특히 수동 트랜잭션(`WorkbenchTransaction`) 중 문장 취소가 불확실해 물리 연결을 폐기("uncertainty discards the physical connection")할 때, 서버 측 미결 트랜잭션이 즉시 롤백되었다고 단정하면 `Rollback-request confirmation is not rollback proof`([plan.md](../plan.md)) 계약과 충돌한다.
- **수정안:** 수동 트랜잭션 중 취소 미확정으로 물리 연결을 폐기할 때, 해당 `WorkbenchTransaction`과 선행 쓰기 문장이 포함된 `Operation Journal` 상태를 `rolled_back`이 아닌 `outcome_unknown`(또는 읽기 전용이었던 경우에만 `aborted_disconnected`)으로 전이하는 규칙을 [plan.md](../plan.md)에 명시한다.

### [High-1] 관리자 권한(Elevated/High-Integrity) 실행 거부 시 콘솔 앱 내 모드 분리 계약 누락
- **근거:** [plan.md](../plan.md), [spec.md](../spec.md), [Console lib.rs](../../../../apps/console/src-tauri/src/lib.rs)
- **문제 (Privilege/Security):** 단일 Tauri 프로세스(`apps/console`)에서 백업 관리 UI는 허용하되 Workbench만 고무결성(Elevated) 실행을 거부한다고 했으나,① 프로세스 시작 시점이 아닌 `workbench_request` 진입 시점에 토큰 무결성 수준(Integrity Level)을 검사하는지, ② Elevated 상태에서 `client-state.sqlite3` 파일 생성 및 `WorkbenchState` 초기화 자체를 차단하는지 명시되지 않았다(Elevated 상태에서 파일이 한 번이라도 생성되면 표준 권한 재실행 시 ACL 충돌 발생).
- **수정안:** 고무결성 토큰 감지 시 `WorkbenchState`가 `client-state.sqlite3` 및 DPAPI 초기화 전에 즉시 `ElevatedExecutionRefused`로 단락(short-circuit)되어 사용자 디렉터리에 어떠한 파일도 생성하지 않도록 [plan.md](../plan.md)에 못박는다.

### [High-2] 읽기 전용 및 조회 경로(F2/F3/F6)의 부작용 SQL·락 타임아웃 방어 계약 미비
- **근거:** [plan.md](../plan.md), [plan.md](../plan.md), [spec.md](../spec.md), [spec.md](../spec.md)
- **문제 (Security/Performance/Consistency):** ① 30초 캐시된 `workbench_access_lease`로 허용되는 "읽기(read)"의 판별 기준이 클라이언트 AST 분류에만 의존하면 `SELECT nextval(...)`, 함수를 통한 쓰기, `PRAGMA` 등이 서비스 재검증 없이 실행될 수 있다. ② Frozen snapshot 조회 및 `export-all` 장기 트랜잭션의 최대 유지 시간(`idle_in_transaction_session_timeout` / 스냅샷 상한)과 `Verified Apply`의 `SELECT ... FOR UPDATE` 잠금 대기 상한(`lock_timeout`)이 정의되지 않아 운영 DB 백업·체크포인트를 블로킹할 수 있다.
- **수정안:** ① 캐시된 Lease는 엔진 세션이 서버 측 읽기 전용 모드(`SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY` / `SET SESSION TRANSACTION READ ONLY` / SQLite `SQLITE_OPEN_READONLY`)로 고정된 핸들에서만 허용한다. ② Snapshot 트랜잭션 최대 수명과 `Verified Apply` 락 획득 타임아웃(예: `NOWAIT` 또는 명시적 짧은 `lock_timeout`)을 [plan.md](../plan.md)에 추가한다.

### [High-3] 대량 Import(F6) 및 다중 문장 스크립트(F3)의 멱등성·재시도 경계 공백
- **근거:** [plan.md](../plan.md), [plan.md](../plan.md), [spec.md](../spec.md), [spec.md](../spec.md)
- **문제 (Idempotency/Consistency):** Chunked import에서 배치 K가 `outcome_unknown`으로 끝났을 때, 저널의 단일 작업 키 `(owner,workspace,target,kind,key)`만으로는 어느 배치까지 확정되었는지와 재시도 차단 범위가 불분명하다. 또한 다중 문장 스크립트 실행 시 드라이버가 단일 프로토콜 패킷으로 묶어 보내는지, 문장별 순차 전송으로 `Statement Outcome` 벡터를 기록하는지 정의되지 않았다.
- **수정안:** ① Chunked import는 `(job_operation_id, batch_index, byte_offset_range)` 단위로 배치 상태를 기록하고, 배치 `outcome_unknown` 발생 시 즉시 후속 배치를 중단하며 자동 재실행을 금지한다. ② 다중 문장 스크립트는 문장 단위 순차 디스패치(또는 엔진별 프로토콜 한계 명시)로 `Statement Outcome` 인덱스를 확정하도록 명시한다.

### [High-4] 백업 실행(Backup Run)과 대화형 무거운 작업(F3/F6) 간 대상 DB 경합 제어 부재
- **근거:** [spec.md](../spec.md), [spec.md](../spec.md), [plan.md](../plan.md)
- **문제 (Performance/Consistency):** 로컬 프로세스 퍼밋만 분리했을 뿐, 동일 `Database Target`에서 백업 서비스가 덤프/복원을 수행하는 동안 Workbench가 DDL, 대량 Import, 장기 트랜잭션을 동시에 실행해 백업 일관성 실패나 메타데이터 잠금 데드락을 유발하는 것을 제어하지 않는다.
- **수정안:** `workbench.binding.authorize` 응답에 해당 Target의 현재 활성 백업/복원 작업 상태를 포함하고, 백업/복원 실행 중에는 Workbench의 쓰기/DDL/대량 Import 디스패치를 거부하거나 명시적 운영자 경고를 강제한다.

## 2. 트립와이어(Tripwires) 측정 가능성 및 갭/중복 검증

| 항목 | 평가 및 근거 | 보완 요구사항 |
|---|---|---|
| **M1 (입력 응답 p95)** | [plan.md](../plan.md): 200회 상호작용 정의는 있으나 시작·종료 타임스탬프 경계가 모호함 | 키/스크롤 입력 이벤트 발생(`performance.now()`)부터 다음 `requestAnimationFrame` 커밋(또는 취소 상태 UI 반영)까지로 측정 지점을 고정할 것 |
| **M2 (프로세스 트리 메모리)** | [plan.md](../plan.md): Windows WebView2는 브라우저 서브프로세스 트리를 공유·폴링 주기에 따라 변동함 | Tauri 호스트 프로세스 Working Set과 WebView2 자식 프로세스 Working Set 합산을 분리 기록하고, GC/유휴 안정화 후 baseline 측정 절차를 명시할 것 |
| **M3 / M4** | [plan.md](../plan.md): 허용치 0 및 연속 미완료 Wave 정의는 명확함 | W2 장애 주입(소켓 강제 종료, 중복/미래 ACK) 시점을 결정론적 테스트 훅으로 `section-03`/`section-05`에 바인딩할 것 |
| **문서 간 갭/중복** | [team-review.md](../team-review.md)와 [domain-dictionary-database-management.md](../../../domain-dictionary-database-management.md)의 용어는 28개 표제어(+본문 `BootEpoch`, `Inline Cell Budget`)로, "30 core terms" 서술과 표 행 수가 미세하게 불일치함 | 사전 표의 행 수를 정확히 30개로 맞추거나 설명 문구를 실제 표 기준으로 정규화할 것 |

## 3. 하위 설계(`db-schema` / `ipc-spec` / `sections`) 위임 가능 여부 판정

- **즉시 `plan.md` / `spec.md` 교정 필수 (위임 불가):**
  1. [Critical-1] `restoreplane-secrets` 재사용 vs 신규 크레이트 범위 축소, 로컬 영속 Outbox/Inbox 제거 여부.
  2. [Critical-2] `workbench.binding.authorize`를 위한 Operational Store v4 유지 가능 여부 vs additive migration 확정.
  3. [Critical-3] 취소 불확실성으로 물리 연결 폐기 시 수동 트랜잭션 및 저널의 `outcome_unknown` 전이 불변식.
  4. [High-1, High-2] Elevated 모드 시 파일 미생성 보장 및 30초 Read Lease의 서버 측 읽기 전용 세션 강제 조건.
- **하위 문서로 위임 가능 (`db-schema.md`, `ipc-spec.md`, `sections/*`):**
  - `Typed Cell`의 와이어 직렬화 포맷, `client-state.sqlite3`의 정확한 DDL/인덱스, 배치별 Import 체크포인트 컬럼 정의, WebView2/Rust 간 `Credit Window` 프레임 헤더 필드 및 엔진별 capability matrix 상세 행은 상위 불변식 교정 후 하위 설계 문서에서 구체화해도 안전하다.
