> 완성한 첫 리비전. 다른 섹션 작성 시 이 문서를 참조/변경할 수 있음.

# S08 — Catalog(SQLite) + Manifest(동봉/서명/rebuild)

## Background

이 시스템은 "백업 호스트 자체가 랜섬웨어에 완전히 장악되어도 오프사이트에서 단독 복원이 가능해야 한다"는
전제를 가진다(C3/H5/P0-6, `team-review.md`). 이 전제는 로컬에 있는 모든 것 — SQLite 카탈로그 포함 — 을
**신뢰할 수 없는 캐시**로 취급하게 만든다.

- **Catalog(SQLite)**: 조회 UX(최근 백업 목록, 미완료 업로드, 검증 이력)를 빠르게 제공하기 위한 **로컬 캐시**.
  랜섬웨어/침입자가 파일을 지우거나 값을 조작할 수 있는 위치에 존재한다는 전제로 설계한다.
- **Manifest(매니페스트)**: 각 백업의 암호문(ciphertext)과 **동일 목적지에 동봉**되어 저장되는, 오프라인
  ed25519 개인키로 서명된 JSON. `db-schema.md`가 정의한 필드가 진실 소스(source of truth)다.

`plan.md` 파이프라인 6~8단계("매니페스트 생성 → 서명 → 목적지별 업로드 → Catalog 기록")와
`team-review.md`의 세 가지 결정이 이 섹션의 골격이다.

- **P0-6 / H5**: SQLite는 랜섬웨어가 있는 호스트에 있다 → 카탈로그가 통째로 사라지거나 조작되어도
  `catalog rebuild --from <dest>`로 목적지에 동봉된 매니페스트만으로 완전히 재구성할 수 있어야 한다.
  또한 **복원/검증처럼 보안적으로 의미 있는 모든 판단은 로컬 SQLite 값을 절대 단독 근거로 쓰지 않고**,
  목적지의 서명된 매니페스트를 다시 읽어 다시 검증한다.
- **P1-5**: 스케줄러의 catch-up(놓친 실행 몰아치기)이 동일 job을 중복 실행하지 않도록 이중 방어
  (OS 네임드 뮤텍스 + SQLite `runs` 부분 UNIQUE 인덱스)를 건다.

## Requirements

### 기능 요구사항 (FR)
- **FR1** `db-schema.md` DDL을 그대로 구현한다: `backups`, `artifacts`(backup 자식, 목적지별 부분성공 1급),
  `verifications`, `runs`(overlap guard), `meta`. 필드/타입/PK/FK/부분 인덱스를 임의로 바꾸지 않는다.
- **FR2** SQLite는 `PRAGMA journal_mode=WAL; PRAGMA busy_timeout=5000;` + **단일 writer 커넥션**으로만
  쓰기를 수행한다(P1-5, WAL 리더는 다수 허용).
- **FR3** Manifest는 `db-schema.md`의 JSON 스키마 필드를 전부 포함하고, 본체(서명 필드 제외)를
  ed25519로 서명해 `signature` 필드에 담는다. 백업 암호문과 **같은 목적지·같은 논리적 위치**에
  `<object_key>.manifest.json`으로 동봉한다.
- **FR4** `catalog rebuild --from <dest>`는 SQLite를 새로 만들고(또는 비우고) 목적지에서 매니페스트만
  읽어 `backups`/`artifacts`/`verifications`를 100% 재구성한다. 서명 검증에 실패한 매니페스트는
  카탈로그에 반영하지 않고 별도 리포트로만 남긴다.
- **FR5** 복원(`restore`)·검증(`verify`) 경로는 SQLite에서 읽은 값을 **복원 실행의 최종 근거로 쓰지
  않는다**. 반드시 목적지에서 매니페스트 원본을 다시 받아 서명을 재검증한 뒤에만 진행한다(H5).
- **FR6** overlap guard: (a) OS 네임드 뮤텍스로 같은 job의 동시 기동을 즉시 차단(빠른 실패),
  (b) `runs(job) WHERE status='running'` 부분 UNIQUE 인덱스로 DB 레벨에서 최종 방어.
  두 방어 중 하나가 우회되어도(다른 프로세스 그룹, 뮤텍스 미해제 등) 다른 하나가 막는다.
- **FR7** 롤백/치환 탐지: 매니페스트에 job 단위 **단조증가 시퀀스(`manifest_seq`)**와
  **이전 매니페스트 해시 체인(`prev_manifest_blake3`)**을 넣어, 목적지의 과거 버전 객체를 몰래
  최신인 척 되돌려 놓거나(rollback), 다른 백업의 서명된 매니페스트를 가져다 붙이는(substitution)
  공격을 `catalog rebuild`/`verify` 시점에 탐지한다.

### 비기능 요구사항 (NFR)
- **크래시 안전성**: 카탈로그 파일이 손상/삭제되어도 데이터 유실 없이(매니페스트가 살아있는 한)
  전체 이력을 복구할 수 있어야 한다.
- **성능**: 카탈로그 쓰기가 덤프 파이프라인(S05)의 임계 경로를 막지 않는다. 쓰기는 짧은 트랜잭션이며
  비동기 호출자는 결과를 await만 하고 블로킹되지 않는다.
- **이식성**: 1차 타깃은 Windows(Task Scheduler, DPAPI와 동일 호스트)이나, 네임드 뮤텍스/파일 경로는
  크로스플랫폼 크레이트로 추상화해 리눅스 개발환경에서도 테스트 가능하게 한다.

## Dependencies

- **S01 (스캐폴딩 + Config)**: 카탈로그 DB 경로, 목적지 목록, job 정의(job_name, db_alias 등)를
  config에서 받는다. `catalog rebuild` CLI 서브커맨드의 인자 파싱(`--from`, `--dry-run`, `--force`)은
  S01 clap 정의에 속하고, 본 섹션은 그 핸들러 함수만 구현한다.
- **S02 (SecretManager)**: ed25519 서명/검증을 수행하는 `ManifestSigner`/`ManifestVerifier` 트레이트의
  **구현체**를 제공한다. S08은 트레이트만 정의/소비하고 키 관리는 전혀 하지 않는다(관심사 분리,
  `team-review.md` P0-5와 동일 원칙). S02가 아직 코드로 존재하지 않으므로, 본 섹션은 트레이트
  시그니처를 **잠정 계약(provisional contract)**으로 정의하고 S02 구현 시 이 계약에 맞춘다.
- **S05 (Pipeline)**: 덤프 완료 후 `Manifest` 값을 채워 넘겨준다(플레인텍스트 해시, 바이트 수,
  압축/암호화 파라미터 등은 S05가 파이프라인 중 계측한 값).
