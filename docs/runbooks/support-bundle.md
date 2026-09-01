# Diagnostics and support bundle runbook

support bundle은 durable `support_bundle.create` Operation Run으로 생성한다. 직접 디렉터리를
압축하거나 raw service log를 첨부하는 대체 절차가 아니다.

## 생성

```powershell
$cli = "$env:ProgramFiles\RestorePlane\restoreplane.exe"
& $cli service diagnostics
& $cli service support-bundle
```

명령 성공은 작업 접수를 뜻한다. service worker가 완료하면 기본 경로
`%ProgramData%\RestorePlane\support-bundles\support-bundle-<time>-<id>.tar.zst`에 생성된다.

장기 운영 telemetry는 support bundle과 별도로
`%ProgramData%\RestorePlane\telemetry-archives`에 보관된다. 서비스는 30일보다 오래된
Activity와 terminal notification outbox를 시간당 최대 10개, batch당 5,000행으로
처리한다. archive에는 message, attributes, payload 원문 대신 BLAKE3 hash만 들어가며
open/acknowledged Attention과 Attention ledger는 삭제하지 않는다.
Operation Run/Work의 terminal 상태와 결과 checksum을 함께 확인한다.

## 정확한 allowlist

archive에는 최상위 파일 다섯 개만 있어야 한다.

| 파일 | 내용 |
|---|---|
| `activity.json` | 최대 7일/500건의 allowlisted Activity metadata |
| `config-metadata.json` | secret value 없는 구성·authority·version metadata |
| `diagnostics.json` | queue, lease, permit, spool, WAL, tool, destination, outbox 상태 |
| `engine-matrix.json` | 현재 build의 Engine Matrix와 limitation |
| `manifest.json` | build/revision, 포함·제외 목록, redaction version, BLAKE3 checksum |

명시적 제외 항목은 secret values, raw stdout, raw stderr, process environment, connection
strings, backup content다. 각 입력은 bounded되고 archive 전체와 각 entry도 다시 크기와
canary를 검사한다.

## 전달 전 검증

1. bundle 생성 Operation Run이 `succeeded`인지 확인한다.
2. 결과의 BLAKE3 checksum과 파일을 연결한다.
3. `manifest.json`의 build/revision이 incident 대상과 같은지 확인한다.
4. included가 정확히 네 data file이고 excluded 여섯 항목이 기록됐는지 확인한다.
5. 승인된 보안 전달 채널만 사용하고 수신자/보존 기간을 incident에 기록한다.

테스트 canary를 실제 운영 secret으로 사용하지 않는다. release gate는 고유 canary를 원시
입력과 archive에 주입해 `SUPPORT_BUNDLE_CANARY_DETECTED`가 partial file 없이 실패하는지
검증한다.

## 오류 처리

| 코드 | 의미 | 조치 |
|---|---|---|
| `SUPPORT_BUNDLE_CANARY_DETECTED` | forbidden literal 발견 | bundle 폐기, secret rotate, redaction 수정 |
| `SUPPORT_BUNDLE_INPUT_TOO_LARGE` | allowlisted 입력 상한 초과 | 기간/Activity source를 점검; raw log 추가 금지 |
| `SUPPORT_BUNDLE_ARCHIVE_TOO_LARGE` | entry/전체 archive 상한 초과 | producer 결함으로 처리 |
| `SUPPORT_BUNDLE_ARCHIVE_INVALID` | entry path/count/checksum 구조 오류 | bundle 전달 금지, current build로 재생성 |
| `SUPPORT_BUNDLE_IO_FAILED` | 보호 경로/디스크 I/O 실패 | ACL, reparse point, 여유 공간 확인 |

실패한 `.partial` 파일을 rename해 사용하지 않는다. output root가 symlink/reparse point이면
생성이 거부되므로 ACL을 완화하거나 다른 사용자 경로로 우회하지 않는다.
