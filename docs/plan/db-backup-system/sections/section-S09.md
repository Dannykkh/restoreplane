> 완성한 첫 리비전. 다른 섹션 작성 시 이 문서를 참조/변경할 수 있음.

# S09 — Restorer + Verifier (샌드박스 / L1-L3)

> 의존: S05(Pipeline — 인코딩 체인의 역방향 복호), S08(Catalog/Manifest — 서명 게이트/진실 소스).
> 관련 리뷰: red-team.md C3(Restore-as-Exec RCE, Origin Authentication), domain-technical.md §4(검증 레벨),
> db-schema.md(verifications 테이블, manifest verify 필드), plan.md "복원 격리 샌드박스" / "DT 검증레벨".

## Background

이 섹션은 백업 시스템의 **마지막이자 가장 위험한 경로**를 다룬다. `red-team.md` C3은 이 경로를 두 층의
공격면으로 규정한다.

1. **Restore-as-Exec (RCE)**: 논리 덤프의 복원은 수동적인 데이터 적재가 아니라 **코드 실행**이다.
   PostgreSQL의 `COPY ... FROM PROGRAM 'cmd'`, `CREATE FUNCTION ... LANGUAGE plpython3u/plperlu`,
   psql 메타커맨드 `\!`(backslash-bang), MySQL의 `LOAD DATA LOCAL INFILE`, `INTO OUTFILE`가 전부 복원
   호스트에서 OS 명령을 실행한다. **검증(Verifier)이 스케줄에 따라 "최신 백업을 자동 복원"하므로,
   오염된 덤프 하나가 무인 상태에서 RCE로 이어진다.**
2. **Origin Authentication 부재**: age STREAM(대칭 AEAD)은 비트플립에 대한 **무결성**만 보장할 뿐,
   **누가 그 백업을 만들었는가(출처)**를 증명하지 못한다. DEK를 쥔 공격자(C1/C2)나 append-only
   목적지에 `PutObject`할 수 있는 온박스 공격자는 **완벽하게 유효한(복호되는) 악성 백업**을 만들어
   "최신"으로 선택되게 할 수 있다.

그래서 이 섹션의 복원/검증은 "복호가 깨끗하다 = 신뢰할 수 있다"는 등식을 **거부**한다. 두 개의 독립
방어선을 강제한다.

- **출처 인증 게이트 (C3-a)**: 복원/검증을 시작하기 전 **반드시** 목적지에서 매니페스트를 다시 읽어
  오프라인 ed25519 개인키로 만든 서명을 검증한다. **미서명·서명 불일치 매니페스트는 즉시 거부**하며,
  로컬 SQLite 카탈로그의 어떤 값도 복원 실행의 근거로 쓰지 않는다(S08 `load_and_verify_manifest_for_restore`
  재사용, H5). 서명이 유효한 백업만이 "우리 백업 호스트가 만든 것"으로 신뢰된다.
- **격리 샌드박스 (C3-b)**: 서명이 유효해도 덤프 내용 자체는 **적대적 입력**으로 취급한다. 실제 복원은
  매니페스트 `server_version`에 맞춘 **일회용 Docker 컨테이너** 안에서만 수행하며, 아웃바운드 네트워크
  차단, 비특권 DB 유저, `local_infile=0`, `COPY FROM PROGRAM` 차단, `pg_restore --no-privileges`(절대
  슈퍼유저 아님), 신뢰 불가 덤프를 **대화형 psql로 파이프하지 않음** 규칙을 강제한다. 서명 게이트가
  위조·치환을 막고, 샌드박스가 만약의 실행을 봉쇄하는 **심층 방어(defense-in-depth)**다.

**Verifier는 Restorer + 단언(assertions)**이다. 같은 복호·복원 기계 위에 검증 레벨별 단언을 얹는다.
`domain-technical.md` §4의 3단계 검증 레벨을 그대로 구현한다.

- **L1 파일 무결성**: 재복호(age/chacha20) + 재압축해제(zstd)가 에러 없이 끝까지 되고, 재계산한
  blake3가 매니페스트 `plaintext_blake3`와 일치. 비용 거의 없음 → **매 백업 직후 자동**.
- **L2 포맷 유효성**: `pg_restore --list`로 custom 포맷 TOC 파싱(데이터를 풀지 않고 헤더/목차 손상만
  확인). 비용 낮음 → **매 백업 직후 자동**.
- **L3 실제 복원**: 샌드박스에 실제 복원 → (a) 스키마 해시 대조, (b) 테이블별 row count 대조. **비교
  대상은 라이브 프로덕션 DB가 아니라 매니페스트에 백업 시점 캡처된 값**이다(원본은 백업 후 계속
  바뀌므로). 비용 큼 → **주 1회 + 스키마 변경 감지 시 즉시**.

**"verified"라는 단어는 절대 단독으로 쓰지 않는다** — 항상 레벨과 함께 기록한다("verified(L1)").
그리고 L3 검증에 쓴 임시 인스턴스 버전(`restore_target_version`)을 항상 기록해 원본과의 **버전 갭을
투명하게** 남긴다(도메인 리뷰 §4.3, db-schema.md `verifications.restore_target_version`).

## Requirements

### 기능 요구사항 (FR)

- **FR1 (서명 게이트, C3)** 복원·검증 진입점은 S08 `load_and_verify_manifest_for_restore`를 통해 목적지
  원본 매니페스트의 ed25519 서명을 재검증한 뒤에만 진행한다. **미서명/서명 불일치는 `Err`로 즉시 중단**
  (경고 후 통과 절대 금지). 카탈로그 SQLite 값은 후보 나열/힌트로만 쓰고 복원 판단 근거로 쓰지 않는다.
- **FR2 (복호 체인)** S05 인코딩 체인(`평문 → blake3 tee → zstd → age(DEK) → 스풀`)을 **정확히 역순**으로
  복호한다: `암호문 → age/chacha20 복호 → zstd 디코드 → (Directory면 untar) → 평문`. 복호 체인은 S05의
  인코딩 구현과 **반드시 대칭**이어야 하며, 청크 프레이밍·nonce prefix 상수를 공유한다.
- **FR3 (DEK unwrap 3경로)** S02 `SecretManager::unwrap(&WrapSet, &Credential)`로 DEK를 복구한다.
  `Credential`은 CLI 서브셰이프에서 결정한다: 기본(무인) = `Dpapi`, 대화형 비번 = `Password`,
  `--with-recovery-code` = `Recovery`. S09는 DEK 원본 바이트를 직접 만들지 않는다(P0-5 분리).
- **FR4 (복원 순서 globals→db, PG)** PostgreSQL은 `pairs_with_globals_backup_id`로 짝지어진 globals 덤프를
  **먼저** 복원(role/tablespace 생성)한 뒤 DB 덤프를 복원한다. 순서 위반 시 role 부재로 복원이 실패하므로
  `RestorePlan`이 순서를 강제한다(domain-technical.md §1.2, S03 R12).
- **FR5 (샌드박스, C3)** L3 실제 복원과 `restore`의 기본 대상은 **일회용 Docker 컨테이너**다.
  매니페스트 `server_version` 메이저에 맞춘 이미지 선택, `--network none`, 비특권(`--cap-drop ALL`
  `--security-opt no-new-privileges`), 호스트 바인드 마운트로 신뢰 불가 덤프를 넣지 않음(stdin 파이프),
  비슈퍼유저 복원 role, PG `pg_restore --no-privileges`, MySQL `--local-infile=0`, 위험 SQL 사전 스캔.
- **FR6 (검증 레벨)** `verify --level {1|2|3}`을 구현한다. 결과는 `verifications` 테이블(db-schema.md)에
  `level`, `result(pass|fail)`, `restore_target_version`, `restored_rows`, `schema_hash`, `notes`로 기록.
  매니페스트 `verify` 블록도 갱신(레벨·method·row_counts·restore_target_version).
- **FR7 (검증 주기)** L1/L2는 매 백업 직후 자동 수행. L3는 각 job당 주 1회 + **스키마 변경 감지 시 즉시**
  (직전 백업 매니페스트 `schema_hash`와 비교해 달라지면 트리거).
- **FR8 (restore CLI 서브셰이프)** `restore`는 다음 인자 조합을 지원한다:
  `--backup <id>`(카탈로그의 후보 지정), `--from-destination <name>`(목적지에서 직접, 카탈로그 무시),
  `--to <docker|dir>`(샌드박스 복원 vs 디렉토리 산출), `--decrypt-only <path>`(복원 없이 평문 덤프만 산출),
  `--with-recovery-code`(비번 대신 복구코드 경로로 unwrap).

### 비기능 요구사항 (NFR)

- **봉쇄 우선(containment-first)**: L3/복원의 어떤 코드 경로도 신뢰 불가 덤프를 백업 호스트의 셸/네트워크에
  노출하지 않는다. 실패 시 컨테이너를 **항상** 강제 제거(`docker rm -f`)한다.
- **평문 비유출(M2)**: `--decrypt-only`를 제외하면 복호된 평문 덤프는 **디스크에 떨어지지 않고** stdin
  파이프로만 컨테이너에 전달한다. `--decrypt-only`는 명시적 경로 + 경고 + 제한 ACL을 요구한다.
- **버전 갭 투명성**: 샌드박스 버전이 원본과 다르면 "다른 버전으로 검증됨"을 결과와 알림에 반드시 표기.
- **결정성/재현성**: 스키마 해시는 정규화된 `--schema-only` 덤프에 대해 계산해 공백/순서 차이로 인한
  오탐을 없앤다.

## Dependencies

- **S05 (Pipeline)**: 인코딩 체인(`HashingWriter → zstd::Encoder → age::StreamWriter → 스풀`)의 **역방향**을
  구현하려면 S05가 최종 확정한 payload 암호화 방식(age recipient vs chacha20poly1305 STREAM)과 청크
  프레이밍·nonce prefix 상수를 공유해야 한다. 본 섹션은 S05의 `EncryptChain`과 대칭인 `DecryptChain`을
  제공하며, 둘은 **같은 상수 모듈**(`crate::pipeline::consts`)을 참조한다. S05가 age 경로면 age Decryptor,
  chacha20 STREAM이면 `DecryptorBE32`를 쓴다(아래 9.2에서 두 경로 모두 제시, `[확인 필요]`).