- **S06/S07 (Destination 구현체, 아직 미착수)**: `catalog rebuild --from <dest>`가 목적지에서 매니페스트
  객체 목록을 나열/다운로드해야 한다. S06은 S05 → S08 순으로 의존하므로(`sections/index.md` 레이어),
  S08이 S06을 직접 의존하면 순환이 생긴다. 따라서 **S08은 자체적으로 작은 `ManifestSource` 트레이트를
  정의**하고, 로컬 파일시스템 구현체(`LocalManifestSource`)만 이 섹션에서 제공한다(1차 walking
  skeleton = PG→Local 요건 충족). S6/S7가 완성되면 `S3Dest`/`SshDest`에 동일 트레이트를 구현해
  그대로 재사용한다 — 새 rebuild 로직을 만들 필요가 없다.

## Reference Libraries

| 크레이트 | 용도 | 비고 |
|---|---|---|
| `rusqlite` (`bundled`, `backup` features) | SQLite 접근, WAL/PRAGMA, 트랜잭션 | `bundled`로 시스템 SQLite 버전 차이 제거(Windows 배포 단순화) |
| `serde` / `serde_json` | Manifest ↔ JSON, 구조체 필드 선언 순서가 곧 정준(canonical) 직렬화 순서 | HashMap 대신 struct/Vec만 사용해 결정적 바이트열 보장 |
| `blake3` | manifest 해시 체인(`prev_manifest_blake3`), 무결성 지문 | S02/S05와 해시 알고리즘 통일 |
| `uuid` (`v4`, `serde`) | `backup_id`, `verifications.id`, `runs.id` | |
| `time` (`formatting`, `parsing`, `serde-well-known`) | RFC3339 `TIMESTAMPTZ` 문자열 | `OffsetDateTime` + `Rfc3339` |
| `base64` | 서명 바이트 ↔ `signature.sig` 문자열 | |
| `named-lock` | 크로스플랫폼 OS 네임드 뮤텍스(overlap guard 1차 방어) | Windows: `CreateMutexW` 래핑, Unix: flock 기반 |
| `async-trait` | `ManifestSource`, `ManifestSigner` 등 async 트레이트 | |
| `thiserror` | 에러 타입 | |
| `tokio` (`rt-multi-thread`, `sync`, `macros`, `fs`) | writer 스레드와의 채널 통신, async 카탈로그 API | |

`Cargo.toml` 발췌:

```toml
[dependencies]
rusqlite = { version = "0.31", features = ["bundled", "backup"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
blake3 = "1"
uuid = { version = "1", features = ["v4", "serde"] }
time = { version = "0.3", features = ["formatting", "parsing", "serde-well-known"] }
base64 = "0.22"
named-lock = "0.4"
async-trait = "0.1"
thiserror = "1"
tokio = { version = "1", features = ["rt-multi-thread", "sync", "macros", "fs"] }

[dev-dependencies]
ed25519-dalek = { version = "2", features = ["rand_core"] } # 테스트용 서명자 구현
tempfile = "3"
```

> 버전 고정(`=x.y.z`)과 `cargo-deny`/`cargo-audit` 게이트는 S11(보안 하드닝) 소관. 여기 표기한 버전은
> 설계 시점 참고치이며, 실제 `Cargo.lock` 커밋 시 S11 정책을 따른다.

## Implementation

### 8.1 SQLite 연결 관리 — WAL + PRAGMA + 단일 writer

writer는 전용 OS 스레드가 `rusqlite::Connection` 하나를 독점 소유하고, 나머지 코드는 채널로 명령을
보내는 **싱글 라이터 액터** 패턴을 쓴다. 읽기는 WAL 덕분에 별도의 읽기 전용 커넥션을 매번 열어도
writer를 막지 않는다.

```rust
// src/catalog/conn.rs
use rusqlite::{Connection, OpenFlags};
use std::path::Path;

pub const SCHEMA_SQL: &str = include_str!("schema.sql");

/// writer 전용 커넥션. 프로세스 전체에서 단 하나만 존재해야 한다(P1-5).
pub fn open_writer_conn(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open(db_path)?;
    apply_pragmas(&conn)?;
    conn.execute_batch(SCHEMA_SQL)?; // CREATE TABLE IF NOT EXISTS로 작성됨, 멱등
    Ok(conn)
}

/// 읽기 전용 커넥션. 몇 개든 동시에 열어도 WAL 리더끼리는 서로 막지 않는다.
pub fn open_reader_conn(db_path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        db_path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    // 리더도 busy_timeout은 걸어둔다: writer의 짧은 checkpoint 구간과 겹칠 수 있음.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(conn)
}

fn apply_pragmas(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "synchronous", "NORMAL")?; // WAL + NORMAL = 크래시 안전, 성능 균형
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}
```

```sql
-- src/catalog/schema.sql  (db-schema.md DDL과 1:1 동일. 이 파일이 유일한 정본 스키마 소스)
CREATE TABLE IF NOT EXISTS backups (
  id TEXT PRIMARY KEY,
  job TEXT NOT NULL,
  source TEXT NOT NULL,
  engine TEXT NOT NULL,
  kind TEXT NOT NULL,
  pairs_with_globals_backup_id TEXT,
  started_at TEXT NOT NULL,
  finished_at TEXT,
  status TEXT NOT NULL,
  server_version TEXT,
  dump_tool TEXT,
  dump_tool_version TEXT,
  dump_format TEXT,
  charset TEXT,
  plaintext_blake3 TEXT,
  plaintext_bytes INTEGER,
  cipher_bytes INTEGER,
  dek_wrap_gen INTEGER,
  manifest_signed INTEGER,
  schema_version INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS artifacts (
  backup_id TEXT NOT NULL REFERENCES backups(id),
  dest_name TEXT NOT NULL,
  dest_kind TEXT NOT NULL,
  object_key TEXT NOT NULL,
  object_checksum TEXT,
  immutability TEXT,
  retention_until TEXT,
  status TEXT NOT NULL,
  uploaded_at TEXT,
  PRIMARY KEY (backup_id, dest_name)
);

CREATE TABLE IF NOT EXISTS verifications (
  id TEXT PRIMARY KEY,
  backup_id TEXT NOT NULL REFERENCES backups(id),
  verified_at TEXT NOT NULL,
  level INTEGER NOT NULL,
  result TEXT NOT NULL,
  restore_target_version TEXT,
  restored_rows INTEGER,
  schema_hash TEXT,
  notes TEXT
);

CREATE TABLE IF NOT EXISTS runs (
  id TEXT PRIMARY KEY,
  job TEXT NOT NULL,
  started_at TEXT NOT NULL,
  status TEXT NOT NULL
);
CREATE UNIQUE INDEX IF NOT EXISTS idx_runs_single_active ON runs(job) WHERE status='running';

CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT);

CREATE INDEX IF NOT EXISTS idx_backups_job_started ON backups(job, started_at);
CREATE INDEX IF NOT EXISTS idx_artifacts_status ON artifacts(status);
CREATE INDEX IF NOT EXISTS idx_verifications_backup_level ON verifications(backup_id, level);
```

