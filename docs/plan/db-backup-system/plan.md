# Implementation Plan — Ransomware-Resilient DB Backup System (Rust)

> 이 문서는 spec.md + team-review.md(3인 적대적 리뷰) + 사전 v3를 통합한 최종 구현 계획이다.
> 낯선 독자도 이것만으로 구현할 수 있도록 자립적으로 작성됨.

## 전략 선택 (Strategy Decision)

넓은 설계 결정을 후보 비교로 확정했다.

### 후보
- **S1. Rust 오케스트레이터 + 네이티브 덤프 도구 호출** (채택)
- **S2. 순수 Rust DB 드라이버로 자체 덤프 포맷 구현** (기각)
- **S3. 기존 도구(restic/pgBackRest) 래핑 + 얇은 스케줄러만** (부분 흡수)

### 채점 (1-5, 근거 요약)
| 기준 | S1 | S2 | S3 |
|------|----|----|----|
| 요구 충족(P1~P5) | 5 (복원 호환 공식 보장) | 3 (복원 정합성 직접 책임) | 4 (도구별 DB 커버 편차) |
| 도메인 적합성 | 5 | 2 (버전별 포맷 재현 지옥) | 4 |
| 리스크/복잡도 | 4 | 1 (복원불가 리스크 최고) | 3 (외부 도구 제약 종속) |
| 점진성/되돌리기 | 5 | 3 | 4 |
| 노력/비용 | 4 | 1 | 4 |
| **합계** | **23** | 10 | 19 |

### 결정
**S1 채택.** 덤프/복원은 pg_dump/mysqldump/pg_restore/psql/mysql 공식 도구가 담당(복원 호환성 보장),
Rust는 오케스트레이션·봉투암호화·서명·스트리밍·전송·스케줄·검증을 담당. S3의 좋은 아이디어(불변 리포지토리,
append-only, 매니페스트 기반 카탈로그 재구성)는 흡수. S2는 "복원 안 되는 백업" 리스크가 프로젝트 목표와
정면 충돌해 기각.

## 확정 아키텍처 (리뷰 반영 후)

```
CLI (clap)
  init | doctor | test-connection | add-db | add-dest | run [--db X] [--dry-run]
  | verify | restore | rotate-key | register-dpapi | keys status
  | catalog [rebuild --from <dest>] | prune(격리 프로파일) | schedule

Core traits
  BackupSource { plan() -> DumpPlan(1..N parts + globals); spawn(part) -> DumpArtifact }
      DumpArtifact = Stream(AsyncRead) | Directory(TempDir)
      impls: PostgresSource(+globals), MySqlSource(engine scan)
  Destination { writer(key) -> Writer; capabilities() -> DestCaps }
      DestCaps { immutability: None|Governance|Compliance|ServerAppendOnly, checksum_required, resumable }
      impls: S3Dest(opendal, Object Lock), SshDest(russh SFTP), LocalDest(opendal fs = 스풀)
  SecretManager { dek lifecycle, wrap/unwrap 3경로, sign/verify }
  Restorer(기계) / Verifier(단언) — 공유 기계 위 assert 계층
  Tunnel(russh direct-tcpip, host-key 핀)
  Catalog(SQLite, 캐시) + Manifest(목적지 동봉, 진실 소스)
  Scheduler(Windows Task Scheduler, schtasks XML)
```

## 파이프라인 — 옵션 C (스풀-후-팬아웃)

```
1) BackupSource.plan(): PG면 [globals] + [db1..] , MySQL이면 [db] (+엔진 스캔)
2) 매 run 버전 재검증: client_major >= server_major 아니면 HARD FAIL
3) 덤프 실행 → DumpArtifact
     Stream: pg_dump -Fc -f - / mysqldump ...  → 파이프
     Directory: mydumper / pg_dump -Fd → tar로 말아 파이프
4) 파이프: dump → blake3(tee, 평문해시) → zstd::Encoder → age(백업별 DEK) StreamWriter → 로컬 스풀파일
5) Commit Protocol:
     - 자식 exit code 확인(≠0 or stderr 치명 패턴 → FAIL)
     - finish inner→outer: zstd.finish() → age.finish() → 스풀 flush
     - "복사~커밋" 사이 early-return 금지 (절단 백업 방지)
6) 매니페스트 생성(도메인 필드 전체) → 오프라인 개인키로 서명
7) 스풀파일 + 서명 매니페스트를 목적지별 독립 업로드(JoinSet):
     - S3: multipart, Object Lock Governance 상속, checksum_algorithm 설정, multipart-only-commit
     - SSH: russh SFTP, host-key 핀
     - Local: 스풀파일 자체가 LocalDest
   목적지별 retry/resume, 부분 성공 리포트
8) Catalog(SQLite)에 backups/artifacts 기록. 매니페스트가 진실 소스.
9) 검증 L1(재복호+재해제+평문해시 대조), L2(pg_restore --list) 자동 수행.
```

## 봉투암호화 최종 설계 (SecretManager)

