# DB Backup System — 개발 계획서

> [!IMPORTANT]
> 이 파일은 초기 v1 설계 이력을 포함한다. 실행 권한, 스케줄, Operational Store, Backup Scope,
> 암호화 파이프라인의 현재 정본은 [ADR-001](adr/ADR-001-execution-authority.md)과
> [Program-owned backup engine](plan/program-owned-backup-engine.md)이다.

> 목적: HeidiSQL/DBeaver처럼 여러 DB(MySQL, PostgreSQL / 버전 상이)에 접속해
> 정해진 시간에 무인으로 백업하고, 랜섬웨어가 못 건드리는 외부 위치에 보관·복구하는 시스템.
> 언어: Rust (메모리 안전, 리소스 해제 누락 방지, 단일 실행파일 배포).

## 0. 위협 모델 (설계의 출발점)

랜섬웨어 대응이 1순위 목표. 두 문제를 분리한다.

- **문제 A — 자격증명 보호**: 설정파일(DB 접속정보, SSH 비번) 암호화 + 키 관리.
- **문제 B — 랜섬웨어 내성**: 백업본을 "공격자가 삭제/변조 못 하는 곳"에 보관.
  - 설정파일 암호화는 문제 B를 해결하지 못한다. 별도로 다뤄야 한다.
  - 원칙: 3-2-1 (사본 3개, 매체 2종, 오프사이트 1개) + 불변성(Object Lock) + **정기 복원 테스트**.
  - 목적지 자격증명은 가능하면 append-only/write-only로 발급 (공격자가 훔쳐도 기존 백업 삭제 불가).

## 1. 확정된 방향 (사용자 결정)

| 항목 | 결정 |
|------|------|
| 저장 목적지 | S3 호환 + Object Lock, 원격 SSH/rsync, 로컬/NAS — **3종 모두** (플러그형) |
| 키 관리 | **봉투 암호화 하이브리드** (비번 1회 → OS 키저장소 무인화 + 이식성) |
| 대상 DB | MySQL + PostgreSQL **동시 지원** |
| 스케줄 | singleton Windows service 안의 durable scheduler가 Operational Store cursor를 소유; Plan별 작업 스케줄러 등록 금지 |

> 아래 초기 CLI/TOML 예시는 v1 역사적 설계 스케치다. production schedule, Backup Scope,
> queue와 retention의 현재 계약은 [README](../README.md)와
> [Program-owned backup engine](plan/program-owned-backup-engine.md)을 따른다. TOML의 DB별
> `schedule`을 근거로 Plan별 Windows Task Scheduler 항목을 새로 만들지 않는다.

## 2. 아키텍처

```
[Orchestrator (Rust, tokio)]
  init / add-db / add-dest / run / schedule / restore-test / verify
        │
        ├── SecretStore  : 봉투암호화 + OS 키저장소 (keyring/DPAPI)
        ├── BackupSource : trait { Postgres, MySQL }  → 네이티브 덤프 도구 호출
        ├── Pipeline     : dump stdout → 압축(zstd) → ChaCha20-Poly1305 STREAM → 스풀·팬아웃
        └── Destination  : trait { S3(opendal), Ssh(rsync/sftp), Local }
```

### 핵심 결정 A — 덤프는 네이티브 도구 호출 (재구현 금지)
- PostgreSQL: `pg_dump` / `pg_dumpall`
- MySQL: `mysqldump` (필요 시 `mydumper`로 병렬)
- Rust는 오케스트레이션·암호화·전송·스케줄·검증만 담당.
- **Gotcha**: `pg_dump`는 서버 버전 이상이어야 안전. 서버 버전이 제각각 → config에 DB별 클라이언트 도구 경로/버전 지정. `mysqldump`도 8.x 계열 옵션 차이 주의.
- 순수 Rust 드라이버(sqlx 등)는 "복원 가능한 표준 덤프"를 못 만들므로 v1 제외.

### 핵심 결정 B — 스트리밍 파이프라인
`pg_dump stdout → zstd 압축 → ChaCha20-Poly1305 STREAM 암호화 → managed spool → Destination 업로드`를 스트리밍으로 연결.
큰 DB도 메모리 상수로 처리. RAII로 터널/파일핸들 해제 누락 방지.