writer 액터: 클로저를 채널로 받아 단일 커넥션 위에서 순차 실행한다.

```rust
// src/catalog/writer.rs
use crate::catalog::conn::open_writer_conn;
use rusqlite::Connection;
use std::path::PathBuf;
use tokio::sync::{mpsc, oneshot};

type WriteJob = Box<dyn FnOnce(&mut Connection) -> anyhow::Result<()> + Send>;

pub struct CatalogWriterHandle {
    tx: mpsc::Sender<(WriteJob, oneshot::Sender<anyhow::Result<()>>)>,
}

impl CatalogWriterHandle {
    /// 전용 OS 스레드를 하나 띄우고, 그 스레드가 db_path의 유일한 writer 커넥션을 소유한다.
    pub fn spawn(db_path: PathBuf) -> anyhow::Result<Self> {
        let (tx, mut rx) =
            mpsc::channel::<(WriteJob, oneshot::Sender<anyhow::Result<()>>)>(64);

        std::thread::Builder::new()
            .name("catalog-writer".into())
            .spawn(move || {
                let mut conn = match open_writer_conn(&db_path) {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!("catalog writer 커넥션 오픈 실패: {e}");
                        return;
                    }
                };
                while let Some((job, reply)) = rx.blocking_recv() {
                    let result = job(&mut conn);
                    let _ = reply.send(result);
                }
            })?;

        Ok(Self { tx })
    }

    pub async fn run<F>(&self, job: F) -> anyhow::Result<()>
    where
        F: FnOnce(&mut Connection) -> anyhow::Result<()> + Send + 'static,
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        self.tx.send((Box::new(job), reply_tx)).await?;
        reply_rx.await?
    }
}
```

### 8.2 Manifest 타입 — `db-schema.md` 필드 전체 + 롤백 방지 확장 필드

구조체 필드 선언 순서가 `serde_json` 직렬화 순서를 결정한다(HashMap을 쓰지 않는 한 결정적).
서명 대상 바이트열이 항상 동일하게 재현되려면 이 성질이 필수다.

```rust
// src/manifest/types.rs
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub manifest_version: u32,
    pub backup_id: String,
    pub job_name: String,
    pub db_alias: String,
    pub created_at: String, // RFC3339
    pub db_engine: String,
    pub server_version: String,
    pub server_version_detected_at: String,
    pub database_name: String,
    pub charset: Option<String>,
    pub collation: Option<String>,
    #[serde(default)]
    pub extensions: Vec<ExtensionInfo>,
    #[serde(default)]
    pub non_transactional_tables: Vec<String>,
    pub dump_tool: String,
    pub dump_tool_version: String,
    pub dump_format: String,
    pub dump_flags: String,
    pub pairs_with_globals_backup_id: Option<String>,
    pub binlog_coordinates: Option<String>,
    pub gtid_set: Option<String>,
    pub compression: CompressionInfo,
    pub encryption: EncryptionInfo,
    pub plaintext_blake3: String,
    pub plaintext_bytes: u64,
    pub compressed_bytes: u64,
    pub encrypted_bytes: u64,
    pub checksum_final: String,
    pub destination: DestinationInfo,
    pub verify: VerifyInfo,

    // --- S08 추가 필드 (db-schema.md 대비 추가, H5 롤백/치환 탐지 전용) ---
    // manifest_version:1 문서에 없어도 #[serde(default)]로 하위호환 파싱된다.
    /// 같은 job_name 내에서 1부터 단조증가. 목적지 객체를 과거 시점으로 되돌리는
    /// 롤백 공격을 catalog rebuild 시점에 탐지하기 위함.
    #[serde(default)]
    pub manifest_seq: u64,
    /// 같은 job의 직전 매니페스트(서명 포함 전체 바이트)의 blake3. 체인이 끊기면 변조/치환 의심.
    #[serde(default)]
    pub prev_manifest_blake3: Option<String>,

    /// 서명 계산 시에는 반드시 None으로 두고 계산 후 채운다.
    pub signature: Option<SignatureInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtensionInfo { pub name: String, pub version: String }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressionInfo { pub algo: String, pub level: i32 }

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncryptionInfo {
    pub algo: String,
    pub dek_gen: u64,
    pub unwrap_paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DestinationInfo {
    #[serde(rename = "type")]
    pub kind: String, // s3 | ssh | local
    pub bucket: Option<String>,
    pub object_key: String,
    pub object_lock_mode: Option<String>,
    pub retain_until: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyInfo {
    pub level: u8,
    pub verified_at: Option<String>,
    pub method: Option<String>,
    #[serde(default)]
    pub row_counts: std::collections::BTreeMap<String, u64>, // BTreeMap: 결정적 순서
    pub restore_target_version: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SignatureInfo {
    pub alg: String, // "ed25519"
    pub pubkey_fpr: String,
    pub sig: String, // base64
}
```

### 8.3 서명 / 검증 — S02 SecretManager 잠정 계약

