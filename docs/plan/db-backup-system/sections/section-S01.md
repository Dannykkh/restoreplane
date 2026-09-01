# S01 — 프로젝트 스캐폴딩 + Config 스키마 + CLI

> 레이어 0. 의존하는 섹션 없음. S02(SecretManager) ~ S11(보안 하드닝) 전 구간이 이 섹션의
> 크레이트(`dbbackup-core`, `dbbackup-config`, `dbbackup-cli`) 위에 얹힌다.
> 이 문서 하나만으로 낯선 개발자가 처음부터 구현할 수 있도록 자립적으로 작성됨.

## Background

이 프로젝트는 랜섬웨어 내성을 갖춘 Windows용 Rust DB 백업 오케스트레이터다(`plan.md` S1 채택:
Rust 오케스트레이터 + 네이티브 덤프 도구 호출). 전체 아키텍처는 `CLI(clap) → Core traits
(BackupSource/Destination/SecretManager/Restorer/Verifier/Tunnel/Catalog/Scheduler)`로
구성되며, 이 섹션(S01)은 그 중 가장 아래층인 **cargo 워크스페이스 레이아웃 + CLI 명령 표면 +
Config 스키마**를 확정한다.

`team-review.md`의 아키텍처 리뷰(P2-1, P2-2)는 최초 설계의 두 가지 구멍을 지적했다.

- **P2-1 (Config 갭)**: `[meta] schema_version` 부재(마이그레이션 불가), `[[ssh]]`(터널) host-key
  핀 부재(MITM 구멍), S3 목적지 필드 불완전(endpoint/region/bucket/prefix/object_lock/
  checksum_algo), `enc:` 필드에 key-id/wrap-gen 없음(rotate-key 결정성 불가), `[[jobs]]` 바인딩
  불완전(source→dest, required_immutability, retention/verify/schedule), `[policy]` 불완전
  (max_parallel_dumps/retry/spool_dir/min_free_disk/소스별 dump_tool_version 핀), `[notify]`
  부재, prune 신원이 무인 run과 같은 프로파일에 있으면 안 됨(append-only 무효화).
- **P2-2 (CLI 표면 갭)**: 최초 표면(init/add-db/add-dest/run/schedule/restore/verify/catalog)에
  `test-connection`(무인 실행 전 사전 점검 없이는 첫 실패가 새벽 2시), `rotate-key`, `doctor`,
  `prune`(격리 신원), `catalog rebuild`, `keys status`, `register-dpapi`가 빠져 있었음.

이 섹션은 위 두 갭을 모두 반영한 **최종 확정 스키마 + CLI 표면**을 정의한다. 실제 암호화
(S02), 파이프라인(S05), DB 소스(S03/S04), 목적지 업로드(S06/S07), 카탈로그(S08), 복원/검증
(S09), 스케줄 등록(S10), 공급망 하드닝(S11) 로직은 이 섹션의 범위가 아니며, 해당 커맨드
핸들러는 명확한 `TODO(SXX)` 스텁으로 남긴다. 컴파일은 항상 성공해야 하고, 미구현 경로는
런타임에 즉시 명확한 에러로 실패해야 한다(조용한 부분 동작 금지).

## Requirements

### 기능 요구사항
- cargo 워크스페이스: `dbbackup-core`(공용 타입/trait), `dbbackup-config`(스키마+로드/저장/
  마이그레이션/검증), `dbbackup-cli`(바이너리, clap 표면 + 커맨드 디스패치).
- CLI 명령 표면(정확히 아래 13개 최상위 + 서브커맨드):
  `init | doctor | test-connection | add-db | add-dest | run [--db][--dry-run] | verify |
  restore | rotate-key | register-dpapi | keys status | catalog [rebuild] | prune | schedule`.
- TOML 기반 Config: `[meta]`(schema_version), `[[database]]`, `[[ssh]]`(터널, host-key 핀),
  `[[destination]]`(s3/ssh/local, object_lock.mode/retention, checksum_algo), `[[jobs]]`
  (source→dest 바인딩 + required_immutability + retention/verify/schedule), `[policy]`
  (max_parallel_dumps, retry, spool_dir, min_free_disk, 소스별 dump_tool_version 핀),
  `[notify]`.
- 자격증명 필드는 항상 `enc:<wrap_gen>:<base64>` 형식의 전용 타입(`EncField`)이며 평문
  `String`으로 스키마에 존재해서는 안 된다.
- prune 전용 프로파일(`prune.toml`/`PruneProfile`)은 메인 `Config`와 완전히 분리된 타입이며,
  `run` 커맨드 경로는 이 타입을 참조/로드할 수 없어야 한다(구조적으로 불가능해야 함).
- `schema_version` 기반 마이그레이션 스캐폴드(레지스트리)가 v1부터 존재해야 함(마이그레이션이
  0개라도 인프라는 있어야 함).

### 비기능 요구사항
- Windows 우선 빌드(개발자는 다른 OS에서도 빌드 가능해야 하지만 대상 실행 환경은 Windows).
- `cargo clippy --workspace --all-targets -- -D warnings` 경고 0.
- 후속 섹션이 새 크레이트(`dbbackup-secrets`, `dbbackup-pipeline`, `dbbackup-catalog`,
  `dbbackup-source-*`, `dbbackup-dest-*`, `dbbackup-scheduler`)를 워크스페이스 멤버로 추가하기만
  하면 되도록 워크스페이스 구조가 열려 있어야 함.
- `Cargo.lock` 커밋(H6 공급망 요구사항 선반영).

## Dependencies (which sections/crates)

- **의존하는 섹션**: 없음 (레이어 0).
- **이 섹션에 의존하는 섹션**: S02, S03, S04, S05, S06, S07, S08, S09, S10, S11 전체 —
  이들은 모두 `dbbackup-core`의 타입(enum, `EncField`, `SecretEncryptor` trait)과
  `dbbackup-config`의 `Config`/스키마 타입을 가져다 쓴다.
- **이 섹션이 신규 생성하는 크레이트**: `dbbackup-core`, `dbbackup-config`, `dbbackup-cli`.

## Reference Libraries (crate + version, research.md 기준)

| 크레이트 | 버전 | 용도 | 비고 |
|---|---|---|---|
| `clap` | 4.6.2 (derive feature) | CLI 파싱 | research.md §7. derive API 사용 |
| `serde` | 1.x (derive feature) | 직렬화/역직렬화 | 표준 |
| `toml` | 0.9.x | Config 파일 파싱/직렬화 | research.md §7. serde 통합 |
| `anyhow` | 1.x | 커맨드 핸들러 레벨 에러(컨텍스트 체이닝) | 바이너리 크레이트에서만 사용 |
| `thiserror` | 1.x | 라이브러리 크레이트(`dbbackup-core`/`dbbackup-config`)의 구조적 에러 타입 | `anyhow`와 역할 분리 |
| `base64` | 0.22.x | `EncField`의 `enc:<wrap_gen>:<base64>` 인코딩 | 표준 |
| `rpassword` | 7.x | 대화형 비밀번호 입력(에코 없음) | `add-db` 등에서 사용, S02 연동 전까지 자리표시자 |
| `tracing` + `tracing-subscriber` | 0.1.x / 0.3.x | 구조적 로깅 뼈대 | 시크릿 스크럽 정책은 S11에서 확정, 이 섹션은 초기화만 |

