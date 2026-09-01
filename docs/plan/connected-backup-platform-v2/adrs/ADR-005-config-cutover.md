# ADR-005: One-way Operational Store Cutover

- 상태: 승인됨
- 날짜: 2026-07-26

## Context

기존 TOML과 job별 Scheduler에서 Operational Store와 service로 옮기는 동안 두 실행 경로가
동시에 살아 있으면 중복 백업과 상태 분열이 발생한다.

## Decision

1. 기존 TOML, keystore, Catalog, Scheduler XML을 recovery bundle로 저장한다.
2. 새 SQLite에 shadow import하고 stable ID, Secret Ref, 관계, schedule diff를 검증한다.
3. 구 Scheduler task를 정지한다.
4. service health와 imported plan을 dry-run한다.
5. 단일 cutover marker를 기록한다.
6. marker 이후 구 binary와 직접 run 경로는 실행을 거부한다.
7. TOML은 versioned export/import 표면으로만 유지한다.

## Rollback

cutover 완료 전에는 recovery bundle과 scheduler 상태로 되돌릴 수 있다. cutover 이후 새
데이터가 생성되면 구 binary로 downgrade하지 않고 새 export/import 또는 forward fix를 사용한다.

## Consequences

완전한 양방향 dual-write를 피한다. migration은 schema version별 멱등 step과 checksum을 갖는다.