```rust
// src/manifest/sign.rs
use crate::manifest::types::{Manifest, SignatureInfo};
use thiserror::Error;

/// S02(SecretManager)가 구현해야 하는 잠정 계약.
/// S08은 키 자체를 절대 다루지 않는다 — 오프라인 개인키는 S02/운영자 손 밖으로 나가지 않는다.
pub trait ManifestSigner {
    /// canonical_bytes(서명 필드 제외 JSON)에 대한 ed25519 서명을 만든다.
    fn sign(&self, canonical_bytes: &[u8]) -> Result<RawSignature, SignError>;
}

pub trait ManifestVerifier {
    /// pubkey_fpr로 지정된 공개키로 canonical_bytes에 대한 sig를 검증한다.
    /// 실패 시 반드시 Err — 절대 "경고만 하고 통과"하지 않는다(H5).
    fn verify(
        &self,
        canonical_bytes: &[u8],
        sig: &[u8],
        pubkey_fpr: &str,
    ) -> Result<(), VerifyError>;
}

pub struct RawSignature {
    pub bytes: Vec<u8>,
    pub pubkey_fpr: String,
}

#[derive(Debug, Error)]
pub enum SignError {
    #[error("서명 실패: {0}")]
    Backend(String),
}

#[derive(Debug, Error)]
pub enum VerifyError {
    #[error("서명 검증 실패(위조 또는 손상 의심)")]
    Invalid,
    #[error("알 수 없는 pubkey_fpr: {0}")]
    UnknownKey(String),
}

#[derive(Debug, Error)]
pub enum ManifestError {
    #[error("매니페스트가 서명되지 않음 — 신뢰 불가, 거부")]
    Unsigned,
    #[error("직렬화 실패: {0}")]
    Serialize(#[from] serde_json::Error),
    #[error(transparent)]
    Sign(#[from] SignError),
    #[error("서명 검증 실패 — 위조/치환 의심")]
    TamperedOrForged,
    #[error("base64 디코드 실패: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// signature 필드를 제외한 결정적 바이트열. 서명/검증 양쪽에서 동일 함수를 사용해야
/// "무엇에 서명했는지"와 "무엇을 검증하는지"가 항상 일치한다.
fn canonical_bytes_for_signing(manifest: &Manifest) -> Result<Vec<u8>, ManifestError> {
    let mut unsigned = manifest.clone();
    unsigned.signature = None;
    Ok(serde_json::to_vec(&unsigned)?)
}

pub fn seal_manifest(
    manifest: &mut Manifest,
    signer: &dyn ManifestSigner,
) -> Result<(), ManifestError> {
    manifest.signature = None;
    let body = canonical_bytes_for_signing(manifest)?;
    let raw = signer.sign(&body)?;
    manifest.signature = Some(SignatureInfo {
        alg: "ed25519".to_string(),
        pubkey_fpr: raw.pubkey_fpr,
        sig: base64::Engine::encode(&base64::engine::general_purpose::STANDARD, raw.bytes),
    });
    Ok(())
}

/// 목적지에서 읽은 매니페스트를 사용하기 전 항상 이 함수를 통과시켜야 한다.
/// 이 함수를 거치지 않은 Manifest 값은 "신뢰되지 않은 입력"으로 취급한다.
pub fn verify_manifest(
    manifest: &Manifest,
    verifier: &dyn ManifestVerifier,
) -> Result<(), ManifestError> {
    let sig_info = manifest.signature.as_ref().ok_or(ManifestError::Unsigned)?;
    let mut unsigned = manifest.clone();
    unsigned.signature = None;
    let body = canonical_bytes_for_signing(&unsigned)?;
    let sig_bytes =
        base64::Engine::decode(&base64::engine::general_purpose::STANDARD, &sig_info.sig)?;
    verifier
        .verify(&body, &sig_bytes, &sig_info.pubkey_fpr)
        .map_err(|_| ManifestError::TamperedOrForged)?;
    Ok(())
}

/// 체인 해시는 "서명까지 포함한 완전한 바이트열"에 대해 계산한다 — 이전 항목의 서명 자체를
/// 다음 항목이 커밋하는 구조라 서명 부위만 바꿔치기하는 공격도 체인 검증에서 걸린다.
pub fn full_signed_bytes(manifest: &Manifest) -> Result<Vec<u8>, ManifestError> {
    Ok(serde_json::to_vec(manifest)?)
}

pub fn blake3_of_signed_manifest(manifest: &Manifest) -> Result<String, ManifestError> {
    let bytes = full_signed_bytes(manifest)?;
    Ok(blake3::hash(&bytes).to_hex().to_string())
}
```

### 8.4 매니페스트 동봉 — `ManifestSource` (S08 정의, S06/S07가 나중에 구현)

```rust
// src/manifest/source.rs
use async_trait::async_trait;

#[async_trait]
pub trait ManifestSource: Send + Sync {
    /// job_filter가 Some이면 해당 job의 매니페스트만, None이면 전부 나열.
    /// 반환값은 목적지 상대 키(예: "prod-pg/2026/07/16/app.manifest.json").
    async fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>>;
    async fn read_manifest_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>>;
}

/// 1차 walking skeleton(PG→Local, plan.md 슬라이스 1)용 구현.
/// S6/S7 완성 후 S3Dest/SshDest도 동일 트레이트를 구현해 재사용한다.
pub struct LocalManifestSource {
    pub root: std::path::PathBuf,
}

#[async_trait]
impl ManifestSource for LocalManifestSource {
    async fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>> {
        let root = self.root.clone();
        let job_filter = job_filter.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            let mut out = Vec::new();
            for entry in walkdir::WalkDir::new(&root).into_iter().filter_map(Result::ok) {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    && p.to_string_lossy().ends_with(".manifest.json")
                {
                    let rel = p.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/");
                    if let Some(ref job) = job_filter {
                        if !rel.starts_with(job.as_str()) {
                            continue;
                        }
                    }
                    out.push(rel);
                }
            }
            Ok(out)
        })
        .await?
    }

    async fn read_manifest_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(tokio::fs::read(self.root.join(key)).await?)
    }
}
```

> `walkdir`는 이 함수 하나만을 위한 의존이라 S06/S07가 완성되면 opendal 기반 리스팅으로 대체될 수
> 있음을 명시해 둔다. 실제 목적지 업로드/멀티파트 커밋은 S05/S06 소관이며, 본 섹션은 "이미 업로드된
> 매니페스트를 읽어 검증/재구성"하는 소비자 역할만 한다.

### 8.5 Catalog Upsert — 매니페스트를 SQLite 캐시에 반영

```rust
// src/catalog/upsert.rs
use crate::catalog::writer::CatalogWriterHandle;
use crate::manifest::types::Manifest;
use rusqlite::params;

pub async fn upsert_backup_from_manifest(
    writer: &CatalogWriterHandle,
    m: Manifest,
    dest_name: String,
    dest_kind: String,
) -> anyhow::Result<()> {
    writer
        .run(move |conn| {
            let tx = conn.transaction()?;
            let kind = if m.database_name == "cluster" { "globals" } else { "db" };
            tx.execute(
                "INSERT INTO backups
                 (id, job, source, engine, kind, pairs_with_globals_backup_id,
                  started_at, finished_at, status, server_version, dump_tool,
                  dump_tool_version, dump_format, charset, plaintext_blake3,
                  plaintext_bytes, cipher_bytes, dek_wrap_gen, manifest_signed, schema_version)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)
                 ON CONFLICT(id) DO UPDATE SET
                    status=excluded.status, finished_at=excluded.finished_at,
                    plaintext_blake3=excluded.plaintext_blake3,
                    manifest_signed=excluded.manifest_signed",
                params![
                    m.backup_id, m.job_name, m.db_alias, m.db_engine, kind,
                    m.pairs_with_globals_backup_id, m.created_at, "success",
                    m.server_version, m.dump_tool, m.dump_tool_version, m.dump_format,
                    m.charset, m.plaintext_blake3, m.plaintext_bytes as i64,
                    m.encrypted_bytes as i64, m.encryption.dek_gen as i64,
                    m.signature.is_some() as i32, m.manifest_version as i64,
                ],
            )?;
            tx.execute(
                "INSERT INTO artifacts
                 (backup_id, dest_name, dest_kind, object_key, object_checksum,
                  immutability, retention_until, status, uploaded_at)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,'committed',?8)
                 ON CONFLICT(backup_id, dest_name) DO UPDATE SET
                    object_checksum=excluded.object_checksum, status='committed'",
                params![
                    m.backup_id, dest_name, dest_kind, m.destination.object_key,
                    m.checksum_final, m.destination.object_lock_mode,
                    m.destination.retain_until, m.created_at,
                ],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
}
```

