사용자 소유 Workbench와 privileged service를 분리하는 방향은 타당합니다. 다만 **쓰기 기능은 아래 계약 확정과 엔진별 실행 증거 확보를 조건으로 활성화해야 합니다.** 이는 지정된 9개 문서에 대한 독립 설계 판단이며, 리뷰어 의견이나 문서의 소스 분석 서술을 검증된 런타임 사실로 간주하지 않았습니다.

운영 흐름은 다음 계약으로 연결돼야 합니다.

1. 서비스가 실제 호출 주체·활성 Workspace·정규 대상·프로필 revision을 검증하고, 사용자 Rust 엔진이 별도 사용자 자격증명으로 연결합니다. 표시용 대상 이름과 실행 권한은 구분합니다. [spec F1](../spec.md)
2. 인증된 엔진 역량 안에서 typed 조회 → 원본·키·스키마에 결합한 초안 → 명시적 검토 → 독립된 대상 트랜잭션으로 적용합니다. 사용자 SQL과 수동 트랜잭션에는 임의 래퍼를 삽입하지 않습니다. [spec F3–F5](../spec.md)
3. 로컬 요청·상태·outbox를 원자적으로 기록하고, 전송 의도를 먼저 영속화합니다. 원격 결과는 증거에 따라 기록하며, inbox·projection 갱신은 별도의 로컬 원자적 경계입니다. [spec 일관성 계약](../spec.md)
4. 백업은 별도 보호 작업입니다. 백업 완료 후 SQL은 새로운 명시적 사용자 동작으로 실행하고, 복원 대상은 실제 identity와 완료 증거를 확인한 새 Read-only 세션으로 엽니다. [spec F7](../spec.md)

Critical은 **쓰기 활성화 전 해소해야 할 계약 공백**입니다.

- **C1 — 엔진별 typed 일관성 인증이 구체적이지 않습니다.** PostgreSQL/MySQL/MariaDB/SQLite 각각에 서버·드라이버 버전/설정, 손실 없는 codec, 컬럼 출처, 유효한 unique key, 비교·정렬·NULL 의미, 잠금·격리, matched/changed/no-op 판정표를 작성해야 합니다. 정밀숫자·bytes·시간 메타데이터, collation상 동등한 다른 값, ABA 한계를 포함하고 미인증 조합은 편집을 거부하십시오. [architecture I3](architecture-analysis.md), [RT-08](redteam-analysis.md)
- **C2 — Connection Generation만으로 늦은 취소를 차단할 수 없습니다.** 같은 물리 연결에서 A 완료 후 B가 시작되면 A의 취소가 B에 도달할 수 있다는 설계 위험입니다. 물리 연결별 Execution Epoch와 완료·취소·다음 실행의 공동 admission barrier를 정의하고, 취소 정리가 불확실하면 연결을 폐기해야 합니다. [RT-01](redteam-analysis.md)
- **C3 — commit 불명 상태의 해제 기준이 부족합니다.** 원격 commit과 로컬 저널 사이의 간극은 outbox나 멱등키로 없어지지 않습니다. 서버 증거에 의한 결과 확정과 운영자의 불확실성 수용을 분리하고, 후자는 기존 `outcome_unknown`을 성공으로 바꾸거나 재전송하지 못하게 해야 합니다. 수동 트랜잭션의 statement 성공과 최종 commit 결과도 분리하십시오. [architecture C4](architecture-analysis.md), [RT-07](redteam-analysis.md)
- **C4 — 사용자 실행 주체와 권한 철회의 의미를 확정해야 합니다.** Rust에서 OS user/logon/integrity를 확인하고 elevated 실행 정책을 명시하십시오. 30초 lease와 즉시 무효화 약속은 그대로 양립하지 않습니다. 새 쓰기의 권한 admission 시점, archive/revision 변경과의 선후관계, 이미 허용·전송된 작업의 처리를 정의해야 합니다. 직전 재검증도 원격 SQL을 소급 철회하지는 못합니다. [architecture C1–C3](architecture-analysis.md), [RT-03](redteam-analysis.md)
- **C5 — 원자성 표현이 실제 효과 범위보다 넓습니다.** 편집 테이블의 transactional storage만으로 trigger·비트랜잭션 테이블·외부 효과까지 되돌린다고 인증할 수 없습니다. 보장을 인증된 트랜잭션 효과로 한정하고, 명시적 SQL script는 statement별 결과와 aggregate partial/unknown을 기록하십시오. import는 batch별 원자성, backup-before-change는 별도 복구 수단으로 표시해야 합니다. [RT-02/04](redteam-analysis.md), [spec F6](../spec.md)

Important는 다음 운영 계약입니다.

