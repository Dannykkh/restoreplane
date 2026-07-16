# 보안 정책 및 리포지토리 위생

이 문서는 팀 리뷰(Red Team)의 지적 C1~C4, H3~H6, M7을 운영 규칙으로 고정한다.

## 절대 커밋 금지 (공개 리포 정책)

다음은 로컬 + 오프라인에만 존재해야 하며 리포에 절대 올리지 않는다. `.gitignore`가 1차 차단하고,
CI의 시크릿 스캐너(gitleaks)가 2차 차단한다.

- `dbbackup.toml`, `prune.toml` — 실제 설정(호스트/유저/버킷/스케줄 = 정찰 지도, H3)
- `keystore.json`, `*.keystore.json` — KEK wrap + 서명 개인키(KEK 래핑됨이라도 미커밋, H4)
- `dpapi.keyfile`, `*.keyfile`, `*.dek` — DPAPI entropy/키 재료(C1)
- `*.recovery-code` — 복구 코드(C4: 오프라인 브루트포스 표적)
- `catalog.sqlite*` — 로컬 카탈로그 캐시
- `spool/`, `.staging/` — 평문/암호문 스풀

> **왜 공개 리포인가**: "도구를 오픈소스로 공개"와 "내 접속정보·키를 리포에 올림"은 별개다.
> 코드만 공개하고 개인 자산은 로컬에 둔다. 암호화된 wrap을 공개 리포에 올리면 오프라인
> 무제한 비번 브루트포스가 가능해진다(C4) — 그래서 wrap조차 커밋하지 않는다.

## 암호화 신뢰 근원

- 마스터 비밀번호 → Argon2id → KEK. 백업별 랜덤 DEK를 KEK로 wrap.
- 3경로 unwrap: 비밀번호(이식성) / 복구 코드(비번 분실 복구) / DPAPI Machine-scope(무인).
- **비밀번호 + 복구 코드를 동시에 잃으면 과거 백업은 영구 복호 불가**. 복구 코드는 init 시 1회만
  표시되며 오프라인 보관이 필수다.
- 매니페스트는 오프라인 ed25519로 서명한다. 복원/검증은 서명 검증을 통과한 백업만 처리한다(C3).

## 랜섬웨어 방어 (강제)

- 백업 목적지 자격증명은 **append-only**(PutObject만, Delete*/lifecycle/retention/bypass 명시 Deny).
  `dbbackup-dest-s3`의 `APPEND_ONLY_IAM_POLICY` 참조.
- **prune는 백업 호스트에서 실행 금지** — 별도 격리 신원(`prune.toml` 프로파일). 무인 `run`은 이 프로파일을 로드하지 않는다(H1).
- 매 `run` 시 목적지 자가테스트: Delete/PutLifecycle/PutObjectRetention이 실제로 거부되는지,
  versioning ON + 기본 보존이 걸려있는지 확인. 과권한/미잠금이면 거부/경고(H2).
- 복원은 격리 샌드박스(아웃바운드 차단, 비특권, local_infile=0, COPY PROGRAM 차단)에서만(C3).

## 공급망 (H6)

- `Cargo.lock` 커밋(바이너리 프로젝트). 보안 크리티컬 크레이트는 `=x.y.z`로 정확히 핀.
- CI에서 `cargo deny check` + `cargo audit`로 High/Critical 권고·yanked·라이선스 위반 시 빌드 실패.
- 번들 덤프 바이너리(pg_dump/mysqldump)는 해시 검증 후 실행(로컬 교체 방지).

## 취약점 신고

보안 이슈는 공개 이슈로 올리지 말고 비공개로 관리자에게 알린다.