> **중요**: 이 함수가 채우는 SQLite 값은 "조회 캐시"일 뿐이다. 복원 실행 여부를 이 테이블의
> `status`/`manifest_signed` 값만으로 결정하지 않는다 — 8.7의 재검증 원칙을 반드시 함께 적용한다.

### 8.6 Overlap Guard — 네임드 뮤텍스 + 부분 UNIQUE 인덱스 + 정체(stale) 회수

```rust
// src/catalog/overlap.rs
use crate::catalog::writer::CatalogWriterHandle;
use named_lock::{NamedLock, NamedLockGuard};
use rusqlite::params;
use time::OffsetDateTime;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum OverlapError {
    #[error("job '{0}'이 이미 실행 중(overlap guard)")]
    AlreadyRunning(String),
    #[error("OS 네임드 뮤텍스 획득 실패: {0}")]
    MutexUnavailable(String),
    #[error(transparent)]
    Db(#[from] anyhow::Error),
}

/// 정체된 running row 인정 기준. run 자체는 대개 수십 분 내 끝나므로 넉넉히 잡는다.
const MAX_RUN_SECONDS: i64 = 6 * 3600;

pub struct RunGuard {
    run_id: String,
    job: String,
    // Drop 시 자동 해제. 소유권을 여기 갖고 있는 동안만 뮤텍스가 걸려 있다.
    _mutex_guard: NamedLockGuard,
    finished: bool,
}

impl RunGuard {
    /// 정상 종료 시 명시적으로 호출한다. async DB 갱신은 Drop에서 보장 불가능하므로
    /// (S05의 commit protocol과 동일 원칙) 반드시 명시 호출 경로로 처리한다.
    pub async fn finish(mut self, writer: &CatalogWriterHandle, status: &'static str) -> anyhow::Result<()> {
        let run_id = self.run_id.clone();
        writer
            .run(move |conn| {
                conn.execute(
                    "UPDATE runs SET status=?1 WHERE id=?2",
                    params![status, run_id],
                )?;
                Ok(())
            })
            .await?;
        self.finished = true;
        Ok(())
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        if !self.finished {
            // finish()를 호출하지 않고 드롭됨(패닉/크래시 등). DB 상태는 'running'으로 남고,
            // 다음 시도 때 아래 stale 회수 로직이 처리한다. 여기서는 관측 가능하게 로그만 남긴다.
            tracing::warn!(job = %self.job, run_id = %self.run_id,
                "RunGuard가 finish() 없이 드롭됨 — 다음 실행 시 stale로 회수됨");
        }
    }
}

pub async fn try_acquire_run(
    writer: &CatalogWriterHandle,
    job: &str,
) -> Result<RunGuard, OverlapError> {
    // 1차 방어: OS 네임드 뮤텍스로 빠르게 실패(동일 호스트, 다른 프로세스).
    let lock_name = format!("Global\\dbbackup-run-{}", short_hash(job));
    let named_lock = NamedLock::create(&lock_name)
        .map_err(|e| OverlapError::MutexUnavailable(e.to_string()))?;
    let mutex_guard = named_lock
        .try_lock()
        .map_err(|_| OverlapError::AlreadyRunning(job.to_string()))?;

    // 2차 방어: SQLite 부분 UNIQUE 인덱스(다른 호스트/뮤텍스 우회 상황까지 방어).
    let job_owned = job.to_string();
    let run_id = Uuid::new_v4().to_string();
    let started_at = OffsetDateTime::now_utc();
    let started_at_str = started_at
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap();

    let insert_result = writer
        .run({
            let run_id = run_id.clone();
            let job_owned = job_owned.clone();
            let started_at_str = started_at_str.clone();
            move |conn| {
                let tx = conn.transaction()?;
                // 정체된 running row 회수: MAX_RUN_SECONDS 초과 시 강등시켜 새 실행이 들어갈 자리를 만든다.
                tx.execute(
                    "UPDATE runs SET status='coalesced'
                     WHERE job=?1 AND status='running'
                       AND (unixepoch('now') - unixepoch(started_at)) > ?2",
                    params![job_owned, MAX_RUN_SECONDS],
                )?;
                // 실제 삽입. 여전히 다른 running row가 있으면 부분 UNIQUE 인덱스가 막는다.
                let inserted = tx.execute(
                    "INSERT INTO runs (id, job, started_at, status) VALUES (?1,?2,?3,'running')",
                    params![run_id, job_owned, started_at_str],
                );
                match inserted {
                    Ok(_) => { tx.commit()?; Ok(()) }
                    Err(rusqlite::Error::SqliteFailure(e, _))
                        if e.code == rusqlite::ErrorCode::ConstraintViolation =>
                    {
                        tx.rollback()?;
                        Err(anyhow::anyhow!("overlap"))
                    }
                    Err(e) => Err(e.into()),
                }
            }
        })
        .await;

    match insert_result {
        Ok(()) => Ok(RunGuard {
            run_id,
            job: job.to_string(),
            _mutex_guard: mutex_guard,
            finished: false,
        }),
        Err(_) => Err(OverlapError::AlreadyRunning(job.to_string())),
        // mutex_guard는 이 스코프를 벗어나며 자동 해제됨.
    }
}

fn short_hash(s: &str) -> String {
    blake3::hash(s.as_bytes()).to_hex()[..16].to_string()
}
```

### 8.7 Catalog Rebuild — `catalog rebuild --from <dest>` (P0-6)