> `age`, `argon2`, `chacha20poly1305`, `russh`, `opendal`, `zstd` 등 암호화/전송 크레이트는
> 이 섹션에서 사용하지 않는다(각각 S02, S06/S07, S05 소관). H6 공급망 정책상 이런
> 보안 critical 크레이트는 `=x.y.z` 정확히 핀하되, 그 작업은 해당 크레이트를 실제로
> 추가하는 섹션에서 수행한다.

## Implementation

### 1. 워크스페이스 레이아웃

```
D:\git\dbbackupsystem\
  Cargo.toml                      (workspace manifest)
  rust-toolchain.toml
  .gitignore
  Cargo.lock                      (커밋 대상, H6)
  crates\
    dbbackup-core\
      Cargo.toml
      src\lib.rs                  (공용 enum, EncField, SecretEncryptor trait)
    dbbackup-config\
      Cargo.toml
      src\
        lib.rs
        schema.rs                 (Config 및 하위 구조체)
        migrate.rs                 (schema_version 마이그레이션 레지스트리)
        validate.rs                 (교차참조/필수필드 검증)
        prune.rs                    (PruneProfile — Config와 완전 분리)
      tests\
        config_roundtrip.rs
        migration.rs
        validate.rs
    dbbackup-cli\
      Cargo.toml
      src\
        main.rs
        cli.rs                     (clap derive Cli/Command/*Args)
        commands\
          mod.rs
          init.rs
          doctor.rs
          test_connection.rs
          add_db.rs
          add_dest.rs
          run.rs
          verify.rs
          restore.rs
          rotate_key.rs
          register_dpapi.rs
          keys.rs
          catalog.rs
          prune.rs
          schedule.rs
      tests\
        cli_parsing.rs
  config\
    dbbackup.example.toml
    prune.example.toml
```

향후 섹션은 워크스페이스 루트 `Cargo.toml`의 `members` 배열에 자신의 크레이트를 추가하기만
하면 된다(예: S02가 `crates/dbbackup-secrets` 추가, S05가 `crates/dbbackup-pipeline` 추가).
이 섹션은 그 세 진입점만 만든다.

```toml
# Cargo.toml (workspace root)
[workspace]
resolver = "2"
members = [
  "crates/dbbackup-core",
  "crates/dbbackup-config",
  "crates/dbbackup-cli",
  # S02+ : "crates/dbbackup-secrets", "crates/dbbackup-pipeline",
  #        "crates/dbbackup-catalog", "crates/dbbackup-source-pg", ...
]

[workspace.package]
version = "0.1.0"
edition = "2021"
rust-version = "1.91"
license = "MIT"

[workspace.dependencies]
clap = { version = "4.6.2", features = ["derive"] }
serde = { version = "1", features = ["derive"] }
toml = "0.9"
anyhow = "1"
thiserror = "1"
base64 = "0.22"
rpassword = "7"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter"] }
```

```toml
# rust-toolchain.toml
[toolchain]
channel = "1.97.0"
components = ["clippy", "rustfmt"]
profile = "minimal"
```

```gitignore
# .gitignore — 리포 정책: 도구 코드만 공개, config/wrap/복구코드/catalog/keyfile/스풀은 로컬
/target
dbbackup.toml
prune.toml
*.dek
*.recovery-code
*.keyfile
catalog.sqlite*
spool/
.staging/
# 예외: config/*.example.toml 은 secret 없는 예시라 커밋 대상
```

### 2. `dbbackup-core` — 공용 타입

```toml
# crates/dbbackup-core/Cargo.toml
[package]
name = "dbbackup-core"
version.workspace = true
edition.workspace = true

[dependencies]
serde.workspace = true
thiserror.workspace = true
base64.workspace = true
```

```rust
// crates/dbbackup-core/src/lib.rs
use std::{fmt, str::FromStr};

use base64::{engine::general_purpose::STANDARD, Engine as _};
use serde::{de::Error as DeError, Deserialize, Deserializer, Serialize, Serializer};

// ---------------------------------------------------------------------
// EncField — config에 저장되는 암호화 필드의 와이어 포맷.
// 형식: enc:<wrap_gen>:<base64 ciphertext>
// wrap_gen은 이 필드가 어느 DEK wrap 세대로 암호화됐는지 표시한다(db-schema.md의
// backups.dek_wrap_gen과 동일 개념). rotate-key(S02)가 이 값을 보고 재래핑 대상을
// 결정적으로 찾아낸다(P2-1: "enc: 필드는 key-id/버전을 반드시 carry").
// ---------------------------------------------------------------------
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncField {
    pub wrap_gen: u32,
    pub ciphertext: Vec<u8>,
}

impl EncField {
    pub const PREFIX: &'static str = "enc:";

    pub fn new(wrap_gen: u32, ciphertext: Vec<u8>) -> Self {
        Self { wrap_gen, ciphertext }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum EncFieldError {
    #[error("enc field missing '{}' prefix", EncField::PREFIX)]
    MissingPrefix,
    #[error("enc field malformed: expected enc:<wrap_gen>:<base64>")]
    Malformed,
    #[error("enc field wrap_gen is not a valid u32: {0}")]
    InvalidWrapGen(#[source] std::num::ParseIntError),
    #[error("enc field ciphertext is not valid base64: {0}")]
    InvalidBase64(#[source] base64::DecodeError),
}

impl FromStr for EncField {
    type Err = EncFieldError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let rest = s.strip_prefix(Self::PREFIX).ok_or(EncFieldError::MissingPrefix)?;
        let (wrap_gen_str, b64) = rest.split_once(':').ok_or(EncFieldError::Malformed)?;
        let wrap_gen: u32 = wrap_gen_str.parse().map_err(EncFieldError::InvalidWrapGen)?;
        let ciphertext = STANDARD.decode(b64).map_err(EncFieldError::InvalidBase64)?;
        Ok(EncField { wrap_gen, ciphertext })
    }
}

impl fmt::Display for EncField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}{}:{}", Self::PREFIX, self.wrap_gen, STANDARD.encode(&self.ciphertext))
    }
}

impl Serialize for EncField {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for EncField {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse().map_err(DeError::custom)
    }
}

// ---------------------------------------------------------------------
// SecretEncryptor — S02(SecretManager)가 구현할 경계. dbbackup-cli의 커맨드
// 핸들러가 SecretManager 크레이트 자체에 의존하지 않고도 "평문 -> EncField"
// 변환을 요청할 수 있게 하는 트레이트. S02가 붙기 전까지는
// NotYetImplementedEncryptor가 자리표시자로 동작해 항상 명확한 에러를 낸다.
// ---------------------------------------------------------------------
pub trait SecretEncryptor {
    fn wrap(&self, plaintext: &str) -> Result<EncField, SecretEncryptError>;
}

#[derive(Debug, thiserror::Error)]
pub enum SecretEncryptError {
    #[error("secret encryption is not available: {0}")]
    Unavailable(&'static str),
}

pub struct NotYetImplementedEncryptor;

impl SecretEncryptor for NotYetImplementedEncryptor {
    fn wrap(&self, _plaintext: &str) -> Result<EncField, SecretEncryptError> {
        Err(SecretEncryptError::Unavailable(
            "SecretManager not implemented yet (see section S02)",
        ))
    }
}

// ---------------------------------------------------------------------
// 공용 도메인 enum — domain-dictionary.md 규범 용어의 TOML/JSON 직렬화 값과
// db-schema.md의 컬럼 값(예: artifacts.immutability)이 동일한 문자열을 쓴다.
// ---------------------------------------------------------------------
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Postgresql,
    Mysql,
    Mariadb,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DestKind {
    S3,
    Ssh,
    Local,
}

/// Destination.capabilities()가 자가보고하는 값과 동일 어휘(P1-2, S06/S07 소관).
/// 여기서는 job이 "요구하는" 레벨을 config에 적을 때 쓰는 값으로 정의한다.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImmutabilityCapability {
    None,
    Governance,
    Compliance,
    ServerAppendOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChecksumAlgo {
    Sha256,
    Crc32c,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectLockMode {
    None,
    Governance,
    Compliance,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NotifyChannel {
    None,
    Webhook,
    Email,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enc_field_roundtrip() {
        let f = EncField::new(3, b"hello".to_vec());
        let s = f.to_string();
        assert_eq!(s, "enc:3:aGVsbG8=");
        let back: EncField = s.parse().unwrap();
        assert_eq!(back, f);
    }

    #[test]
    fn enc_field_rejects_missing_prefix() {
        assert!(matches!("3:aGVsbG8=".parse::<EncField>(), Err(EncFieldError::MissingPrefix)));
    }

    #[test]
    fn not_yet_implemented_encryptor_fails_loudly() {
        let e = NotYetImplementedEncryptor;
        assert!(e.wrap("secret").is_err());
    }
}
```

