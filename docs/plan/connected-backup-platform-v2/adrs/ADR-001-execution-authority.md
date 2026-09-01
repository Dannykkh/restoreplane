# ADR-001: Windows Service Process as the Single Execution Authority

- 상태: 승인됨
- **날짜**: 2026-07-26
- **결정자**: RestorePlane project owner
- **태그**: windows-service, scheduler, operational-store, execution-authority

## Context

Tauri, CLI, Windows Task Scheduler가 각각 native backup process를 시작하면 전역 concurrency,
취소, 재시작 복구, 중복 방지를 시스템 전체에서 보장할 수 없다. UI가 닫히거나 Windows가
재시작된 뒤에도 예약 백업은 계속돼야 한다. 동시에 SCM의 service definition을 Plan 설정
저장소로 사용하면 프로그램 데이터와 OS process lifecycle이 결합된다.

## Decision Drivers

- UI process 수명과 무인 백업 실행을 분리한다.
- schedule, queue, retention, fencing, resource permit을 하나의 권위 경계에서 처리한다.
- Plan별 OS task/service 등록과 다중 native process 진입점을 제거한다.
- crash 뒤 Operational Store와 signed artifact evidence로 실행을 재조정할 수 있어야 한다.

## Considered Options

| 옵션 | 장점 | 단점 |
|---|---|---|
| Tauri 내부 dispatcher | UI 통합이 단순하다 | UI 종료·로그아웃 시 예약 실행이 중단되고 전역 singleton을 보장하기 어렵다 |
| Plan마다 Windows Task Scheduler 또는 drain CLI 기동 | 기존 scheduler를 재사용할 수 있다 | Plan별 OS 상태, 잦은 process startup, overlap과 resource governor 경합이 생긴다 |
| singleton Windows service process 안의 프로그램 소유 engine | 무인 실행과 단일 권위, durable queue를 함께 제공한다 | installer, IPC, ACL, update/drain 설계가 필요하다 |

## Decision

singleton `restoreplane-service` process를 유일한 production execution authority로 사용한다.
Windows Service Control Manager는 그 process의 시작·중지·재시작 lifecycle만 소유하고 Plan
configuration이나 Plan별 task를 소유하지 않는다.

service process 안의 internal durable scheduler가 Operational Store에 저장된 published Plan,
schedule, cursor를 평가해 Plan Occurrence와 Work Item을 만든다. queue worker와 retention
runtime도 같은 process와 Store 경계를 사용한다. Tauri와 CLI는 authenticated local IPC로
command를 제출하며 native backup/restore를 직접 시작하지 않는다. Windows Task Scheduler와
direct legacy CLI는 검증된 migration/recovery compatibility 경로에만 남긴다.

## Consequences

### Positive

- Console 종료와 무관하게 일정, queue, retention이 지속된다.
- global/source/destination permit과 fencing을 모든 Workspace에 일관되게 적용할 수 있다.
- Plan configuration과 실행 이력이 Operational Store의 control backup 경계에 남는다.

### Negative

- signed installer, 최소권한 service SID, local IPC, drain/update 절차가 필수다.
- service process 장애는 모든 Workspace의 새 claim 시작에 영향을 준다.

### Risks

- legacy Task와 service가 동시에 활성화되면 duplicate execution이 생길 수 있다. Cutover marker와
  execution-authority gate가 이를 차단한다.
- service definition을 scheduler로 오해할 수 있다. 제품 문서와 diagnostics는 SCM lifecycle과
  program-owned schedule ownership을 구분한다.

## Related

- [ADR-002: Authoritative Data Ownership](ADR-002-authoritative-data.md)
- [ADR-009: Separate Install and Durable Data Roots](ADR-009-separate-install-and-data-roots.md)
- [Program-owned backup engine](../../program-owned-backup-engine.md)
- [Windows install runbook](../../../runbooks/install.md)
