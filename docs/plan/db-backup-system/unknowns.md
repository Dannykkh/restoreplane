# Unknowns Map (Blindspot Pass)

## Known Knowns (확정)
- 언어 Rust, 오케스트레이터는 Windows에서 구동.
- 덤프는 pg_dump/mysqldump 네이티브 호출(재구현 안 함).
- 파이프라인: dump stdout → zstd → age 암호화 → 목적지 스트리밍.
- 키: 봉투암호화(Argon2id KEK + 랜덤 DEK) + OS 키저장소(DPAPI) 무인화.
- 목적지 3종(S3 Object Lock / SSH rsync / 로컬), DB 2종(MySQL/PostgreSQL) 동시.
- 스케줄: Windows 작업 스케줄러 등록.
- 1순위 목표: 랜섬웨어 내성.

## Known Unknowns (사용자 미결정)
- KU1. pg_dump/mysqldump 배포: 사용자 설치 요구 vs 번들. (버전 매트릭스)
- KU2. S3 Object Lock 모드: Governance vs Compliance.
- KU3. 백업 형태: 전체 논리 덤프만 vs PITR(WAL 아카이빙) 포함.
- KU4. 실패 알림 채널.
- KU5. 복원 검증 대상(스크래치 DB 인스턴스 유무).
- KU6. DB 대수/규모/네트워크 토폴로지(직결 vs 바스티온).

## Unknown Knowns (암묵적 선호 추정)
- 단일 운영자(개인) 사용 → 멀티유저/RBAC 불필요. [inferred]
- CLI 우선, GUI는 후순위. [inferred]
- DB 서버는 대부분 Linux(SSH 터널 전제). [inferred]

## Unknown Unknowns (사용자가 모를 수 있는 위험)
- UU1. Compliance 모드 자기 잠금: 잘못 설정 시 본인도 삭제 불가 → 스토리지 비용 폭증 위험.
- UU2. pg_dump 버전 < 서버 버전이면 덤프 실패/불완전 — 서버 업그레이드 시 조용히 깨짐.
- UU3. DPAPI 키는 이식 불가 — PC 이전/OS 재설치 시 무인 키 소실. 봉투암호화 비번 재입력으로만 복구 가능(비번 분실 시 전 백업 복호화 불가 = 치명).
- UU4. 대용량 덤프 중 네트워크 끊김 → 부분 업로드/재시도 정책 필요.
- UU5. mysqldump --single-transaction은 MyISAM/DDL 중엔 일관성 미보장.
- UU6. 백업 자격증명이 DeleteObject 권한을 가지면 Object Lock 외 경로로 삭제 위험.

## Architecture-changing Questions (인터뷰 우선순위)
1. 백업 형태: 논리 덤프만 vs PITR 포함 (스코프 대폭 변동)
2. S3 Object Lock 모드 + 마스터 비번 분실 복구 정책 (보안 경계 + 자기 잠금)
3. 덤프 도구 번들 vs 사용자 설치 (배포 아키텍처)
4. DB 대수/규모/토폴로지 (병렬성·타임아웃·터널 설계)
5. 복원 검증 대상 인스턴스 (Phase 5 설계)
