# Connected Backup Platform v2 Traceability

이 표는 요구사항이 구현 섹션, 흐름, 운영 시나리오, QA, 출시 gate 중 하나에도 연결되지 않는
누락을 방지한다. 범위 변경 시 같은 변경에서 이 표를 갱신한다.

## Functional Requirements

| Spec Group | Primary Sections | Flow | Operation | QA | Exit Gate |
|---|---|---|---|---|---|
| FR-CON Connections | 02, 03, 07 | F01 | OP-M02, OP-E01 | QA-M02, QA-E01 | G0-B, G3 |
| FR-DIS Discovery | 02, 07 | F01 | OP-M02, OP-M03, OP-E07 | QA-M02-04, QA-M03, QA-E07 | G0-B, G3 |
| FR-DST Destinations | 02, 05, 07 | F01/F03 | OP-M04, OP-E03 | QA-M04, QA-M07-03/05, QA-E03 | G1, G2 |
| FR-PLN Plans | 02, 04, 07 | F02 | OP-M05, OP-E01/E02 | QA-M05, QA-E01/E02 | G0-B, G1 |
| FR-DSP Queue/Dispatcher | 04 | F02/F04 | OP-M06, OP-E02/E04 | QA-M06, QA-E02/E04, QA-N01~05 | G1 |
| FR-SVC Service/IPC | 03, 04 | F04/F06 | OP-M09, OP-E09/E14 | QA-I01~10, QA-E09/E14 | G1, Launch |
| FR-RUN Backup Execution | 04, 05, 06 | F02/F03 | OP-E01~04 | QA-E01~04, QA-N02~05 | G1, G2 |
| FR-SNP Snapshots/Activity | 05, 07, 08 | F03/F04/F05 | OP-M06/M07 | QA-M06/M07 | G2, G3 |
| FR-VRF Verification | 05, 06, 08 | F03/F05 | OP-M07/M08, OP-E05 | QA-M07/M08, QA-E05 | G2, G3 |
| FR-RST Restore | 03, 06, 08 | F05 | OP-M08, OP-E05/E06 | QA-M08, QA-E05/E06 | G2, G3 |
| FR-NOT Notifications | 04, 06, 07 | F02/F04 | OP-M06/M09, OP-E08 | QA-M06-05, QA-M09-03, QA-E08 | G2, G3 |
| FR-MIG Config/Cutover | 01, 02, 03, 04 | F06 | OP-M09, OP-E09/E10 | QA-M09-04~06, QA-E09/E10 | G0, G1 |
| FR-ENG Engine Expansion | 06, 09, 10 | provider contract | OP-E11/E12 | QA-E11/E12 | G2, G4, Launch |

## Non-functional Requirements

| NFR | Primary Sections | QA Evidence |
|---|---|---|
| Reliability/idempotency/fencing | 02, 04, 05 | QA-M05-05, QA-M06-05, QA-E02/E04, QA-N02 |
| Performance/backpressure | 04, 05, 07 | QA-M06-06, QA-E02, QA-N01/N08/N09 |
| Security/secret/tool trust | 01, 03, 05, 06 | QA-M04-05, QA-M07-04, QA-M08-03/04, QA-I03/05/07/08, QA-N06/07 |
| Accessibility/UI truth | 01, 07, 08 | QA-M01-04, QA-M02-05, QA-N10~12 |
| Operability/control backup | 02, 03, 04, 10 | QA-M09-04/05, QA-E09/E10 |
| Compatibility/Engine Matrix | 06, 09 | QA-M03-02, QA-E11/E12 |

## ADR Enforcement

| ADR | Enforced By | Primary QA |
|---|---|---|
| ADR-001 Execution Authority | 01, 03, 04 | QA-E09, QA-M09-06 |
| ADR-002 Authority Boundary | 02, 05 | QA-M07-06, QA-E10 |
| ADR-003 Fenced Delivery | 02, 04 | QA-M06-05, QA-E02/E04 |
| ADR-004 Recovery Point/Replicas | 05, 08 | QA-M07-01~05, QA-E03 |
| ADR-005 Config Cutover | 01, 02, 03 | QA-E09 |
| ADR-006 Secret Custody | 02, 03 | QA-M02-02, QA-M04-05, QA-N06 |
| ADR-007 Local IPC Authorization | 02, 03 | QA-I01~10, QA-E06/E14 |
| ADR-008 Offline Cutover Recovery | 01, 03 | rollback fault matrix, `rollback.md` |
| ADR-009 Install/Data Root Separation | 03, 10 | `scripts/tests/install-lifecycle.Tests.ps1` |
| ADR-010 Manifest-last Commit Recovery | 04, 05 | S05-X01, startup recovery unit/integration tests |

## Explicitly Deferred

| Scope | Reason | Re-entry Gate |
|---|---|---|
| Public HTTP API/RBAC | local contract와 위협 모델이 먼저 안정돼야 함 | section-10 별도 ADR |
| Remote Agent | identity, offline queue, upgrade, split-brain 계약 필요 | section-10 별도 protocol |
| Redis/Valkey supported 등급 | export보다 restore runbook/testbed가 선행 | Engine Matrix restore evidence |
| 백업 내용 브라우징 | 민감 데이터와 대용량 query 위협 모델 미정 | 별도 product/security review |
