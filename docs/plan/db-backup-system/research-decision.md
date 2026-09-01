# Research Decision

## 대상 프로젝트
DB 백업 시스템 (Rust). 그린필드 — 기존 코드 없음. 초기 입력: `docs/PLAN.md`.

## 선택된 리서치
| 유형 | 실행 | 이유 |
|------|------|------|
| Library/Tech (Rust 크레이트 + 덤프 도구) | O | age/rustcrypto, russh/ssh2, opendal S3 Object Lock, keyring/DPAPI, argon2, pg_dump/mysqldump 버전 호환성 실증 필요 |
| Architecture/Domain (랜섬웨어 내성 백업) | O | 3-2-1, immutability(Object Lock Governance vs Compliance), append-only 자격증명, 복원 검증 패턴 |
| Codebase | Skip | 그린필드. 재구현 대상 없음 |
| Competitor | 경량 | restic/borg/pgBackRest 등 기존 도구의 설계 교훈만 참조 |
| Academic | Skip | 실무 엔지니어링 과제, 학술 불필요 |

## 리스크 레벨
중상 — 암호화 키 관리와 랜섬웨어 내성이 잘못되면 "백업이 있는데 못 쓰는" 최악 시나리오. 실증 검증 필수.