- **S08 (Catalog/Manifest)**: 서명 게이트(`load_and_verify_manifest_for_restore`), 매니페스트 타입(`Manifest`,
  `VerifyInfo`), `ManifestSource`/`ManifestVerifier` 트레이트, 복원 후 검증 결과를 카탈로그에 기록하는
  writer 액터(`CatalogWriterHandle`)를 그대로 소비한다. **S09는 이 계약들을 소비만 하고 재정의하지 않는다.**
- **S02 (SecretManager)**: `SecretManager::unwrap(&WrapSet, &Credential) -> Result<Dek>`, `Credential` enum,
  `Dek::payload_decryptor(nonce_prefix)`(chacha20 STREAM 경로) 또는 DEK→X25519 identity(age 경로).
  **WrapSet은 매니페스트와 함께 목적지에 동봉되어 서명 대상**이므로(S02), 서명 게이트를 통과한 백업의
  WrapSet만 신뢰한다.
- **S03/S04 (BackupSource)**: 매니페스트에 **백업 시점** `schema_hash`와 `source_row_counts`(테이블별)를
  채워 넣는 주체다. L3의 비교 기준값이 이 필드들이다. 이 필드는 db-schema.md의 매니페스트에 대한 **S09
  소비 확장 필드**로, `#[serde(default)]`로 하위호환 파싱한다(없으면 L3b는 "복원본 카운트 기록만" 하고
  비교는 skip + `notes`에 명시). 본 섹션은 이 필드를 **소비**하며, 채우는 책임은 S03/S04에 있다(계약 고정).
- **S01 (CLI/Config)**: `restore`/`verify` 서브커맨드의 clap 파싱과 Docker 실행 파일 경로·이미지 매핑 정책을
  config에서 받는다. 본 섹션은 핸들러 로직만 구현한다.
- **S06/S07 (Destination)**: `ManifestSource`/객체 다운로드 구현체. walking skeleton(PG→Local)에서는 S08의
  `LocalManifestSource`와 로컬 파일 읽기로 충분하며, S3/SSH는 동일 트레이트로 나중에 주입된다(순환 회피,
  S08과 동일 원칙).

## Reference Libraries

| 크레이트 | 용도 | 비고 |
|---|---|---|
| `age` (`=0.12.1`) | payload 복호(age 경로): `Decryptor` → `StreamReader` | S05와 동일 핀. 0.12 `decrypt` 시그니처 `[확인 필요]` |
| `chacha20poly1305` (`0.10`, `stream`) | payload 복호(대안 경로): `DecryptorBE32` | S02 확정 기본 경로. S05 인코딩과 청크 상수 공유 필수 |
| `zstd` (`=0.13.3`) | `stream::read::Decoder<R>` 스트리밍 압축해제 | S05 인코딩과 동일 핀 |
| `blake3` (`1.x`) | L1 평문 재해시 → `plaintext_blake3` 대조 | S05/S08과 알고리즘 통일 |
| `tar` (`0.4`) | Directory 아티팩트(`-Fd`/mydumper) untar | S05가 tar로 말아 스트림화한 것의 역 |
| `tokio` (`process`, `io-util`, `rt-multi-thread`, `fs`) | `docker` CLI 구동, stdin 파이프, 컨테이너 헬스 폴링 | `docker` 크레이트 대신 CLI 호출(의존 최소화, 배포 단순) |
| `serde` / `serde_json` | 매니페스트/검증 결과 직렬화 | S08과 동일 |
| `uuid` (`v4`) | `verifications.id`, 컨테이너 이름 유일성 | |
| `time` (`serde-well-known`) | `verified_at`(RFC3339) | S08과 동일 |
| `zeroize` (`1.x`) | 복호 평문/DEK Drop 소거 | S02와 동일. `--decrypt-only` 외 평문은 메모리 스코프 최소화 |
| `thiserror` / `anyhow` | 에러 타입 | |

`Cargo.toml` 발췌:

```toml
[dependencies]
age = { version = "=0.12.1", features = ["armor"] }
chacha20poly1305 = { version = "0.10", features = ["stream"] }
zstd = "=0.13.3"
blake3 = "1"
tar = "0.4"
tokio = { version = "1", features = ["process", "io-util", "rt-multi-thread", "macros", "fs"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
uuid = { version = "1", features = ["v4"] }
time = { version = "0.3", features = ["formatting", "parsing", "serde-well-known"] }
zeroize = "1"
thiserror = "1"
anyhow = "1"

[dev-dependencies]
tempfile = "3"
ed25519-dalek = { version = "2", features = ["rand_core"] } # 테스트 서명자(S08과 공유)
```

> `age`/`zstd`/`chacha20poly1305` 등 security-critical 크레이트의 `=x.y.z` 핀과 `cargo-deny`/`cargo-audit`
> 게이트는 S11 소관. Docker 자체는 크레이트가 아니라 외부 런타임 의존이며, `doctor`(S01)가 `docker version`
> 가용성을 사전 점검한다.

## Implementation

### 9.1 복원 입력 로드 + 출처 인증 게이트 (C3-a, FR1)

복원/검증의 **모든** 진입점이 통과하는 단일 게이트다. 카탈로그는 "어떤 백업을 복원할지" 후보를 고르는
UX에만 쓰이고, 실제 신뢰 판단은 목적지에서 새로 읽은 서명된 바이트에 대해서만 내린다.

```rust
// src/restore/inputs.rs
use crate::manifest::restore_gate::load_and_verify_manifest_for_restore; // S08
use crate::manifest::sign::{ManifestError, ManifestVerifier};
use crate::manifest::source::ManifestSource;
use crate::manifest::types::Manifest;
use crate::secret::wrap::WrapSet; // S02
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RestoreInputError {
    #[error("서명 게이트 실패(미서명/위조/치환 의심): {0}")]
    Signature(#[from] ManifestError),
    #[error("WrapSet 로드/서명 커버리지 실패 — DEK unwrap 신뢰 불가")]
    WrapSetUntrusted,
    #[error("목적지 I/O 실패: {0}")]
    Io(String),
    #[error("globals 짝을 찾을 수 없음(pairs_with_globals_backup_id={0})")]
    MissingGlobals(String),
}

/// 서명 검증을 통과한, 복원에 필요한 모든 신뢰된 입력.
pub struct RestoreInputs {
    /// 서명 검증 완료된 DB 매니페스트.
    pub db_manifest: Manifest,
    /// PG: 짝지어진 globals 매니페스트(서명 검증 완료). MySQL/globals-less면 None.
    pub globals_manifest: Option<Manifest>,
    /// 매니페스트와 함께 동봉·서명 커버된 DEK wrap 스탠자 묶음(S02).
    pub wrap_set: WrapSet,
}

/// 복원/검증 시작점. object_key(사용자 지정 또는 카탈로그 힌트)만 입력으로 받고,
/// 실제 신뢰 판단은 여기서 목적지에서 새로 읽은 바이트에 대해서만 수행한다(H5).
pub async fn load_trusted_inputs(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    db_manifest_key: &str,
) -> Result<RestoreInputs, RestoreInputError> {
    // (1) DB 매니페스트: 서명 검증 통과해야만 반환됨(미서명/위조는 여기서 Err).
    let db_manifest =
        load_and_verify_manifest_for_restore(source, verifier, db_manifest_key).await?;

    // (2) PG globals 짝: DB 매니페스트가 지목한 globals도 독립적으로 서명 검증.
    let globals_manifest = match &db_manifest.pairs_with_globals_backup_id {
        Some(gid) => {
            let gkey = globals_key_for(source, gid).await?;
            Some(load_and_verify_manifest_for_restore(source, verifier, &gkey).await?)
        }
        None => None,
    };

    // (3) WrapSet: 매니페스트와 동봉·서명 커버된 사이드카. 서명 게이트를 통과한 매니페스트가
    //     참조하는 것만 신뢰한다. WrapSet 바이트의 blake3가 매니페스트에 핀되어 있으므로,
    //     서명이 유효 == WrapSet도 위조되지 않음(간접 커버리지).
    let wrap_set = load_wrapset_verified(source, &db_manifest).await?;

    Ok(RestoreInputs { db_manifest, globals_manifest, wrap_set })
}

async fn globals_key_for(
    source: &dyn ManifestSource,
    globals_backup_id: &str,
) -> Result<String, RestoreInputError> {
    // globals 매니페스트 키 규약: 목적지에서 backup_id로 매니페스트를 조회.
    // (S06/S07 완성 후에는 인덱스 조회로 대체 가능. walking skeleton은 전량 나열 후 매칭.)
    let all = source
        .list_manifests(None)
        .await
        .map_err(|e| RestoreInputError::Io(e.to_string()))?;
    for k in all {
        let bytes = source
            .read_manifest_bytes(&k)
            .await
            .map_err(|e| RestoreInputError::Io(e.to_string()))?;
        if let Ok(m) = serde_json::from_slice::<Manifest>(&bytes) {
            if m.backup_id == globals_backup_id {
                return Ok(k);
            }
        }
    }
    Err(RestoreInputError::MissingGlobals(globals_backup_id.to_string()))
}

async fn load_wrapset_verified(
    source: &dyn ManifestSource,
    db_manifest: &Manifest,
) -> Result<WrapSet, RestoreInputError> {
    // 규약: <object_key>.wrapset.json 사이드카. 매니페스트 encryption.dek_gen과 일치해야 하고,
    // 바이트 blake3가 매니페스트에 핀된 값과 일치해야 한다(서명이 그 핀 값을 커버 → 간접 인증).
    let wrapset_key = format!("{}.wrapset.json", db_manifest.destination.object_key);
    let bytes = source
        .read_manifest_bytes(&wrapset_key)
        .await
        .map_err(|_| RestoreInputError::WrapSetUntrusted)?;
    let hash = blake3::hash(&bytes).to_hex().to_string();
    if Some(&hash) != db_manifest.wrapset_blake3.as_ref() {
        return Err(RestoreInputError::WrapSetUntrusted); // 핀 불일치 = 치환 의심
    }
    let ws: WrapSet =
        serde_json::from_slice(&bytes).map_err(|_| RestoreInputError::WrapSetUntrusted)?;
    if ws.dek_wrap_gen != db_manifest.encryption.dek_gen as u32 {
        return Err(RestoreInputError::WrapSetUntrusted);
    }
    Ok(ws)
}
```