```
DEK: 백업별(에폭별) 랜덤 32B — C2(정적 DEK 영구노출) 방지
Payload: chacha20poly1305 STREAM (또는 age recipient=DEK 유도 X25519) 키=DEK
DEK wrap 3경로(각각 self-describing 스탠자, chacha20poly1305):
  (a) Argon2id-KEK(마스터 비번, salt+params 저장) — 이식성
  (b) 복구코드-KEK(>=128bit 엔트로피, init 1회 표시) — 비번 분실 복구
  (c) DPAPI Machine-scope + 비커밋 로컬 keyfile 유도 entropy — 무인 실행
서명: 오프라인 ed25519 개인키로 매니페스트 서명, 공개키로 검증. 미서명 거부.
리포 정책: 도구 코드만 공개. config/wrap/복구코드/catalog/keyfile/스풀은 .gitignore(로컬+오프라인).
```

## 랜섬웨어 방어 (강제)
- append-only 자격증명(PutObject only, Delete*/lifecycle/retention/bypass 명시 Deny).
- prune는 **백업 호스트에서 실행 금지** — S3 서버측 lifecycle 또는 오프호스트 MFA 신원(별도 prune 프로파일, 무인 run이 로드 안 함).
- 매 run 자가 테스트: probe key로 DeleteObject/PutLifecycle 시도 → 거부 확인, versioning ON + 기본 보존 확인. 과권한/미잠금이면 refuse/loud-warn.
- Object Lock Governance(사용자 결정), 오설정 자기잠금 방지(Compliance는 명시 opt-in).
- 복원 격리 샌드박스: 임시 컨테이너, 아웃바운드 차단, 비특권, local_infile=0, COPY PROGRAM 차단, pg_restore --no-privileges, 슈퍼유저 금지.

## 구현 슬라이스 (Phasing)
1. **Walking skeleton: Postgres→Local, 봉투암호화+백업별DEK+서명+catalog+매니페스트+restore+verify(L1/L2/L3).** 하중 리스크(finish/절단, DEK wrap/unwrap, 평문해시 tee, +0 검증) 조기 증명. Local-only는 P1 미충족이라 "MVP 완료" 아님.
2. **S3 목적지 + Object Lock (두 번째로, 마지막 아님).** 진짜 랜섬웨어 방어 + 최대 외부 리스크(opendal retention/checksum). append-only 자가테스트 포함.
3. **MySQL 소스(두 번째 BackupSource).** 트레이트 시임 검증(DumpArtifact/globals/엔진스캔).
4. **무인 경로: DPAPI Machine-scope + schtasks XML(StartWhenAvailable) + overlap guard.**
5. **SSH 목적지 + russh 터널 + 옵션 C 팬아웃 3목적지 + prune/rotate-key + doctor.**

MVP-DoD = PG+MySQL + Local + S3(Object Lock) + 무인 + 검증. SSH/터널/mydumper는 fast-follow.

## Critical Findings 대응 매핑
| Finding | 대응 |
|---------|------|
| C1 DPAPI 구멍 | 비커밋 keyfile entropy, 최소권한 계정, blob 미커밋 |
| C2 정적 DEK | 백업별 DEK |
| C3 복원 RCE/위조 | 오프라인 서명 + 복원 샌드박스 |
| C4 오프라인 브루트포스 | 도구만 공개, wrap 리포 밖, 강한 비번 게이트 |
| H1/H2 append-only | prune 오프호스트, 매 run 자가테스트 |
| H3/H4 메타데이터/blob | config/blob 리포 밖 |
| H5 catalog 변조 | 서명 매니페스트가 진실 소스, catalog rebuild |
| H6 공급망 | Cargo.lock 커밋, 크레이트 핀, cargo-deny/audit, host-key 핀, 바이너리 해시 |
| P0-1/2/3 commit | exit code + finish 순서 + multipart-only-commit |
| P0-4 팬아웃 | 옵션 C |
| P0-5 SecretManager | wrap/payload 분리 |
| P0-6 catalog | 매니페스트 동봉 + rebuild |
| DT globals | pg_dumpall --globals-only 1급, pairs 기록 |
| DT 엔진스캔 | MyISAM 경고, non_transactional_tables |
| DT 버전 | 매 run 재검증 하드페일 |
| DT 검증레벨 | L1/L2/L3, verified에 레벨 표기 |

## Open Questions (구현 중 확정)
- DB 실제 대수/최대 크기 → max_parallel_dumps, mydumper 전환 임계, RTO 목표.
- L3 검증 주기(주 1회 + 스키마 변경 시 즉시 권장) 및 Docker 이미지 매핑.
- 알림 채널(webhook/email) 실제 선택.
- [확인 필요] opendal 0.58 per-object retention 헤더, keyring 4.x store 크레이트명, russh 터널 예제.

## External Review 결정 (Step 13-15)
3인 독립 적대적 전문가 리뷰(Red Team/Architecture/Domain-Technical)가 외부 LLM 리뷰 가치를 이미 초과 달성했고,
유일한 구조적 모순(공개 리포)을 사용자 결정으로 해소함. 별도 Gemini/Codex 외부 리뷰는 중복이라 생략(문서화).
