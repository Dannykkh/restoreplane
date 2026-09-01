# ADR-009: Separate Install and Durable Data Roots

- **상태**: accepted
- **날짜**: 2026-08-24
- **결정자**: DbBackup project owner
- **태그**: windows-service, installer, data-root, repair, uninstall

## Context

DbBackup은 signed binary와 bundled native tools를 배치하면서 Operational Store, secret, spool,
control backup과 recovery material을 장기간 보존해야 한다. binary 손실이나 repair/update가
프로그램 데이터를 함께 삭제하면 일정과 복구 근거를 잃는다. 반대로 mutable data를 executable
directory에 두면 service account ACL과 binary read/execute ACL도 분리할 수 없다.

## Decision Drivers

- binary 교체·손실과 durable data 손실의 blast radius를 분리한다.
- service `binPath`만으로 실행 파일과 데이터 위치를 재현할 수 있게 한다.
- normal repair, uninstall, same-or-newer reinstall에서 Store와 recovery material을 보존한다.
- recursive file operation이 다른 lifecycle의 root를 침범하지 않게 한다.

## Considered Options

| 옵션 | 장점 | 단점 |
|---|---|---|
| binary와 모든 data를 한 root에 배치 | 설치 인자가 적고 개발 환경이 단순하다 | update/uninstall/삭제 실수가 Store·secret·spool까지 확산된다 |
| InstallRoot와 DataRoot를 나누되 동일 경로나 중첩을 허용 | 일부 기본 경로를 유연하게 구성할 수 있다 | parent root의 recursive operation과 ACL 변경이 다른 경계를 침범한다 |
| 서로 다른 non-nested absolute roots와 explicit `--data-root` | lifecycle, ACL, recovery 경계가 명확하다 | custom path를 모든 install/repair/update/uninstall 호출에서 일관되게 유지해야 한다 |

## Decision

`InstallRoot`와 `DataRoot`를 서로 다른 non-nested absolute path로 사용한다.

- `InstallRoot`는 running service executable의 parent이며 CLI와 bundled tools를 포함한다.
- `DataRoot`는 Operational Store, service identity/secrets, spool, control backup, support bundle,
  telemetry archive, update journal을 포함한다.
- service definition은 quoted `dbbackup-service.exe --data-root "<DataRoot>"`를 저장한다.
- `--data-root`를 사용하면 정확히 한 번의 absolute path만 허용한다. 잘못된 추가 인자, 상대
  경로, 중복 인자, 같거나 중첩된 두 roots는 fail closed한다. 인자가 없는 legacy service
  definition은 `%ProgramData%\DbBackup`으로 제한해 호환하지만 새 lifecycle script는 항상
  explicit argument를 기록한다.
- repair는 기존 service entry를 대상으로 binary/service definition/ACL을 복구하고 DataRoot를
  보존한다.
- normal uninstall은 claim drain 뒤 service entry만 제거하며 InstallRoot와 DataRoot를 삭제하지
  않는다.
- uninstall 뒤 재등록은 동일하거나 더 새 signed package의 install을 기존 DataRoot에 연결한다.
  프로그램 파일만 유실되고 service entry가 남은 경우에는 repair를 사용한다.

## Consequences

### Positive

- 프로그램 파일이 손상돼도 schedule, queue, retention, secret과 recovery evidence가 DataRoot에 남는다.
- InstallRoot에는 read/execute 중심 ACL, DataRoot에는 service/owner 전용 mutable ACL을 적용할 수 있다.
- uninstall을 데이터 폐기와 명확히 분리할 수 있다.

### Negative

- custom roots를 사용한 운영자는 모든 lifecycle command에 같은 두 경로를 전달해야 한다.
- service entry가 없는 상태와 binary만 없는 상태의 복구 명령이 각각 install과 repair로 다르다.

### Risks

- 잘못된 custom DataRoot로 reinstall하면 빈 Store처럼 보이거나 authority가 달라질 수 있다. 시작
  후 service definition, execution authority, claims 상태를 검사한다.
- DataRoot 손실을 binary repair로 복구할 수 있다는 오해가 생길 수 있다. DataRoot 복구는
  검증된 control backup과 recovery material을 사용하는 별도 재해 복구로 취급한다.

## Related

- [ADR-001: Windows Service Process as the Single Execution Authority](ADR-001-execution-authority.md)
- [ADR-002: Authoritative Data Ownership](ADR-002-authoritative-data.md)
- [ADR-006: Secret Store Custody Across UI and Service](ADR-006-secret-custody.md)
- [Windows install, repair, and uninstall runbook](../../../runbooks/install.md)
- [Program-owned backup engine](../../program-owned-backup-engine.md)