```rust
// src/manifest/rebuild.rs
use crate::catalog::writer::CatalogWriterHandle;
use crate::manifest::sign::{blake3_of_signed_manifest, verify_manifest, ManifestVerifier};
use crate::manifest::source::ManifestSource;
use crate::manifest::types::Manifest;
use std::collections::BTreeMap;

pub struct RebuildArgs {
    pub job_filter: Option<String>,
    pub dry_run: bool,
    /// 체인 불연속(gap/rollback 의심)이 있어도 검증 가능한 항목만 강제로 반영.
    pub force: bool,
}

pub struct RebuildReport {
    pub accepted: usize,
    pub rejected_unsigned_or_tampered: Vec<String>,
    pub chain_breaks: Vec<ChainBreak>,
}

pub struct ChainBreak {
    pub job_name: String,
    pub at_seq: u64,
    pub reason: String,
}

pub async fn catalog_rebuild(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    writer: &CatalogWriterHandle,
    args: RebuildArgs,
) -> anyhow::Result<RebuildReport> {
    let keys = source.list_manifests(args.job_filter.as_deref()).await?;

    let mut rejected = Vec::new();
    let mut by_job: BTreeMap<String, Vec<Manifest>> = BTreeMap::new();

    // 1) 전부 읽고, 서명 검증 실패는 즉시 배제(카탈로그에 절대 반영하지 않는다).
    for key in &keys {
        let bytes = source.read_manifest_bytes(key).await?;
        let manifest: Manifest = match serde_json::from_slice(&bytes) {
            Ok(m) => m,
            Err(_) => { rejected.push(key.clone()); continue; }
        };
        if verify_manifest(&manifest, verifier).is_err() {
            rejected.push(key.clone());
            continue;
        }
        by_job.entry(manifest.job_name.clone()).or_default().push(manifest);
    }

    // 2) job별로 manifest_seq 순으로 정렬 후 해시 체인 연속성 검증.
    let mut chain_breaks = Vec::new();
    let mut accepted_manifests: Vec<(Manifest, String)> = Vec::new(); // (manifest, dest_name placeholder)

    for (job_name, mut manifests) in by_job {
        manifests.sort_by_key(|m| m.manifest_seq);

        let mut prev_hash: Option<String> = None;
        let mut prev_seq: Option<u64> = None;
        for m in manifests {
            let expected_prev = prev_hash.clone();
            let seq_ok = match prev_seq {
                None => true, // 첫 항목
                Some(p) => m.manifest_seq == p + 1,
            };
            let hash_ok = m.prev_manifest_blake3 == expected_prev;

            if !seq_ok || !hash_ok {
                chain_breaks.push(ChainBreak {
                    job_name: job_name.clone(),
                    at_seq: m.manifest_seq,
                    reason: if !seq_ok {
                        "manifest_seq 불연속(gap 또는 역행) — 롤백/치환 의심".to_string()
                    } else {
                        "prev_manifest_blake3 불일치 — 체인 위조 의심".to_string()
                    },
                });
                if !args.force {
                    // 이 job의 이후 항목은 신뢰 사슬이 끊긴 시점부터 반영하지 않는다.
                    break;
                }
            }

            prev_hash = Some(blake3_of_signed_manifest(&m)?);
            prev_seq = Some(m.manifest_seq);
            accepted_manifests.push((m, job_name.clone()));
        }
    }

    if args.dry_run {
        return Ok(RebuildReport {
            accepted: accepted_manifests.len(),
            rejected_unsigned_or_tampered: rejected,
            chain_breaks,
        });
    }

    // 3) 실제 반영. schema_version/pubkey_fpr 등 meta는 매니페스트에서 유도 가능한 것만 채운다.
    //    argon2_params/dpapi_entropy_ref는 매니페스트에 없는 로컬 비밀 설정이므로 여기서 복구되지
    //    않는다 — 운영자가 별도로 SecretManager(S02) unlock 절차를 다시 수행해야 한다.
    let n = accepted_manifests.len();
    for (m, dest_name) in accepted_manifests {
        let dest_kind = m.destination.kind.clone();
        crate::catalog::upsert::upsert_backup_from_manifest(writer, m, dest_name, dest_kind)
            .await?;
    }

    Ok(RebuildReport {
        accepted: n,
        rejected_unsigned_or_tampered: rejected,
        chain_breaks,
    })
}
```

CLI 핸들러(파싱은 S01 clap 정의, 여기서는 로직만):

```rust
// src/cli/catalog_cmd.rs (S01의 clap 서브커맨드에서 호출)
pub async fn cmd_catalog_rebuild(
    from_dest_root: std::path::PathBuf,
    dry_run: bool,
    force: bool,
    catalog_db_path: std::path::PathBuf,
    verifier: &dyn crate::manifest::sign::ManifestVerifier,
) -> anyhow::Result<()> {
    let source = crate::manifest::source::LocalManifestSource { root: from_dest_root };
    let writer = crate::catalog::writer::CatalogWriterHandle::spawn(catalog_db_path)?;
    let report = crate::manifest::rebuild::catalog_rebuild(
        &source, verifier, &writer,
        crate::manifest::rebuild::RebuildArgs { job_filter: None, dry_run, force },
    ).await?;

    println!("반영: {}건, 거부(미서명/변조): {}건, 체인 불연속: {}건",
        report.accepted, report.rejected_unsigned_or_tampered.len(), report.chain_breaks.len());
    for cb in &report.chain_breaks {
        println!("  [경고] job={} seq={} — {}", cb.job_name, cb.at_seq, cb.reason);
    }
    if !report.chain_breaks.is_empty() && !force {
        println!("체인 불연속이 감지되어 해당 시점 이후는 반영하지 않았습니다. \
                   내용을 확인한 뒤 --force로 강제 반영하거나, 목적지를 직접 재조사하세요.");
    }
    Ok(())
}
```

### 8.8 복원 경로 재검증 원칙 (H5) — "카탈로그를 단독 신뢰하지 않는다"

카탈로그 값은 목록 표시/필터링 UX 전용이다. 실제로 복원(S09 `Restorer`)을 실행하기 직전에는
**반드시** 목적지에서 매니페스트를 다시 받아 재검증한다. 아래는 S09가 소비할 계약 함수다.

