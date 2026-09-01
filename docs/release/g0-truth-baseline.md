# G0 Truth Baseline

`release/gates/g0-claims.json`이 capability 상태의 machine-readable 정본이다. 이 문서는
운영자가 읽기 위한 projection이며 JSON보다 강한 보장을 주장하지 않는다.

## Runtime Boundary

- Execution authority: `legacy_direct`
- Cutover mode: `rehearsal_only`
- Durable dispatcher: 구현·managed_pass, production authority 전환 전
- L2/L3: service-owned verification/restore Operation Run에서 지원
- Restore behavior: signed reader, create-new 기본값, native restore/content oracle
- Release promotion: G5 자동 gate와 별개로 signing/clean fixture/owner metric 전에는 parked

## Capability Projection

| Capability | Claim | Automated evidence | Managed evidence |
|---|---|---|---|
| connection.probe | supported | automated_pass | not_applicable |
| database.discovery_ui | supported | automated_pass | not_applicable |
| backup.postgresql | supported | automated_pass | managed_pass |
| backup.mysql | supported | automated_pass | managed_pass |
| backup.mariadb | supported | automated_pass | managed_pass |
| destination.local | supported | automated_pass | managed_pass |
| destination.s3 | experimental | automated_pass | not_run |
| destination.ssh | experimental | automated_pass | not_run |
| verification.l1 | supported | automated_pass | not_applicable |
| verification.l2 | supported | automated_pass | managed_pass |
| verification.l3 | supported | automated_pass | managed_pass |
| verification.automatic | supported | automated_pass | not_applicable |
| restore.decrypt_only | supported | automated_pass | managed_pass |
| restore.database_target | supported | automated_pass | managed_pass |
| retention.policy | supported | automated_pass | not_applicable |
| retention.execution | supported | automated_pass | not_applicable |
| scheduler.legacy_task | supported | automated_pass | managed_pass |
| dispatcher.durable | supported | automated_pass | managed_pass |
| concurrency.max_parallel_dumps | supported | automated_pass | managed_pass |
| dashboard.protection_compliance | supported | automated_pass | not_applicable |
| notification.durable_delivery | supported | automated_pass | not_applicable |
| tauri.release_bundle | experimental | known_gap | not_run |
| service.diagnostics | supported | automated_pass | not_applicable |
| support.bundle | supported | automated_pass | not_applicable |
| telemetry.archive | supported | automated_pass | not_applicable |
| windows.install_update | experimental | automated_pass | not_run |
| engine.sqlserver | experimental | automated_pass | not_run |
| engine.sqlite | supported | automated_pass | managed_pass |

`not_run`은 실패나 성공이 아니라 현재 revision에서 managed fixture가 실행되지 않았다는 뜻이다.
ignored test는 자동 pass 집계에 포함하지 않는다.