> `Manifest`에 `wrapset_blake3: Option<String>` 필드가 필요하다 — S08의 `manifest_seq` 확장과 동일하게
> `#[serde(default)]`로 추가하며, 이 값은 S05/S08이 매니페스트 서명 전에 채운다(서명이 이 핀을 커버해야
> WrapSet 치환이 봉쇄됨). 이는 db-schema.md 대비 **S09가 요구하는 확장 필드**이며 문서 동기화 대상이다.

### 9.2 복호 체인 — age/chacha20 복호 → zstd 디코드 → (untar) (FR2)

S05 인코딩 체인의 **정확한 역순**이다. `age`/`zstd`는 동기 `std::io::Read` API이고 덤프를 컨테이너
stdin으로 흘려야 하므로, 복호는 **동기 리더**로 구성한 뒤 `spawn_blocking` 또는 전용 스레드에서 pump한다.

```rust
// src/restore/decrypt.rs
use crate::secret::dek::Dek;        // S02
use crate::pipeline::consts;        // S05와 공유: NONCE_PREFIX, PAYLOAD_CHUNK 등
use std::io::{self, Read};

/// 목적지 스풀/오브젝트(암호문)를 열어 평문(zstd 디코드 후) Read를 반환한다.
/// 반환된 Read를 끝까지 소비하면 곧 "복호 + 압축해제"가 완료된다.
///
/// 경로 A(chacha20 STREAM, S02 확정 기본): DecryptorBE32로 프레임 단위 복호.
/// 경로 B(age recipient, S05가 age를 최종 채택한 경우): age::Decryptor → StreamReader.
/// 두 경로 모두 S05 인코딩과 **대칭**이어야 하므로 청크 상수/nonce prefix를 pipeline::consts에서 공유한다.
pub fn open_plaintext_reader<R: Read + Send + 'static>(
    cipher: R,
    dek: &Dek,
) -> io::Result<Box<dyn Read + Send>> {
    // ---- 경로 A: chacha20poly1305 STREAM (기본) ----
    let stream_reader = StreamDecryptReader::new(cipher, dek.payload_decryptor(&consts::NONCE_PREFIX));
    let zstd_reader = zstd::stream::read::Decoder::new(stream_reader)?; // 압축해제
    Ok(Box::new(zstd_reader))

    // ---- 경로 B(대안): age ----
    // let dec = age::Decryptor::new(cipher).map_err(to_io)?;
    // let identities = [dek_to_age_identity(dek)]; // [확인 필요] DEK→x25519 identity 구성
    // let age_reader = match dec {
    //     age::Decryptor::Recipients(d) => d.decrypt(identities.iter().map(|i| i as _)).map_err(to_io)?,
    //     _ => return Err(io::Error::new(io::ErrorKind::InvalidData, "예상치 못한 age 모드")),
    // };
    // Ok(Box::new(zstd::stream::read::Decoder::new(age_reader)?))
}

/// chacha20poly1305 STREAM(BE32) 복호를 std::io::Read로 노출하는 어댑터.
/// 인코더가 쓴 것과 **동일한 고정 청크 크기**로 암호문을 읽어 순차 복호한다(마지막 청크만 decrypt_last).
struct StreamDecryptReader<R: Read> {
    inner: R,
    dec: Option<chacha20poly1305::aead::stream::DecryptorBE32<chacha20poly1305::ChaCha20Poly1305>>,
    plainbuf: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read> StreamDecryptReader<R> {
    fn new(
        inner: R,
        dec: chacha20poly1305::aead::stream::DecryptorBE32<chacha20poly1305::ChaCha20Poly1305>,
    ) -> Self {
        Self { inner, dec: Some(dec), plainbuf: Vec::new(), pos: 0, eof: false }
    }

    /// 다음 암호문 청크(고정 크기 + 16B 태그)를 읽어 plainbuf를 채운다.
    fn fill(&mut self) -> io::Result<()> {
        const CT_CHUNK: usize = consts::PAYLOAD_CHUNK + 16; // 평문청크 + Poly1305 태그
        let mut ctbuf = vec![0u8; CT_CHUNK];
        let n = read_full_or_eof(&mut self.inner, &mut ctbuf)?;
        let dec = self.dec.take().expect("STREAM 복호기 이미 소진");
        if n == CT_CHUNK {
            // 아직 마지막 아님: decrypt_next로 이어감.
            let pt = dec
                .decrypt_next(&ctbuf[..n])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "STREAM 청크 인증 실패"))?;
            // decrypt_next는 &mut self 소비 형태 API에 따라 재보관 필요 — [확인 필요] 0.10 시그니처.
            self.dec = Some(dec_after(dec_placeholder())); // 실제 구현은 아래 주석 참조
            self.plainbuf = pt;
        } else {
            // 마지막 청크: decrypt_last로 STREAM 종료. 여기서 조기 EOF(절단)면 인증 실패로 잡힘.
            let pt = dec
                .decrypt_last(&ctbuf[..n])
                .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "STREAM 마지막 청크 인증 실패(절단?)"))?;
            self.plainbuf = pt;
            self.eof = true;
        }
        self.pos = 0;
        Ok(())
    }
}

impl<R: Read> Read for StreamDecryptReader<R> {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.pos < self.plainbuf.len() {
                let n = std::cmp::min(out.len(), self.plainbuf.len() - self.pos);
                out[..n].copy_from_slice(&self.plainbuf[self.pos..self.pos + n]);
                self.pos += n;
                return Ok(n);
            }
            if self.eof {
                return Ok(0);
            }
            self.fill()?;
        }
    }
}

fn read_full_or_eof<R: Read>(r: &mut R, buf: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    Ok(filled)
}
```

> 위 `StreamDecryptReader::fill`의 `decrypt_next` 재보관은 `chacha20poly1305 0.10` STREAM API의 정확한
> 소유권 형태에 맞춰 조정한다(`[확인 필요]` — `EncryptorBE32`/`DecryptorBE32`는 청크마다 카운터를 진행하며
> 일부 버전은 `&mut self`, 일부는 소비형이다). 핵심 계약은 불변이다: **고정 청크 프레이밍은 S05 인코더와
> 반드시 동일**하고, **마지막 청크는 `decrypt_last`로만** 처리해 절단을 인증 실패로 검출한다.

Directory 아티팩트(`pg_dump -Fd` / mydumper — S05가 tar로 말아 스트림화한 경우)는 위 평문 Read 위에
`tar::Archive`를 얹어 임시 디렉토리로 풀어낸다. 이 임시 디렉토리는 **컨테이너 안**에 만들거나(권장),
불가피하게 호스트에 만들면 제한 ACL + 사용 후 안전 삭제한다(M2).

```rust
// src/restore/decrypt.rs (계속)
pub enum PlaintextDump {
    /// custom/plain: 단일 스트림. 컨테이너 stdin으로 파이프.
    Stream(Box<dyn Read + Send>),
    /// directory: untar된 디렉토리(컨테이너 내부 경로 권장).
    Directory(std::path::PathBuf),
}

pub fn materialize_dump(
    plaintext: Box<dyn Read + Send>,
    dump_format: &str,
    untar_into: Option<&std::path::Path>,
) -> io::Result<PlaintextDump> {
    match dump_format {
        "directory" => {
            let dir = untar_into.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "directory 포맷엔 untar 경로 필요")
            })?;
            let mut ar = tar::Archive::new(plaintext);
            ar.set_preserve_permissions(false);
            ar.unpack(dir)?;
            Ok(PlaintextDump::Directory(dir.to_path_buf()))
        }
        _ => Ok(PlaintextDump::Stream(plaintext)), // custom | plain
    }
}
```

### 9.3 DEK unwrap — Credential 선택 (FR3)

```rust
// src/restore/unlock.rs
use crate::secret::{Credential, Dek, SecretError, SecretManager}; // S02
use crate::secret::wrap::WrapSet;
use zeroize::Zeroizing;

/// CLI 서브셰이프 → Credential 매핑.
pub enum UnlockMode {
    Unattended,                      // 기본: DPAPI(무인)
    Password(Zeroizing<String>),     // 대화형 비번 입력
    Recovery(Zeroizing<String>),     // --with-recovery-code
}

pub fn unwrap_dek(
    sm: &SecretManager,
    wrap_set: &WrapSet,
    mode: UnlockMode,
) -> Result<Dek, SecretError> {
    let cred = match mode {
        UnlockMode::Unattended => Credential::Dpapi,
        UnlockMode::Password(pw) => Credential::Password(pw),
        UnlockMode::Recovery(code) => Credential::Recovery(code),
    };
    // S02가 우선순위(DPAPI→비번→복구코드)로 사용 가능한 스탠자를 찾아 DEK 복구.
    sm.unwrap(wrap_set, &cred)
}
```

### 9.4 Restorer(기계) + 복원 순서 globals→db (FR4)

