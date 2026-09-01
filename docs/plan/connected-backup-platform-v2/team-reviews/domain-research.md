# Domain Research — Database Backup Operations

## Domain Complexity

- Level: MEDIUM
- Why: 범용 개발도구이지만 native DB 도구, 외부 저장소, Windows 스케줄러, 복원 시
  destructive operation을 연결한다. 특정 산업 규제는 범위가 아니지만 데이터 손실 위험과
  엔진별 운영 의미가 크다.

## Standards / Regulations

| Name | Relevance | Source |
|------|-----------|--------|
| 각 DB vendor backup/restore semantics | 일관성·권한·도구 호환성의 진실 소스 | `research/web.md`의 공식 문서 목록 |
| S3 Object Lock semantics | 불변성 capability와 보존 정책 | AWS 공식 문서, 기존 v1 리서치 |
| Windows process/service lifecycle | dispatcher와 child process 종료·재시작 | Microsoft/Tokio 공식 문서 요약인 `research/web.md` |
| 조직별 privacy/compliance | 백업 데이터와 로그 보존에 영향 | 배포 환경마다 달라 제품 공통 규제로 확정하지 않음 |

## Existing Solutions / APIs

| Name | Type | Relevance | Source |
|------|------|-----------|--------|
| Databasement | 오픈소스 self-hosted 제품 | 정보구조, multi-engine, agent 기준선 | `research/github.md` |
| SQLBackupAndFTP | Windows 상용 제품 | 짧은 desktop 설정 동선과 목적지 폭 | `research/competitors.md` |
| Databasus | 오픈소스 백업 관리자 | restore verification과 PITR 강조 | `research/competitors.md` |
| GNIZA | Web/TUI backup manager | SSH pull과 rclone 목적지 확장 | `research/competitors.md` |
| Tokio | Rust async runtime | child process orchestration과 bounded blocking | `research/web.md` |

## Process Implications for Domain Process Expert

- Connection Profile 등록과 실제 probe는 저장 전후의 명확한 상태 전이가 필요하다.
- discovery 결과와 선택된 Database Target은 동일 목록이 아니다.
- Backup Plan은 선언이고 Backup Run은 실행 기록이므로 수정·삭제 규칙을 분리해야 한다.
- Snapshot은 commit protocol을 통과한 artifact만 의미한다.
- retry, cancel, stale lease, RPO violation은 성공/실패 이분법 밖의 운영 상태다.
- Restore Run은 읽기 권한, 대상 확인, eligibility gate, smoke validation을 포함한다.
- catalog rebuild는 장애 시 정식 복구 업무이며 숨은 CLI 유틸리티로만 두면 안 된다.

## Technical Implications for Domain Technical Expert

- native tool 버전과 server version compatibility를 engine matrix로 관리해야 한다.
- persistent queue의 lease, heartbeat, idempotency, recovery 규칙이 필요하다.
- OS thread보다 외부 process와 CPU/I/O/destination resource permit이 부하 모델의 핵심이다.
- cancellation은 child process tree와 partial artifact cleanup까지 포함한다.
- engine capability는 probe/discover/backup/verify/restore/validate 단위로 표현해야 한다.
- 운영 SQLite, 서명 Manifest, destination inventory 사이 truth ownership을 명시해야 한다.
- 신규 engine은 실제 restore testbed를 통과한 뒤 지원 상태로 승격해야 한다.

## Explicitly Skipped / Follow-up Needed

- 특정 산업 규제와 데이터 거주 요건은 배포 대상이 정해질 때 별도 검토한다.
- 수요가 확인되지 않은 cloud destination별 가격·API 비교는 이번 단계에서 반복하지 않았다.
- DB별 native tool 세부 옵션은 해당 engine wave의 공식 문서 조사로 제한한다.
- Step 5에서 이미 수행한 경쟁사와 Tokio 조사를 반복하지 않았다.

## Dictionary Updates

| 행위 | 용어 | 정의 (제안) | 근거 |
|------|------|-------------|------|
| ADD | Engine Matrix | 엔진·서버 버전·native tool·기능별 검증 상태 표 | Support Contract의 실제 증적 단위가 필요 |
| ADD | RPO Violation | 마지막 유효 Snapshot이 정책 RPO를 넘긴 운영 상태 | 실행 성공/실패와 독립된 판단 필요 |
| REFINE | Snapshot | commit protocol을 통과한 artifact와 Manifest의 논리 단위 | partial artifact 배제 명확화 |
