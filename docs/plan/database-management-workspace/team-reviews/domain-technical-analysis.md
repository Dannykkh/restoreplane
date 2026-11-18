# 데이터베이스 트랜잭션 및 시스템 통합 전문가 분석

> **검토 기준**: 정적 설계 문서 9종 검토 완료. 리뷰어 의견은 런타임 검증 사실이 아니며, 실제 드라이버·장애 주입·성능 실측은 수행하지 않음(`NOT RUN`).

## 1. End-to-End 기술 계약 및 통합 경계

1. **권한 분리 및 세션 승인 (User-Owned Engine vs Privileged Service)**
   - 임의 대화형 SQL과 드라이버 세션은 현재 Windows 사용자의 Tauri Rust Workbench가 소유하며, `backup_service`는 일반 SQL 엔드포인트를 열지 않는다([spec.md](../spec.md), [architecture-analysis.md](architecture-analysis.md)).
   - 서비스의 머신 DPAPI(`CRYPTPROTECT_LOCAL_MACHINE`) 비밀은 내보내거나 대화형으로 자동 재사용하지 않고, Workbench는 사용자 DPAPI 자격증명과 로컬 SQLite 저널을 별도 소유한다([spec.md](../spec.md), [architecture-analysis.md](architecture-analysis.md)).
   - 서비스는 파이프 주체(SID/세션) 기반의 단기 승인 리스(`workbench.access_prepare`형 계약)로 정규 비비밀 접속 메타데이터·활성 Workspace·프로필/타겟 리비전만 검증해 발급한다([architecture-analysis.md](architecture-analysis.md)).

2. **단일 트랜잭션 경계, 이중 쓰기(Dual-Write) 및 Outbox/Inbox**
   - 로컬 `operation_journal`과 원격 타겟 DB는 XA/2PC로 묶이지 않는다. 로컬 SQLite 트랜잭션 1회가 요청/다이제스트·연산 상태/버전·`outbox_event`를 원자적으로 기록하고, 소비 측 `inbox_receipt(eventId)`와 프로젝션 갱신이 별도 로컬 원자 트랜잭션을 형성한다([spec.md](../spec.md)).
   - 이벤트 전달은 최소 1회(at-least-once)이며 타겟 SQL의 정확히 1회(exactly-once)를 보장하지 않는다. 와이어 전송 전 `dispatching`/`commit_requested` 의도를 먼저 영속화한다([spec.md](../spec.md), [architecture-analysis.md](architecture-analysis.md)).

3. **엔진별 타입 일관성 및 원자성 한계 (Typed Per-Engine Consistency & Atomicity Limits)**
   - `typed_cell`은 표시 문자열과 분리된 NULL·정밀 정수(`i64`/`u64`)·decimal·특수 부동소수·bytes·시간/시간대 메타데이터를 보존한다([spec.md](../spec.md), [UX-06](ux-analysis.md)).
   - `verified_apply`는 단일 타겟 트랜잭션 안에서 `스키마/식별자 재확인 → 원본 행 잠금 읽기 → 타입 비교 → 조건부 변경 → matched-row 검증 → COMMIT` 순서로 수행한다([spec.md](../spec.md)).
   - 원자성 보장은 인증된 트랜잭션 스토리지 내 대상 테이블 변경에 한정된다. DDL 암묵적 커밋, 다중 문 스크립트의 선행 자동 커밋, 트리거의 비트랜잭션/외부 부작용, 백업 선행 실행(`F7`)은 전체 롤백 경계에 포함되지 않는다([spec.md](../spec.md), [redteam-analysis.md](redteam-analysis.md), [RT-04](redteam-analysis.md)).

## 2. Critical / Important 발견 사항 및 구체적 해결책