```rust
// src/restore/restorer.rs
use crate::restore::decrypt::{materialize_dump, open_plaintext_reader, PlaintextDump};
use crate::restore::inputs::RestoreInputs;
use crate::restore::sandbox::{Sandbox, SandboxSpec};
use crate::manifest::source::ManifestSource;
use crate::secret::Dek;
use std::io::Read;

/// 복원 대상: 샌드박스(기본/검증) 또는 평문 산출(디렉토리/파일).
pub enum RestoreTarget {
    Sandbox,                              // 일회용 Docker 컨테이너(C3)
    DecryptOnly(std::path::PathBuf),      // 복원 없이 평문 덤프만 저장(escape hatch, M2 경고)
}

pub struct Restorer<'a> {
    pub source: &'a dyn ManifestSource,
    pub dek: Dek,
    pub docker_bin: std::path::PathBuf, // config에서
    pub image_policy: crate::restore::sandbox::ImagePolicy,
}

impl<'a> Restorer<'a> {
    /// 복원 순서를 강제한다: PG는 globals 먼저 → DB 나중. MySQL은 DB만.
    pub async fn restore(
        &self,
        inputs: &RestoreInputs,
        target: RestoreTarget,
    ) -> anyhow::Result<RestoreOutcome> {
        match target {
            RestoreTarget::DecryptOnly(path) => self.decrypt_only(inputs, &path).await,
            RestoreTarget::Sandbox => self.restore_into_sandbox(inputs).await,
        }
    }

    async fn restore_into_sandbox(&self, inputs: &RestoreInputs) -> anyhow::Result<RestoreOutcome> {
        let spec = SandboxSpec::from_manifest(&inputs.db_manifest, &self.image_policy)?;
        // 컨테이너 기동(비특권 + --network none). Drop 시 항상 rm -f.
        let sandbox = Sandbox::launch(&self.docker_bin, spec).await?;

        // (1) PG globals 먼저 — role/tablespace 없으면 DB 복원이 실패(FR4).
        if let Some(gm) = &inputs.globals_manifest {
            let g_plain = self.open_dump(gm).await?; // plain SQL 스트림
            // 위험 SQL 사전 스캔 후, 샌드박스 내부에서만 적용(대화형 psql 금지).
            sandbox.apply_globals_sql(g_plain).await?;
        }

        // (2) DB 덤프 — custom 포맷은 pg_restore --no-privileges(절대 슈퍼유저 아님)로 stdin 파이프.
        let db_plain = self.open_dump(&inputs.db_manifest).await?;
        sandbox.restore_db_dump(db_plain, &inputs.db_manifest).await?;

        let target_version = sandbox.server_version().await?;
        Ok(RestoreOutcome {
            restore_target_version: target_version,
            sandbox: Some(sandbox), // 검증 단계가 이어서 사용(Verifier). Drop 시 정리.
        })
    }

    /// 매니페스트가 가리키는 암호문 오브젝트를 열어 복호 평문 스트림/디렉토리를 만든다.
    async fn open_dump(
        &self,
        m: &crate::manifest::types::Manifest,
    ) -> anyhow::Result<PlaintextDump> {
        // 목적지에서 암호문 오브젝트를 읽는다(S06/S07; skeleton은 로컬 파일).
        let cipher_bytes = self
            .source
            .read_manifest_bytes(&m.destination.object_key) // 실제로는 read_object; 대칭 트레이트
            .await?;
        let dek = self.dek.clone();
        let fmt = m.dump_format.clone();
        // 동기 복호는 blocking 스레드에서.
        let plain: PlaintextDump = tokio::task::spawn_blocking(move || {
            let reader = open_plaintext_reader(std::io::Cursor::new(cipher_bytes), &dek)?;
            materialize_dump(reader, &fmt, None)
        })
        .await??;
        Ok(plain)
    }

    async fn decrypt_only(
        &self,
        inputs: &RestoreInputs,
        out_path: &std::path::Path,
    ) -> anyhow::Result<RestoreOutcome> {
        tracing::warn!(
            path = %out_path.display(),
            "--decrypt-only: 평문 덤프가 디스크에 기록됩니다(M2). 제한 ACL 경로를 사용하고 사용 후 삭제하세요."
        );
        let plain = self.open_dump(&inputs.db_manifest).await?;
        match plain {
            PlaintextDump::Stream(mut r) => {
                let mut f = std::fs::File::create(out_path)?;
                std::io::copy(&mut r, &mut f)?;
            }
            PlaintextDump::Directory(dir) => {
                // 디렉토리 포맷은 그대로 이동/복사.
                copy_dir_recursive(&dir, out_path)?;
            }
        }
        Ok(RestoreOutcome { restore_target_version: "n/a(decrypt-only)".into(), sandbox: None })
    }
}

pub struct RestoreOutcome {
    pub restore_target_version: String,
    pub sandbox: Option<Sandbox>,
}
```

### 9.5 샌드박스(Docker) 기동 (C3-b, FR5)

`docker` CLI를 `tokio::process`로 구동한다. 신뢰 불가 덤프는 **호스트 바인드 마운트가 아니라 stdin 파이프**로
컨테이너에 넣어, 백업 호스트 파일시스템/네트워크에 노출하지 않는다.

