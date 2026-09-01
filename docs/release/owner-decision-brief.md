# Owner Decision Brief — Connected Backup Platform v2 launch

## 현재 판정

2026-07-30 기준 R10의 실행 가능한 제품 경계는 구현됐다. update fault matrix, minimum-reader
guard, diagnostics, durable support bundle, Windows install/repair/update/uninstall 계약,
bounded telemetry archive, exact Rust MSRV/current toolchain, SBOM/audit policy와 G5 자동
행렬은 current source에서 실행할 수 있다.

General Availability 승격은 아직 승인하지 않는다. machine-readable 판정은
`artifacts/g5/release-matrix.json`의 `status=pass`, `promotionStatus=parked`다. 아래 항목은
코드 결손이 아니라 외부 자격증명·관리형 fixture·제품 수치 승인이다.

관리자 권한의 unsigned development fixture에서는 clean install, live IPC 진단, repair,
support bundle, 14단계 update, control-backup rehearsal, uninstall-preservation을 실제로
통과했다. 이 결과는 SCM/update 동작의 로컬 증거이며 D1/D2의 서명과 D4의 독립 clean VM
증거를 대체하지 않는다.

## 필요한 결정과 증거

| ID | Owner 결정/제공 항목 | 현재 안전한 기본값 | 승격을 해제하는 증거 |
|---|---|---|---|
| D1 | production Authenticode 인증서와 custody owner | production release 중단 | 제한된 `production-signing` environment에서 PFX/HSM signing, 예상 subject/thumbprint, SignTool 및 Windows trust 검증 |
| D2 | RFC 3161 timestamp provider와 장애 정책 | timestamp 실패 시 release 실패 | signed binary마다 유효 timestamp와 `signtool verify /pa /all /v` 통과 |
| D3 | managed Windows release runner 승인 | tag release는 self-hosted `restoreplane-managed` runner에서 대기 | Docker, Chromium, 충분한 디스크를 갖춘 clean runner에서 `-RunManaged -RequireUpstreamEvidence` 통과 |
| D4 | clean signed install/update fixture 승인 | `windows.install_update`와 Tauri bundle은 experimental 유지 | 별도 Windows VM에서 install, repair, update, support bundle, uninstall-preservation evidence pass |
| D5 | controlled beta/GA stop metric 수치 | `pending_owner_approval`, beta 진입 중단 | queue freshness, restore drill pass rate, RPO detect time, repair backlog, notification delivery, support bundle failure rate의 승인값 |
| D6 | SQL Server/SqlPackage 관리형 fixture | SQL Server는 experimental/candidate, Plan 게시 차단 | 승인된 SQL Server와 Microsoft SqlPackage identity/hash/signature로 BACPAC export/import 및 content oracle pass |
| D7 | Tauri updater endpoint와 public key custody | 자동 updater 비활성, signed offline bundle만 후보 | HTTPS endpoint, updater public key, private-key custody, signed updater artifact와 downgrade rehearsal |

## 권고 결정 순서

1. D1/D2의 signing custody와 production environment approval을 먼저 정한다.
2. D3 clean managed runner를 등록하고 같은 revision/tree의 G0~G5를 재수집한다.
3. D4 signed lifecycle fixture를 실행해 signing metadata와 fixture evidence를 G5에 입력한다.
4. internal dogfood 측정 뒤 D5 수치를 승인한다. 측정 전 임의 절대 수치를 GA 기준으로 만들지
   않는다.
5. D6과 D7은 독립 capability 승격으로 처리한다. SQL Server 또는 updater가 늦어져도
   SQLite와 offline signed 설치의 증거를 과장하지 않는다.

## 승인 후 실행 명령

관리형 runner:

```powershell
.\scripts\test-release-matrix.ps1 `
    -RunManaged `
    -RequireUpstreamEvidence
```

서명/clean fixture evidence가 생성된 뒤 최종 판정:

```powershell
.\scripts\test-release-matrix.ps1 `
    -SigningMetadataPath <signing-metadata.json> `
    -SignedFixtureEvidencePath <release-fixture-evidence.json>
```

owner stop metrics가 `approved`가 아니면 나머지 증거가 통과해도
`promotionStatus=parked`가 정상이다.

## 명시적으로 범위 밖에 유지할 항목

public HTTP API, organization RBAC, Remote Agent, backup content browser는 initial local v2에
포함하지 않는다. MongoDB/Firebird, Redis/Valkey, Azure Blob, SMB, rclone도 각각
backup/restore/reader/conformance evidence가 생기기 전에는 구현 또는 supported claim으로
간주하지 않는다.
