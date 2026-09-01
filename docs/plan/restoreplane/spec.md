# Feature Spec — Ransomware-Resilient RestorePlane (Rust)

> [!IMPORTANT]
> 이 문서는 초기 v1 설계 기록이며 현재 실행 정본이 아니다. Windows Task Scheduler 기반의
> job별 실행 설명은 [ADR-001](../../adr/ADR-001-execution-authority.md)과
> [Program-owned backup engine](../program-owned-backup-engine.md)으로 대체됐다.

## Overview
여러 데이터베이스(MySQL, PostgreSQL; 버전 상이)에 접속해 **정해진 시간에 무인으로** 논리 백업을 만들고,
압축·암호화하여 **랜섬웨어가 삭제/변조할 수 없는 오프사이트**에 보관하고, **실제 복원 가능함을 정기 검증**하는
Rust CLI 도구. Windows에서 구동. 단일 운영자 사용.

## Context Map

### 공유 전제
- 목표: 랜섬웨어를 겪은 뒤, "백업이 있는데 못 쓰는" 최악을 방지하는 신뢰 가능한 백업/복구 체계 확보.
- 산업: 개인/소규모 인프라 운영. 범위: MVP → 수평 확장.
- 성공 기준: 정해진 시간에 무인 백업 성공 + 백업본이 랜섬웨어에 불변 + 정기 test-restore 통과.

### 이해관계자
| 역할 | 설명 | 관련 섹션 |
|------|------|-----------|
| 운영자(개인) | 도구 설치·설정·복구 수행 주체 | (Step 19 backfill) |
| 스케줄러(무인) | Windows Task Scheduler가 도구를 호출 | (Step 19 backfill) |

### 에코시스템 맵
| 시스템 | 대상 | 연동 방식 | 관련 섹션 |
|--------|------|-----------|-----------|
| PostgreSQL 서버 | 백업 원본 | pg_dump/pg_dumpall 호출(직결 or SSH 터널) | (backfill) |
| MySQL/MariaDB 서버 | 백업 원본 | mysqldump 호출(직결 or SSH 터널) | (backfill) |
| S3 호환 스토리지 | 오프사이트 목적지 | OpenDAL, Object Lock(Governance) | (backfill) |
| 원격 서버(SSH) | 오프사이트 목적지 | russh SFTP/rsync | (backfill) |
| 로컬/NAS | 목적지(최소방어선) | OpenDAL fs | (backfill) |
| OS 키저장소(DPAPI) | 무인 키 보관 | windows-dpapi Machine scope | (backfill) |
| Windows Task Scheduler | 스케줄 트리거 | schtasks XML 등록 | (backfill) |

### 기존 환경
그린필드. 기존 코드 없음. 깃허브 공개 리포 전제(→ 평문 secret 커밋 금지, 암호화 config만 커밋).

## Problem Statement
| # | 핵심 문제 | 영향 | 우선순위 | 해결 섹션 |
|---|-----------|------|----------|-----------|
| P1 | 랜섬웨어가 백업본까지 암호화/삭제 → 복구 불가 | 사업/데이터 전손 | 🔴 필수 | (backfill) |
| P2 | DB별 접속 방식·버전 상이로 자동화 어려움 | 백업 누락/불완전 | 🔴 필수 | (backfill) |
| P3 | 자격증명 평문 노출(깃허브 전제) | 자격증명 탈취 | 🟠 중요 | (backfill) |
| P4 | 무인 자동실행 ↔ 키 이식성 충돌 | 자동화 불가 or 이식 불가 | 🟠 중요 | (backfill) |
| P5 | 백업이 실제 복원되는지 미검증 | "죽은 백업" | 🟠 중요 | (backfill) |

## 확정 결정 (인터뷰)
| 결정 | 값 | 근거 |
|------|-----|------|
| 백업 형태 | 논리 덤프만(v1), PITR 보류 | 스코프 관리, 랜섬웨어 복구엔 충분 |
| Object Lock | Governance 모드 | 랜섬웨어 차단 + 본인 복구 가능 |
| 덤프 도구 배포 | 번들(하이브리드 폴백) | 버전 매트릭스를 프로그램이 관리 |
| 비번 분실 복구 | 복구 코드 발급 | init 시 오프라인 recovery key 1회 |

## 아키텍처 요약
```
CLI (clap): init | add-db | add-dest | run [--db X] | schedule | restore | verify | catalog
   │
   ├─ SecretManager : Argon2id KEK + 랜덤 DEK, age multi-recipient wrapping
   │                  경로 3: 비번 KEK / 복구 코드 / DPAPI Machine-scope(무인)
   ├─ Config        : TOML, secret 필드는 DEK 암호화(enc:...)
   ├─ BackupSource  : trait { PostgresSource, MySqlSource } → 네이티브 덤프 도구 호출
   ├─ Pipeline      : dump stdout → zstd::Encoder → age StreamWriter → Destination writer
   ├─ Destination   : trait { S3Dest(opendal, Object Lock), SshDest(russh), LocalDest(opendal fs) }
   ├─ Tunnel        : russh direct-tcpip (DB가 바스티온 뒤일 때)
   ├─ Catalog       : 로컬 SQLite — 백업 이력/체크섬/검증 상태 기록
   ├─ Scheduler     : Windows Task Scheduler 등록(schtasks XML, StartWhenAvailable)
   └─ Verifier      : 임시 인스턴스로 test-restore + 체크섬 대조 (+0)
```