### 핵심 결정 C — 봉투 암호화 + OS 키저장소
```
사용자 비밀번호 ──Argon2id──▶ KEK ──암호화──▶ [wrapped DEK] (리포 커밋 안전)
랜덤 DEK ──▶ config(DB/SSH 비밀)를 암호화
최초 셋업 시 KEK(또는 비번)를 OS 키저장소(DPAPI)에 저장 → 무인 자동실행
다른 PC 이동 → 같은 비번 재입력 → KEK 복원 → 이식성 확보
```
- 깃허브에는 `[wrapped DEK]` + 암호화된 config만 올라감. 평문 키/비번 절대 커밋 안 함(.gitignore 이중 차단).

## 3. 설정 스키마 (TOML, 초안)

```toml
# config.toml  — secrets 필드는 DEK로 암호화되어 저장됨
[[database]]
name        = "prod-postgres"
kind        = "postgres"           # postgres | mysql
host        = "10.0.0.5"
port        = 5432
user        = "backup_ro"
password    = "enc:...."           # DEK 암호화된 값
client_tool = "C:/pg16/bin/pg_dump.exe"
schedule    = "0 3 * * *"          # 매일 03:00
ssh_tunnel  = "bastion-1"          # 선택: 아래 [[ssh]] 참조
dest        = ["s3-immutable", "nas-local"]

[[ssh]]
name        = "bastion-1"
host        = "bastion.example.com"
user        = "tunnel"
auth        = "key"                # key | password
key_path    = "~/.ssh/id_ed25519"
passphrase  = "enc:...."           # 키 패스프레이즈도 DEK 암호화

[[destination]]
name    = "s3-immutable"
kind    = "s3"                     # s3 | ssh | local
bucket  = "my-db-backups"
region  = "ap-northeast-2"
object_lock = true                 # 불변성
retention_days = 30

[[destination]]
name = "nas-local"
kind = "local"
path = "Z:/backups"
```

## 4. Rust 크레이트 스택

| 역할 | 크레이트 | 비고 |
|------|----------|------|
| 런타임 | `tokio` | async |
| 암호화 | `age` (파일 암호화) / `aes-gcm`·`chacha20poly1305` | |
| KDF | `argon2` (Argon2id) | 비번 → KEK |
| OS 키저장소 | `keyring` / Windows `windows`(DPAPI) | 무인화 |
| SSH/터널 | `russh`(순수 Rust) 또는 `ssh2`(libssh2) | 바스티온 경유 |
| 목적지 | `opendal` | S3/로컬 등 추상화 |
| 압축 | `zstd` | |
| 설정 | `serde` + `toml` | |
| CLI | `clap` | |
| 스케줄 | singleton service process 안의 프로그램 소유 durable scheduler. Windows Task Scheduler는 이관·복구 호환 전용 | |

> 크레이트 버전/최신 API는 구현 착수 시 context7로 재확인 필요.

## 5. 단계별 계획

- **Phase 0 — 스캐폴딩 + 키관리**: cargo 프로젝트, trait 정의(BackupSource/Destination/SecretStore), `init` 명령(KEK/DEK 생성 → OS 키저장소 등록), config 암호화/복호화 왕복 검증.
- **Phase 1 — 수직 슬라이스 (Postgres → Local)**: pg_dump → zstd → age → 로컬 저장 → SHA256 검증. end-to-end 1개 완성.
- **Phase 2 — DB 확장 (MySQL)**: BackupSource trait에 MySQL 구현 추가, 다중 DB config, 실패 격리(하나 실패해도 나머지 진행).
- **Phase 3 — 목적지 확장 (S3 Object Lock + SSH/rsync)**: Destination trait 구현, 불변성 업로드, append-only 자격증명.
- **Phase 4 — SSH 터널 + 무인 스케줄**: 바스티온 경유 접속, singleton service 내부 durable scheduler, 무인 복호화, 실패 알림. Windows Task Scheduler는 legacy 이관·복구 호환에만 사용한다.
- **Phase 5 — 복원 검증**: 백업을 임시 DB에 실제 복원해 "살아있는 백업" 자동 확인. 백업의 완성 = 복원 성공.

## 6. 리스크 / 미해결

- [x] `pg_dump`/`mysqldump`/`mariadb-dump`는 production에서 감사된 `InstallRoot/tools` bundle만 사용한다. bundle이나 외부 audit 기준이 없으면 release/install이 fail-closed한다.
- [ ] S3 Object Lock 세부 모드(Governance vs Compliance) 선택.
- [ ] 실패 알림 채널(이메일/슬랙 등) 미정.
- [x] native attempt는 6시간 pre-commit hard deadline을 적용하고, pre-commit retry는 dump/upload를 처음부터 다시 실행한다. crash-resumable upload는 현재 범위 밖이다.
