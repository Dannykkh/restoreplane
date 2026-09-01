# Team Review Summary

## Industry Context

- 산업군: 범용 데이터베이스 백업 운영 도구
- 도메인 복잡도: MEDIUM
- UX 전문가: 내부 독립 에이전트
- 아키텍처 전문가: 내부 독립 에이전트
- Red Team: 내부 독립 에이전트
- 도메인 프로세스 전문가: 외부 Codex CLI
- 도메인 기술 전문가: 외부 Codex CLI
- 도메인 리서치: 기존 Step 5 자료를 사용한 bounded synthesis

## Overall Verdict

제품 방향은 GO다. 단, 현재 사양 그대로 신규 UI나 엔진 구현을 시작하는 것은 NO-GO다.
Wave 0에서 실행 권한, 상태 진실, 중복 방지, 프로세스 수명주기, 마이그레이션 안전성을 먼저
확정하고 테스트로 증명해야 한다.

## Critical Findings

1. **단일 execution authority**
   - [Architecture/Red Team/Domain Technical] Tauri와 독립된 `dbbackup-service` Windows
     singleton을 유일한 실행권자로 둔다. UI, CLI, Task Scheduler는 enqueue/wake만 한다.

2. **권위 소스 분리**
   - [Architecture/Red Team/Domain Technical] Operational Store는 연결·계획·queue·Run·Activity의
     권위 소스, Secret Store는 비밀의 권위 소스, signed Manifest는 Recovery Point artifact의
     권위 소스다. Catalog projection만 Manifest에서 rebuild할 수 있다.

3. **at-least-once + fenced commit**
   - [Architecture/Red Team/Domain Technical] exactly-once 실행을 약속하지 않는다. Plan
     Occurrence idempotency, Attempt, Lease Heartbeat, Fencing Token, conditional commit,
     Reconciliation으로 occurrence당 complete Recovery Point 하나를 보장한다.

4. **실행 cardinality와 복구 원자성**
   - [Architecture/Red Team/Domain Process] `Backup Plan Revision → Plan Occurrence →
     Backup Run(one Target) → Recovery Point → Artifact Part → Artifact Replica`로 분리한다.
     PostgreSQL globals 등 required part는 최종 set manifest 전에 Snapshot으로 노출하지 않는다.

5. **목적지 내구성 정책**
   - [Red Team/Architecture/UX] Destination membership은 required/best-effort 역할을 갖고 기본은
     `all-required`다. 실행 성공과 Protection Compliance를 분리한다.

6. **Process Supervisor와 spool 안전성**
   - [Architecture/Red Team/Domain Technical] Windows Job Object, process tree kill, pipe drain,
     timeout, wait/reap, orphan cleanup을 공통 supervisor가 소유한다. dump 전 볼륨별
     Spool Reservation을 획득한다.

7. **TOML→SQLite cutover**
   - [Architecture/Red Team] shadow import, 참조 검증, recovery bundle, scheduler freeze,
     cutover marker, 구 binary guard, rollback rehearsal이 필요하다.

8. **복원 경계**
   - [UX/Red Team/Domain Process] Snapshot ID 수동 입력을 제거한다. 서비스가 Restore
     Eligibility와 confirmation token을 강제하고 새 DB를 기본 대상으로 한다.
     restore-as-exec는 격리·최소권한·object inventory를 요구한다.

9. **Support Contract와 false-green 제거**
   - [모든 리뷰] declared, observed, certified capability의 교집합만 “지원”으로 표시한다.
     ignored live test는 pass가 아니다. SQL Server DACPAC은 schema-only이며 데이터 백업
     후보는 BACPAC을 별도 검증한다.

10. **제품 정보구조와 영속 Activity**
    - [UX/Domain Process] Connections, Databases, Plans, Activity, Snapshots, Recovery를
      독립 수명주기로 제공하고 모든 긴 작업을 persistent Work Item으로 보낸다.

11. **접근성**
    - [UX] 모든 form label/name/error 관계, keyboard journey, live region, table semantics를
      release gate로 둔다. 현재 UI의 label 누락은 Wave 0~UI foundation에서 먼저 수정한다.

12. **control-plane recovery**
    - [Architecture/Red Team] Manifest rebuild는 Plan과 queue를 복구하지 못한다. Operational
      Store의 암호화 export, integrity check, DR drill이 필요하다.

## Important Findings

- Queue 상태 전이를 CAS와 허용 전이표로 고정하고 `cancel_requested`와 terminal `cancelled`를
  구분한다.