| 등급 | 항목 및 위험 요약 | 구체적 해결책 (설계 반영 계약) | 근거 문서 |
|---|---|---|---|
| **Critical** | **C-TX1. 커밋 불확실성(`outcome_unknown`)과 재시도/대사 위험**<br>COMMIT 전송 후 소켓/프로세스 단절이나 타임아웃 발생 시 원격 반영 여부를 알 수 없으며, 현재 행 값 일치만으로는 트리거·시퀀스·선행 실행 부재를 증명할 수 없다. | 전송 전 저널 기록 의무화; 응답 유실 시 `outcome_unknown`으로 격리하고 자동 재전송 금지. 서버 증적 기반 확정과 운영자 인지(`Operator Acknowledgment`)를 분리하고, 인지 후 재작업 시 기존 멱등성 키 재사용을 금지한다. | [spec.md](../spec.md), [spec.md#L82-L84](../spec.md), [architecture-analysis.md (C4)](architecture-analysis.md), [redteam-analysis.md (RT-07)](redteam-analysis.md), [ux-analysis.md (UX-02)](ux-analysis.md) |
| **Critical** | **C-TX2. 지연 취소(Late Cancel)와 실행 에포크(`Execution Epoch`) 경쟁**<br>쿼리 A 완료 직후 동일 물리 연결에서 쿼리 B가 시작될 때, 지연 도착한 PostgreSQL cancel 또는 MySQL `KILL QUERY`가 B를 타격할 수 있다. | `connection_generation` 외에 물리 세션별 `Execution Epoch`와 취소 정착(settlement) 장벽을 둔다. A의 취소 레인이 정착되기 전까지 B 디스패치를 차단하고, 정착 여부가 불확실하면 해당 물리 연결을 폐기한다. | [spec.md](../spec.md), [spec.md#L108](../spec.md), [architecture-analysis.md (C3)](architecture-analysis.md), [redteam-analysis.md (RT-01)](redteam-analysis.md) |
| **Critical** | **C-TX3. 30초 승인 리스와 즉시 무효화·수동 트랜잭션 수명 충돌**<br>캐시된 30초 리스 동안 Workspace 보관/프로필 수정이 발생하면 구 핸들로 쓰기가 전송될 수 있으며, 리스 만료가 원격 트랜잭션 롤백을 의미하지 않는다. | 쓰기·보호 연계 디스패치 직전에는 항상 최신 서비스 리비전/소유권을 즉시 검증한다. 리스 만료 시 신규 디스패치만 차단하고 취소/롤백/상태 조회는 허용하며, 로컬 리스 만료를 원격 롤백 증거로 처리하지 않는다. | [spec.md](../spec.md), [architecture-analysis.md (C1, C3)](architecture-analysis.md), [redteam-analysis.md (RT-03)](redteam-analysis.md) |
| **Important** | **I-TX1. 다중 인스턴스·재시작·보존주기(Retention)에 따른 중복 디스패치 및 비밀 잔류**<br>로컬 저널 복원·다중 창 경쟁·오래된 멱등성 키 퍼지 시 이미 전송된 쓰기가 재실행되거나, SQL/파라미터가 SQLite WAL에 평문 잔류할 수 있다. | 단일 로컬 저널 writer와 `Journal Epoch`(부팅/복원 세대)를 도입하고, `dispatching` 상태 쓰기는 리스 만료 시 타 인스턴스 인수를 금지한다. 재시도 보존 기간 동안 메타데이터 툼스톤을 유지하고 만료 키 재사용을 거부한다. 저장 허용 쿼리만 바인드 전 사용자 DPAPI로 암호화하고 기본 원문 SQL/결과 디스크 캐시를 금지한다. | [spec.md](../spec.md), [spec.md#L80-L85](../spec.md), [architecture-analysis.md (I4)](architecture-analysis.md), [redteam-analysis.md (RT-06, RT-09)](redteam-analysis.md) |
| **Important** | **I-TX2. 엔진별 고유 키·No-op·부분 스크립트·스트리밍 크레딧 회계 미확정**<br>MySQL matched vs changed rows, 부분/표현식 인덱스 오인, keyset을 스냅샷으로 오해, 다중 문 일부 커밋, 중복 ACK 크레딧 증폭 위험이 존재한다. | 완전한 비중첩 non-null 고유 키만 편집 허용하고 어댑터별 `CLIENT_FOUND_ROWS`/잠금 읽기/콜레이션 비교를 인증한다. 스크립트는 문장별 `Statement Outcome`을 기록한다. `credit_window`는 스트림 ID별 송신 시퀀스 원장과 최고 연속 ACK만 반영하며 갭 발생 시 SQL 재실행을 금지한다. | [spec.md](../spec.md), [spec.md#L65-L70](../spec.md), [domain-research.md (D3, D5, D6)](domain-research.md), [redteam-analysis.md (RT-02, RT-05, RT-08)](redteam-analysis.md) |

## 3. 구현 및 게이트 통과를 위해 반드시 증명해야 할 사항 (What Must Be Proven)

1. **커밋 경계 장애 주입 (T6)**: COMMIT 전·중·후 소켓/프로세스 강제 종료 시 `outcome_unknown` 전이, 자동 재시도 0회, 재시작 후 격리 유지 증명([spec.md](../spec.md)).
2. **취소-디스패치 경쟁 증명 (T8, RT-01)**: 쿼리 A 완료 직전 취소 송신 및 쿼리 B 대기 시, B가 A의 취소 신호를 받지 않거나 구 연결이 안전하게 폐기됨을 엔진별로 증명([spec.md](../spec.md), [redteam-analysis.md](redteam-analysis.md)).
3. **엔진별 어댑터 매트릭스 인증 (T3, T4, RT-08)**: PostgreSQL/MySQL/MariaDB/SQLite 대상 `u64::MAX`/decimal/NULL/시간대 무손실 왕복, MySQL no-op 대 row conflict 구분, SQLite `BUSY_SNAPSHOT` 및 배치 전체 롤백 증명([spec.md](../spec.md), [redteam-analysis.md](redteam-analysis.md)).
4. **크레딧/ACK 및 비밀 무잔류 검증 (T2, T7, RT-05, RT-06)**: 중복·미래·타 세대 ACK 주입 시 크레딧 불변, 느린 소비자에서 메모리 상한 유지, 크래시 전후 SQLite/WAL/로그/지원 번들 내 secret canary 부재 증명([spec.md](../spec.md), [redteam-analysis.md](redteam-analysis.md)).

## 4. NOT RUN 및 한계

- **NOT RUN**: 실제 PostgreSQL/MySQL/MariaDB/SQLite 라이브 연결, 드라이버/TLS 통합, 소켓/프로세스 kill 장애 주입, WebView 대량 그리드 메모리·지연 실측(제안된 256KiB 배치/8MiB 크레딧/32MiB UI/64MiB 네이티브 한도는 미실측 가설임)([spec.md](../spec.md), [research.md](../research.md)).

## 5. Dictionary Updates

| Action | Term | Definition | Evidence |
|---|---|---|---|
| ADD | Workbench Access Lease | 서비스가 파이프 주체·활성 Workspace·타겟·정규 접속 메타데이터 리비전을 검증해 발급하는 단기 메모리 승인. 원격 트랜잭션 중단 증거가 아님. | [spec.md](../spec.md), [architecture-analysis.md (C1)](architecture-analysis.md), [redteam-analysis.md (RT-03)](redteam-analysis.md) |
| ADD | Execution Epoch | 물리 연결 내 개별 실행 문장 세대와 취소 레인 정착(settlement) 경계. 정착 전 차기 쿼리 디스패치를 차단함. | [spec.md](../spec.md), [redteam-analysis.md (RT-01)](redteam-analysis.md) |
| ADD | Statement Outcome | 다중 문 스크립트 내 개별 문장의 디스패치·완료·부작용 상태. 후속 실패가 선행 자동 커밋을 지우지 않음. | [spec.md](../spec.md), [redteam-analysis.md (RT-02)](redteam-analysis.md) |
| ADD | Operator Acknowledgment | `outcome_unknown` 상태를 운영자가 인지하고 새 독립 작업을 여는 행위. 원격 커밋/롤백 증거로 승격되지 않음. | [spec.md](../spec.md), [redteam-analysis.md (RT-07)](redteam-analysis.md), [ux-analysis.md (UX-02)](ux-analysis.md) |
| ADD | Journal Epoch | 로컬 사용자 저널의 부팅·소유·복원 세대 식별자. 구 저널 복원이나 타 인스턴스가 전송된 쓰기를 재디스패치하는 것을 방지함. | [spec.md](../spec.md), [redteam-analysis.md (RT-09)](redteam-analysis.md) |
| REFINE | Outcome Unknown | 원격 실행/커밋 결과 미확정 상태. 현재 행 값 일치·취소 요청·창 닫기로 해소되지 않으며 자동 재전송을 금지함. | [domain-dictionary-database-management.md](../../../domain-dictionary-database-management.md), [domain-research.md](domain-research.md), [ux-analysis.md](ux-analysis.md) |
| REFINE | Verified Apply | 인증된 단일 타겟 트랜잭션의 조건부 변경. 보증 범위는 대상 트랜잭션 스토리지 효과로 한정되며 트리거/외부 효과 및 백업 검증 레벨(`Verify Level`)과 구분됨. | [domain-dictionary-database-management.md](../../../domain-dictionary-database-management.md), [redteam-analysis.md (RT-04)](redteam-analysis.md), [ux-analysis.md](ux-analysis.md) |
| REFINE | Credit Window | 스트림 ID별 송신 시퀀스 원장과 최고 연속 ACK로만 갱신되는 인코딩 바이트/행 허용량. 디코딩 힙 예산 및 쿼리 재실행과 분리됨. | [domain-dictionary-database-management.md](../../../domain-dictionary-database-management.md), [architecture-analysis.md (I1)](architecture-analysis.md), [redteam-analysis.md (RT-05)](redteam-analysis.md) |
| CONFLICT | Lease / Transaction Owner | 기본 사전([domain-dictionary.md](../../../domain-dictionary.md))의 용어는 서비스 `Work Item` 및 `Operational Store` 전용이므로, Workbench 승인/트랜잭션/저널 소유권에는 반드시 `Workbench` 접두사를 부여해 분리함. | [domain-dictionary.md](../../../domain-dictionary.md), [architecture-analysis.md](architecture-analysis.md) |
