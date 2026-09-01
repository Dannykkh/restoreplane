# ADR-006: Secret Store Custody Across UI and Service

- 상태: 승인됨
- 날짜: 2026-07-26

## Context

Tauri는 interactive user로, `restoreplane-service`는 최소권한 service account로 실행된다. 사용자
범위 DPAPI secret은 service가 열 수 없고, 무제한 machine-scope secret은 같은 머신의 다른
주체에 과도하게 노출될 수 있다.

## Options

| 옵션 | 장점 | 단점 |
|------|------|------|
| 사용자 범위 DPAPI | 사용자 격리 | UI 종료 후 service가 secret을 열 수 없음 |
| machine-scope DPAPI만 사용 | 무인 실행 가능 | ACL이 약하면 머신 내 노출 범위가 큼 |
| service-owned machine DPAPI + 설치별 entropy/ACL | 무인 실행과 파일 격리 | installer와 recovery 절차가 복잡 |

## Decision

- service가 machine-scope DPAPI와 설치별 entropy/key file을 사용해 secret payload를 소유한다.
- key file, secret store, Operational Store, spool의 ACL을 service SID와 설치 시 등록한
  owner SID로 제한한다.
- UI/CLI는 authenticated IPC의 create/rotate command 동안만 평문 secret을 전달하고 service가
  즉시 암호화·zeroize한다.
- Operational Store에는 versioned `SecretRef`와 비민감 metadata만 저장한다.
- backup writer, restore reader, DB credential, signing key, KEK를 별도 purpose로 관리한다.
- 기본 export에는 secret이 없다. secret 포함 recovery bundle은 재인증, 별도 암호화,
  one-time recovery material을 요구한다.
- local administrator에 대한 비밀 보장은 하지 않는다고 위협 모델에 명시한다.

## Required Tests

- UI user, service SID, 다른 local user, administrator의 access matrix
- service restart와 Windows reboot 후 unattended decrypt
- secret rotation 중 queued Plan Revision의 reference 의미
- migration/rollback/recovery-code 경로
- canary secret이 DB, IPC log, process argv/env, Activity, support bundle에 0건