### 3. `dbbackup-config` — Config 스키마

```toml
# crates/dbbackup-config/Cargo.toml
[package]
name = "dbbackup-config"
version.workspace = true
edition.workspace = true

[dependencies]
dbbackup-core = { path = "../dbbackup-core" }
serde.workspace = true
toml.workspace = true
thiserror.workspace = true
```

```rust
// crates/dbbackup-config/src/schema.rs
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use dbbackup_core::{ChecksumAlgo, DestKind, Engine, ImmutabilityCapability, NotifyChannel, ObjectLockMode};
use serde::{Deserialize, Serialize};

use crate::ConfigError;

pub const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub schema_version: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    pub meta: Meta,
    #[serde(default, rename = "database")]
    pub databases: Vec<Database>,
    #[serde(default, rename = "ssh")]
    pub tunnels: Vec<Ssh>,
    #[serde(default, rename = "destination")]
    pub destinations: Vec<Destination>,
    #[serde(default, rename = "jobs")]
    pub jobs: Vec<Job>,
    pub policy: Policy,
    #[serde(default)]
    pub notify: Option<Notify>,
}

impl Config {
    /// 새 config를 위한 최소 골격(`init`이 채워 나감).
    pub fn empty() -> Self {
        Config {
            meta: Meta { schema_version: CURRENT_SCHEMA_VERSION },
            databases: Vec::new(),
            tunnels: Vec::new(),
            destinations: Vec::new(),
            jobs: Vec::new(),
            policy: Policy::default(),
            notify: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        // Windows 메모장 등이 붙이는 UTF-8 BOM은 toml 파서가 처리하지 못한다 — 방어적 스트립.
        let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        crate::migrate::load_and_migrate(raw)
    }

    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let raw = toml::to_string_pretty(self).map_err(ConfigError::Serialize)?;
        std::fs::write(path, raw).map_err(|e| ConfigError::Io(path.to_path_buf(), e))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Database {
    pub alias: String,
    pub engine: Engine,
    pub host: String,
    pub port: u16,
    pub dbname: String,
    pub user: String,
    pub password: dbbackup_core::EncField,
    /// [[ssh]] name 참조 — 바스티온 경유 시.
    #[serde(default)]
    pub tunnel: Option<String>,
    /// PostgreSQL 전용: pg_dumpall --globals-only 동봉 여부(P2-1: "Globals toggle").
    /// MySQL/MariaDB에서는 무시되며 validate()가 경고 대상으로 표시한다.
    #[serde(default = "default_true")]
    pub dump_globals: bool,
    /// MySQL/MariaDB 전용: None이면 [policy]의 기본값을 따른다.
    #[serde(default)]
    pub single_transaction: Option<bool>,
}

/// 백업 호스트 -> DB 서버 바스티온 터널. host_key_fingerprint는 필수(옵션 아님) —
/// blind-accept는 MITM 구멍이라는 것이 team-review.md P2-1/H6의 결론.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Ssh {
    pub name: String,
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    pub user: String,
    pub private_key_path: PathBuf,
    /// 예: "SHA256:base64..." — russh host-key 검증에 사용(S07).
    pub host_key_fingerprint: String,
}

/// s3 | ssh | local 세 종류를 한 구조체에 담되, kind별 필드는 모두 Option으로 둔다.
/// (내부적으로 tag+flatten enum도 고려했으나 toml serde 직렬화 쪽 알려진 함정이 있어
/// 스키마 안정성을 우선해 평탄한 구조를 택함 — Test Scenarios/Risk 절 참고.)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Destination {
    pub name: String,
    pub kind: DestKind,

    // --- s3 ---
    pub endpoint: Option<String>,
    pub region: Option<String>,
    pub bucket: Option<String>,
    #[serde(default)]
    pub prefix: String,
    pub access_key_id: Option<dbbackup_core::EncField>,
    pub secret_access_key: Option<dbbackup_core::EncField>,
    pub checksum_algo: Option<ChecksumAlgo>,
    pub object_lock: Option<ObjectLockConfig>,

    // --- ssh (russh 직결, [[ssh]] 터널과 별개 host-key 핀) ---
    pub tunnel: Option<String>,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub user: Option<String>,
    pub private_key_path: Option<PathBuf>,
    pub host_key_fingerprint: Option<String>,

    // --- ssh 원격 경로 / local 경로 공용 ---
    pub path: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectLockConfig {
    pub mode: ObjectLockMode,
    pub retention_days: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    pub name: String,
    /// Database.alias 참조.
    pub source: String,
    /// Destination.name 참조(1개 이상, 옵션 C 팬아웃).
    pub destinations: Vec<String>,
    pub required_immutability: ImmutabilityCapability,
    #[serde(default)]
    pub retention: Retention,
    #[serde(default)]
    pub verify: VerifyPolicy,
    /// cron 유사 표현식. 실제 등록/파싱은 S10(schtasks) 소관, 여기서는 원문만 보관.
    pub schedule: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

/// GFS 등급별 보존(daily/weekly/monthly 세대 수).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Retention {
    #[serde(default)]
    pub daily: u32,
    #[serde(default)]
    pub weekly: u32,
    #[serde(default)]
    pub monthly: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyPolicy {
    /// 1=파일무결성 2=포맷유효성 3=실제복원 (domain-dictionary.md Verify Level).
    #[serde(default = "default_verify_level")]
    pub level: u8,
    /// "every_run" | "weekly" | cron 표현식.
    #[serde(default = "default_verify_schedule")]
    pub schedule: String,
}

impl Default for VerifyPolicy {
    fn default() -> Self {
        VerifyPolicy { level: default_verify_level(), schedule: default_verify_schedule() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Policy {
    #[serde(default = "default_max_parallel_dumps")]
    pub max_parallel_dumps: u32,
    #[serde(default)]
    pub retry: RetryPolicy,
    pub spool_dir: PathBuf,
    #[serde(default = "default_min_free_disk_mb")]
    pub min_free_disk_mb: u64,
    /// db alias -> 핀 고정된 덤프 도구 버전(P2-1: "dump_tool_version pin per source").
    #[serde(default)]
    pub dump_tool_version: HashMap<String, String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            max_parallel_dumps: default_max_parallel_dumps(),
            retry: RetryPolicy::default(),
            spool_dir: PathBuf::from("spool"),
            min_free_disk_mb: default_min_free_disk_mb(),
            dump_tool_version: HashMap::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetryPolicy {
    #[serde(default = "default_retry_attempts")]
    pub attempts: u32,
    #[serde(default = "default_backoff_secs")]
    pub backoff_secs: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy { attempts: default_retry_attempts(), backoff_secs: default_backoff_secs() }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Notify {
    pub channel: NotifyChannel,
    #[serde(default)]
    pub webhook_url: Option<dbbackup_core::EncField>,
    #[serde(default)]
    pub email_to: Option<String>,
}

fn default_true() -> bool { true }
fn default_ssh_port() -> u16 { 22 }
fn default_verify_level() -> u8 { 2 }
fn default_verify_schedule() -> String { "every_run".to_string() }
fn default_max_parallel_dumps() -> u32 { 1 }
fn default_min_free_disk_mb() -> u64 { 2048 }
fn default_retry_attempts() -> u32 { 3 }
fn default_backoff_secs() -> u64 { 30 }
```

