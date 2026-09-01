# Section Index

<!--SECTION_MANIFEST
[
  {"id":"S01","title":"프로젝트 스캐폴딩 + Config 스키마 + CLI","deps":[],"layer":0},
  {"id":"S02","title":"SecretManager (봉투암호화/서명/복구코드)","deps":["S01"],"layer":1},
  {"id":"S11","title":"보안 하드닝 & 공급망 (gitignore/cargo-deny/로그 스크럽)","deps":["S01"],"layer":1},
  {"id":"S05","title":"Pipeline + Commit Protocol (옵션 C 스풀)","deps":["S01","S02"],"layer":2},
  {"id":"S08","title":"Catalog(SQLite) + Manifest(동봉/서명/rebuild)","deps":["S01","S02"],"layer":2},
  {"id":"S03","title":"BackupSource + PostgresSource (globals/버전/확장)","deps":["S05"],"layer":3},
  {"id":"S04","title":"MySqlSource (엔진스캔/플래그/MariaDB분기)","deps":["S05"],"layer":3},
  {"id":"S06","title":"Destination + LocalDest + S3Dest (Object Lock/자가테스트)","deps":["S05"],"layer":3},
  {"id":"S09","title":"Restorer + Verifier (샌드박스/L1-L3)","deps":["S05","S08"],"layer":3},
  {"id":"S07","title":"SshDest + Tunnel (russh SFTP/host-key 핀)","deps":["S06"],"layer":4},
  {"id":"S10","title":"Scheduler(schtasks) + Overlap Guard + 알림","deps":["S05","S08"],"layer":4}
]
SECTION_MANIFEST-->

## 섹션 목록 (의존성 레이어 순)
| ID | 제목 | 의존 | 레이어 |
|----|------|------|--------|
| S01 | 프로젝트 스캐폴딩 + Config 스키마 + CLI | - | 0 |
| S02 | SecretManager (봉투암호화/서명/복구코드) | S01 | 1 |
| S11 | 보안 하드닝 & 공급망 | S01 | 1 |
| S05 | Pipeline + Commit Protocol (옵션 C) | S01,S02 | 2 |
| S08 | Catalog + Manifest | S01,S02 | 2 |
| S03 | BackupSource + PostgresSource | S05 | 3 |
| S04 | MySqlSource | S05 | 3 |
| S06 | Destination + LocalDest + S3Dest | S05 | 3 |
| S09 | Restorer + Verifier | S05,S08 | 3 |
| S07 | SshDest + Tunnel | S06 | 4 |
| S10 | Scheduler + Overlap Guard + 알림 | S05,S08 | 4 |

## Ecosystem Coverage Check
- PostgreSQL 서버 → S03 / MySQL·MariaDB → S04 / S3 Object Lock → S06 / SSH → S07 / 로컬 → S05,S06 /
  DPAPI → S02 / Task Scheduler → S10. 모든 에코시스템 시스템이 커버됨.

## 구현 순서 (plan.md 슬라이스와 정합)
Slice1: S01→S02→S05→S08→S03→S09(PG→Local walking skeleton, 검증 포함)
Slice2: S06(S3 Object Lock) / Slice3: S04(MySQL) / Slice4: S10(무인) / Slice5: S07(SSH터널) + prune/rotate
S11은 전 슬라이스 병행(cross-cutting).