```rust
// src/restore/sandbox.rs
use crate::restore::decrypt::PlaintextDump;
use crate::manifest::types::Manifest;
use tokio::io::AsyncWriteExt;
use tokio::process::Command;

pub struct ImagePolicy {
    /// 엔진+메이저 → 이미지 태그(config에서 매핑). 예: ("postgresql",16) -> "postgres:16".
    pub map: std::collections::BTreeMap<(String, u32), String>,
}

pub struct SandboxSpec {
    pub image: String,
    pub engine: String,     // postgresql | mysql | mariadb
    pub server_major: u32,
    pub container_name: String,
    pub db_name: String,
    pub restricted_role: String, // 비슈퍼유저 복원 role(FR5)
}

impl SandboxSpec {
    pub fn from_manifest(m: &Manifest, policy: &ImagePolicy) -> anyhow::Result<Self> {
        let major = parse_major(&m.server_version)?; // 예: "PostgreSQL 16.3" -> 16
        let key = (m.db_engine.clone(), major);
        let image = policy.map.get(&key).cloned().ok_or_else(|| {
            anyhow::anyhow!(
                "매니페스트 server_version({} major={})에 매칭되는 Docker 이미지가 없음 — \
                 원본과 다른 버전으로 검증하면 restore_target_version에 갭이 기록됨",
                m.server_version, major
            )
        })?;
        Ok(Self {
            image,
            engine: m.db_engine.clone(),
            server_major: major,
            container_name: format!("dbbackup-verify-{}", uuid::Uuid::new_v4()),
            db_name: m.database_name.clone(),
            restricted_role: "restore_target".into(),
        })
    }
}

pub struct Sandbox {
    docker: std::path::PathBuf,
    spec: SandboxSpec,
    removed: bool,
}

impl Sandbox {
    pub async fn launch(docker: &std::path::Path, spec: SandboxSpec) -> anyhow::Result<Self> {
        // 봉쇄 플래그(C3): 네트워크 완전 차단, 모든 capability 제거, 권한상승 금지, tmpfs 데이터 디렉토리,
        // 읽기전용 루트FS(데이터 dir만 tmpfs 쓰기). 호스트 마운트 없음.
        let mut cmd = Command::new(docker);
        cmd.arg("run").arg("-d").arg("--rm")
            .args(["--name", &spec.container_name])
            .args(["--network", "none"])              // 아웃바운드 차단
            .args(["--cap-drop", "ALL"])
            .args(["--security-opt", "no-new-privileges"])
            .args(["--pids-limit", "512"])
            .args(["--memory", "2g"]);

        match spec.engine.as_str() {
            "postgresql" => {
                cmd.args(["--tmpfs", "/var/lib/postgresql/data:rw,noexec,nosuid"])
                    .args(["-e", "POSTGRES_PASSWORD=verify_only_ephemeral"])
                    .args(["-e", &format!("POSTGRES_DB={}", spec.db_name)]);
            }
            "mysql" | "mariadb" => {
                cmd.args(["--tmpfs", "/var/lib/mysql:rw,noexec,nosuid"])
                    .args(["-e", "MYSQL_ROOT_PASSWORD=verify_only_ephemeral"])
                    .args(["-e", &format!("MYSQL_DATABASE={}", spec.db_name)]);
            }
            other => anyhow::bail!("알 수 없는 엔진: {other}"),
        }
        cmd.arg(&spec.image);
        // MySQL/MariaDB: 서버 자체에서 LOAD DATA LOCAL 무력화(FR5).
        if matches!(spec.engine.as_str(), "mysql" | "mariadb") {
            cmd.args(["--local-infile=0", "--skip-symbolic-links"]);
        }

        let status = cmd.status().await?;
        anyhow::ensure!(status.success(), "docker run 실패");

        let sb = Self { docker: docker.to_path_buf(), spec, removed: false };
        sb.wait_ready().await?;
        // 비슈퍼유저 복원 role 준비.
        sb.prepare_restricted_role().await?;
        Ok(sb)
    }

    async fn wait_ready(&self) -> anyhow::Result<()> {
        // 헬스 폴링: PG는 pg_isready, MySQL은 mysqladmin ping. 타임아웃 60s.
        let (bin, args): (&str, Vec<&str>) = match self.spec.engine.as_str() {
            "postgresql" => ("pg_isready", vec!["-q"]),
            _ => ("mysqladmin", vec!["ping", "--silent"]),
        };
        for _ in 0..60 {
            let ok = self.exec(bin, &args, None).await.map(|o| o.success).unwrap_or(false);
            if ok { return Ok(()); }
            tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        }
        anyhow::bail!("샌드박스 DB가 준비되지 않음(timeout)")
    }

    async fn prepare_restricted_role(&self) -> anyhow::Result<()> {
        if self.spec.engine == "postgresql" {
            // 슈퍼유저 아님. COPY FROM PROGRAM은 pg_execute_server_program 권한이 필요한데 부여하지 않음(FR5).
            let sql = format!(
                "CREATE ROLE {r} LOGIN NOSUPERUSER NOCREATEROLE NOCREATEDB;",
                r = self.spec.restricted_role
            );
            self.psql_internal(&sql).await?;
        }
        Ok(())
    }

    /// PG globals(plain SQL) 적용. 위험 SQL 사전 스캔 후 샌드박스 내부에서만 실행.
    /// **대화형 psql 금지** — 파일 스트림을 스캔한 뒤 안전 확인된 것만 비대화 psql로 넘긴다.
    pub async fn apply_globals_sql(&self, plain: PlaintextDump) -> anyhow::Result<()> {
        let mut buf = Vec::new();
        if let PlaintextDump::Stream(mut r) = plain {
            use std::io::Read;
            r.read_to_end(&mut buf)?;
        } else {
            anyhow::bail!("globals는 단일 SQL 스트림이어야 함");
        }
        scan_dangerous_sql(&buf)?; // \! / COPY PROGRAM / plpythonu 등 발견 시 거부
        // psql은 ON_ERROR_STOP + 파일 입력. globals는 신뢰 게이트(서명) 통과분이지만 스캔으로 이중 방어.
        self.exec_with_stdin(
            "psql",
            &["-v", "ON_ERROR_STOP=1", "--no-psqlrc", "-U", "postgres", "-d", &self.spec.db_name],
            &buf,
        )
        .await?
        .ok_or_ctx("globals 적용 실패")
    }

    /// DB 덤프 복원. custom 포맷은 pg_restore(비대화, 백슬래시 메타 미해석) + --no-privileges + 비슈퍼유저.
    pub async fn restore_db_dump(
        &self,
        plain: PlaintextDump,
        m: &Manifest,
    ) -> anyhow::Result<()> {
        match self.spec.engine.as_str() {
            "postgresql" => match m.dump_format.as_str() {
                "custom" => {
                    // pg_restore는 psql이 아니므로 \! 등 메타커맨드를 실행하지 않는다(구조적 안전).
                    // --no-privileges: GRANT/OWNER 재생성 안 함. --no-owner: 복원 role 소유로.
                    // 접속 유저는 restricted_role(비슈퍼유저) → COPY FROM PROGRAM 불가.
                    let args = [
                        "-U", &self.spec.restricted_role,
                        "-d", &self.spec.db_name,
                        "--no-privileges", "--no-owner", "--single-transaction",
                        "--exit-on-error",
                    ];
                    let bytes = read_stream(plain)?;
                    self.exec_with_stdin("pg_restore", &args, &bytes).await?
                        .ok_or_ctx("pg_restore 실패")
                }
                "plain" => {
                    // plain SQL은 psql 경유가 불가피 → 반드시 사전 스캔 + ON_ERROR_STOP + 비대화.
                    let bytes = read_stream(plain)?;
                    scan_dangerous_sql(&bytes)?;
                    let args = ["-v", "ON_ERROR_STOP=1", "--no-psqlrc",
                                "-U", &self.spec.restricted_role, "-d", &self.spec.db_name];
                    self.exec_with_stdin("psql", &args, &bytes).await?
                        .ok_or_ctx("psql plain 복원 실패")
                }
                "directory" => {
                    // directory는 컨테이너 내부 경로에 이미 untar된 상태를 전제(호스트 노출 없음).
                    anyhow::bail!("directory 포맷 복원은 컨테이너-내부 경로 주입 경로로 처리(후속)")
                }
                other => anyhow::bail!("알 수 없는 dump_format: {other}"),
            },
            "mysql" | "mariadb" => {
                // 클라이언트도 --local-infile=0로 LOAD DATA LOCAL 차단(서버측 0과 이중 방어).
                let bytes = read_stream(plain)?;
                scan_dangerous_sql(&bytes)?;
                let args = ["--local-infile=0", "-u", "root",
                            "-pverify_only_ephemeral", &self.spec.db_name];
                self.exec_with_stdin("mysql", &args, &bytes).await?
                    .ok_or_ctx("mysql 복원 실패")
            }
            other => anyhow::bail!("알 수 없는 엔진: {other}"),
        }
    }

    pub async fn server_version(&self) -> anyhow::Result<String> {
        let sql = if self.spec.engine == "postgresql" { "SHOW server_version" } else { "SELECT VERSION()" };
        let out = self.query_scalar(sql).await?;
        Ok(out.trim().to_string())
    }

    // --- docker exec 헬퍼(내부 DB 클라이언트는 컨테이너 안에서 실행) ---
    async fn exec(&self, bin: &str, args: &[&str], _stdin: Option<&[u8]>) -> anyhow::Result<ExecOut> {
        let mut c = Command::new(&self.docker);
        c.arg("exec").arg(&self.spec.container_name).arg(bin).args(args);
        let out = c.output().await?;
        Ok(ExecOut { success: out.status.success(), stdout: out.stdout, stderr: out.stderr })
    }

    async fn exec_with_stdin(&self, bin: &str, args: &[&str], stdin: &[u8]) -> anyhow::Result<ExecOut> {
        let mut c = Command::new(&self.docker);
        c.arg("exec").arg("-i").arg(&self.spec.container_name).arg(bin).args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = c.spawn()?;
        child.stdin.take().unwrap().write_all(stdin).await?;
        // (stdin drop으로 EOF 전달)
        let out = child.wait_with_output().await?;
        Ok(ExecOut { success: out.status.success(), stdout: out.stdout, stderr: out.stderr })
    }

    async fn psql_internal(&self, sql: &str) -> anyhow::Result<()> {
        self.exec("psql", &["-U", "postgres", "-d", &self.spec.db_name, "-c", sql], None)
            .await?
            .ok_or_ctx("psql 내부 명령 실패")
    }

    async fn query_scalar(&self, sql: &str) -> anyhow::Result<String> {
        let out = if self.spec.engine == "postgresql" {
            self.exec("psql", &["-tAX", "-U", "postgres", "-d", &self.spec.db_name, "-c", sql], None).await?
        } else {
            self.exec("mysql", &["-N", "-B", "-u", "root", "-pverify_only_ephemeral",
                                 "-e", sql, &self.spec.db_name], None).await?
        };
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        if !self.removed {
            // 항상 강제 제거(봉쇄). Drop에서 async 불가하므로 blocking으로.
            let _ = std::process::Command::new(&self.docker)
                .args(["rm", "-f", &self.spec.container_name])
                .status();
        }
    }
}

struct ExecOut { success: bool, stdout: Vec<u8>, stderr: Vec<u8> }
impl ExecOut {
    fn ok_or_ctx(self, ctx: &str) -> anyhow::Result<()> {
        if self.success { Ok(()) }
        else { anyhow::bail!("{ctx}: {}", String::from_utf8_lossy(&self.stderr)) }
    }
}

fn read_stream(p: PlaintextDump) -> anyhow::Result<Vec<u8>> {
    match p {
        PlaintextDump::Stream(mut r) => {
            use std::io::Read;
            let mut b = Vec::new();
            r.read_to_end(&mut b)?;
            Ok(b)
        }
        PlaintextDump::Directory(_) => anyhow::bail!("스트림이 필요한데 디렉토리 포맷"),
    }
}
```

위험 SQL 사전 스캐너(심층 방어 — 서명 게이트가 1차, 샌드박스 격리가 2차, 이 스캔이 3차):

```rust
// src/restore/sqlscan.rs
/// plain SQL/덤프 텍스트에서 Restore-as-Exec 패턴을 탐지한다(C3). 발견 시 복원 거부.
/// custom 포맷은 pg_restore가 백슬래시 메타를 해석하지 않으므로 이 스캔의 주 대상은 plain/MySQL SQL.
pub fn scan_dangerous_sql(bytes: &[u8]) -> anyhow::Result<()> {
    let text = String::from_utf8_lossy(bytes).to_ascii_lowercase();
    const PATTERNS: &[&str] = &[
        "copy ", "from program",          // PG: COPY ... FROM PROGRAM
        "language plpythonu", "language plpython3u", "language plperlu", "language plr",
        "\n\\!", "\\! ",                  // psql backslash-bang
        "load data local", "into outfile", "into dumpfile", // MySQL
        "sys_exec", "sys_eval", "lo_import", "lo_export",
    ];
    for p in PATTERNS {
        // "copy ... from program"은 두 토큰을 함께 봐야 오탐이 적음: 여기선 보수적으로 개별 매칭 후
        // "from program"이 실제로 존재하면 즉시 거부(정상 백업엔 등장하지 않음).
        if *p == "copy " { continue; }
        if text.contains(p) {
            anyhow::bail!("위험 SQL 패턴 감지('{p}') — Restore-as-Exec 의심, 복원 거부(C3)");
        }
    }
    Ok(())
}
```

> `scan_dangerous_sql`은 **거짓음성 가능성**을 전제로 한 보조 방어선이다(난독화된 페이로드를 100% 잡지
> 못함). 진짜 방어는 (1) 서명 게이트로 애초에 신뢰된 백업만 복원, (2) `--network none` + 비특권 + 비슈퍼유저
> + `--no-privileges`로 만약의 실행을 봉쇄하는 것이다. 스캔은 "정상 백업엔 절대 없을" 패턴을 시끄럽게
> 거부해 조기 경보를 주는 역할이다.

### 9.6 Verifier — L1/L2/L3 (FR6)

Verifier는 Restorer의 복호·복원 기계 위에 레벨별 단언을 얹는다.

