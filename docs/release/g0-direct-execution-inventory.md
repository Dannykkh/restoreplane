# G0 Direct Execution Inventory

G1 durable gate 전 production execution authority는 `legacy_direct`다. 아래 경로를 service
실행으로 오인하거나 동시에 enable하지 않는다.

| 시작 주체 | 코드 위치 | Process owner | Singleton/overlap | Cancellation | Secret 전달 | stdout/stderr | Rollback |
|---|---|---|---|---|---|---|---|
| Tauri run | `apps/console/src-tauri/src/run.rs::spawn_stream` | Tauri sidecar child | 앱 프로세스 범위, catalog job guard에 의존 | Tauri process 종료 또는 child 종료 | 환경/키스토어 unlock 경계 | Tauri event stream | sidecar action 중단, 기존 CLI 유지 |
| CLI manual | `crates/restoreplane-cli/src/commands/run.rs::run` | 호출한 `restoreplane.exe` | job별 `RunGuard`, 전체 singleton 아님 | 호출 process 종료 | env, prompt 또는 DPAPI | 현재 console | 같은 binary에서 재실행 |
| CLI worker | `crates/restoreplane-cli/src/commands/run.rs::execute_job` | 호출한 CLI process | job별 overlap만 방지 | process tree 전체 보장 없음 | 해제한 secret을 메모리에서 adapter에 전달 | pipeline가 tail을 수집 | spool/manifest commit 경계에서 재실행 |
| Task Scheduler | `crates/restoreplane-cli/src/commands/schedule.rs` | job별 Windows task | task/job 단위, service singleton 아님 | schtasks task 제어 | DPAPI machine-scope 경로 | Task Scheduler 실행 context | task disable/remove 후 legacy manual 실행 |
| Task XML | `crates/restoreplane-scheduler/src/xml.rs::render_task_xml` | `restoreplane run --job ... --scheduled` | task 설정에 의존 | Windows Task Scheduler | config path와 DPAPI 경로 | 등록된 task 실행 로그 | 기존 XML 재등록 |
| Verify/restore | `apps/console/src-tauri/src/run.rs`, `crates/restoreplane-cli/src/commands/verify.rs`, `crates/restoreplane-cli/src/commands/restore.rs` | Tauri sidecar 또는 CLI process | durable Work Item ledger 없음 | 호출 process/child 종료 | env, prompt, recovery code 또는 DPAPI | console/Tauri event stream | source artifact를 변경하지 않고 재실행 |

새 process 시작점이 생기면 같은 변경에서 이 표와 `g0-claims.json`을 갱신한다.
