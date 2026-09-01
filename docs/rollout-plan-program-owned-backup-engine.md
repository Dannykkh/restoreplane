# Program-owned backup engine rollout plan

## Release decision

코드와 자동 fixture가 통과해도 기존 설치를 자동으로 service authority로 전환하지 않는다.
서명된 배포 후보 package는 다음 package qualification 입력과 G0~G5 managed evidence가
준비될 때 만들 수 있다.

- owner가 별도 신뢰 채널로 승인한 RestorePlane Authenticode signer thumbprint
- PG18, MySQL8, MariaDB11 client와 dependency/license를 모두 포함한 사전 감사 native-tool tree
- 위 tree의 owner-approved `native-tools-audit.json` SHA-256
- 현재 source revision에 연결된 G0~G5 managed evidence

어느 하나라도 없으면 workflow가 package 생성을 거부한다. package가 생성돼도 owner가
rollout stop metric과 observation window를 승인하기 전에는 release 상태가 `parked`이며,
배포·promotion을 시작할 수 없다. 누락된 binary를 `PATH`나 임의 URL에서 보완하지 않는다.

## Baseline preservation

2026-08-24에 확인한 기존 설치는 `legacy_direct`/`rehearsal_only`였고 설치 root에 감사된
`tools/` bundle이 없었다. 이 문서는 그 설치를 변경하거나 production-ready로 간주하지 않는다.
rollout 시작 시 read-only `service status`와 `service diagnostics`를 다시 수집해 실제 상태를
기준으로 판단한다.

기존 Operational Store, legacy task, schedule, secret, artifact는 검증된 cutover 전까지 현재
authority에 남긴다. 새 standalone install만 pristine v4 Store와 attested package/binary에서
service authority를 bootstrap할 수 있다.

## Cohorts

| 단계 | 대상 | 진입 조건 | 성공 조건 | 중단 조건 |
|---|---|---|---|---|
| 0. Package qualification | 격리된 managed Windows runner | 외부 signer/audit 기준 준비 | package/manifest/native tree 검증, 설치 lifecycle, 전체 release gate 통과 | hash/signature/license/dependency 불일치 |
| 1. Clean canary | 비운영 새 Windows VM, 새 `DataRoot` | 단계 0 통과 | service authority, PostgreSQL/MySQL/MariaDB 선택 Scope backup·restore, scheduler restart, retention dry evidence 통과 | duplicate, missed occurrence, stale fence, secret canary, restore 실패 |
| 2. Existing-store rehearsal | 운영 복제본 또는 격리된 기존 Store 사본 | control backup과 recovery bundle 검증 | shadow diff, G1 cutover evidence, rollback disposition 확인; claims는 계속 닫힘 | ambiguous authority, schema/checksum drift, recovery bundle 실패 |
| 3. Controlled beta | owner가 지정한 소수 Workspace | 단계 1·2 및 stop metric 승인 | RPO, verification, destination replica, queue freshness, spool/permit, retention 상태가 승인 창 동안 정상 | 아래 safety invariant 또는 stop metric 위반 |
| 4. General availability | 승인된 cohort 확대 | beta evidence와 rollback rehearsal 승인 | release claims matrix의 모든 advertised capability가 managed evidence와 일치 | evidence 없는 promotion 또는 공급망 입력 만료 |

## Zero-tolerance safety invariants

다음 값의 허용치는 항상 0이다.

- 설명되지 않은 missed Plan Occurrence
- occurrence/Scope당 duplicate committed Recovery Point
- stale fencing token commit
- support bundle 또는 Activity의 secret canary 검출
- whole-database로 잘못 표시된 partial Scope
- signed Manifest/required replica 증거 없는 Recovery Point 승격
- Set Manifest는 남았지만 payload가 retention으로 삭제된 상태

하나라도 관찰되면 새 cohort와 provider promotion을 즉시 중단하고
[`runbooks/incident.md`](runbooks/incident.md)를 따른다.

## Preflight and execution