```rust
// src/verify/mod.rs
use crate::restore::decrypt::{open_plaintext_reader, PlaintextDump, materialize_dump};
use crate::restore::inputs::RestoreInputs;
use crate::restore::restorer::{RestoreTarget, Restorer};
use crate::manifest::types::Manifest;
use std::collections::BTreeMap;
use std::io::Read;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyLevel { L1 = 1, L2 = 2, L3 = 3 }

pub struct VerifyOutcome {
    pub level: VerifyLevel,
    pub result: VerifyResult,               // Pass | Fail
    pub restore_target_version: Option<String>, // L3만 Some — 버전 갭 투명화
    pub restored_rows: Option<u64>,
    pub schema_hash: Option<String>,
    pub notes: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult { Pass, Fail }

pub struct Verifier<'a> {
    pub restorer: Restorer<'a>,
}

impl<'a> Verifier<'a> {
    pub async fn verify(
        &self,
        inputs: &RestoreInputs,
        level: VerifyLevel,
    ) -> anyhow::Result<VerifyOutcome> {
        match level {
            VerifyLevel::L1 => self.l1_file_integrity(inputs).await,
            VerifyLevel::L2 => self.l2_format_validity(inputs).await,
            VerifyLevel::L3 => self.l3_full_restore(inputs).await,
        }
    }

    /// L1: 재복호 + 재압축해제가 끝까지 되고, 재계산 blake3 == 매니페스트 plaintext_blake3.
    async fn l1_file_integrity(&self, inputs: &RestoreInputs) -> anyhow::Result<VerifyOutcome> {
        let m = &inputs.db_manifest;
        let expected = m.plaintext_blake3.clone();
        let cipher = self.restorer.read_cipher(m).await?;
        let dek = self.restorer.dek.clone();
        let got = tokio::task::spawn_blocking(move || -> anyhow::Result<String> {
            let mut reader = open_plaintext_reader(std::io::Cursor::new(cipher), &dek)?;
            let mut hasher = blake3::Hasher::new();
            let mut buf = [0u8; 64 * 1024];
            loop {
                let n = reader.read(&mut buf)?; // 여기서 STREAM 마지막 청크 인증까지 강제됨
                if n == 0 { break; }
                hasher.update(&buf[..n]);
            }
            Ok(hasher.finalize().to_hex().to_string())
        })
        .await??;

        let pass = got == expected;
        Ok(VerifyOutcome {
            level: VerifyLevel::L1,
            result: if pass { VerifyResult::Pass } else { VerifyResult::Fail },
            restore_target_version: None,
            restored_rows: None,
            schema_hash: None,
            notes: if pass {
                "재복호+재압축해제 성공, 평문 blake3 일치".into()
            } else {
                format!("평문 blake3 불일치: expected={expected} got={got}")
            },
        })
    }

    /// L2: pg_restore --list로 custom TOC 파싱(데이터 미전개). MySQL은 등가물이 약하므로 헤더 검사로 대체.
    async fn l2_format_validity(&self, inputs: &RestoreInputs) -> anyhow::Result<VerifyOutcome> {
        let m = &inputs.db_manifest;
        let cipher = self.restorer.read_cipher(m).await?;
        let dek = self.restorer.dek.clone();
        let fmt = m.dump_format.clone();
        let engine = m.db_engine.clone();

        let (pass, notes) = tokio::task::spawn_blocking(move || -> anyhow::Result<(bool, String)> {
            let mut reader = open_plaintext_reader(std::io::Cursor::new(cipher), &dek)?;
            match (engine.as_str(), fmt.as_str()) {
                ("postgresql", "custom") => {
                    // 평문(custom 바이너리)을 임시 파일로 떨궈 pg_restore --list 실행(로컬 pg_restore 사용,
                    // 데이터를 DB에 넣지 않으므로 실행 위험 없음 — TOC만 읽음).
                    let mut tf = tempfile::NamedTempFile::new()?;
                    std::io::copy(&mut reader, &mut tf)?;
                    let out = std::process::Command::new("pg_restore")
                        .arg("--list").arg(tf.path()).output()?;
                    if out.status.success() && !out.stdout.is_empty() {
                        Ok((true, format!("pg_restore --list OK, TOC {} bytes", out.stdout.len())))
                    } else {
                        Ok((false, format!("pg_restore --list 실패: {}",
                            String::from_utf8_lossy(&out.stderr))))
                    }
                }
                (_, "plain") | ("mysql", _) | ("mariadb", _) => {
                    // MySQL 등가물 부재(도메인 §4.2): 최소 헤더 검사.
                    let mut head = [0u8; 256];
                    let n = reader.read(&mut head)?;
                    let s = String::from_utf8_lossy(&head[..n]);
                    let ok = s.contains("-- MySQL dump") || s.contains("-- MariaDB")
                        || s.contains("PostgreSQL database dump");
                    Ok((ok, if ok { "덤프 헤더 확인(약한 L2)".into() }
                            else { "예상 헤더 없음".into() }))
                }
                _ => Ok((false, "L2 미지원 포맷 조합".into())),
            }
        })
        .await??;

        Ok(VerifyOutcome {
            level: VerifyLevel::L2,
            result: if pass { VerifyResult::Pass } else { VerifyResult::Fail },
            restore_target_version: None,
            restored_rows: None, schema_hash: None, notes,
        })
    }

    /// L3: 샌드박스 실제 복원 → (a) 스키마 해시 대조, (b) row count 대조(매니페스트 캡처값 vs 복원본).
    async fn l3_full_restore(&self, inputs: &RestoreInputs) -> anyhow::Result<VerifyOutcome> {
        let outcome = self.restorer.restore(inputs, RestoreTarget::Sandbox).await?;
        let sandbox = outcome.sandbox.expect("L3는 샌드박스 필수");
        let target_version = outcome.restore_target_version.clone();

        // (a) 복원본에서 정규화 스키마 해시를 뽑아 매니페스트 백업시점 schema_hash와 비교.
        let restored_schema_hash = compute_schema_hash(&sandbox).await?;
        let schema_pass = match &inputs.db_manifest.schema_hash {
            Some(expected) => &restored_schema_hash == expected,
            None => true, // 백업시점 값 없음 → 비교 skip, 기록만(하위호환)
        };

        // (b) row counts: 복원본 실측 vs 매니페스트 source_row_counts(백업 시점 캡처, 라이브 DB 아님).
        let restored_counts = collect_row_counts(&sandbox).await?;
        let (rows_pass, rows_note, total_restored) =
            compare_row_counts(&inputs.db_manifest.source_row_counts, &restored_counts);

        let pass = schema_pass && rows_pass;
        let mut notes = String::new();
        if inputs.db_manifest.schema_hash.is_none() {
            notes.push_str("[백업시점 schema_hash 없음 → 스키마 비교 skip] ");
        }
        if !schema_pass {
            notes.push_str(&format!(
                "스키마 해시 불일치(expected={:?} got={}) ",
                inputs.db_manifest.schema_hash, restored_schema_hash
            ));
        }
        notes.push_str(&rows_note);
        // 버전 갭 투명화(도메인 §4.3).
        if version_gap(&inputs.db_manifest.server_version, &target_version) {
            notes.push_str(&format!(
                " [주의: 원본({})과 다른 버전({})으로 검증됨 — 완전한 보증 아님]",
                inputs.db_manifest.server_version, target_version
            ));
        }

        Ok(VerifyOutcome {
            level: VerifyLevel::L3,
            result: if pass { VerifyResult::Pass } else { VerifyResult::Fail },
            restore_target_version: Some(target_version),
            restored_rows: Some(total_restored),
            schema_hash: Some(restored_schema_hash),
            notes,
        })
        // sandbox는 여기서 Drop → docker rm -f (봉쇄).
    }
}
```

스키마 해시/row count 수집 및 대조:

```rust
// src/verify/asserts.rs
use crate::restore::sandbox::Sandbox;
use std::collections::BTreeMap;

/// 복원본에서 --schema-only 덤프를 떠 정규화(주석/빈줄/후행공백 제거) 후 blake3.
/// 정규화로 공백·순서 잡음을 제거해 오탐을 없앤다(도메인 §4.3a).
pub async fn compute_schema_hash(sb: &Sandbox) -> anyhow::Result<String> {
    let raw = sb.dump_schema_only().await?; // 컨테이너 내부에서 pg_dump --schema-only 등
    let normalized: String = raw
        .lines()
        .map(|l| l.trim_end())
        .filter(|l| !l.is_empty() && !l.starts_with("--"))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(blake3::hash(normalized.as_bytes()).to_hex().to_string())
}

/// 복원본 각 테이블의 실제 row count(라이브 프로덕션 DB가 아니라 샌드박스 복원본에서 집계).
pub async fn collect_row_counts(sb: &Sandbox) -> anyhow::Result<BTreeMap<String, u64>> {
    sb.count_rows_per_table().await // information_schema 순회 후 SELECT count(*)
}

/// 매니페스트 백업시점 카운트 vs 복원본 카운트 비교. 라이브 DB는 절대 건드리지 않는다(도메인 §4.3b).
pub fn compare_row_counts(
    manifest_captured: &BTreeMap<String, u64>,
    restored: &BTreeMap<String, u64>,
) -> (bool, String, u64) {
    let total_restored: u64 = restored.values().sum();
    if manifest_captured.is_empty() {
        return (
            true,
            format!("[백업시점 source_row_counts 없음 → row 비교 skip, 복원본 총 {total_restored}행 기록]"),
            total_restored,
        );
    }
    let mut mismatches = Vec::new();
    for (table, &expected) in manifest_captured {
        let got = restored.get(table).copied().unwrap_or(0);
        if got != expected {
            mismatches.push(format!("{table}: expected={expected} got={got}"));
        }
    }
    if mismatches.is_empty() {
        (true, format!("row count 전부 일치({}테이블)", manifest_captured.len()), total_restored)
    } else {
        (false, format!("row count 불일치: {}", mismatches.join(", ")), total_restored)
    }
}

fn version_gap(source_version: &str, target_version: &str) -> bool {
    parse_major_lossy(source_version) != parse_major_lossy(target_version)
}
```

검증 결과를 db-schema.md `verifications` 테이블 + 매니페스트 `verify` 블록에 기록:

```rust
// src/verify/record.rs
use crate::catalog::writer::CatalogWriterHandle; // S08
use crate::verify::{VerifyLevel, VerifyOutcome, VerifyResult};
use rusqlite::params;

pub async fn record_verification(
    writer: &CatalogWriterHandle,
    backup_id: &str,
    o: &VerifyOutcome,
) -> anyhow::Result<()> {
    let id = uuid::Uuid::new_v4().to_string();
    let verified_at = time::OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)?;
    let level = o.level as i64;
    let result = if o.result == VerifyResult::Pass { "pass" } else { "fail" };
    let (rtv, rows, shash, notes) = (
        o.restore_target_version.clone(),
        o.restored_rows.map(|r| r as i64),
        o.schema_hash.clone(),
        o.notes.clone(),
    );
    let bid = backup_id.to_string();
    writer.run(move |conn| {
        conn.execute(
            "INSERT INTO verifications
             (id, backup_id, verified_at, level, result, restore_target_version,
              restored_rows, schema_hash, notes)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![id, bid, verified_at, level, result, rtv, rows, shash, notes],
        )?;
        Ok(())
    }).await
}
```

### 9.7 검증 주기 정책 — L1/L2 매 백업, L3 주간 + 스키마 변경 (FR7)