## 봉투암호화 상세 (P3, P4, 복구코드)
```
init:
  1) 사용자 마스터 비밀번호 입력
  2) 랜덤 salt → Argon2id(m=64MiB,t=3,p=1) → KEK
  3) 랜덤 32B DEK 생성
  4) DEK를 age recipient로 다중 wrapping:
       - wrap#1: KEK (비번 경로 — 이식성: 다른 PC서 같은 비번)
       - wrap#2: 복구 코드(랜덤 고엔트로피, 1회 표시 후 사용자 오프라인 보관)
       - wrap#3: DPAPI Machine-scope로 KEK 보관 → 무인 자동실행
  5) config의 DB/SSH secret 필드를 DEK로 암호화(enc:...)
  6) 리포엔 [wrapped DEK 3종] + 암호화 config만 → 깃허브 안전
run(무인):
  DPAPI로 KEK 복원 → DEK unwrap → config 복호화 → 백업
이전 PC:
  같은 비번 입력 → KEK 재유도 → DEK unwrap. (DPAPI 캐시는 재등록)
비번 분실:
  복구 코드로 DEK unwrap → 새 비번 재설정.
```

## Risk Assessment
| 위험 요소 | 영향도 | 발생 확률 | 완화 전략 |
|-----------|--------|-----------|-----------|
| 마스터 비번+복구코드 동시 분실 | High | Low | init 시 복구코드 오프라인 보관 강력 안내, 미보관 시 진행 차단 옵션 |
| pg_dump < 서버 버전 → 덤프 실패 | High | Med | 번들 버전 매트릭스 관리, 실행 전 서버 버전 감지 후 클라이언트 선택 |
| append-only 자격증명에 Delete 권한 혼입 | High | Med | IAM 정책 템플릿 제공(PutObject only), prune는 별도 격리 신원 |
| Compliance 오설정 자기잠금 | Med | Low | 기본 Governance, Compliance는 명시적 opt-in + 경고 |
| 파이프라인 finish() 누락 → truncation | High | Low | RAII 래퍼로 finish 강제, 백업 후 즉시 복호화 헤더 검증 |
| 대용량 덤프 중 네트워크 끊김 | Med | Med | OpenDAL multipart, 실패 시 재시도/재개 정책, 부분 객체 정리 |
| DPAPI 키 이식 불가(PC 재설치) | Med | Med | 비번+복구코드가 진짜 신뢰근원, DPAPI는 편의 캐시로만 취급 |
| mysqldump 덤프중 DDL 비일관 | Med | Low | --single-transaction 문서화, 백업 창 DDL 회피 안내 |
| 백업은 되나 복원 미검증("죽은 백업") | High | Med | Verifier로 예약 test-restore, 실패 시 알림 |

## Test Scenarios (핵심 기능별)
### init / 키관리
- 정상: 비번 입력 → KEK/DEK 생성 → 복구코드 표시 → DPAPI 등록 → config 왕복 암복호화 성공.
- 에러: 약한 비번 거부, 복구코드 미확인 시 경고, DPAPI 등록 실패 시 폴백(비번 매실행) 안내.
- 엣지: 같은 config를 다른 PC로 이동 → 비번 재입력으로 복호화 성공.

### backup run (Postgres/MySQL)
- 정상: dump → zstd → age → 목적지 업로드 → 체크섬 catalog 기록, 3종 목적지 모두 성공.
- 에러: DB 접속 실패/도구 버전 불일치 → 해당 DB만 실패 격리, 나머지 진행, 종료코드/알림.
- 엣지: 대용량 스트리밍 중 목적지 일시 단절 → 재시도, 부분 객체 정리. 하나의 목적지만 실패 시 부분 성공 리포트.

### immutability
- 정상: Object Lock Governance 버킷에 업로드 → 보존기간 내 삭제 시도 거부됨.
- 에러: 백업 자격증명으로 기존 객체 삭제 시도 → Deny.
- 엣지: 보존기간 만료 후 lifecycle(격리 신원)로만 prune.

### restore / verify
- 정상: 최신 백업을 임시 인스턴스에 복원 → 행 수/체크섬 대조 통과 → catalog에 verified 기록.
- 에러: 복원 실패/체크섬 불일치 → 알림 + 해당 백업 unverified 표시.
- 엣지: 복구코드 경로로 복호화 후 복원(비번 분실 시나리오).

### schedule
- 정상: schtasks XML 등록, 예약 시각 무인 실행.
- 엣지: 예약 시각에 PC 꺼짐 → 재부팅 후 StartWhenAvailable로 catch-up 실행.

## Out of Scope (v1)
- PITR/WAL 아카이빙(추후), GUI, 멀티유저/RBAC, 물리 백업(pg_basebackup), 실시간 CDC.

## Open Questions
- DB 실제 대수/최대 크기(동시성·타임아웃 튜닝).
- 복원 테스트 주기(매 백업 vs 주간) 및 임시 인스턴스 실체(로컬/Docker).
- 알림 채널 실제 선택(webhook/email).