```rust
// crates/dbbackup-config/src/migrate.rs
use crate::{schema::{Config, CURRENT_SCHEMA_VERSION}, ConfigError};

/// v_n -> v_(n+1) 변환. 항상 raw toml::Value 레벨에서 동작해 Config 구조체가
/// 최신 스키마만 알면 되게 한다(deny_unknown_fields와 공존 가능).
pub type MigrationFn = fn(toml::Value) -> Result<toml::Value, ConfigError>;

/// (target_version, migration) 오름차순. v1은 최초 스키마라 마이그레이션이 없다 —
/// 이 배열이 비어 있어도 인프라(레지스트리 자체)는 항상 존재해야 한다는 것이 P2-1의 요지.
pub static MIGRATIONS: &[(u32, MigrationFn)] = &[
    // (2, migrate_v1_to_v2),  // 예시: v2가 생기면 여기 추가.
];

pub fn load_and_migrate(raw: &str) -> Result<Config, ConfigError> {
    let mut value: toml::Value = toml::from_str(raw).map_err(ConfigError::Parse)?;
    let mut version = read_schema_version(&value)?;

    if version > CURRENT_SCHEMA_VERSION {
        return Err(ConfigError::UnsupportedSchemaVersion {
            found: version,
            max_supported: CURRENT_SCHEMA_VERSION,
        });
    }

    for (target_version, migration) in MIGRATIONS {
        if version < *target_version {
            value = migration(value)?;
            version = *target_version;
        }
    }

    value.try_into::<Config>().map_err(ConfigError::Deserialize)
}

fn read_schema_version(value: &toml::Value) -> Result<u32, ConfigError> {
    value
        .get("meta")
        .and_then(|m| m.get("schema_version"))
        .and_then(|v| v.as_integer())
        .map(|v| v as u32)
        .ok_or(ConfigError::MissingSchemaVersion)
}
```

```rust
// crates/dbbackup-config/src/validate.rs
use std::collections::HashSet;

use dbbackup_core::DestKind;

use crate::schema::Config;

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ValidationError {
    #[error("duplicate database alias: {0}")]
    DuplicateDatabaseAlias(String),
    #[error("duplicate destination name: {0}")]
    DuplicateDestinationName(String),
    #[error("duplicate ssh tunnel name: {0}")]
    DuplicateTunnelName(String),
    #[error("duplicate job name: {0}")]
    DuplicateJobName(String),
    #[error("job '{job}' references unknown source database '{source}'")]
    UnknownSourceRef { job: String, source: String },
    #[error("job '{job}' references unknown destination '{dest}'")]
    UnknownDestinationRef { job: String, dest: String },
    #[error("database '{db}' references unknown ssh tunnel '{tunnel}'")]
    UnknownTunnelRef { db: String, tunnel: String },
    #[error("destination '{dest}' (ssh) references unknown ssh tunnel '{tunnel}'")]
    UnknownDestinationTunnelRef { dest: String, tunnel: String },
    #[error("destination '{0}' (s3) is missing required field '{1}'")]
    MissingS3Field(String, &'static str),
    #[error("destination '{0}' has object_lock set but no checksum_algo (S3 requires a checksum when Object Lock is enabled)")]
    S3ChecksumRequiredForObjectLock(String),
    #[error("destination '{0}' (ssh) is missing required field '{1}'")]
    MissingSshField(String, &'static str),
    #[error("destination '{0}' (local) is missing required field 'path'")]
    MissingLocalPath(String),
    #[error("job '{0}' verify.level must be 1, 2 or 3")]
    InvalidVerifyLevel(String),
}

/// 실패 즉시 반환하지 않고 발견한 모든 문제를 모아 반환한다 — `add-db`/`run` 양쪽에서
/// 재사용하며, 사용자가 한 번에 여러 오류를 고칠 수 있게 한다.
pub fn validate(config: &Config) -> Result<(), Vec<ValidationError>> {
    let mut errors = Vec::new();

    let mut db_aliases = HashSet::new();
    for db in &config.databases {
        if !db_aliases.insert(db.alias.clone()) {
            errors.push(ValidationError::DuplicateDatabaseAlias(db.alias.clone()));
        }
    }

    let mut tunnel_names = HashSet::new();
    for t in &config.tunnels {
        if !tunnel_names.insert(t.name.clone()) {
            errors.push(ValidationError::DuplicateTunnelName(t.name.clone()));
        }
    }

    for db in &config.databases {
        if let Some(tunnel) = &db.tunnel {
            if !tunnel_names.contains(tunnel) {
                errors.push(ValidationError::UnknownTunnelRef {
                    db: db.alias.clone(),
                    tunnel: tunnel.clone(),
                });
            }
        }
    }

    let mut dest_names = HashSet::new();
    for dest in &config.destinations {
        if !dest_names.insert(dest.name.clone()) {
            errors.push(ValidationError::DuplicateDestinationName(dest.name.clone()));
        }

        match dest.kind {
            DestKind::S3 => {
                if dest.endpoint.is_none() {
                    errors.push(ValidationError::MissingS3Field(dest.name.clone(), "endpoint"));
                }
                if dest.bucket.is_none() {
                    errors.push(ValidationError::MissingS3Field(dest.name.clone(), "bucket"));
                }
                if dest.access_key_id.is_none() {
                    errors.push(ValidationError::MissingS3Field(dest.name.clone(), "access_key_id"));
                }
                if dest.secret_access_key.is_none() {
                    errors.push(ValidationError::MissingS3Field(dest.name.clone(), "secret_access_key"));
                }
                if dest.object_lock.is_some() && dest.checksum_algo.is_none() {
                    errors.push(ValidationError::S3ChecksumRequiredForObjectLock(dest.name.clone()));
                }
            }
            DestKind::Ssh => {
                if let Some(tunnel) = &dest.tunnel {
                    if !tunnel_names.contains(tunnel) {
                        errors.push(ValidationError::UnknownDestinationTunnelRef {
                            dest: dest.name.clone(),
                            tunnel: tunnel.clone(),
                        });
                    }
                }
                if dest.path.is_none() {
                    errors.push(ValidationError::MissingSshField(dest.name.clone(), "path"));
                }
                if dest.user.is_none() {
                    errors.push(ValidationError::MissingSshField(dest.name.clone(), "user"));
                }
                if dest.host_key_fingerprint.is_none() {
                    errors.push(ValidationError::MissingSshField(dest.name.clone(), "host_key_fingerprint"));
                }
            }
            DestKind::Local => {
                if dest.path.is_none() {
                    errors.push(ValidationError::MissingLocalPath(dest.name.clone()));
                }
            }
        }
    }

    let mut job_names = HashSet::new();
    for job in &config.jobs {
        if !job_names.insert(job.name.clone()) {
            errors.push(ValidationError::DuplicateJobName(job.name.clone()));
        }
        if !db_aliases.contains(&job.source) {
            errors.push(ValidationError::UnknownSourceRef {
                job: job.name.clone(),
                source: job.source.clone(),
            });
        }
        for dest in &job.destinations {
            if !dest_names.contains(dest) {
                errors.push(ValidationError::UnknownDestinationRef {
                    job: job.name.clone(),
                    dest: dest.clone(),
                });
            }
        }
        if !(1..=3).contains(&job.verify.level) {
            errors.push(ValidationError::InvalidVerifyLevel(job.name.clone()));
        }
    }

    if errors.is_empty() { Ok(()) } else { Err(errors) }
}
```