```rust
// src/verify/cadence.rs
use time::{Duration, OffsetDateTime};

/// 이번 백업에 대해 지금 수행할 검증 레벨 집합을 결정한다.
pub struct CadenceInput<'a> {
    pub job: &'a str,
    pub now: OffsetDateTime,
    pub current_schema_hash: Option<&'a str>,   // 이번 매니페스트 schema_hash
    pub prev_schema_hash: Option<&'a str>,      // 직전 성공 백업의 schema_hash
    pub last_l3_at: Option<OffsetDateTime>,     // 이 job의 마지막 L3 성공 시각(카탈로그 조회)
}

pub fn levels_due(input: &CadenceInput) -> Vec<super::VerifyLevel> {
    use super::VerifyLevel::*;
    // L1/L2는 항상(매 백업 직후, 비용 거의 없음).
    let mut due = vec![L1, L2];

    // 스키마 변경 감지 → 즉시 L3(변경 후 첫 백업이 복원되는지 다음 주까지 미루지 않음, 도메인 §4).
    let schema_changed = match (input.current_schema_hash, input.prev_schema_hash) {
        (Some(c), Some(p)) => c != p,
        (Some(_), None) => true, // 첫 백업/이전 기록 없음 → L3로 한 번 확증
        _ => false,
    };
    // 주간 경과 → L3.
    let weekly_due = match input.last_l3_at {
        Some(t) => input.now - t >= Duration::days(7),
        None => true, // 여태 L3 이력 없음
    };
    if schema_changed || weekly_due {
        due.push(L3);
    }
    due
}
```

### 9.8 restore/verify CLI 서브셰이프 (FR8)

파싱은 S01 clap 정의, 여기서는 핸들러 로직만.

```rust
// src/cli/restore_cmd.rs
use crate::restore::inputs::load_trusted_inputs;
use crate::restore::restorer::{RestoreTarget, Restorer};
use crate::restore::unlock::{unwrap_dek, UnlockMode};
use zeroize::Zeroizing;

pub struct RestoreCliArgs {
    pub backup: Option<String>,           // --backup <id> (카탈로그 후보 힌트)
    pub from_destination: Option<String>, // --from-destination <name> (카탈로그 무시, 목적지 직접)
    pub to: RestoreTargetArg,             // --to docker|dir
    pub decrypt_only: Option<std::path::PathBuf>, // --decrypt-only <path>
    pub with_recovery_code: bool,         // --with-recovery-code
}

pub enum RestoreTargetArg { Docker, Dir(std::path::PathBuf) }

pub async fn cmd_restore(
    args: RestoreCliArgs,
    deps: &crate::cli::Deps, // source, verifier, secret_manager, docker_bin, image_policy 등(S01 조립)
) -> anyhow::Result<()> {
    // (1) 복원 대상 매니페스트 키 결정. --from-destination이면 목적지 직접 조회(카탈로그 신뢰 배제, H5),
    //     --backup이면 카탈로그에서 "키 힌트"만 가져오고 실제 신뢰는 서명 게이트에서.
    let db_manifest_key = resolve_manifest_key(&args, deps).await?;

    // (2) 서명 게이트 통과한 신뢰 입력 로드(미서명/위조는 여기서 Err → 즉시 중단, C3).
    let inputs = load_trusted_inputs(deps.source(), deps.verifier(), &db_manifest_key).await?;

    // (3) 자격증명 경로 선택 → DEK unwrap(S02).
    let mode = if args.with_recovery_code {
        UnlockMode::Recovery(prompt_secret("복구 코드 입력")?)
    } else if deps.is_unattended() {
        UnlockMode::Unattended
    } else {
        UnlockMode::Password(prompt_secret("마스터 비밀번호 입력")?)
    };
    let dek = unwrap_dek(deps.secret_manager(), &inputs.wrap_set, mode)?;

    // (4) 복원 실행.
    let restorer = Restorer {
        source: deps.source(),
        dek,
        docker_bin: deps.docker_bin(),
        image_policy: deps.image_policy(),
    };
    let target = match (args.decrypt_only, args.to) {
        (Some(path), _) => RestoreTarget::DecryptOnly(path),
        (None, RestoreTargetArg::Docker) => RestoreTarget::Sandbox,
        (None, RestoreTargetArg::Dir(p)) => RestoreTarget::DecryptOnly(p), // dir 산출도 평문 경로
    };
    let outcome = restorer.restore(&inputs, target).await?;

    println!("복원 완료. restore_target_version={}", outcome.restore_target_version);
    Ok(())
}

fn prompt_secret(label: &str) -> anyhow::Result<Zeroizing<String>> {
    // rpassword 등으로 no-echo 입력. 여기선 시그니처만.
    Ok(Zeroizing::new(rpassword::prompt_password(format!("{label}: "))?))
}
```

## Test Scenarios

1. **미서명 매니페스트 거부 (C3 핵심)**: `signature: null`인 매니페스트를 목적지에 두고 `restore`/`verify`
   호출 → `load_trusted_inputs`가 `ManifestError::Unsigned`로 **즉시 중단**하고, DEK unwrap·Docker 기동·
   복원 어느 단계에도 진입하지 않는지 확인(서명 게이트가 최상단임을 실증).
2. **위조(변조) 매니페스트 거부**: 서명된 매니페스트의 본체 1바이트를 변조 → `verify_manifest` 실패로
   `TamperedOrForged`, 복원 차단.
3. **오염 덤프의 샌드박스 봉쇄 (C3 핵심)**: `COPY xyz FROM PROGRAM 'touch /tmp/pwned'`(PG)와
   `LOAD DATA LOCAL INFILE`(MySQL)를 심은 **유효 서명** 덤프(테스트 서명자로 정상 서명)를 L3 복원 →
   (a) `scan_dangerous_sql`이 거부하거나, (b) 스캔을 우회해도 `--network none`+비슈퍼유저+`--no-privileges`로
   OS 명령이 실행되지 않고, (c) 컨테이너가 항상 `rm -f`로 제거되는지 확인. **호스트에 `/tmp/pwned`가
   생기지 않아야 통과**(봉쇄 증명).
4. **비대화 psql 경로 강제**: plain SQL 덤프에 `\! rm -rf` 메타커맨드를 심고 복원 → `scan_dangerous_sql`
   거부 + (설령 통과해도) `pg_restore`(custom) 경로는 백슬래시를 해석하지 않음을 확인. 대화형 psql로
   신뢰 불가 덤프를 파이프하는 코드 경로가 존재하지 않는지 코드 리뷰 체크.
5. **L1 파일 무결성 pass/fail**: 정상 백업 → L1 pass(blake3 일치). 암호문 중간 바이트 변조 → STREAM 마지막
   청크 인증 실패 또는 blake3 불일치로 L1 fail, `notes`에 expected/got 기록.
6. **절단(truncated) 암호문 검출**: 스풀 파일 꼬리를 잘라 `decrypt_last`가 태그를 못 맞추게 함 →
   L1이 "절단?" 인증 실패로 fail(S05 R4 commit protocol의 역방향 검증).
7. **L2 포맷 유효성**: 정상 custom 덤프 → `pg_restore --list` 성공, TOC 비어있지 않음. custom 헤더를
   깨뜨린 덤프 → L2 fail.
8. **L3 스키마 해시 대조**: 백업시점 `schema_hash`가 있는 매니페스트로 L3 → 복원본 정규화 스키마 해시가
   일치하면 pass. 복원 대상 이미지에서 컬럼이 누락되게 조작하면 해시 불일치로 fail.
9. **L3 row count 대조(라이브 DB 미접촉)**: 매니페스트 `source_row_counts`(백업시점 캡처값)와 복원본
   실측을 비교 → 일치 시 pass. **테스트는 원본/라이브 DB에 접근하지 않고 매니페스트 값만 사용함을 확인**
   (도메인 §4.3b 원칙 실증).
10. **복원 순서 globals→db 강제 (PG)**: globals 없이 DB 덤프만 복원 시 role 부재로 실패하고, globals→db
    순서로는 성공하는지 확인. `pairs_with_globals_backup_id`가 없거나 짝을 못 찾으면 `MissingGlobals`로 중단.
11. **버전 갭 투명화**: 매니페스트 `server_version`(예: PG 16)과 다른 이미지(예: PG 15)로 L3 →
    결과 `restore_target_version`에 15가 기록되고 `notes`에 "다른 버전으로 검증됨" 경고가 포함되는지 확인.
12. **DEK unwrap 3경로**: `--with-recovery-code`→`Recovery`, 무인→`Dpapi`(Windows), 대화형→`Password`가
    각각 올바른 `Credential`로 매핑되어 S02 `unwrap`을 호출하는지(경로별 성공/실패) 확인.
13. **verified는 항상 레벨 표기**: `verifications` insert와 알림 문자열에 레벨 없는 "verified"가 없는지
    (grep 기반 린트) 확인. L1만 통과한 백업이 "verified(L1)"로만 표기되고 "복원 검증됨"으로 오인되지
    않는지 확인.
14. **주기 정책**: `levels_due`가 (a) 매번 L1/L2 포함, (b) `prev_schema_hash != current`면 L3 추가,
    (c) `last_l3_at`이 7일 초과면 L3 추가, (d) 둘 다 아니면 L3 미포함을 반환하는지 표 기반 테스트.
15. **--decrypt-only escape hatch**: 복원 없이 평문 덤프 파일이 생성되고, 경고 로그가 출력되며, Docker를
    전혀 기동하지 않는지 확인.
16. **샌드박스 항상 정리**: L3 도중 강제 패닉/에러를 주입해도 `Sandbox::drop`이 `docker rm -f`를 실행해
    컨테이너 잔존이 없는지(`docker ps -a`에 미존재) 확인.

## Implementation Strategy

1. **9.2 복호 체인 먼저(L1의 토대)**: S05 `EncryptChain`과 대칭인 `DecryptChain`을 만들고, S05 인코딩
   출력에 대한 **라운드트립**(인코딩→복호→평문 재해시 == 원본 해시)을 닫는다. 이것이 S05 T7과 동일한
   경로이므로 S05 테스트 픽스처를 재사용한다. 청크 상수/nonce prefix는 `pipeline::consts`로 공유.
2. **9.1 서명 게이트 통합**: S08 `load_and_verify_manifest_for_restore`를 소비하는 `load_trusted_inputs`를
   구현하고, **미서명/위조 거부 테스트(시나리오 1~2)를 가장 먼저** green으로 만든다(C3 최상단 방어선).
