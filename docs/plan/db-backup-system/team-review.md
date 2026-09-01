# Team Review — 통합 (Red Team + Architecture + Domain-Technical)

개별 상세: [team-reviews/red-team.md](team-reviews/red-team.md), [team-reviews/architecture.md](team-reviews/architecture.md), [team-reviews/domain-technical.md](team-reviews/domain-technical.md)

## Critical Findings (v1 설계에 반드시 반영)

### 보안 (Red Team)
- **C1 DPAPI Machine-scope 구멍**: 로컬 임의 프로세스가 DEK 복원 가능. entropy를 공개 바이너리에 하드코딩하면 무의미.
  → entropy를 비커밋 로컬 keyfile에서 유도, 최소권한 서비스 계정, DPAPI blob 미커밋.
- **C2 정적 DEK + WORM = 회전 불가 영구 노출**: 하나의 DEK로 전 백업 암호화 → 한 wrap 유출 시 전체 영구 노출, 불변 스토리지라 재암호화 불가.
  → **백업별/에폭별 DEK**. 유출 blast radius를 1개 백업으로 한정. age multi-recipient로 자연 구현.
- **C3 복원=코드실행 RCE + 출처 인증 부재**: 논리 복원은 COPY FROM PROGRAM/LOAD DATA LOCAL 등 코드 실행. 대칭 AEAD는 무결성만, DEK 보유자는 유효한 악성 백업 위조 가능. auto-verify가 write를 exec으로 전환.
  → **오프라인 개인키(ed25519/minisign)로 매니페스트 서명, 미서명 거부**. 복원은 **격리 샌드박스**(임시 컨테이너, 아웃바운드 차단, 비특권, local_infile=0, COPY PROGRAM 차단, 슈퍼유저 금지).
- **C4 공개 리포 = 오프라인 브루트포스**: wrapped DEK + salt + params가 공개 리포에 있으면 무제한 오프라인 비번 공격.
  → **[사용자 결정 필요]** private 리포 + wrap/메타데이터를 리포 밖으로. 강한 마스터 비번 강제(zxcvbn 게이트).
- **H1/H2 append-only 미강제**: prune 신원이 백업 호스트에 있으면 무의미. IAM 템플릿을 런타임 자가검증 안 함.
  → prune는 **백업 호스트에서 절대 실행 금지**(S3 lifecycle 서버측 or 오프호스트 MFA 신원). 매 run 시 delete/lifecycle이 **실제로 거부되는지 자가 테스트**, versioning/기본보존 확인.
- **H3 메타데이터 유출**: enc: 필드만 암호화하면 호스트/유저/버킷/바스티온/스케줄이 공개 리포에 평문 노출(정찰 지도).
  → private 리포 or 전체 config 암호화.
- **H6 공급망**: Cargo.lock 커밋, security-critical 크레이트 `=x.y.z` 핀, cargo-deny/audit CI, russh host-key 정책 필수(blind-accept=MITM), 번들 덤프 바이너리 해시 검증.

### 아키텍처
- **P0-1/2/3 commit protocol**: 절단된 덤프가 유효한 백업으로 커밋되는 최악. Rust `Drop`은 async finish 보장 못 함.
  → 자식 exit code 확인 + finish inner→outer 명시 + multipart-only-commit + 매니페스트 커밋. "스트림 복사~커밋" 사이 early-return 금지.