```rust
// crates/dbbackup-config/src/prune.rs
//
// prune 전용 프로파일 — 삭제 권한을 가진 격리 신원을 담는다. plan.md/team-review.md의
// H1/H2("prune는 백업 호스트에서 실행 금지, 별도 프로파일, 무인 run이 로드 안 함")를
// 구조적으로 강제하기 위해 schema.rs의 Config와 완전히 별도 타입으로 둔다.
// dbbackup-cli의 `run` 커맨드 모듈은 이 모듈을 import하지 않는다(코드 리뷰 체크 항목).
use std::path::Path;

use dbbackup_core::EncField;
use serde::{Deserialize, Serialize};

use crate::{schema::Meta, ConfigError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneProfile {
    pub meta: Meta,
    /// 이 신원이 prune을 수행할 대상 Destination.name (main config 참조는 이름 문자열로만,
    /// Config 타입을 직접 참조하지 않는다).
    pub destination: String,
    pub identity: PruneIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneIdentity {
    pub access_key_id: EncField,
    pub secret_access_key: EncField,
}

impl PruneProfile {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw = std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        toml::from_str(raw).map_err(ConfigError::Parse)
    }
}
```

```rust
// crates/dbbackup-config/src/lib.rs
pub mod migrate;
pub mod prune;
pub mod schema;
pub mod validate;

use std::path::PathBuf;

pub use schema::Config;
pub use validate::{validate, ValidationError};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("cannot read config file {0}: {1}")]
    Io(PathBuf, #[source] std::io::Error),
    #[error("cannot parse TOML: {0}")]
    Parse(#[source] toml::de::Error),
    #[error("cannot deserialize config: {0}")]
    Deserialize(#[source] toml::de::Error),
    #[error("cannot serialize config: {0}")]
    Serialize(#[source] toml::ser::Error),
    #[error("config is missing [meta] schema_version")]
    MissingSchemaVersion,
    #[error("config schema_version {found} is newer than this binary supports (max {max_supported}); upgrade dbbackup")]
    UnsupportedSchemaVersion { found: u32, max_supported: u32 },
}
```

### 4. TOML 예시 (`config/dbbackup.example.toml`)

```toml
[meta]
schema_version = 1

[[database]]
alias = "app"
engine = "postgresql"
host = "10.0.0.5"
port = 5432
dbname = "app"
user = "backup_reader"
password = "enc:1:BASE64_PLACEHOLDER=="
dump_globals = true

[[database]]
alias = "shop"
engine = "mysql"
host = "10.0.0.9"
port = 3306
dbname = "shop"
user = "backup_reader"
password = "enc:1:BASE64_PLACEHOLDER=="
tunnel = "bastion1"
single_transaction = true

[[ssh]]
name = "bastion1"
host = "bastion.example.com"
port = 22
user = "tunnel"
private_key_path = "C:/keys/bastion_id_ed25519"
host_key_fingerprint = "SHA256:AbCdEfGhIjKlMnOpQrStUvWxYz0123456789ABCDEFG"

[[destination]]
name = "s3-primary"
kind = "s3"
endpoint = "https://s3.ap-northeast-2.amazonaws.com"
region = "ap-northeast-2"
bucket = "mycompany-backups"
prefix = "prod/"
access_key_id = "enc:1:BASE64_PLACEHOLDER=="
secret_access_key = "enc:1:BASE64_PLACEHOLDER=="
checksum_algo = "sha256"

[destination.object_lock]
mode = "governance"
retention_days = 35

[[destination]]
name = "ssh-secondary"
kind = "ssh"
tunnel = "bastion1"
path = "/mnt/backup-repo"
user = "backupuser"
private_key_path = "C:/keys/dest_id_ed25519"
host_key_fingerprint = "SHA256:ZzYyXxWwVvUuTtSsRrQqPpOoNnMmLlKkJjIiHhGgFf"

[[destination]]
name = "local-spool"
kind = "local"
path = "D:/backup-spool"

[[jobs]]
name = "prod-pg-nightly"
source = "app"
destinations = ["s3-primary", "ssh-secondary", "local-spool"]
required_immutability = "governance"
schedule = "0 3 * * *"

[jobs.retention]
daily = 7
weekly = 4
monthly = 12

[jobs.verify]
level = 2
schedule = "every_run"

[[jobs]]
name = "shop-mysql-nightly"
source = "shop"
destinations = ["s3-primary", "local-spool"]
required_immutability = "governance"
schedule = "30 3 * * *"

[jobs.retention]
daily = 7
weekly = 4
monthly = 12

[policy]
max_parallel_dumps = 2
spool_dir = "D:/backup-spool/.staging"
min_free_disk_mb = 5120

[policy.retry]
attempts = 3
backoff_secs = 30

[policy.dump_tool_version]
app = "16.3"
shop = "8.0.37"

[notify]
channel = "webhook"
webhook_url = "enc:1:BASE64_PLACEHOLDER=="
```

```toml
# config/prune.example.toml — run이 절대 로드하지 않는 격리 프로파일
[meta]
schema_version = 1

destination = "s3-primary"

[identity]
access_key_id = "enc:1:BASE64_PLACEHOLDER=="
secret_access_key = "enc:1:BASE64_PLACEHOLDER=="
```

### 5. `dbbackup-cli` — clap 표면

```toml
# crates/dbbackup-cli/Cargo.toml
[package]
name = "dbbackup-cli"
version.workspace = true
edition.workspace = true

[[bin]]
name = "dbbackup"
path = "src/main.rs"

[dependencies]
dbbackup-core = { path = "../dbbackup-core" }
dbbackup-config = { path = "../dbbackup-config" }
clap.workspace = true
anyhow.workspace = true
rpassword.workspace = true
tracing.workspace = true
tracing-subscriber.workspace = true
```