3. **9.6 L1/L2**: 복호 체인 위에 L1(blake3 대조)·L2(`pg_restore --list`)를 얹는다. Docker 없이 동작하므로
   CI에서 빠르게 검증 가능. 절단/변조 실패 케이스(시나리오 5~7)를 함께.
4. **9.5 샌드박스**: `docker` 가용 환경에서 PG 이미지 기동→비슈퍼유저 role→stdin 복원까지 스모크.
   봉쇄 플래그(`--network none` 등)와 `Drop` 정리를 먼저 확립하고, 오염 덤프 봉쇄(시나리오 3)를 통과시킨다.
5. **9.4 Restorer + 순서**: globals→db 순서 강제(시나리오 10)와 `--decrypt-only`(시나리오 15).
6. **9.6 L3**: 샌드박스 위에 스키마 해시/row count 단언(시나리오 8~9, 11). `source_row_counts`/`schema_hash`가
   매니페스트에 아직 없을 수 있으므로 **없으면 skip+기록**으로 하위호환하고, S03/S04에 필드 채움을 요청(계약).
7. **9.7 주기 + 9.8 CLI**: `levels_due` 순수 함수 테스트(시나리오 14) → CLI 핸들러 배선.
8. 전 과정에서 **"verified" 단독 사용 금지 린트**(시나리오 13)와 **신뢰 불가 덤프가 호스트 셸/네트워크에
   닿는 코드 경로 부재**를 코드 리뷰 체크리스트로 강제.

## Quality Gate

- `cargo test -p dbbackup-restore`: 시나리오 1~16 전부 green. 단, Docker 필요 시나리오(3,8,9,11,16)는
  `#[ignore]` + `--ignored`로 분리해 Docker 없는 CI에서도 나머지는 돌게 한다(그리고 Docker 있는 게이트에서
  전량 실행).
- **실패 케이스 우선 테스트**: 서명 검증(미서명/위조), L1(변조/절단), 오염 덤프 봉쇄를 "성공 케이스보다
  먼저" 유지 — "통과만 테스트하는" 함정 방지(S08 Quality Gate와 동일 규율).
- `cargo clippy --all-targets -- -D warnings`: 복호/서명/샌드박스 경로에 `unwrap()`/`expect()`/`panic!`
  금지(테스트 제외). 특히 **서명 검증 실패는 반드시 `Err` 전파**(경고 후 통과 금지, C3).
- **봉쇄 불변식 린트**: `--network none`, `--cap-drop ALL`, `--security-opt no-new-privileges`,
  `pg_restore ... --no-privileges`(슈퍼유저 접속 없음), MySQL `--local-infile=0`가 코드에 존재하고
  제거/약화되지 않았는지 grep 기반 CI 체크.
- **호스트 노출 부재**: 신뢰 불가 덤프를 `docker run -v`(바인드 마운트)로 넣거나 대화형 `psql`로 파이프하는
  코드가 없는지 확인(stdin 파이프 + 비대화만 허용).
- L3 통합 테스트는 실제 Docker + 실제 pg_restore로 수행(과도한 모킹 금지 — 실제 복원 위험/에러 경로를 놓침).

## Risk & Rollback

| 리스크 | 완화 |
|---|---|
| 복호 체인이 S05 인코딩과 미묘하게 비대칭(청크 크기/nonce prefix/age vs chacha20 불일치)이면 전 복원이 깨짐 | 상수(`pipeline::consts`) 단일 소스 공유 + S05 라운드트립 테스트를 S09에서도 재사용해 대칭성을 CI로 고정. age/chacha20 경로 선택은 S05 확정을 따르며 `[확인 필요]` 플래그로 추적 |
| `scan_dangerous_sql`이 난독화 페이로드를 놓칠 수 있음(거짓음성) | 스캔은 3차 보조 방어선으로 명시. 진짜 봉쇄는 서명 게이트(1차)+샌드박스 격리(2차, `--network none`/비슈퍼유저). 스캔 실패가 곧 RCE로 이어지지 않도록 설계(심층 방어) |
| Docker 미설치/데몬 미가동 환경에서 L3/복원 불가 | `doctor`(S01)가 `docker version`을 사전 점검하고, L3는 "skip + 사유 기록"으로 degrade(무인 실행이 조용히 실패하지 않도록 알림). L1/L2는 Docker 없이 동작하므로 최소 보증은 유지 |
| 샌드박스 버전이 원본과 다르면 "복원됨"이 실제 프로덕션 버전 복원을 보증하지 않음 | `restore_target_version` 항상 기록 + `notes`에 갭 경고 + 알림 표기(투명화). 이미지 매핑을 원본 메이저와 동일하게 유지하도록 config 권장 |
| 컨테이너 잔존(누수)으로 리소스/보안 위험 | `Sandbox::Drop`에서 항상 `docker rm -f`, `--rm` 병행, 이름 UUID로 유일성. 시나리오 16으로 잔존 없음 검증 |
| `--decrypt-only`가 평문 덤프를 디스크에 남김(M2) | 명시적 경로 요구 + 경고 로그 + 문서에 "제한 ACL/사용 후 삭제" 명시. 기본 경로(샌드박스)는 stdin 파이프로 평문 비유출 |
| `source_row_counts`/`schema_hash`가 매니페스트에 아직 없어 L3 비교가 빈다 | 없으면 skip + `notes` 명시 + 복원본 실측 기록으로 degrade. S03/S04에 백업시점 캡처를 계약으로 요청(문서 동기화 후속 작업) |
| 매니페스트 확장 필드(`wrapset_blake3`, `source_row_counts`, `schema_hash`)가 db-schema.md와 표면적으로 어긋나 보임 | 전부 `#[serde(default)]` 하위호환. db-schema.md에 "S09 소비 확장 필드"로 반영하는 문서 동기화를 후속으로 명시(S08의 `manifest_seq` 확장과 동일 패턴) |

## Acceptance Criteria

- [ ] 복원·검증의 모든 진입점이 S08 서명 게이트를 통과하며, **미서명/위조 매니페스트는 DEK unwrap·Docker
      기동 이전에 즉시 거부**된다(테스트 1~2, C3).
- [ ] 복호 체인이 S05 인코딩 체인의 역순으로 동작하고, S05 인코딩 출력에 대한 라운드트립(평문 재해시 ==
      원본 blake3)이 닫힌다. 절단/변조 암호문은 STREAM 인증 실패 또는 blake3 불일치로 검출된다(테스트 5~6).
- [ ] L1(blake3 대조)·L2(`pg_restore --list`)가 Docker 없이 동작하고 매 백업 직후 자동 수행 가능하다.
- [ ] L3 실제 복원이 `--network none`·비특권·비슈퍼유저·`--no-privileges`·`--local-infile=0` 샌드박스에서만
      수행되며, 오염 덤프가 호스트에 부작용(파일 생성/네트워크 접속)을 남기지 않는다(테스트 3~4, C3).
- [ ] PG 복원 순서가 globals→db로 강제되고, 짝(`pairs_with_globals_backup_id`)을 못 찾으면 중단한다(테스트 10).
- [ ] L3가 스키마 해시와 row count를 **매니페스트 캡처값(라이브 DB 아님)**과 대조하고, `restore_target_version`을
      항상 기록해 버전 갭을 투명화한다(테스트 8~9, 11, 도메인 §4).
- [ ] 검증 결과가 db-schema.md `verifications` 테이블(level/result/restore_target_version/restored_rows/
      schema_hash/notes)에 기록되고, "verified"는 항상 레벨과 함께 표기된다(테스트 13).
- [ ] 검증 주기 `levels_due`가 L1/L2 매 백업 + L3 주간·스키마 변경 시를 정확히 반환한다(테스트 14).
- [ ] `restore` CLI 서브셰이프(`--backup`/`--from-destination`/`--to`/`--decrypt-only`/`--with-recovery-code`)가
      각각 올바른 입력 해석·자격증명 경로·대상으로 배선된다(테스트 12, 15).
- [ ] 샌드박스는 실패·패닉 시에도 항상 `docker rm -f`로 정리되어 컨테이너가 잔존하지 않는다(테스트 16).
- [ ] `cargo clippy -D warnings` 통과, 서명 검증/복호 실패 경로에 `unwrap`/`panic` 없음.

## Files

- `src/restore/inputs.rs` — 출처 인증 게이트, `RestoreInputs`, WrapSet 서명 커버리지 확인(C3-a)
- `src/restore/decrypt.rs` — 복호 체인(age/chacha20 → zstd → untar), `PlaintextDump`
- `src/restore/unlock.rs` — `UnlockMode` → S02 `Credential` 매핑, DEK unwrap(3경로)
- `src/restore/restorer.rs` — `Restorer`(기계), 복원 순서 globals→db, `--decrypt-only`
- `src/restore/sandbox.rs` — Docker 샌드박스 기동/봉쇄/정리(C3-b), 비대화 복원
- `src/restore/sqlscan.rs` — Restore-as-Exec 패턴 사전 스캐너(3차 방어선)
- `src/verify/mod.rs` — `Verifier`, `VerifyLevel`/`VerifyOutcome`, L1/L2/L3
- `src/verify/asserts.rs` — 스키마 해시/row count 수집·대조(라이브 DB 미접촉)
- `src/verify/cadence.rs` — `levels_due`(L1/L2 매 백업, L3 주간+스키마 변경)
- `src/verify/record.rs` — `verifications` 테이블/매니페스트 verify 블록 기록
- `src/cli/restore_cmd.rs` — `restore`/`verify` CLI 핸들러(파싱은 S01)
- `src/pipeline/consts.rs` — S05와 공유하는 청크 크기/nonce prefix 상수(대칭성 보장)
- `tests/restore_signature_gate.rs` — 시나리오 1~2(미서명/위조 거부)
- `tests/decrypt_roundtrip.rs` — 시나리오 5~6(L1/절단)
- `tests/verify_levels.rs` — 시나리오 7~9, 11(L2/L3)
- `tests/sandbox_containment.rs` — 시나리오 3~4, 16(오염 덤프 봉쇄/정리, `#[ignore]` Docker 게이트)
- `tests/restore_order.rs` — 시나리오 10(globals→db)
- `tests/cadence.rs` — 시나리오 14(주기 정책)
</content>
</invoke>
