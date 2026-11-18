# 보안 정책 및 리포지토리 위생

이 문서는 팀 리뷰(Red Team)의 지적 C1~C4, H3~H6, M7을 운영 규칙으로 고정한다.

## 절대 커밋 금지 (공개 리포 정책)

다음은 로컬 + 오프라인에만 존재해야 하며 리포에 절대 올리지 않는다. `.gitignore`가 1차 차단하고,
CI의 시크릿 스캐너(gitleaks)가 2차 차단한다.

- `restoreplane.toml`, `prune.toml` — 실제 설정(호스트/유저/버킷/스케줄 = 정찰 지도, H3)
- `keystore.json`, `*.keystore.json` — KEK wrap + 서명 개인키(KEK 래핑됨이라도 미커밋, H4)
- `dpapi.keyfile`, `*.keyfile`, `*.dek` — DPAPI entropy/키 재료(C1)
- `*.recovery-code` — 복구 코드(C4: 오프라인 브루트포스 표적)
- `catalog.sqlite*` — 로컬 카탈로그 캐시
- `spool/`, `.staging/` — 평문/암호문 스풀

> **왜 공개 리포인가**: "도구를 오픈소스로 공개"와 "내 접속정보·키를 리포에 올림"은 별개다.
> 코드만 공개하고 개인 자산은 로컬에 둔다. 암호화된 wrap을 공개 리포에 올리면 오프라인
> 무제한 비번 브루트포스가 가능해진다(C4) — 그래서 wrap조차 커밋하지 않는다.

## 사용자 소유 Database Workbench

개발 중인 Workbench의 일반 SQL과 interactive credentials는 현재 사용자 런타임에서 처리한다. privileged backup service는 닫힌 대상 바인딩/메타데이터 및 기존 백업·복원 명령을 담당한다. 관리자 권한 Workbench는 사용자 파일/DPAPI 초기화 전에 거부하며, 사용자 자격증명과 명시적 저장 쿼리는 CurrentUser DPAPI로 보호한다.

실행 저널에는 SQL·매개변수·행 값 대신 범위·digest·상태/version·확인 근거를 기록한다. 확인되지 않은 효과는 자동 replay하지 않는다. 중지/연결 종료/운영자 확인은 rollback 증거가 아니며, arbitrary SQL의 target exactly-once·XA/2PC·외부 함수 효과의 롤백을 보장하지 않는다. 부분 내보내기 파일에는 사용자 데이터가 들어가므로 위치와 정리 선택을 표시한다.

별도 dirty/clean anchor와 잠금은 DB만 삭제/되돌린 경우를 감지하지만, 악의적인 같은 owner가 DB와 anchor 전체를 함께 바꾸는 위협까지 막지 않는다. 지원 범위와 실제 검증은 [사용 안내](docs/runbooks/workbench.md)와 [검증 장부](docs/plan/database-management-workspace/checklist-status.md)에 따른다. 이 CurrentUser 경계와 아래 무인 백업용 Machine-scope 경계는 별개다.

## 암호화 신뢰 근원

- 마스터 비밀번호 → Argon2id → KEK. 백업별 랜덤 DEK를 KEK로 wrap.
- 3경로 unwrap: 비밀번호(이식성) / 복구 코드(비번 분실 복구) / DPAPI Machine-scope(무인).
- **비밀번호 + 복구 코드를 동시에 잃으면 과거 백업은 영구 복호 불가**. 복구 코드는 init 시 1회만
  표시되며 오프라인 보관이 필수다.
- 매니페스트는 오프라인 ed25519로 서명한다. 복원/검증은 서명 검증을 통과한 백업만 처리한다(C3).

## 랜섬웨어 방어 (강제)

- 백업 목적지 자격증명은 **append-only**(PutObject만, Delete*/lifecycle/retention/bypass 명시 Deny).
  `restoreplane-dest-s3`의 `APPEND_ONLY_IAM_POLICY` 참조.
- **prune는 백업 호스트에서 실행 금지** — 별도 격리 신원(`prune.toml` 프로파일). 무인 `run`은 이 프로파일을 로드하지 않는다(H1).
- 매 `run` 시 목적지 자가테스트: Delete/PutLifecycle/PutObjectRetention이 실제로 거부되는지,
  versioning ON + 기본 보존이 걸려있는지 확인. 과권한/미잠금이면 거부/경고(H2).
- service restore Operation Run은 서명/manifest/Replica reader를 확인한 뒤 create-new
  기본값으로 native restore와 content oracle을 실행한다. overwrite는 별도 confirmation
  token이 필요하다. legacy direct command와 service authority를 동시에 production
  enable하지 않는다(C3).

## 공급망 (H6)

- `Cargo.lock` 커밋(바이너리 프로젝트). 보안 크리티컬 크레이트는 `=x.y.z`로 정확히 핀.
- CI에서 `cargo deny check` + `cargo audit`로 High/Critical 권고·yanked·라이선스 위반 시 빌드 실패.
- release workflow action은 검토한 commit SHA에 고정한다. Rust/frontend CycloneDX SBOM,
  cargo audit/deny와 npm audit을 release evidence로 보존한다.