```rust
// crates/dbbackup-cli/src/cli.rs
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Parser, Debug)]
#[command(name = "dbbackup", version, about = "Ransomware-resilient DB backup orchestrator (Windows)")]
pub struct Cli {
    /// Config 파일 경로.
    #[arg(long, global = true, default_value = "dbbackup.toml")]
    pub config: PathBuf,

    /// 로그 상세도 (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    pub verbose: u8,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// 최초 설정: 마스터 비밀번호, DEK, 복구코드, DPAPI 등록.
    Init(InitArgs),
    /// 환경 자가진단: 덤프 도구 바이너리/버전, 디스크 여유공간, DPAPI 가용성.
    Doctor,
    /// DB 접속/터널/목적지 쓰기가능 여부를 무인 스케줄링 전에 점검.
    TestConnection(TestConnectionArgs),
    /// [[database]] 항목 추가.
    AddDb(AddDbArgs),
    /// [[destination]] 항목 추가.
    AddDest(AddDestArgs),
    /// 백업 잡 실행.
    Run(RunArgs),
    /// 검증(L1/L2/L3) 수행.
    Verify(VerifyArgs),
    /// 백업 복원.
    Restore(RestoreArgs),
    /// 비밀번호/복구코드/DPAPI 재등록 및 DEK 재래핑.
    RotateKey(RotateKeyArgs),
    /// 새 PC/재설치 시 DPAPI Machine-scope 래핑 재등록.
    RegisterDpapi,
    /// 키 관련 조회.
    Keys {
        #[command(subcommand)]
        action: KeysAction,
    },
    /// 로컬 카탈로그 조작.
    Catalog {
        #[command(subcommand)]
        action: CatalogAction,
    },
    /// 격리 신원으로 보존정책 적용. run이 절대 호출하지 않음.
    Prune(PruneArgs),
    /// Windows Task Scheduler 등록/갱신.
    Schedule(ScheduleArgs),
}

#[derive(Subcommand, Debug)]
pub enum KeysAction {
    /// 어떤 wrap 경로(password/recovery/dpapi)가 활성인지 표시.
    /// 복구코드 자체는 init 시 1회만 표시하며 여기서 재출력하지 않는다.
    Status,
}

#[derive(Subcommand, Debug)]
pub enum CatalogAction {
    /// 서명된 매니페스트로부터 로컬 SQLite 카탈로그 재구성.
    Rebuild(CatalogRebuildArgs),
}

#[derive(Args, Debug)]
pub struct InitArgs {
    #[arg(long)]
    pub yes: bool,
}

#[derive(Args, Debug)]
pub struct TestConnectionArgs {
    #[arg(long)]
    pub db: Option<String>,
}

#[derive(Args, Debug)]
pub struct AddDbArgs {
    #[arg(long)]
    pub alias: String,
    #[arg(long, value_enum)]
    pub engine: EngineArg,
    #[arg(long)]
    pub host: String,
    #[arg(long)]
    pub port: u16,
    #[arg(long)]
    pub dbname: String,
    #[arg(long)]
    pub user: String,
    #[arg(long)]
    pub tunnel: Option<String>,
    #[arg(long, default_value_t = true)]
    pub dump_globals: bool,
}

#[derive(Args, Debug)]
pub struct AddDestArgs {
    #[arg(long)]
    pub name: String,
    #[arg(long, value_enum)]
    pub kind: DestKindArg,
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// 특정 [[database]] alias의 잡만 실행. 생략 시 활성화된 모든 잡.
    #[arg(long)]
    pub db: Option<String>,
    /// 계획만 세우고 덤프/업로드는 실행하지 않음.
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct VerifyArgs {
    #[arg(long)]
    pub backup: Option<String>,
    #[arg(long, value_parser = clap::value_parser!(u8).range(1..=3))]
    pub level: Option<u8>,
}

#[derive(Args, Debug)]
pub struct RestoreArgs {
    #[arg(long)]
    pub backup: String,
    #[arg(long = "from-destination")]
    pub from_destination: String,
    #[arg(long = "to")]
    pub to: Option<String>,
    /// 임포트 없이 평문 덤프만 산출.
    #[arg(long = "decrypt-only")]
    pub decrypt_only: bool,
    #[arg(long = "with-recovery-code")]
    pub with_recovery_code: bool,
}

#[derive(Args, Debug)]
pub struct RotateKeyArgs {
    #[arg(long)]
    pub new_recovery_code: bool,
    #[arg(long)]
    pub re_register_dpapi: bool,
}

#[derive(Args, Debug)]
pub struct CatalogRebuildArgs {
    #[arg(long = "from")]
    pub from_destination: String,
}

#[derive(Args, Debug)]
pub struct PruneArgs {
    /// 격리 프로파일 경로. 기본 config(dbbackup.toml)는 절대 여기 오지 않는다.
    #[arg(long, default_value = "prune.toml")]
    pub prune_config: PathBuf,
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args, Debug)]
pub struct ScheduleArgs {
    #[arg(long)]
    pub job: String,
    #[arg(long)]
    pub remove: bool,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EngineArg {
    Postgresql,
    Mysql,
    Mariadb,
}

impl From<EngineArg> for dbbackup_core::Engine {
    fn from(v: EngineArg) -> Self {
        match v {
            EngineArg::Postgresql => dbbackup_core::Engine::Postgresql,
            EngineArg::Mysql => dbbackup_core::Engine::Mysql,
            EngineArg::Mariadb => dbbackup_core::Engine::Mariadb,
        }
    }
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum DestKindArg {
    S3,
    Ssh,
    Local,
}
```

```rust
// crates/dbbackup-cli/src/main.rs
mod cli;
mod commands;

use clap::Parser;
use cli::{Cli, Command};

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    match cli.command {
        Command::Init(args) => commands::init::run(&cli.config, args),
        Command::Doctor => commands::doctor::run(&cli.config),
        Command::TestConnection(args) => commands::test_connection::run(&cli.config, args),
        Command::AddDb(args) => commands::add_db::run(&cli.config, args),
        Command::AddDest(args) => commands::add_dest::run(&cli.config, args),
        Command::Run(args) => commands::run::run(&cli.config, args),
        Command::Verify(args) => commands::verify::run(&cli.config, args),
        Command::Restore(args) => commands::restore::run(&cli.config, args),
        Command::RotateKey(args) => commands::rotate_key::run(&cli.config, args),
        Command::RegisterDpapi => commands::register_dpapi::run(&cli.config),
        Command::Keys { action } => commands::keys::run(&cli.config, action),
        Command::Catalog { action } => commands::catalog::run(&cli.config, action),
        // 주의: PruneArgs.prune_config만 넘어간다 — cli.config(dbbackup.toml)는
        // prune 경로에 절대 도달하지 않는다(H1/H2 구조적 강제).
        Command::Prune(args) => commands::prune::run(args),
        Command::Schedule(args) => commands::schedule::run(&cli.config, args),
    }
}

fn init_tracing(verbose: u8) {
    let level = match verbose {
        0 => tracing::Level::INFO,
        1 => tracing::Level::DEBUG,
        _ => tracing::Level::TRACE,
    };
    tracing_subscriber::fmt().with_max_level(level).init();
}
```

```rust
// crates/dbbackup-cli/src/commands/mod.rs
pub mod add_db;
pub mod add_dest;
pub mod catalog;
pub mod doctor;
pub mod init;
pub mod keys;
pub mod prune;
pub mod register_dpapi;
pub mod restore;
pub mod rotate_key;
pub mod run;
pub mod schedule;
pub mod test_connection;
pub mod verify;
```

대표 핸들러 3개(`add_db`, `run`, `prune`)는 실제 동작을 보이고, 나머지(`doctor`,
`test_connection`, `add_dest`, `verify`, `restore`, `rotate_key`, `register_dpapi`, `keys`,
`catalog`, `schedule`)는 동일 패턴의 스텁이다: config 로드 → (필요시) validate → 실제 로직은
`anyhow::bail!("... 은 아직 구현되지 않음 (섹션 SXX 참고)")`.