- Resource Permit을 일부 잡은 채 기다리지 않고 원자 admission, fixed ordering, aging을 사용한다.
- Plan 수정은 Plan Revision을 만들고 queued occurrence는 immutable execution spec을 참조한다.
- phase checkpoint와 replica retry로 목적지 장애 때 DB dump를 반복하지 않는다.
- declared/observed/certified Capability와 probe freshness를 분리한다.
- schedule은 UTC occurrence ledger와 timezone, DST, sleep/resume, bounded catch-up 정책을 갖는다.
- retry에는 jitter, circuit breaker, budget, deadline을 적용한다.
- 엔진별 Consistency Grade를 기록하고 공통 “백업 성공”으로 의미 차이를 숨기지 않는다.
- Activity와 debug log를 분리하고 control transaction이 로그 폭주에 굶지 않게 한다.
- signing key rotation, native tool identity, version/hash/signature를 evidence에 포함한다.
- notification routing과 전송 실패도 Activity와 Attention State로 관리한다.
- 100,000 Activity Event의 pagination, 검색 index, query performance를 검증한다.
- control DB, spool, destination의 용량·보존·orphan artifact 운영 절차를 둔다.

## Nice-to-Have

- Plan Template, saved view, 내부 deep link
- queue simulation과 예상 백업 창·용량·비용
- adaptive concurrency, bandwidth cap, 시간대별 resource profile
- signed audit bundle
- command palette와 복원 준비 점검
- 향후 Agent/API 화면

## Dismissed or Deferred

- **Databasement 포크**: 구현 런타임 이중화와 Windows-first 목표 훼손 때문에 채택하지 않는다.
- **초기 8개 엔진 동시 구현**: Wave 0~기존 3개 엔진 gate 전에는 시작하지 않는다.
- **distributed queue/consensus**: single Windows service와 local SQLite에 과도하다.
- **dynamic plugin ABI**: 정적 adapter registry가 현재 범위에 더 안전하다.
- **초기 HTTP API/RBAC/SSO/MCP**: application contract만 보존하고 후속 wave로 미룬다.
- **Redis/Valkey “지원” 표기**: restore runbook과 testbed 전에는 experimental export로만 둔다.
- **SQL Server DACPAC 데이터 백업**: schema-only capability로 분리한다.

## Domain Decisions

- accepted-by-default: UI와 독립된 Windows service singleton을 execution authority로 채택
- accepted-by-default: at-least-once attempt + fenced idempotent commit 채택
- accepted-by-default: Backup Run은 Database Target 하나만 담당
- accepted-by-default: required artifact part를 묶은 Recovery Point를 복원 원자 단위로 채택
- accepted-by-default: Destination 기본 성공 정책은 `all-required`
- accepted-by-default: 새 DB 복원을 기본으로 하고 overwrite는 별도 고위험 승인
- accepted-by-default: SQL Server Wave 1은 BACPAC 후보 검증, DACPAC은 schema-only
- accepted-by-default: SQLite one-way cutover + TOML export + 구 binary guard
- deferred-by-default: signed hash-chain audit ledger는 기본 occurrence ledger 이후 필요성 재평가
- deferred-by-default: Agent, API, RBAC, SSO, MCP는 single-node service 안정화 이후

## Dictionary Changes

- accepted-by-default: `Plan Occurrence`를 `Scheduled Occurrence`의 규범 용어로 사용
- accepted-by-default: `Attempt`, `Fencing Token`, `Process Supervisor`, `Spool Reservation`,
  `Reconciliation` 추가
- accepted-by-default: `Recovery Point`를 required part의 원자적 복원 단위로 추가
- accepted-by-default: `Artifact Part`, `Artifact Replica`, `Required Destination` 추가
- accepted-by-default: `Operational Store`, `Protection Compliance`, `Consistency Grade`,
  `Engine Matrix`, `Probe Result`, `Attention State` 추가
- inferred: `Snapshot`은 committed Recovery Point의 UI/검색 projection으로 정의
- inferred: `Artifact Replica`를 `Destination Copy`보다 우선
- unresolved-conflict: 없음

## Global Dictionary Sync

- inferred-skip: 신규 용어는 백업 orchestration bounded context에 특화돼 글로벌 범용 사전에
  자동 반영하지 않는다.

## Impact on Plan

최종 `plan.md`의 phase 번호가 이 리뷰 초안보다 세분화되었으며 아래 매핑이 정본이다.

- Phase 0은 UI나 신규 엔진이 아니라 진실성 감사와 execution authority ADR로 시작한다.
- Phase 1에서 Operational Store schema와 cutover rehearsal을 완성한다.
- Phase 2는 service/IPC/Secret Store shell만 만들고 production 실행권은 넘기지 않는다.
- Phase 3에서 fenced queue, Process Supervisor, Resource Permit, Reconciliation을 통과한 뒤
  실제 cutover한다.
- Phase 4에서 기존 세 엔진의 full Support Contract와 managed restore를 통과시킨다.
- Phase 5에서 Databasement 수준의 제품 UI를 application service에 연결한다.
- Phase 6부터 SQL Server BACPAC 후보와 SQLite를 각각 독립적으로 출시한다.
- Agent/API와 나머지 엔진은 Phase 7 확장 wave로 둔다.
