# ADR-007: Local IPC Authentication and Authorization

- 상태: 승인됨
- 날짜: 2026-07-26

## Context

Windows service는 backup, delete, secret, restore 같은 고위험 command를 수행한다. “로컬
사용자”라는 사실만으로 권한을 부여하면 multi-user PC나 권한 상승 상황에서 위험하다.

## Decision

- local named pipe를 사용하고 명시적 security descriptor로 service SID, 설치 owner SID,
  승인된 administrator만 연결을 허용한다.
- service는 named-pipe client token을 impersonate해 실제 SID와 session을 확인한다.
- command authorization을 세 등급으로 나눈다.
  - read: 상태·inventory
  - operate: enqueue, cancel, probe
  - destructive/admin: overwrite restore, retention delete, secret rotate, migration
- destructive/admin command는 owner/admin SID와 service가 발급한 짧은 confirmation token을
  모두 요구한다.
- request에는 protocol version, request ID, idempotency key, issued/expiry 시각, body length를
  넣고 replay와 oversize를 거부한다.
- mutating command의 SID, command, idempotency key, canonical payload digest와 redacted
  결과 참조를 Operational Store에 지속한다. service restart 뒤 같은 key·같은 payload는 기존
  결과를 반환하고 같은 key·다른 payload는 거부한다.
- domain mutation이 commit된 뒤 ledger 결과를 기록하기 전에 service가 중단되면 handler를
  다시 실행하지 않는다. 동일 key·payload의 만료된 replay는 durable
  `APPLICATION_OUTCOME_UNKNOWN` terminal 결과로 닫고, operator가 inventory와 Activity를
  확인하게 한다. 이는 결과 재생 계약의 의도적인 fail-closed 예외다.
- confirmation token은 SID, action, resource ID/fingerprint, canonical payload digest,
  expiry에 결합한다. raw token은 저장하지 않고 digest만 보존하며 Run/Work Item 생성과 같은
  transaction에서 한 번만 소비한다.
- 초기 v2는 설치 owner 한 명의 local control plane만 지원한다. 조직 RBAC 의미를 IPC에
  미리 일반화하지 않는다.
- 거부된 command도 secret 없이 Activity Event에 기록한다.

## Required Tests

- unauthorized SID, 다른 session, stale/replayed request 거부
- malformed length, oversized payload, unknown version, fuzz input
- 같은 idempotency key의 중복 enqueue가 하나의 Plan Occurrence만 생성
- 같은 idempotency key의 다른 payload 충돌과 service restart 후 결과 재사용
- mutation commit 뒤 ledger completion crash에서 handler 재실행 0회와
  `APPLICATION_OUTCOME_UNKNOWN` replay
- confirmation token이 다른 Snapshot/target/overwrite policy에 재사용되지 않음
- consumed/expired/revoked token의 상태 역전과 재사용이 DB trigger/CAS에서 거부됨
- pipe disconnect 중 partial command와 secret buffer cleanup