```rust
// crates/dbbackup-cli/src/commands/add_db.rs
use std::path::Path;

use dbbackup_config::{schema::Database, Config};
use dbbackup_core::{NotYetImplementedEncryptor, SecretEncryptor};

use crate::cli::AddDbArgs;

pub fn run(config_path: &Path, args: AddDbArgs) -> anyhow::Result<()> {
    let mut config = Config::load(config_path)?;

    let prompt = format!("Password for '{}': ", args.alias);
    let password = rpassword::prompt_password(prompt)?;

    // S02(SecretManager)가 붙기 전까지는 항상 명확히 실패한다 — 평문을 config에
    // 쓰는 조용한 폴백은 절대 하지 않는다.
    let encryptor = NotYetImplementedEncryptor;
    let password_enc = encryptor
        .wrap(&password)
        .map_err(|e| anyhow::anyhow!("cannot save credential yet: {e}"))?;

    config.databases.push(Database {
        alias: args.alias.clone(),
        engine: args.engine.into(),
        host: args.host,
        port: args.port,
        dbname: args.dbname,
        user: args.user,
        password: password_enc,
        tunnel: args.tunnel,
        dump_globals: args.dump_globals,
        single_transaction: None,
    });

    dbbackup_config::validate(&config)
        .map_err(|errs| anyhow::anyhow!("config invalid after add-db: {errs:?}"))?;
    config.save(config_path)?;

    println!("added database '{}' to {}", args.alias, config_path.display());
    Ok(())
}
```

```rust
// crates/dbbackup-cli/src/commands/run.rs
use std::path::Path;

use dbbackup_config::Config;

use crate::cli::RunArgs;

pub fn run(config_path: &Path, args: RunArgs) -> anyhow::Result<()> {
    let config = Config::load(config_path)?;
    dbbackup_config::validate(&config).map_err(|errs| anyhow::anyhow!("{errs:?}"))?;

    let jobs: Vec<_> = config
        .jobs
        .iter()
        .filter(|j| j.enabled)
        .filter(|j| args.db.as_deref().map_or(true, |db| j.source == db))
        .collect();

    if jobs.is_empty() {
        anyhow::bail!("no matching enabled jobs for --db={:?}", args.db);
    }

    for job in jobs {
        if args.dry_run {
            println!(
                "[dry-run] would run job '{}': {} -> {:?}",
                job.name, job.source, job.destinations
            );
            continue;
        }
        anyhow::bail!("job execution not implemented yet (see section S05 Pipeline)");
    }
    Ok(())
}
```

```rust
// crates/dbbackup-cli/src/commands/prune.rs
use dbbackup_config::prune::PruneProfile;

use crate::cli::PruneArgs;

/// 의도적으로 메인 config 경로(&Path)를 받지 않는다 — prune은 오직
/// PruneProfile(격리 신원)만 로드할 수 있다.
pub fn run(args: PruneArgs) -> anyhow::Result<()> {
    let profile = PruneProfile::load(&args.prune_config)?;

    if args.dry_run {
        println!(
            "[dry-run] would prune destination '{}' using profile {}",
            profile.destination,
            args.prune_config.display()
        );
        return Ok(());
    }

    anyhow::bail!("prune execution not implemented yet (see section S06/S07 Destination)");
}
```

나머지 스텁 예시(`doctor.rs`):

```rust
// crates/dbbackup-cli/src/commands/doctor.rs
use std::path::Path;

pub fn run(config_path: &Path) -> anyhow::Result<()> {
    let _ = dbbackup_config::Config::load(config_path)?;
    anyhow::bail!("doctor self-check not implemented yet (see section S03/S04/S11)");
}
```

## Test Scenarios (normal/error/edge)

### 정상
1. `dbbackup.example.toml`을 `Config::load`로 로드하면 오류 없이 역직렬화되고, `validate()`가
   `Ok(())`를 반환한다(스키마-예시 드리프트 방지용 fixture 테스트로 CI에 고정).
2. `Config` → `toml::to_string_pretty` → 재파싱 왕복이 구조적으로 동일하다(`PartialEq` 파생 후
   비교, 혹은 필드별 비교).
3. `Cli::try_parse_from(["dbbackup","catalog","rebuild","--from","s3-primary"])`가
   `Command::Catalog{action: CatalogAction::Rebuild(CatalogRebuildArgs{from_destination})}`로
   정확히 파싱된다. 13개 최상위 커맨드 전부에 대해 동일한 방식의 파싱 테스트를 둔다.
4. `EncField::from_str("enc:3:aGVsbG8=")`가 `wrap_gen=3, ciphertext=b"hello"`로 파싱되고
   `Display`가 원문과 동일 문자열을 재생성한다.
5. `schema_version = 1`인 config는 `MIGRATIONS`가 빈 배열이라도 `load_and_migrate`가 그대로
   통과한다(마이그레이션 0개 경로 자체가 깨지지 않는지 확인).
6. `dbbackup run --dry-run`이 example config에 대해 각 잡의 계획(잡 이름, source, destinations)을
   출력하고 종료코드 0으로 끝난다.

### 에러
1. `[meta]` 테이블 자체가 없는 TOML → `ConfigError::MissingSchemaVersion`.
2. `engine = "oracle"`(정의되지 않은 enum 값) → serde 역직렬화 에러(허용 값 목록을 포함한
   메시지).
3. job이 존재하지 않는 destination 이름을 참조 → `ValidationError::UnknownDestinationRef`.
4. `kind = "s3"` 목적지에 `object_lock`은 있는데 `checksum_algo`가 없음 →
   `ValidationError::S3ChecksumRequiredForObjectLock`.
5. `schema_version = 99`(현재 바이너리가 지원하는 것보다 미래 버전) →
   `ConfigError::UnsupportedSchemaVersion`으로 즉시 거부(추측 파싱 금지).
6. 오늘(S02 이전) `add-db` 실행 → `NotYetImplementedEncryptor`가 에러를 반환하고, config 파일은
   **부분 기록 없이** 원본 그대로 유지된다(save는 wrap 실패 이후 호출되지 않음을 테스트로 확인).
7. 알 수 없는 최상위 키가 섞인 config(`deny_unknown_fields`) → 로드 실패, 오타/오배치 필드를
   조용히 무시하지 않는다.

### 엣지
1. 같은 alias의 `[[database]]`가 두 번 등장 → `validate()`가
   `ValidationError::DuplicateDatabaseAlias`로 잡아낸다(마지막 항목이 조용히 이기지 않음).
2. 메모장 등으로 저장돼 UTF-8 BOM이 붙은 config 파일 → `Config::load`가 BOM을 스트립하고
   정상 파싱한다.
3. `run --db unknown-alias` → "활성화된 잡 없음" 에러(조용히 전체 실행되지 않음).
4. `[[jobs]]`가 비어 있는 방금 `init`된 config에서 `run`(–db 없이) → 명확한 에러로 종료.
5. `dbbackup.toml`만 존재하고 `prune.toml`이 없는 상태에서 `dbbackup prune` 기본 실행 →
   파일 없음 에러. 이는 곧 `run` 경로가 애초에 `PruneProfile`을 참조할 방법이 없다는 것의
   런타임 증거이기도 하다(컴파일 타임 증거는 `commands::run::run`의 시그니처 검토).
6. 공백이 포함된 Windows 경로(`--config "D:\My Backups\dbbackup.toml"`)가 `PathBuf`로
   정상 파싱된다.

## Implementation Strategy (steps)

1. 워크스페이스 루트 `Cargo.toml` + `rust-toolchain.toml` + `.gitignore` 생성.
2. `dbbackup-core`: enum들, `EncField`(+ `FromStr`/`Display`/`Serialize`/`Deserialize`),
   `SecretEncryptor` trait + `NotYetImplementedEncryptor`. 단위 테스트 포함.