1. release source revision과 package build ID를 고정한다.
2. 전체 자동 gate, ignored-test inventory, managed evidence를 검토한다. `not_run`을 pass로 세지 않는다.
3. [`runbooks/install.md`](runbooks/install.md)의 out-of-band trust 입력으로 clean canary를 설치한다.
4. Console을 닫고 Windows를 재시작한 뒤 internal scheduler가 cursor를 이어가는지 확인한다.
5. whole-database와 explicit-table Scope를 각각 백업하고 create-new restore/content oracle로 검증한다.
6. 6시간 deadline, disk floor, permit, fast-restart reconciliation, mutation outcome-unknown,
   bounded inventory의 failure fixture를 release revision에서 재확인한다.
7. 기존 Store는 G0/G1 cutover 절차가 승인되기 전까지 rehearsal로 유지한다.
8. 각 cohort의 status, diagnostics, support bundle, evidence checksum을 rollout record에 연결한다.

## Observation and promotion

UI의 `unknown`, `partial`, `Attention`, `APPLICATION_OUTCOME_UNKNOWN`을 성공으로 치환하지 않는다.
pagination이 불완전하면 전역 health나 보호율을 계산하지 않는다. verification freshness는 동일한
passed evidence row가 요구 level과 시간창을 함께 만족해야 한다.

promotion은 owner가 stop metric과 observation window를 승인한 뒤 수동으로만 한다. 단순히
service가 `Running`이거나 작은 fixture가 통과했다는 이유로 cohort를 확대하지 않는다.

## Exact build evidence binding

서명 후보 package 생성은 managed G5 qualification이 통과한 뒤 허용한다. 이때 package가
새 build ID를 임의로 계산하지 않고 qualification의 `sourceRevision`,
`sourceTreeDigest`, `buildId`를 그대로 package manifest와 signing metadata에 기록한다.
서명 후보는 pristine lifecycle runner로 전달하기 위한 1일 staging artifact로만 먼저
업로드한다.

clean install, repair, 같은 package update, support bundle, uninstall preservation을 검증한
fixture도 동일한 세 필드와 signer thumbprint를 기록한다. 이후 별도 final reevaluation이
qualification, package manifest, signing metadata, fixture evidence를 다시 결합한다. 누락,
revision/tree/build 불일치는 안정된 reason code로 `status=fail`을 기록한다. 이 재평가가
완료되기 전에는 90일 보관 candidate artifact를 publish하지 않는다.

build ID 선언값만 같아서는 충분하지 않다. signing metadata와 fixture evidence는 실제
`package-manifest.json` SHA-256 및 manifest에 포함된 `restoreplane-service.exe`, `restoreplane.exe`
SHA-256도 기록한다. final reevaluation은 다운로드된 package의 정확한 파일 집합을 다시
열거하고 각 파일을 재해시한다. 따라서 같은 revision/tree/build ID를 복사한 다른 binary나
manifest 변조도 promotion evidence로 사용할 수 없다.

수명주기 smoke만으로 기능 qualification을 대체하지 않는다. 완성된 package의 manifest와
service/CLI digest에서 `artifactId`를 계산한 뒤 별도 ephemeral managed Windows runner가 그
exact package를 설치한다. 검증은 설치된 CLI의 제한된 `service qualification-request`와 실제
service worker만 사용해 PostgreSQL/MySQL/MariaDB의 `explicit_tables` backup, signed Manifest와
required reader, create-new restore/content oracle, L1 verification을 수행한다. cargo example,
직접 dump/restore, 다른 build의 evidence는 fail-closed다. final reevaluation은 이 functional
evidence의 `artifactId`와 세 package digest도 signing metadata 및 실제 package와 정확히 대조한다.

final evidence의 `promotionStatus`는 모든 signing 및 pristine install/update evidence가 같은
identity여도 owner stop metrics가 pending이면 `parked`다. `ready`는 이 동일 identity 계약과
owner 승인이 모두 통과한 경우에만 가능하다. 따라서 candidate package 생성·검증은 owner
승인 전에 진행할 수 있지만 deployment 및 cohort promotion은 계속 중단 상태로 남는다.

## Rollback readiness

rollout 전마다 control backup rehearsal, recovery bundle, previous signed package, minimum-reader
경계를 확인한다. 실제 복귀는
[`rollback-playbook-program-owned-backup-engine.md`](rollback-playbook-program-owned-backup-engine.md)의
결정표와 기존 [`runbooks/rollback.md`](runbooks/rollback.md)를 따른다.