```rust
// src/manifest/restore_gate.rs
use crate::manifest::sign::{verify_manifest, ManifestError, ManifestVerifier};
use crate::manifest::source::ManifestSource;
use crate::manifest::types::Manifest;

/// 복원/L3 검증 직전 호출. 카탈로그의 어떤 값도 이 함수의 입력으로 쓰지 않는다 —
/// object_key만 사용자가 지정(또는 카탈로그에서 "힌트"로만 가져오고), 실제 판단은
/// 이 함수가 목적지에서 새로 읽은 바이트에 대해서만 내린다.
pub async fn load_and_verify_manifest_for_restore(
    source: &dyn ManifestSource,
    verifier: &dyn ManifestVerifier,
    manifest_key: &str,
) -> Result<Manifest, ManifestError> {
    let bytes = source
        .read_manifest_bytes(manifest_key)
        .await
        .map_err(|_| ManifestError::Unsigned)?; // 읽기 실패도 "신뢰 불가"로 취급
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    verify_manifest(&manifest, verifier)?;
    Ok(manifest)
}
```

## Test Scenarios

1. **카탈로그 완전 소실 → rebuild 재구성 (P0-6 핵심)**: 정상 백업 N건을 생성해 로컬 목적지에
   매니페스트+암호문을 동봉한 뒤 `catalog.db` 파일을 삭제. `catalog rebuild --from <dest>` 실행 후
   `backups`/`artifacts` row 수와 필드 값이 원래 카탈로그와 동일한지 비교(N건, 상태, 체크섬 일치).
2. **변조된 매니페스트 거부**: 목적지에 동봉된 매니페스트 파일 1바이트를 직접 수정한 뒤 rebuild →
   해당 항목이 `rejected_unsigned_or_tampered`에 들어가고 `backups`에 반영되지 않는지 확인.
3. **서명 없는 매니페스트 거부**: `signature: null`인 매니페스트를 목적지에 두고 rebuild →
   `ManifestError::Unsigned`로 배제되는지 확인. `verify_manifest` 단위 테스트로도 별도 커버.
4. **롤백(치환) 탐지**: job의 최신 매니페스트(`manifest_seq=5`)를 과거 버전(`manifest_seq=3`)으로
   덮어쓴 뒤 rebuild → `chain_breaks`에 gap이 잡히고, `--force` 없이는 해당 job의 seq 3 이후가
   카탈로그에 반영되지 않는지 확인.
5. **해시 체인 위조 탐지**: `prev_manifest_blake3` 필드만 조작(다른 매니페스트의 해시로 치환)하고
   서명을 그대로 둔(불가능해야 정상이지만, 서명 검증을 우회한 경우를 가정한 방어 심층 테스트) 경우
   — 서명이 본체(prev_manifest_blake3 포함)를 커버하므로 `verify_manifest` 단계에서 이미 걸리는지
   확인(서명 검증이 1차 방어선, 체인 검증은 2차 방어선임을 증명).
6. **로컬 SQLite row 직접 조작 시 복원 차단**: `backups.status`를 SQL로 직접 `'success'`로 조작해도
   `load_and_verify_manifest_for_restore`가 목적지 원본을 다시 검증하므로 복원 절차가 이 조작에
   영향받지 않는지 확인(카탈로그 신뢰 배제 원칙, H5).
7. **overlap guard — 동시 기동 차단**: 동일 job으로 두 프로세스(또는 두 태스크)를 거의 동시에
   `try_acquire_run` 호출 → 정확히 하나만 `Ok(RunGuard)`, 나머지는 `OverlapError::AlreadyRunning`.
8. **stale run 회수**: `runs`에 `status='running'`, `started_at`을 `MAX_RUN_SECONDS` 이전으로 강제
   삽입(크래시 시뮬레이션) → 새 `try_acquire_run` 호출 시 기존 row가 `coalesced`로 강등되고 새 실행이
   성공하는지 확인.
9. **WAL + busy_timeout 동시성**: writer 스레드가 트랜잭션을 여는 동안 다수의 읽기 전용 커넥션이
   동시에 `SELECT`를 수행해도 `SQLITE_BUSY` 없이 완료되는지(busy_timeout 5000ms 이내) 확인.
10. **스키마 멱등성**: `SCHEMA_SQL`을 이미 존재하는 DB에 두 번 실행해도 에러 없이 통과하는지 확인
    (`CREATE TABLE IF NOT EXISTS` / `CREATE INDEX IF NOT EXISTS`).
11. **manifest_seq 하위호환**: `manifest_seq`/`prev_manifest_blake3` 필드가 없는(v1 초기 포맷) 매니페스트도
    `serde_json::from_slice`가 실패하지 않고 기본값(0/None)으로 파싱되는지 확인.

## Implementation Strategy

1. **8.1~8.2 (SQLite 골격) 먼저**: `open_writer_conn`/`open_reader_conn`/`schema.sql`/writer 액터를
   구현하고, 스키마 멱등성 테스트부터 통과시킨다. 이 단계는 다른 어떤 섹션에도 의존하지 않는다.
2. **8.3 (Manifest 타입+서명 계약)**: `ManifestSigner`/`ManifestVerifier` 트레이트를 확정하고,
   테스트 전용 `ed25519-dalek` 기반 구현으로 seal/verify 라운드트립을 먼저 검증한다. 이 트레이트
   시그니처를 S02 담당 구현 시점에 그대로 채택하도록 계약을 고정한다.
3. **8.4 (ManifestSource + LocalManifestSource)**: walking skeleton(PG→Local)에 필요한 최소 구현만
   먼저 만든다. S3/SSH 어댑터는 S06/S07에서 동일 트레이트로 추가.
4. **8.5 (Upsert)**: S05가 아직 없어도, 손으로 만든 `Manifest` 픽스처로 upsert 단위 테스트를 먼저
   작성해 스키마와의 정합을 확인한다.
5. **8.6 (Overlap guard)**: 네임드 뮤텍스는 플랫폼 의존적이므로 `named-lock` 크레이트의 동작을
   먼저 Windows/개발 리눅스 양쪽에서 스모크 테스트한 뒤, stale 회수 로직을 추가한다.
6. **8.7 (Rebuild)**: 8.1~8.4가 갖춰진 뒤 통합 테스트로 작성 — 정상 N건 생성 → 카탈로그 삭제 →
   rebuild → 비교, 이후 변조/롤백 시나리오를 추가한다.
7. **8.8 (복원 게이트)**: S09 구현 착수 시점에 맞춰 계약만 먼저 고정해 두고, 실제 소비는 S09에서.
8. 전 과정에서 **로컬 SQLite 값이 보안 판단에 쓰이는 곳이 있는지 코드 리뷰 체크리스트로 점검**
   (grep으로 `backups.status`/`manifest_signed` 등을 복원/검증 판단 분기에서 직접 참조하는 코드가
   있으면 리젝트).

## Quality Gate

- `cargo test -p dbbackup-catalog`: 위 11개 테스트 시나리오 전부 green.
- `cargo clippy --all-targets -- -D warnings`: 프로덕션 경로에 `unwrap()`/`expect()` 금지
  (테스트 코드 제외), 특히 서명 검증 실패 경로에서 panic 대신 `Result` 전파.