3. `dbbackup-config`: `schema.rs`(전체 구조체) → `ConfigError` → `migrate.rs`(레지스트리,
   `CURRENT_SCHEMA_VERSION = 1`, 빈 `MIGRATIONS`) → `validate.rs` → `prune.rs`(완전 분리 모듈).
4. `config/dbbackup.example.toml`, `config/prune.example.toml` 작성 — 반드시 `schema.rs`
   구조체와 1:1로 역직렬화되는지 fixture 테스트로 고정.
5. `dbbackup-cli`: `cli.rs`(clap derive 전체 표면) → `main.rs`(디스패치, prune만 별도 인자) →
   `commands/*.rs`(add_db/run/prune은 실동작, 나머지는 명시적 스텁).
6. `cargo fmt --all`, `cargo clippy --workspace --all-targets -- -D warnings`,
   `cargo test --workspace` 로컬 통과 확인. CI 파이프라인/`cargo-deny` 연동 자체는 S11 소관이나,
   이 게이트는 S01 완료 조건으로 지금부터 적용한다.
7. `Cargo.lock` 커밋.

## Quality Gate

- `cargo build --workspace` 경고 없이 성공.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings` 통과.
- `cargo fmt --all -- --check` 통과.
- `cargo test --workspace` 전부 통과 (EncField 파싱, config 왕복, 마이그레이션 무동작 경로,
  validate 케이스, CLI 파싱, example TOML fixture).
- `dbbackup.example.toml` / `prune.example.toml`이 각각 `Config`/`PruneProfile`로 오류 없이
  역직렬화됨(테스트로 강제 — 문서 드리프트가 빌드를 깨도록).
- 자격증명 성격 필드(`password`, `access_key_id`, `secret_access_key`, `webhook_url`)는
  모두 `EncField` 타입이며 `String`이 아님(코드 리뷰 체크리스트 + grep으로 재확인 가능).
- `dbbackup-cli::commands::prune` 모듈은 `dbbackup_config::schema::Config`를 import하지 않음.
- `Cargo.lock`이 git에 추가되어 있음(H6).

## Risk & Rollback

- **리스크: 스키마 조기 경직화.** S02(secrets)/S05(pipeline)/S06-07(destination)이 아직 구현되지
  않은 상태에서 스키마를 확정하면 실제 요구가 드러난 뒤 깨지는 변경이 필요할 수 있다.
  **완화**: 커밋 1부터 `schema_version` + `migrate.rs` 레지스트리 존재(P2-1), `Destination`을
  태그드 enum 대신 평탄한 `Option` 필드로 설계해 필드 추가가 항상 additive이도록 함.
- **리스크: CLI 인자 형태가 나중에 바뀜.** 각 `*Args` 구조체는 spec.md/team-review.md에 명시된
  필드만 담았다. **완화**: `#[arg(long)]` 필드 추가는 기존 호출/스크립트에 대해 non-breaking.
- **리스크: `EncField` 와이어 포맷이 S02의 실제 wrap 스킴과 안 맞을 수 있음.** **완화**: 포맷은
  의도적으로 최소화(`wrap_gen` + opaque base64) — ciphertext 내부 구조는 전적으로 S02 소관.
  포맷 자체를 바꿔야 하면 `enc:` → `enc2:` 접두사 변경 정도로, `dbbackup-core::EncField` 한
  파일의 국소 수정.
- **롤백**: 이 시점에는 이 섹션 위에 실제로 구현된 후속 섹션 코드가 없으므로(레이어 0),
  자유롭게 되돌리거나 구조를 바꿔도 파급효과가 없다. `git revert`로 안전하게 되돌릴 수 있다.
  S02 이후가 실제로 이 크레이트들에 의존하기 시작하면, 이후의 스키마 변경은 반드시
  `schema_version` 마이그레이션 경로를 통해야 한다(직접 필드 수정 금지).

## Acceptance Criteria

- [ ] 워크스페이스가 `dbbackup-core`/`dbbackup-config`/`dbbackup-cli` 3개 크레이트로 빌드되고,
      바이너리 이름은 `dbbackup`이다.
- [ ] `dbbackup --help` 및 서브커맨드 `--help`에 13개 최상위 커맨드
      (`init, doctor, test-connection, add-db, add-dest, run, verify, restore, rotate-key,
      register-dpapi, keys, catalog, prune, schedule`) 전부와 `keys status`,
      `catalog rebuild`가 노출되고 정확히 파싱된다.
- [ ] `Config` 구조체가 P2-1의 모든 갭을 커버한다: `[meta] schema_version`, `[[ssh]]` 터널
      host-key 핀, S3 목적지 필드(endpoint/region/bucket/prefix/object_lock/checksum_algo),
      `enc:` 필드의 key-id/wrap-gen, `[[jobs]]` 전체 바인딩(source/destinations/
      required_immutability/retention/verify/schedule), `[policy]` 전체 필드(소스별
      dump_tool_version 포함), `[notify]`.
- [ ] 자격증명 성격 필드는 예외 없이 `EncField` 타입.
- [ ] prune 신원은 `PruneProfile`/`prune.toml`에만 존재하며 `Config`/`run` 경로에서 구조적으로
      도달 불가능하다.
- [ ] `schema_version` 마이그레이션 스캐폴드가 존재하고 최소 1개 테스트(마이그레이션 0개 경로)로
      검증된다.
- [ ] `dbbackup.example.toml`이 `Config::load`로 오류 없이 로드되는 테스트가 있다.
- [ ] `cargo test --workspace` 및 `cargo clippy --workspace -- -D warnings`가 통과한다.
- [ ] `.gitignore`가 `dbbackup.toml`, `prune.toml`, 카탈로그/스풀/키 자료를 제외하고,
      `Cargo.lock`은 추적 대상이다.

## Files (paths to create)

```
D:\git\dbbackupsystem\Cargo.toml
D:\git\dbbackupsystem\rust-toolchain.toml
D:\git\dbbackupsystem\.gitignore
D:\git\dbbackupsystem\crates\dbbackup-core\Cargo.toml
D:\git\dbbackupsystem\crates\dbbackup-core\src\lib.rs
D:\git\dbbackupsystem\crates\dbbackup-config\Cargo.toml
D:\git\dbbackupsystem\crates\dbbackup-config\src\lib.rs
D:\git\dbbackupsystem\crates\dbbackup-config\src\schema.rs
D:\git\dbbackupsystem\crates\dbbackup-config\src\migrate.rs
D:\git\dbbackupsystem\crates\dbbackup-config\src\validate.rs
D:\git\dbbackupsystem\crates\dbbackup-config\src\prune.rs
D:\git\dbbackupsystem\crates\dbbackup-config\tests\config_roundtrip.rs
D:\git\dbbackupsystem\crates\dbbackup-config\tests\migration.rs
D:\git\dbbackupsystem\crates\dbbackup-config\tests\validate.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\Cargo.toml
D:\git\dbbackupsystem\crates\dbbackup-cli\src\main.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\cli.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\mod.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\init.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\doctor.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\test_connection.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\add_db.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\add_dest.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\run.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\verify.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\restore.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\rotate_key.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\register_dpapi.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\keys.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\catalog.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\prune.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\src\commands\schedule.rs
D:\git\dbbackupsystem\crates\dbbackup-cli\tests\cli_parsing.rs
D:\git\dbbackupsystem\config\dbbackup.example.toml
D:\git\dbbackupsystem\config\prune.example.toml
```
