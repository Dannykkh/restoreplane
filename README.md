# dbbackupsystem

랜섬웨어 내성 데이터베이스 백업 오케스트레이터 (Rust, Windows).

여러 데이터베이스(PostgreSQL, MySQL/MariaDB)에 접속해 정해진 시간에 무인으로 논리 백업을 만들고,
압축·암호화·서명하여 **랜섬웨어가 삭제/변조할 수 없는 오프사이트**에 보관하며, **실제 복원 가능함을
정기 검증**한다.

> 이 리포지토리는 **도구 코드만 공개**한다. 개인 설정(`dbbackup.toml`), 키스토어, 복구 코드,
> 로컬 카탈로그, 스풀은 절대 커밋하지 않는다(`.gitignore` 참조). 자세한 이유는 [SECURITY.md](SECURITY.md).

## 설계 원칙 (위협 모델)

1. **자격증명 보호 ≠ 랜섬웨어 방어** — 둘을 분리해 다룬다.
2. **불변 오프사이트 + 서명 + 검증** — S3 Object Lock(Governance), append-only 자격증명,
   오프라인 ed25519 서명, 정기 test-restore가 "복구 가능한 백업"을 보장한다.
3. **복원 = 코드 실행**으로 취급 — 미서명/위조 백업은 복호 전에 거부하고, 실제 복원은 격리 샌드박스에서만.

전체 설계 문서: `docs/plan/db-backup-system/` (spec, plan, db-schema, sections, flow-diagrams).
GUI 디자인 정본: `DESIGN.md`.

## 아키텍처 (크레이트)

| 크레이트 | 역할 |
|----------|------|
| `dbbackup-core` | 공용 타입(EncField, 도메인 enum), SecretEncryptor 경계 |
| `dbbackup-config` | TOML 설정 스키마 + 마이그레이션 + 교차참조 검증 |
| `dbbackup-secrets` | 봉투암호화(Argon2id KEK + 백업별 DEK), 3경로 wrap(비번/복구코드/DPAPI), ed25519 서명, keystore |
| `dbbackup-pipeline` | 스트리밍 파이프라인(blake3 tee→zstd→chacha20 STREAM→스풀), Commit Protocol, 옵션 C 팬아웃, Destination/BackupSource 트레이트, LocalDest |
| `dbbackup-source-pg` | PostgresSource(pg_dump/pg_dumpall globals, 버전 매트릭스) |
| `dbbackup-source-mysql` | MySqlSource(mysqldump, 엔진 스캔, MariaDB 분기, binlog 앵커) |
| `dbbackup-dest-s3` | S3Dest(opendal, Object Lock, append-only 자가테스트) |
| `dbbackup-dest-ssh` | SshDest(russh SFTP, host-key 핀, 터널) |
| `dbbackup-catalog` | 로컬 SQLite 카탈로그(캐시), 매니페스트 기반 rebuild, overlap guard |
| `dbbackup-verify` | 서명 게이트, 복호 체인, 검증 L1/L2/L3, 복원 샌드박스 |
| `dbbackup-scheduler` | Windows Task Scheduler 등록(schtasks XML), 알림, 비밀 스크럽 로거 |
| `dbbackup-cli` | clap CLI(init/run/verify/restore/schedule/catalog/…) |

## 빠른 시작

```powershell
# 빌드 (Rust stable + MSVC 툴체인 필요)
cargo build --release

# 최초 설정: 마스터 비밀번호 → KEK/DEK, 복구 코드 1회 표시, DPAPI 등록, 서명키 생성
target\release\dbbackup.exe init

# 설정에 DB/목적지 추가 후 백업 실행
target\release\dbbackup.exe run --config dbbackup.toml

# 복원 가능성 검증(L1: 복호+해제+해시 대조)
target\release\dbbackup.exe verify --config dbbackup.toml
```

무인 실행 비밀번호는 Windows DPAPI Machine-scope로 캐시되며, 스케줄러는 `schedule` 명령으로 등록한다.

## 상태

- **구현 완료(검증됨)**: 워크스페이스 12개 크레이트, `cargo test --workspace` 그린.
  PG→Local 백업→검증(L1) 왕복이 실제로 동작(라이브 DB 없이 StubSource로 검증).
- **인프라 게이트(`#[ignore]`)**: L2(pg_restore), L3(Docker 샌드박스), 라이브 S3/SSH 업로드는
  실제 인프라가 있을 때만 실행되는 통합 테스트로 분리.
- **후속**: 데스크톱 GUI(Tauri) — 디자인 정본(`DESIGN.md`)과 대시보드 시안 완료, 구현은 후속.

## 개발

```powershell
cargo test --workspace     # 단위/통합 테스트
cargo clippy --workspace   # 린트
cargo deny check           # 라이선스/취약점/의존성 정책
cargo audit                # RustSec 권고 스캔
```

라이선스: MIT.
