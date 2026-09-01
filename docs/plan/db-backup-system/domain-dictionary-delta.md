# Domain Dictionary Delta

## v0 → v1 (Step 8 부산물)
- 신규 생성: 17개 규범 용어 시드(spec.md + interview.md 기반).
- 글로벌 사전 없음 → 프로젝트 사전만 생성.
- 주의 표기: Destination(목적지) vs 대상(DB) 혼용 금지, KEK vs 마스터키 구분.

## v1 → v2 (Step 10 부산물)
- ADD: Manifest, Globals 덤프, DumpArtifact, Spool-once fan-out, Commit Protocol, Immutability Capability, Verify Level, Origin Authentication, Restore-as-Exec, Confidentiality-Rotation Deadlock, Unenforced-Control, RPO/RTO (12개).
- REFINE: Append-only(신규 poisoned put 허용/prune 신원 도달 시 무효), DPAPI Machine-scope(로컬 코드 경계 아님), Encrypted config(메타데이터 유출).
- CONFLICT: 없음(3 전문가 상호 보완, 모순 없음).

## v2 → v3 (Step 11 부산물, 확정)
- CONFLICT 없음 → v2를 그대로 v3로 확정.
- 사용자 결정 반영: 리포=도구만 공개(키/설정 로컬 gitignore), 백업 서명=오프라인 ed25519 도입.
- Phase 4 이후 사전 동결. plan/db-schema/sections는 v3 준수.