- 서명 검증 함수(`verify_manifest`)에 대해 **실패 케이스를 기본값으로 가정하는 테스트**를 최소
  3개 이상 유지(변조/미서명/알 수 없는 키) — "통과 케이스만 테스트하고 실패 케이스는 안 만드는"
  흔한 함정 방지.
- writer 액터 채널이 가득 찼을 때 백프레셔가 걸리는지(무한 대기 대신 명시적 타임아웃 또는 버퍼
  크기 근거 문서화) 확인.
- `SCHEMA_SQL`과 `db-schema.md`의 DDL 블록을 diff하는 CI 스크립트(또는 수동 리뷰 체크리스트 항목)로
  두 문서가 드리프트하지 않게 고정.
- rebuild 경로에 대한 통합 테스트는 실제 파일시스템(tempdir)을 사용해 "목적지에서 읽기"를
  모킹하지 않고 검증(과도한 모킹은 실제 I/O 에러 경로를 놓친다).

## Risk & Rollback

| 리스크 | 완화 |
|---|---|
| 단일 writer 스레드가 덤프 파이프라인 병목이 될 수 있음 | 카탈로그 트랜잭션은 짧고(단일 upsert), 채널 버퍼(64)로 순간 폭주 흡수. 실측 후 필요시 배치 upsert 도입 |
| `named-lock`이 플랫폼별로 동작이 미묘하게 다를 수 있음(Windows `Global\` 네임스페이스 vs 세션 로컬) | 부분 UNIQUE 인덱스가 최종 방어선이므로 뮤텍스 실패/불가용이 곧 overlap 허용으로 이어지지 않음(둘 다 통과해야 실행) |
| 시계 스큐로 `started_at`/`created_at` 비교가 왜곡될 수 있음 | 롤백 탐지의 1차 근거는 벽시계가 아니라 **`manifest_seq`(카운터)**와 **해시 체인**으로 한정, 시간은 사람이 읽는 보조 정보로만 사용 |
| 동일 job을 서로 다른 두 호스트가 목적지에 동시에 쓰는 구성(액티브-액티브)은 seq 충돌을 유발 | 현재 설계는 job당 단일 활성 쓰기 호스트를 전제로 문서화(운영 제약). 멀티호스트 지원은 스코프 밖 — 필요 시 별도 섹션에서 job 네임스페이스 분리로 확장 |
| `catalog rebuild`가 잘못된 결과를 카탈로그에 덮어쓸 위험 | `--dry-run` 기본 권장, 실제 반영 전 리포트로 accepted/rejected/chain_breaks 사람이 확인. 카탈로그 자체가 언제든 버릴 수 있는 캐시이므로 rebuild가 실패해도 매니페스트 원본(진실 소스)은 훼손되지 않음 — 최악의 경우도 "재시도 가능한 실패"로 국한 |
| 매니페스트 스키마에 `manifest_seq`/`prev_manifest_blake3`를 추가한 것이 `db-schema.md` 문서와 표면적으로 어긋나 보일 수 있음 | `#[serde(default)]`로 완전 하위호환, db-schema.md의 기존 필드는 전부 그대로 유지 — 문서 갱신 시 "S08 확장 필드"로 명시 반영 필요(별도 후속 작업으로 문서 동기화 권장) |

## Acceptance Criteria

- [ ] `db-schema.md`의 5개 테이블 DDL이 `schema.sql`에 필드 단위로 1:1 재현되어 있다.
- [ ] SQLite 커넥션은 `journal_mode=WAL`, `busy_timeout=5000`이 적용되고, 쓰기는 단일 writer
      스레드/커넥션을 통해서만 이뤄진다(코드 리뷰로 다른 쓰기 경로가 없음을 확인).
- [ ] `Manifest` 구조체가 `db-schema.md` JSON 스키마의 모든 필드를 포함하고, 서명 대상 바이트열이
      `signature` 필드를 제외한 결정적 직렬화 결과임이 테스트로 증명된다.
- [ ] `catalog rebuild --from <dest>`가 카탈로그 완전 삭제 상태에서 목적지 매니페스트만으로
      원래와 동일한 `backups`/`artifacts` 상태를 재구성한다(테스트 시나리오 1).
- [ ] 미서명/변조/체인 불연속 매니페스트는 rebuild 시 카탈로그에 반영되지 않고 리포트로만 노출된다
      (테스트 시나리오 2~5).
- [ ] 로컬 SQLite 값을 직접 조작해도 복원 게이트(`load_and_verify_manifest_for_restore`)가 목적지
      원본을 재검증하므로 실제 복원 판단에 영향이 없다(테스트 시나리오 6, H5 원칙 실증).
- [ ] overlap guard가 네임드 뮤텍스 + 부분 UNIQUE 인덱스 이중 방어로 동시 기동을 차단하고,
      크래시로 인한 stale `running` row는 `MAX_RUN_SECONDS` 초과 시 자동 회수된다
      (테스트 시나리오 7~8).
- [ ] `cargo clippy -D warnings` 통과, 서명 검증 실패 경로에 `unwrap`/`panic` 없음.

## Files

- `src/catalog/conn.rs` — WAL/PRAGMA 설정, writer/reader 커넥션 오픈
- `src/catalog/schema.sql` — DDL 정본(`db-schema.md`와 동기화 유지)
- `src/catalog/writer.rs` — 싱글 라이터 액터(전용 스레드 + 채널)
- `src/catalog/upsert.rs` — 매니페스트 → SQLite 캐시 반영
- `src/catalog/overlap.rs` — overlap guard(named mutex + 부분 UNIQUE + stale 회수)
- `src/manifest/types.rs` — `Manifest` 및 하위 구조체(db-schema.md 필드 + S08 확장 필드)
- `src/manifest/sign.rs` — `ManifestSigner`/`ManifestVerifier` 계약, seal/verify, 체인 해시
- `src/manifest/source.rs` — `ManifestSource` 트레이트 + `LocalManifestSource`
- `src/manifest/rebuild.rs` — `catalog rebuild --from <dest>` 핵심 로직
- `src/manifest/restore_gate.rs` — 복원 직전 재검증 게이트(S09 소비)
- `src/cli/catalog_cmd.rs` — `catalog rebuild` CLI 핸들러(파싱은 S01)
- `tests/catalog_rebuild.rs` — 시나리오 1~6 통합 테스트
- `tests/overlap_guard.rs` — 시나리오 7~8 통합 테스트
- `tests/wal_concurrency.rs` — 시나리오 9 동시성 테스트