- **I1 — outbox/inbox 재전달과 SQL 실행을 분리하십시오.** 재전달 대상·event ID·operation version·소비자 범위·ACK/GC 조건을 명시해야 합니다. 상태 이벤트 재전달이 대상 SQL 재실행으로 연결되어서는 안 됩니다. 중복은 무효 처리하고 gap은 `operation.get`으로 복구하십시오. 스트림 ACK는 durable inbox receipt와 별개입니다. [spec 계약](../spec.md), [research 정확성 요구](../research.md)
- **I2 — 보존과 다중 실행자 소유권이 미정입니다.** 단일 저널 writer 또는 원자적 실행 claim, Journal Epoch, 공개된 retry horizon, 검증 가능한 키 만료, tombstone 보존을 정의하십시오. lease 만료로 dispatched write를 인계 실행하면 안 됩니다. 미해결 unknown과 미전달 이벤트를 일반 이력 정리로 삭제하지 않으며, 저널 복원·교체 후에는 재조정 전 쓰기를 차단해야 합니다. [RT-09](redteam-analysis.md)
- **I3 — 영속화 전에 민감정보 경계를 적용해야 합니다.** 사용자 DPAPI는 SQLite 전체 암호화를 뜻하지 않습니다. persisted field allowlist, canonical digest 규칙, SQLite bind 전 암호화, WAL/temp/log/support redaction, DPAPI 실패 시 write-disable을 명시하십시오. [RT-06](redteam-analysis.md)
- **I4 — 결과·취소·종료의 관찰 상태를 보존하십시오.** live page, snapshot, 결과 완전성, 백업 artifact 일관성은 별도 주장입니다. ACK는 실제 송신 ledger 안에서만 credit을 반환하고, 중복·미래·다른 세대 ACK는 자원을 늘리지 못해야 합니다. 종료·취소 요청만으로 rollback/완료를 표시하지 마십시오. [RT-05](redteam-analysis.md), [UX-03/05](ux-analysis.md)

반드시 입증할 항목은 다음과 같습니다.

- **엔진별 적합성:** exact typed roundtrip, 키 적격성, 동시 편집·no-op·batch conflict, snapshot 범위, MySQL/MariaDB row-count 설정, SQLite `BUSY_SNAPSHOT`, 실제 취소 대상 격리.
- **장애 경계:** 전송 전후·COMMIT 전후·원격 성공 후 로컬 기록 전 crash, 늦은 cancel, 두 인스턴스, journal 복원, 오래된 키 재사용에서 자동 재실행 0건과 정직한 결과 기록.
- **자원·복구:** slow/no-ACK·중복/gap 이벤트의 메모리 상한, disk-full/DPAPI 실패, 미완료 export 표시, 복원 identity 불일치 거부, 기존 백업 동작 보존. 현재 예산값은 측정치가 아닙니다. [spec T1–T10](../spec.md)

**Dictionary Updates**

아래는 [feature v1](../../../domain-dictionary-database-management.md)에 대한 제안이며, Evidence의 C/I 번호는 위 근거를 가리킵니다.

| Action | Term | Definition | Evidence |
|---|---|---|---|
| ADD | Workbench Capability Matrix | 엔진·서버·드라이버·설정별 타입·키·격리·편집·취소 인증과 검증 시각·증거. 백업 인증과 구분 | C1; architecture I3 |
| ADD | Workbench Access Lease | 호출 주체·대상·revision·만료·철회 의미에 결합한 서비스 발급 접근 검증 | C4; architecture C1 |
| REFINE | Connection Generation | 재접속·무효화 세대이며, 같은 연결 안의 실행·취소 경계는 Execution Epoch로 구분 | C2; v1:10 |
| ADD | Execution Epoch | 물리 연결의 실행 세대와 다음 실행 전 취소 정리 경계 | C2; RT-01 |
| ADD | Workbench Transaction | 한 대상 연결의 transaction identity·격리·상태·최종 결과. 재접속으로 재개 불가 | C3; architecture C3/C4 |
| ADD | Statement Outcome | script/transaction 내부 statement의 전송·완료 증거와 효과 상태 | C3/C5; RT-02 |
| REFINE | Outcome Unknown | 대상 효과가 미확정인 영속 상태. 동일 최종값·취소·운영자 수용은 commit 증거가 아님 | C3; v1:19 |
| REFINE | Verified Apply | 선언된 검사를 통과한 단일 대상 트랜잭션 편집. 미인증 trigger·외부 효과의 rollback 보장 제외 | C1/C5; v1:17 |
| REFINE | Operation Journal | 사용자 소유 요청·전송 의도·버전·결과·전달 증거의 권위 저장소. 원격 원자성 보장 없음 | C3/I3; v1:18 |
| ADD | Journal Epoch | 저널 생성·복원·교체를 구분하여 이전 실행권·키의 재사용을 제한하는 세대 | I2; RT-09 |
| ADD | Deduplication Retention | retry horizon·키 만료·tombstone·receipt 보존/정리 계약. 기존 백업 Retention과 구분 | I1/I2; 부모 사전:30 |
| REFINE | Credit Window | 송신 전 예약하고 유효한 ACK에 한 번만 반환하는 encoded-byte 한도. heap·제어 메시지 예산과 별개 | I4; v1:22 |

**NOT RUN:** 애플리케이션·실제 소스·엔진·드라이버 검사, 장애 주입, 성능·보안·복원·접근성 실행 검증. 파일 변경과 신규 웹 조사는 수행하지 않았습니다. 따라서 위 항목은 구현 결함의 실증이 아니라 계약 수정안과 검증 gate입니다.
