# Research Summary (통합)

상세: [research/rust-stack.md](research/rust-stack.md), [research/ransomware-resilience.md](research/ransomware-resilience.md)

## 확정 스택 (Rust)
| 관심사 | 선택 | 버전 | 비고 |
|--------|------|------|------|
| 스트리밍 암호화 | `age` (라이브러리 API) | 0.12.1 | STREAM(ChaCha20-Poly1305 64KiB 청크), `finish()` 필수 |
| KEK 유도 | `argon2` (Argon2id) | 0.5.3 | m=64MiB,t=3,p=1 권장. params 영속화 필수 |
| OS 키저장소 | `windows-dpapi` (Machine scope + entropy) | - | 무인 실행 견고. keyring 4.x는 옵션 |
| SSH 터널 | `russh` | 0.62.2 | 순수 Rust, Windows 빌드 마찰 없음, direct-tcpip |
| 오브젝트 스토리지 | `opendal` | 0.58.0 | S3+로컬. **SFTP는 Windows 빌드 불가 → russh로** |
| 압축 | `zstd` | 0.13.3 | 스트리밍 Encoder, `finish()` 필수 |
| CLI/설정 | `clap`+`serde`+`toml` | clap 4.6 | derive API |
| 스케줄 | Windows Task Scheduler (`schtasks` XML) | 내장 | `StartWhenAvailable=true`로 재부팅 후 catch-up |

## 핵심 설계 결론
1. **봉투암호화 = age recipient 모드 + 명시적 DEK**. 랜덤 32B DEK로 dump 암호화, DEK를 3경로로 wrapping:
   (a) Argon2id KEK(비번), (b) 복구 코드, (c) DPAPI Machine-scope(무인). age의 multi-recipient로 자연 구현.
2. **파이프라인**: `dump stdout → zstd::Encoder → age StreamWriter → opendal writer`. 각 단계 `finish()` 필수(누락 시 복호화 불가 truncation).
3. **pg_dump 버전 규칙**: 클라이언트 >= 서버(하위호환O, 상위호환X). 번들은 최신 서버 이상 버전으로.
4. **mysqldump**: `--single-transaction --routines --triggers --events` (InnoDB 일관성). MyISAM/덤프중 DDL은 비일관 gotcha. 대용량은 mydumper 옵션(디렉토리 출력이라 파이프 모델 이탈).
5. **Object Lock**: 버킷 레벨 기본 보존(Governance) 설정 → OpenDAL이 쓰는 모든 객체가 불변성 상속. Object Lock 시 checksum 필수(OpenDAL `checksum_algorithm` 설정).

## 랜섬웨어 내성 결론 (ransomware-resilience.md)
1. **append-only 자격증명**: 백업 writer는 PutObject만, Delete*/lifecycle는 명시적 Deny. prune/lifecycle는 **별도 격리 신원**(그러지 않으면 append-only 무의미).
2. **pull 토폴로지**: 백업 호스트가 DB에서 당겨오기. 백업 호스트 침해 시 blast radius 최소화.
3. **복원 테스트 = "+0"**: 체크섬은 bit-rot만 잡음. 예약 test-restore만이 복구 가능성 증명. 3-2-1-1-0 원칙.
4. **암호화는 기밀성일 뿐 랜섬웨어 방어 아님** — 별개 관심사(불변 스토리지가 방어).
5. 개인 운영자엔 Governance + 격리 bypass 신원(또는 짧은 14~35일 Compliance) 균형이 자기잠금 위험 대비 적절.

## Build-time 확인 필요 [확인 필요]
- OpenDAL 0.58 S3 per-object retention 헤더 노출 여부(vs 버킷 기본 보존 의존).
- keyring 4.x store 크레이트명 핀.
- russh 0.62 bastion 터널 예제 + host-key 정책.