- **P0-4 팬아웃 = 옵션 C 채택**: 1회 덤프→zstd→age→로컬 스풀(=LocalDest)→목적지별 독립 업로드. B(in-pipe tee)는 느린 업로드가 DB 스냅샷을 붙잡아 bloat.
- **P0-5 SecretManager 분리**: age recipient는 X25519/scrypt이지 Argon2id 대칭키가 아님. **DEK wrap/unwrap을 payload 암호화와 분리** — DEK를 chacha20poly1305로 3경로 wrap(Argon2id-KEK / 복구코드-KEK / DPAPI). rotate-key 가능해짐.
- **P0-6/H5 catalog rebuild**: SQLite는 랜섬웨어 호스트에 있음. 매니페스트를 목적지 동봉 + `catalog rebuild --from <dest>`. SQLite는 캐시.
- **P1-1 DumpArtifact 시임**: `enum DumpArtifact{Stream, Directory}`. mydumper/pg_dump -Fd는 디렉토리, pg_dumpall globals는 별도 파트. Directory는 tar로 스풀에 말아 동일 파이프.
- **P1-2 Destination 불변성 capability**: `capabilities() -> DestCaps{immutable, checksum_required, resumable}`. job이 요구 불변성 레벨 assert, Local/Ssh가 WORM 위장 못 하게.
- **P1-3 plaintext blake3 tee**: 소스에서 평문 해시(스트리밍), 목적지 체크섬과 별개. verify가 재복호화 후 대조.
- **P1-5 overlap guard**: SQLite WAL+busy_timeout, job 단위 named mutex(catch-up 중복 방지).
- **CLI 추가**: test-connection, rotate-key, doctor, prune(격리), catalog rebuild, keys status, register-dpapi/unlock, restore --decrypt-only/--with-recovery-code.

### DB 복원 정합성 (Domain-Technical)
- **globals 필수**: `pg_dumpall --globals-only` 없으면 role 없어 복원 실패. PG 서버 1대당 globals 덤프 1개, 매니페스트에 pairs_with_globals_backup_id. 복원 순서 globals→DB.
- **PG 포맷 custom(-Fc)** 기본(pg_restore --list 검증 가능). plain은 옵션.
- **MySQL 엔진 스캔**: MyISAM은 --single-transaction 무관하게 비일관 → 덤프 전 스캔, 매니페스트에 non_transactional_tables 경고.
- **mysqldump 플래그**: `--routines --triggers --events`(기본값 제각각), MariaDB/구MySQL 대상엔 `--column-statistics=0` 강제, `--default-character-set=utf8mb4`.
- **버전 재검증은 매 run**: 클라이언트 메이저 < 서버 메이저면 하드 실패. MariaDB vs MySQL은 mysqldump 바이너리 별도 매트릭스 축.
- **매니페스트 필수**: 없으면 "언제 무엇으로 복원할지 모르는 바이트 뭉치". 필드는 domain-technical.md §3.1.
- **검증 레벨 3계층**: L1 파일무결성(매 백업) / L2 포맷유효성(pg_restore --list, 매 백업) / L3 실제복원(주 1회+스키마 변경 시 즉시). "verified"는 항상 레벨 표기. 임시 인스턴스는 원본과 동일 메이저 버전(Docker 동적 선택).
- **charset/binlog 좌표**: mojibake 방지 위해 charset 기록. PITR 미구현이라도 `--source-data=2`로 binlog 좌표만 캡처(과거분 비가역성).
- **GFS + RPO/RTO 명시**: daily7/weekly4/monthly12, retain_until을 등급별로. RPO=백업주기(최대 24h), RTO=논리복원 선형(대용량은 mydumper/pg_restore -j). init/run에 RPO 한계 출력.

## 채택 결정 (자동 통합, accepted-by-default)
- 백업별 DEK, 옵션 C 스풀 팬아웃, SecretManager 분리, DumpArtifact 시임, Destination capability, 매니페스트 동봉+catalog rebuild, 오프라인 서명, 복원 샌드박스, 매 run 버전 재검증, globals 1급, 검증 3레벨, GFS 등급별 retain — 모두 plan.md에 반영.

## 사용자 결정 필요 (Step 11/15 게이트)
- **공개 vs 비공개 리포** (C4/H3/H4): 사용자의 명시적 "깃허브 공개" 전제와 충돌. 보안 경계 변경이라 확인 필요.