- production executable은 SHA-256 manifest와 Authenticode signer thumbprint/publisher를
  설치 전후 확인하고 RFC 3161 timestamp 실패 시 release를 중단한다.
- production install/repair/update는 package와 함께 온 unsigned metadata가 아니라 별도 신뢰
  채널의 `package-manifest.json` SHA-256을 필수 입력으로 받고, 최초 parse 전후와 atomic copy
  뒤 같은 digest인지 다시 확인한다.
- 현재 production 인증서와 clean signed Windows fixture evidence는 외부 승인이 필요하므로
  `windows.install_update`와 Tauri bundle은 experimental이다.

### Windows 설치 ACL과 위협 경계

`InstallRoot`의 정본 ACL은 상속을 끊은 allowlist DACL이다. 서로 구분되는 SYSTEM과 설치 시
확정된 owner SID만 FullControl을 가지며, `NT SERVICE\RestorePlaneService` SID는 ReadAndExecute만
가진다. install/repair/update는 package copy 전에 root를 먼저 보호하고, 전체 manifest tree와
`package-manifest.json`을 복사한 뒤 모든 자식의 DACL을 이 값으로 교체한다. service 시작
직전에는 package hash/file set과 root부터 자식까지의 owner, ACE SID, 권한, 상속 여부를 다시
검증한다. Everyone, Authenticated Users, BUILTIN\Users, Administrators 또는 임의 SID의 추가
allow/deny ACE가 있으면 시작하지 않는다.

이 경계가 방어하는 대상은 권한 상승하지 않은 로컬 사용자다. 승인된 설치 owner, 관리자,
SYSTEM, 또는 이미 탈취된 service identity는 privileged trust boundary 밖에 있다. SYSTEM은
서명된 install/repair/update 유지보수를 위해 InstallRoot에 쓸 수 있다. 따라서 runtime의
dependency handle/hash gate는 manifest에 완전하게 열거된 dependency closure를 ordinary-user
교체와 reparse 공격으로부터 고정하지만, privileged package 변조나 누락된 dependency 목록을
보완하지 않는다. repair/reinstall/update는 persisted owner 또는 SYSTEM 문맥에서만 실행하며,
다른 관리자 계정으로 owner를 암묵적으로 변경하지 않는다.

### Release staging trust boundary

Deployment code cannot establish trust in its own bytes after PowerShell has
already loaded and parsed it. The release staging directory must therefore be
created with a protected exact DACL before extraction. Only SYSTEM and the
approved installer owner may have FullControl; every extracted file and
directory must be normalized to that allowlist, and reparse points are rejected.
Built-in Authenticode and out-of-band thumbprint checks remain mandatory, and
deployment scripts run in a fresh `powershell.exe -NoProfile -NonInteractive
-ExecutionPolicy AllSigned -File` process. AllSigned is defense in depth, not a
substitute for the protected staging root. A local administrator, SYSTEM, the
approved owner, and a compromised service identity remain outside the ordinary
unprivileged-local-user threat model.

### Windows root provenance boundary

The installer never treats a point-in-time ACL check as proof that a pre-existing
directory has always been protected. Install and repair publish a fresh
`InstallRoot` object and record the volume serial plus stable file IDs for
`InstallRoot`, `DataRoot`, and every service control directory in the protected
`DataRoot\install-root-provenance.json` control file. Repair and update fail
closed if that provenance is missing or does not bind the current objects.
Update additionally requires the current complete InstallRoot ACL and package
tree to be exact before any in-place use. A clean reinstall that reuses an
existing DataRoot requires an externally approved `-OwnerSid`; a self-declared
owner in a pre-seeded data tree is not a trust anchor.

Operational roots are supported only as direct children of the OS-proven
Program Files/ProgramData parents or as direct children of a volume root.
Pre-existing nested custom parents are rejected because Windows share locks do
not reveal historical WRITE_DAC/WRITE_OWNER handles. Initial install and repair
quarantine the complete old InstallRoot by its verified handle, then create a
new protected directory object before copying package bytes. Unknown identities
receive no access, including read access, anywhere in the secrets/spool/control
trees. Local administrators, SYSTEM, the approved installer owner, and a
compromised service identity remain outside this ordinary-user boundary.

## Diagnostics와 support bundle

- diagnostics는 queue/lease/permit/spool/WAL/tool/destination/outbox와 telemetry archive
  상태의 bounded aggregate만 반환한다.
- support bundle allowlist는 Activity, config metadata, diagnostics, Engine Matrix,
  manifest 다섯 entry로 고정한다.
- raw stdout/stderr, process environment, connection string, secret value, backup content는
  제외한다. secret canary가 발견되면 partial archive 없이 실패한다.
- support bundle output root의 symlink/reparse point는 거부하고 archive 재검증 뒤에만
  원자 rename한다.
- 30일을 넘긴 Activity와 terminal outbox archive에는 message/attributes/payload 원문을
  넣지 않고 BLAKE3 hash만 기록한다. open/acknowledged Attention과 ledger는 보존한다.

## 취약점 신고

보안 이슈는 공개 이슈로 올리지 말고 비공개로 관리자에게 알린다.
