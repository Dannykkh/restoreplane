# S03 — BackupSource trait + PostgresSource

> 의존: S05(Pipeline — `DumpArtifact`/`DumpStream`/`DumpDir`/`DumpExit` 소비자 계약을 S03이 채운다).
> 레이어 3. 관련 리뷰: domain-technical.md §1(PostgreSQL) — globals 미백업, 버전 침묵실패, 확장 누락,
> per-database vs cluster-wide 감지. plan.md "DT globals/DT 버전/DT 엔진스캔" 대응 매핑.

## Background

PostgreSQL은 `pg_dump`가 **DB 하나의 객체만** 덤프한다. Role/Tablespace는 클러스터 공유 카탈로그
(`pg_authid` 등)에 있어 `pg_dump` 산출물에 절대 포함되지 않는다. 이 사실을 설계에서 놓치면 "백업은
매일 성공했는데 다른 서버에 복원하니 `role "app_user" does not exist`로 실패"하는 사고가 난다
(domain-technical.md §1.2, plan.md "DT globals"). 그래서 이 섹션은 **PostgreSQL 클러스터 1대당
globals 덤프 1개를 DB 덤프와 대등한 1급 산출물**로 취급한다 — `pg_dumpall --globals-only`가 만드는
산출물은 어느 DB에도 속하지 않고, DB 덤프 쪽에 `pairs_with_globals_backup_id`로 짝을 기록해 복원
시 "이 DB 덤프 시점에 유효했던 role 정의가 어느 globals 스냅샷인지"를 추적 가능하게 한다.

이 섹션이 책임지는 두 번째 위험은 **침묵 실패**다. 구버전 `pg_dump`가 신버전 서버를 덤프하면 대부분
즉시 에러로 죽지만, 마이너 차이가 작을수록 에러 없이 돌아가면서 신규 객체 타입을 조용히 누락할
수 있다(domain-technical.md §1.6). 그래서 PostgresSource는 **매 `run`마다** — `init` 1회성이 아니라
— 실제로 실행될 `pg_dump` 바이너리의 버전과 서버 버전을 재조회해 `client_major < server_major`면
어떤 덤프도 시작하지 않고 하드 실패한다.

세 번째 책임은 **확장(extension) 캡처**다. `pg_dump`는 `CREATE EXTENSION IF NOT EXISTS`는 덤프에
넣지만 확장 바이너리 자체는 넣지 않는다 — 복원 대상에 동일 확장·동일 버전이 없으면 복원이 깨진다
(§1.4). 이 섹션은 백업 시점에 `SELECT extname, extversion FROM pg_extension`을 DB별로 캡처해
`DumpPlanPart`에 실어 S08 매니페스트로 넘긴다.

네 번째 책임은 **클러스터 vs 등록 목록 드리프트** 감지다(§1.5). `add-db`로 등록된 DB 목록과
`pg_database WHERE datistemplate = false`로 실제 서버에 존재하는 DB 목록이 어긋나면(신규 DB 미등록,
등록된 DB가 서버에서 삭제됨) `run` 시점에 경고를 낸다 — 자동으로 백업 대상에 편입시키지는 않는다
(암묵적 백업 대상 확장은 별도 사용자 승인 없이 이뤄지면 안 되는 정책 결정, `add-db`가 여전히
명시적 등록 창구).

이 섹션은 **덤프 소스**만 책임진다. 압축·암호화·스풀·팬아웃은 S05가, 매니페스트 직렬화·서명·카탈로그
기록은 S08이 소유한다. PostgresSource가 만드는 `DumpArtifact::Stream`은 S05의 `run_stream_pipeline`이
그대로 소비하는 계약이므로, 이 문서의 `DumpStream`/`DumpExit`은 S05 원본 정의를 재게시한다(타입은
`src/pipeline/artifact.rs`에 실존, 여기서 새로 정의하지 않는다).

## Requirements

| # | 요구사항 | 근거 |
|---|---------|------|
| R1 | `BackupSource` trait: `plan() -> DumpPlan`(globals + per-db parts), `spawn(part) -> DumpArtifact` | plan §확정 아키텍처 |
| R2 | `pg_dump -Fc -f -`(custom, stdout 스트림)를 기본 포맷으로 사용, `plain`/`directory`는 옵션 | DT §1.1 |
| R3 | `pg_dumpall --globals-only`를 DB 덤프와 별개 1급 `DumpPlanPart`로 생성, `pairs_with_globals_backup_id`로 짝 기록 | DT §1.2 |
| R4 | 매 `plan()` 호출마다(=매 `run`마다) 서버 버전을 재조회하고, 선택된 클라이언트 바이너리의 실제 버전을 재확인 | DT §1.6 |
| R5 | `client_major < server_major`면 어떤 `DumpPlanPart`도 만들지 않고 `plan()`을 `Err`로 HARD FAIL | DT §1.6 |
| R6 | DB별 `SELECT extname, extversion FROM pg_extension`을 캡처해 `DumpPlanPart.extensions`에 기록 | DT §1.4 |
| R7 | `pg_database WHERE datistemplate = false AND datallowconn`으로 실제 DB 목록을 조회, 미등록/유실 DB를 `PlanWarning`으로 표면화(자동 편입 금지) | DT §1.5 |
| R8 | 번들 `pg_dump`/`pg_dumpall` 버전 매트릭스(PG13~17)를 서버 메이저 버전으로 선택, 없으면 하드 실패 | DT §1.6, plan §번들 매트릭스 |
| R9 | DB 접속 비밀번호는 자식 프로세스 `argv`에 절대 포함하지 않는다(`PGPASSWORD` env로만 전달) | 보안(ps 노출 방지) |
| R10 | 자식 stderr는 전량 드레인하며 마지막 N줄만 보존(파이프 백프레셔로 인한 데드락 방지) | tokio::process gotcha |
| R11 | `DumpPlanPart`는 S08 매니페스트 필드(`dump_tool`, `dump_tool_version`, `dump_format`, `dump_flags`,
      `extensions`, `charset`, `collation`, `pairs_with_globals_backup_id`)를 그대로 채울 수 있는 형태로 노출 | db-schema.md manifest |
| R12 | 복원 순서 계약(globals → db)을 `DumpPlanPart`/매니페스트 필드로 표현해 S09가 강제할 수 있게 한다 | DT §1.2 |

## Dependencies

- **S05(상위 소비자)** — `PostgresSource::spawn()`이 반환하는 `DumpArtifact::Stream(DumpStream)`을
  S05 `run_stream_pipeline`이 그대로 소비한다. S03은 `DumpStream { reader, wait }` 계약을 지켜야 하며,
  타입 정의는 새로 만들지 않고 S05 `src/pipeline/artifact.rs`를 그대로 사용한다.
- **하위 의존(이 섹션을 소비)**: S08(Manifest — `DumpPlanPart`의 버전/확장/플래그 필드를 그대로
  옮겨 담음), S09(Restorer — `pairs_with_globals_backup_id`로 globals→db 복원 순서를 강제).
- **S01(Config)** — `add-db`로 등록된 DB 별칭 목록, 접속 정보(host/port/user/password 참조), 번들
  바이너리 루트 경로, `sslmode` 등 설정을 공급받는다. 본 섹션은 Config 파싱을 책임지지 않는다.
- **경계 규칙**: S03은 압축/암호화/스풀에 관여하지 않는다(P0-5/옵션 C 분리 유지). `plan()`은 DB
  커넥션(`tokio-postgres`)만 열고, 덤프 자체는 `tokio::process`로 별도 자식 프로세스를 띄운다 —
  둘을 같은 커넥션으로 처리하지 않는다(덤프는 항상 공식 `pg_dump` 바이너리가 수행, S1 결정 재확인).

## Reference Libraries

| 크레이트 | 버전 | 이 섹션에서의 역할 | 주의 |
|---------|------|------|------|
| `tokio` | 1.x | `process::Command`(자식 프로세스), `io::AsyncRead`, `spawn`(stderr 드레인 태스크) | `process` feature 필요, S05와 workspace 공유 |
| `tokio-postgres` | 0.7.x | 버전/확장/DB목록 조회 전용 커넥션(덤프 자체는 담당하지 않음) | `NoTls` 기본, TLS는 `postgres-native-tls`/`tokio-postgres-rustls`로 향후 확장(`[확인 필요: sslmode 지원 범위]`) |
| `async-trait` | 0.1.x | `BackupSource`를 `dyn` 객체로 쓰기 위한 object-safe async trait | 네이티브 async fn in trait은 `dyn` 호환 아님(RPITIT) |
| `uuid` | 1.x | `backup_id` 발급(globals/db 파트 각각) | `v4` feature |
| `chrono` | 0.4.x | `server_version_detected_at`(UTC RFC3339) | `serde` feature(매니페스트 직렬화 대비) |
| `thiserror` | 2.x | `PgSourceError` | S02/S05와 동일 컨벤션 |
| `tracing` | 0.1.x | 경고 로그(`PlanWarning`), stderr tail 진단 | |
| `zeroize` | 1.x | DB 접속 비밀번호(`Zeroizing<String>`) | R9와 정합(비번은 메모리에도 남기지 않음) |

`Cargo.toml`(발췌):
```toml
[dependencies]
tokio = { version = "1", features = ["process", "io-util", "rt-multi-thread", "macros"] }
tokio-postgres = { version = "0.7", features = [] }
async-trait = "0.1"
uuid = { version = "1", features = ["v4"] }
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"
tracing = "0.1"
zeroize = { version = "1", features = ["derive"] }
```

> `sqlx` 대신 `tokio-postgres`를 선택한 이유: 이 섹션은 컴파일 타임 쿼리 검증이 필요 없는 3~4개의
> 단순 조회(`version()`, `pg_extension`, `pg_database`)만 수행하며, `sqlx`의 매크로/오프라인 스키마
> 캐시(`.sqlx/`) 관리 비용을 들일 이유가 없다. `tokio-postgres`는 커넥션풀 없이도 단발 조회에 가볍고,
> S02/S05가 이미 `tokio` 생태계를 전제하므로 의존성 축이 늘지 않는다. `[확인 필요]`: MariaDB 분기가
> 있는 S04에서 `sqlx`가 더 유리하면(MySQL 드라이버 성숙도) 그쪽만 `sqlx`로 갈 수 있다 — 두 섹션이
> 반드시 같은 DB 드라이버 크레이트를 쓸 필요는 없다(BackupSource trait 경계가 이를 흡수한다).

## Implementation

### 0. S05 계약 재게시 (신규 정의 아님)

```rust
// src/pipeline/artifact.rs (S05 원본, 재게시만 함) — PostgresSource::spawn()의 반환 타입 계약.
pub enum DumpArtifact {
    Stream(DumpStream),
    Directory(DumpDir),
}
pub struct DumpStream {
    pub reader: Box<dyn tokio::io::AsyncRead + Unpin + Send>,
    pub wait: futures::future::BoxFuture<'static, anyhow::Result<DumpExit>>,
}
pub struct DumpExit {
    pub code: Option<i32>,
    pub success: bool,
    pub stderr_tail: String,
}
```

PostgresSource는 `Directory`를 만들지 않는다(v1은 `-Fc -f -` 스트림 전용, R2). `directory` 포맷은
"대용량 DB 병렬 복원"이 필요해지는 v1.x 확장 지점으로 명시적으로 스코프 밖에 둔다(DT §1.1).

### 1. 모듈 구조

```
src/source/
  mod.rs               // BackupSource trait, DumpPlan, DumpPlanPart, DumpPartKind, PlanWarning(공용, S04도 사용)
  postgres/
    mod.rs             // PostgresSource, PgConnCfg, PgSourceError
    version.rs         // ToolVersion, VersionProbe, probe_versions, BundledToolMatrix
    catalog_probe.rs   // list_databases, capture_extensions, capture_encoding
    plan.rs            // BackupSource::plan() 구현
    dump.rs            // BackupSource::spawn() 구현, render_flags, stderr tail 드레인
```

### 2. `BackupSource` trait 및 공용 계획 타입 (`src/source/mod.rs`)

```rust
use async_trait::async_trait;
use uuid::Uuid;
use crate::pipeline::artifact::DumpArtifact;

/// 덤프 소스 공통 계약. Postgres(S03)/MySQL(S04)이 이 trait을 구현한다.
/// `dyn BackupSource`로 잡 오케스트레이터(S10 스케줄러, CLI run)에서 다형적으로 다룬다.
#[async_trait]
pub trait BackupSource: Send + Sync {
    /// 백업 실행 계획을 세운다: 서버 상태 조회(버전/DB목록/확장) + 하드 실패 게이트.
    /// 이 함수 자체가 "매 run 버전 재검증"의 실행 지점이다(R4/R5).
    async fn plan(&self) -> anyhow::Result<DumpPlan>;

    /// 계획의 파트 1개를 실제 자식 프로세스로 구동한다. 동기 스폰(프로세스 시작 자체는
    /// non-blocking) — S05 `run_job`이 `source.spawn(&part)?`로 즉시 호출한다.
    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact>;
}

/// 한 번의 `plan()` 호출로 확정된 실행 계획. globals + DB별 파트로 구성된다.
/// MySQL(S04)처럼 클러스터 전역 개념이 없는 엔진은 `globals_part = None`.
pub struct DumpPlan {
    pub cluster_probe: Option<ClusterVersionProbe>, // PG: Some. 매니페스트 server_version_detected_at 근거.
    pub globals_part: Option<DumpPlanPart>,
    pub db_parts: Vec<DumpPlanPart>,
    pub warnings: Vec<PlanWarning>,                  // CLI run이 표면화(경고, HARD FAIL 아님)
}

/// 매니페스트(S08)가 그대로 옮겨 담을 수 있는 형태로 설계된 파트 단위 계획.
pub struct DumpPlanPart {
    pub backup_id: Uuid,
    pub job: String,
    pub source_alias: String,
    pub kind: DumpPartKind,
    /// PG DB 파트만 Some(같은 plan()에서 만들어진 globals_part.backup_id). globals 파트 자신은 None.
    pub pairs_with_globals_backup_id: Option<Uuid>,
    pub dump_tool: &'static str,       // "pg_dump" | "pg_dumpall"
    pub dump_tool_path: std::path::PathBuf,
    pub dump_tool_version: String,     // `pg_dump --version` 원문(재확인된 실제 바이너리 버전, R4)
    pub dump_format: &'static str,     // "custom" | "plain"
    pub dump_flags: String,            // 실행될 전체 커맨드라인, 시크릿 없음(R9 덕분에 그대로 원문)
    pub extensions: Vec<ExtensionInfo>,// DB 파트만 채움(§1.4), globals는 빈 Vec
    pub charset: Option<String>,
    pub collation: Option<String>,
}

pub enum DumpPartKind {
    Globals,
    Database { name: String },
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct ExtensionInfo {
    pub name: String,
    pub version: String,
}

/// 하드 실패는 아니지만 운영자가 반드시 알아야 하는 드리프트(R7).
#[derive(Debug)]
pub enum PlanWarning {
    /// 서버엔 존재하지만 add-db로 등록되지 않은 DB(백업 대상에서 누락될 위험).
    UnregisteredDatabase(String),
    /// add-db로 등록됐지만 서버에서 더는 조회되지 않는 DB(삭제/오탈자 가능성).
    RegisteredDatabaseMissing(String),
}
```

### 3. 버전 재검증 (`version.rs`, R4/R5/R8)

`server_version_num`(PG10+에서 `major*10000 + minor`, 예: `160003` → major 16)을 신뢰 소스로
쓴다 — `SELECT version()` 원문 파싱보다 안정적이고, 원문 문자열은 매니페스트 표시용으로 그대로
같이 보존한다(db-schema.md `server_version`).

```rust
use tokio_postgres::Client;
use chrono::{DateTime, Utc};

#[derive(Clone, Debug)]
pub struct ToolVersion {
    pub major: u32,
    pub raw: String,   // "pg_dump (PostgreSQL) 16.3" 또는 "PostgreSQL 16.3 on ..."
}

#[derive(Clone, Debug)]
pub struct ClusterVersionProbe {
    pub server: ToolVersion,
    pub client: ToolVersion,       // 실제 실행될 pg_dump 바이너리의 --version 재확인 결과
    pub detected_at: DateTime<Utc>,
}

/// 서버 버전 조회. `server_version_num`은 정수라 파싱이 안전하다(원문 파싱 금지, gotcha).
pub async fn probe_server_version(client: &Client) -> Result<ToolVersion, PgSourceError> {
    let row = client
        .query_one("SELECT version(), current_setting('server_version_num')", &[])
        .await
        .map_err(PgSourceError::VersionQueryFailed)?;
    let raw: String = row.get(0);
    let num_str: String = row.get(1);
    let num: u32 = num_str.parse().map_err(|_| PgSourceError::VersionParseFailed(num_str.clone()))?;
    Ok(ToolVersion { major: num / 10000, raw })
}

/// 선택된 `pg_dump` 바이너리를 실제로 실행해 버전을 재확인한다(매트릭스 설정 오류/바이너리
/// 치환에 대한 방어 — "매트릭스가 맞다고 믿는다"가 아니라 실제 바이너리를 물어본다).
pub async fn probe_client_version(pg_dump_path: &std::path::Path) -> Result<ToolVersion, PgSourceError> {
    let out = tokio::process::Command::new(pg_dump_path)
        .arg("--version")
        .output()
        .await
        .map_err(|e| PgSourceError::DumpToolSpawnFailed { path: pg_dump_path.to_path_buf(), source: e })?;
    let raw = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let major = parse_version_major(&raw)
        .ok_or_else(|| PgSourceError::VersionParseFailed(raw.clone()))?;
    Ok(ToolVersion { major, raw })
}

/// "pg_dump (PostgreSQL) 16.3" → 16. 마지막 토큰의 첫 '.' 앞부분만 취함.
fn parse_version_major(raw: &str) -> Option<u32> {
    raw.split_whitespace()
        .last()?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// 서버 버전 조회 → 매트릭스로 클라이언트 선택 → 선택된 바이너리 실버전 재확인 → 하드 실패 게이트.
/// R4/R5의 실행 지점. `plan()`이 매 호출마다 이 함수를 부른다(캐시하지 않는다).
pub async fn probe_and_select(
    client: &Client,
    matrix: &BundledToolMatrix,
) -> Result<(ClusterVersionProbe, std::path::PathBuf, std::path::PathBuf), PgSourceError> {
    let server = probe_server_version(client).await?;
    let pg_dump_path = matrix.select_pg_dump(server.major)?;
    let pg_dumpall_path = matrix.select_pg_dumpall(server.major)?;
    let client_ver = probe_client_version(&pg_dump_path).await?;

    // R5: 클라이언트 메이저 < 서버 메이저는 HARD FAIL. 매트릭스 선택 로직상 정상적으로는
    // 발생하지 않아야 하지만(select_pg_dump가 이미 major>=server 보장), 바이너리 치환/설정
    // 오류에 대한 최후 방어선으로 유지한다(defense-in-depth).
    if client_ver.major < server.major {
        return Err(PgSourceError::ClientTooOld {
            client_major: client_ver.major,
            server_major: server.major,
        });
    }

    Ok((
        ClusterVersionProbe { server, client: client_ver, detected_at: Utc::now() },
        pg_dump_path,
        pg_dumpall_path,
    ))
}
```

### 4. 번들 버전 매트릭스 (PG13~17, R8)

```rust
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// 서버 메이저 버전 → 번들된 클라이언트 디렉토리 매핑. 디렉토리 관례:
/// `tools/pg/pg{major}/pg_dump[.exe]`, `tools/pg/pg{major}/pg_dumpall[.exe]`.
pub struct BundledToolMatrix {
    roots: BTreeMap<u32, PathBuf>,   // major -> tools/pg/pg{major}/
}

impl BundledToolMatrix {
    /// v1 지원 범위: PG13~17(DT §1.6 "현재 지원 버전 범위"). 디렉토리가 없는 메이저는 건너뛴다.
    pub fn from_base_dir(base: &Path) -> Self {
        let mut roots = BTreeMap::new();
        for major in 13..=17u32 {
            let dir = base.join(format!("pg{major}"));
            if dir.is_dir() {
                roots.insert(major, dir);
            }
        }
        Self { roots }
    }

    pub fn select_pg_dump(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dump"))
    }

    pub fn select_pg_dumpall(&self, server_major: u32) -> Result<PathBuf, PgSourceError> {
        self.select_binary(server_major, bin_name("pg_dumpall"))
    }

    /// 정확히 일치하는 메이저가 있으면 그것. 없으면 **server_major 이상인 것 중 가장 낮은 메이저**
    /// (하위호환은 보장되므로 서버보다 너무 앞선 신버전 클라이언트의 신규 문법 리스크를 최소화).
    /// server_major보다 낮은 클라이언트뿐이면(=번들이 서버를 못 따라감) 하드 실패.
    fn select_binary(&self, server_major: u32, filename: &str) -> Result<PathBuf, PgSourceError> {
        let dir = self
            .roots
            .get(&server_major)
            .or_else(|| self.roots.range(server_major..).next().map(|(_, d)| d))
            .ok_or(PgSourceError::NoBundledClientForServer { server_major })?;
        Ok(dir.join(filename))
    }
}

#[cfg(windows)]
fn bin_name(stem: &str) -> &'static str {
    match stem { "pg_dump" => "pg_dump.exe", "pg_dumpall" => "pg_dumpall.exe", _ => unreachable!() }
}
#[cfg(not(windows))]
fn bin_name(stem: &str) -> &'static str {
    match stem { "pg_dump" => "pg_dump", "pg_dumpall" => "pg_dumpall", _ => unreachable!() }
}
```

> `doctor` 커맨드(S01)는 `init`/주기 점검 시 `BundledToolMatrix::from_base_dir(..)`가 PG13~17
> 각각에 대해 `select_pg_dump(major)` 후 `probe_client_version`을 실행해 "매트릭스가 주장하는
> 메이저"와 "실제 바이너리가 응답하는 메이저"가 일치하는지 검증할 것을 권고한다(공급망 치환 방어,
> S11과 정합). 이 섹션은 그 훅 지점만 제공하고 `doctor` 자체 구현은 S01/S11 소관이다.

### 5. DB 목록 감지 + 확장/인코딩 캡처 (`catalog_probe.rs`, R6/R7)

```rust
use tokio_postgres::Client;

/// 클러스터에 실제 존재하는 DB(템플릿 제외, 접속 가능한 것만).
pub async fn list_live_databases(client: &Client) -> Result<Vec<String>, PgSourceError> {
    let rows = client
        .query(
            "SELECT datname FROM pg_database WHERE datistemplate = false AND datallowconn = true",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows.into_iter().map(|r| r.get(0)).collect())
}

/// 중요: `pg_extension`은 **DB별 카탈로그**다. 관리용 커넥션("postgres" DB 등)에서 조회하면
/// 그 DB의 확장만 보인다 — 반드시 대상 DB에 연결해서 조회해야 한다(흔한 실수 지점).
pub async fn capture_extensions(db_client: &Client) -> Result<Vec<crate::source::ExtensionInfo>, PgSourceError> {
    let rows = db_client
        .query("SELECT extname, extversion FROM pg_extension ORDER BY extname", &[])
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok(rows
        .into_iter()
        .map(|r| crate::source::ExtensionInfo { name: r.get(0), version: r.get(1) })
        .collect())
}

/// charset/collation도 DB별(매니페스트 §3.1, mojibake 추적용, DT §2.4 자매 항목의 PG판).
pub async fn capture_encoding(db_client: &Client) -> Result<(String, String), PgSourceError> {
    let row = db_client
        .query_one(
            "SELECT current_setting('server_encoding'), datcollate \
             FROM pg_database WHERE datname = current_database()",
            &[],
        )
        .await
        .map_err(PgSourceError::CatalogQueryFailed)?;
    Ok((row.get(0), row.get(1)))
}
```

### 6. `PostgresSource` 본체 (`mod.rs`)

```rust
use zeroize::Zeroizing;
use std::path::PathBuf;

pub struct PgConnCfg {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: Zeroizing<String>,
    pub admin_database: String,   // 클러스터 전역 조회용 커넥션 대상(기본 "postgres")
}

pub struct PostgresSource {
    pub job: String,
    pub alias: String,
    pub conn: PgConnCfg,
    pub registered_databases: Vec<String>,   // add-db 등록 목록(S01 Config)
    pub matrix: BundledToolMatrix,
}

impl PostgresSource {
    /// 조회 전용 커넥션(덤프는 여기서 안 나감 — 덤프는 항상 tokio::process가 담당, S1 결정 재확인).
    async fn connect(&self, database: &str) -> Result<tokio_postgres::Client, PgSourceError> {
        let mut cfg = tokio_postgres::Config::new();
        cfg.host(&self.conn.host)
            .port(self.conn.port)
            .user(&self.conn.user)
            .password(self.conn.password.as_str())
            .dbname(database)
            .application_name("dbbackup-probe");
        let (client, connection) = cfg
            .connect(tokio_postgres::NoTls) // [확인 필요] sslmode 옵션은 S01 Config 확정 후 분기
            .await
            .map_err(|e| PgSourceError::ConnectionFailed { database: database.to_string(), source: e })?;
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                tracing::warn!(error = %e, "postgres probe connection terminated");
            }
        });
        Ok(client)
    }
}
```

### 7. `plan()` 구현 (`plan.rs`, R3/R5/R6/R7)

```rust
use async_trait::async_trait;
use uuid::Uuid;
use crate::source::{BackupSource, DumpPlan, DumpPlanPart, DumpPartKind, PlanWarning};

#[async_trait]
impl BackupSource for PostgresSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        // 관리용 커넥션(클러스터 전역 조회) — 매 run마다 새로 연다(캐시 금지, R4).
        let admin = self.connect(&self.conn.admin_database).await?;

        // R4/R5: 버전 재검증이 어떤 DumpPlanPart 생성보다 먼저 게이트로 선다.
        let (probe, pg_dump_path, pg_dumpall_path) =
            crate::source::postgres::version::probe_and_select(&admin, &self.matrix).await?;
        tracing::info!(
            server = %probe.server.raw, client = %probe.client.raw,
            "postgres version probe ok"
        );

        // R7: 실제 DB 목록 vs 등록 목록 드리프트 감지.
        let live_dbs = crate::source::postgres::catalog_probe::list_live_databases(&admin).await?;
        let mut warnings = Vec::new();
        for db in &live_dbs {
            if !self.registered_databases.iter().any(|r| r == db) {
                warnings.push(PlanWarning::UnregisteredDatabase(db.clone()));
            }
        }

        // R3: globals 파트를 DB 파트들과 같은 backup_id 발급 방식으로, 하지만 별개 산출물로 생성.
        let globals_backup_id = Uuid::new_v4();
        let globals_flags = render_globals_flags(&self.conn);
        let globals_part = DumpPlanPart {
            backup_id: globals_backup_id,
            job: self.job.clone(),
            source_alias: self.alias.clone(),
            kind: DumpPartKind::Globals,
            pairs_with_globals_backup_id: None,
            dump_tool: "pg_dumpall",
            dump_tool_path: pg_dumpall_path,
            dump_tool_version: probe.client.raw.clone(),
            dump_format: "plain",           // pg_dumpall은 custom 포맷 미지원(DT §1.2)
            dump_flags: globals_flags,
            extensions: Vec::new(),
            charset: None,
            collation: None,
        };

        // DB별 파트: 등록된 DB만 대상(자동 편입 없음), 등록됐지만 서버에 없으면 경고 후 스킵.
        let mut db_parts = Vec::with_capacity(self.registered_databases.len());
        for db in &self.registered_databases {
            if !live_dbs.iter().any(|d| d == db) {
                warnings.push(PlanWarning::RegisteredDatabaseMissing(db.clone()));
                continue;
            }
            // R6: pg_extension은 DB별 카탈로그 — 반드시 해당 DB로 재접속해서 조회한다.
            let db_client = self.connect(db).await?;
            let extensions = crate::source::postgres::catalog_probe::capture_extensions(&db_client).await?;
            let (charset, collation) =
                crate::source::postgres::catalog_probe::capture_encoding(&db_client).await?;

            db_parts.push(DumpPlanPart {
                backup_id: Uuid::new_v4(),
                job: self.job.clone(),
                source_alias: self.alias.clone(),
                kind: DumpPartKind::Database { name: db.clone() },
                pairs_with_globals_backup_id: Some(globals_backup_id), // R12: 복원 순서 계약의 근거
                dump_tool: "pg_dump",
                dump_tool_path: pg_dump_path.clone(),
                dump_tool_version: probe.client.raw.clone(),
                dump_format: "custom",
                dump_flags: render_db_flags(&self.conn, db),
                extensions,
                charset: Some(charset),
                collation: Some(collation),
            });
        }

        for w in &warnings {
            match w {
                PlanWarning::UnregisteredDatabase(db) =>
                    tracing::warn!(db, "서버에 존재하지만 add-db로 등록되지 않음 — 백업에서 누락됨"),
                PlanWarning::RegisteredDatabaseMissing(db) =>
                    tracing::warn!(db, "add-db로 등록됐지만 서버에서 조회되지 않음 — 삭제/오탈자 확인 필요"),
            }
        }

        Ok(DumpPlan {
            cluster_probe: Some(probe),
            globals_part: Some(globals_part),
            db_parts,
            warnings,
        })
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<crate::pipeline::artifact::DumpArtifact> {
        crate::source::postgres::dump::spawn_part(&self.conn, part)
    }
}
```

### 8. `spawn()` 구현 — `tokio::process`로 실제 자식 프로세스 구동 (`dump.rs`, R2/R9/R10)

```rust
use std::process::Stdio;
use std::collections::VecDeque;
use tokio::process::Command;
use crate::pipeline::artifact::{DumpArtifact, DumpStream, DumpExit};
use crate::source::{DumpPlanPart, DumpPartKind};

const STDERR_TAIL_LINES: usize = 200;

pub fn spawn_part(conn: &super::PgConnCfg, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
    let args = match &part.kind {
        DumpPartKind::Globals => globals_args(),
        DumpPartKind::Database { name } => db_args(name),
    };
    spawn_stream(conn, &part.dump_tool_path, &args)
}

/// R9: 비밀번호는 오직 `PGPASSWORD` 환경변수로만 자식에 전달한다 — argv에는 절대 넣지 않는다.
/// (argv는 `ps`/작업관리자에서 다른 로컬 사용자에게도 보일 수 있다.)
fn spawn_stream(
    conn: &super::PgConnCfg,
    tool_path: &std::path::Path,
    args: &[String],
) -> anyhow::Result<DumpArtifact> {
    let mut cmd = Command::new(tool_path);
    cmd.args(args)
        .env("PGPASSWORD", conn.password.as_str())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true); // reader/wait가 버려지면(예: 상위 취소) 자식도 함께 정리

    let mut child = cmd.spawn()
        .map_err(|e| anyhow::anyhow!("덤프 도구 실행 실패({}): {e}", tool_path.display()))?;

    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let wait = Box::pin(async move {
        // R10: stderr을 전량 드레인하며 마지막 N줄만 보존. 드레인하지 않으면 pg_dump가
        // NOTICE/progress 라인을 많이 찍는 경우 파이프 버퍼가 가득 차 자식이 블록될 수 있다.
        let stderr_tail = drain_stderr_tail(stderr).await;
        let status = child.wait().await?;
        Ok(DumpExit {
            code: status.code(),
            success: status.success(),
            stderr_tail,
        })
    });

    Ok(DumpArtifact::Stream(DumpStream {
        reader: Box::new(stdout),
        wait,
    }))
}

async fn drain_stderr_tail(stderr: tokio::process::ChildStderr) -> String {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut lines = BufReader::new(stderr).lines();
    let mut ring: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL_LINES);
    loop {
        match lines.next_line().await {
            Ok(Some(line)) => {
                if ring.len() == STDERR_TAIL_LINES { ring.pop_front(); }
                ring.push_back(line);
            }
            _ => break, // EOF 또는 읽기 오류 — 어느 쪽이든 드레인 종료
        }
    }
    ring.into_iter().collect::<Vec<_>>().join("\n")
}

fn common_conn_args(conn: &super::PgConnCfg) -> Vec<String> {
    vec![
        "-h".into(), conn.host.clone(),
        "-p".into(), conn.port.to_string(),
        "-U".into(), conn.user.clone(),
        "--no-password".into(), // 프롬프트 금지 — PGPASSWORD 없으면 즉시 실패(무인 실행 안전장치)
    ]
}

fn db_args(db: &str) -> Vec<String> {
    let mut a = vec!["-Fc".into(), "-f".into(), "-".into()];
    a.push(db.into());
    a
}

fn globals_args() -> Vec<String> {
    vec!["--globals-only".into()] // pg_dumpall은 -f 생략 시 stdout으로 나간다(plain SQL)
}

/// dump_flags 매니페스트 필드용 원문 렌더링. PGPASSWORD는 애초에 args에 없으므로
/// 별도 redaction 로직이 필요 없다(R9의 직접적 이득 — 이게 이 설계를 이렇게 잡은 이유).
pub fn render_db_flags(conn: &super::PgConnCfg, db: &str) -> String {
    let mut args = common_conn_args(conn);
    args.extend(db_args(db));
    format!("pg_dump {}", args.join(" "))
}

pub fn render_globals_flags(conn: &super::PgConnCfg) -> String {
    let mut args = common_conn_args(conn);
    args.extend(globals_args());
    format!("pg_dumpall {}", args.join(" "))
}
```

> `db_args`/`globals_args`가 `common_conn_args`와 분리 조립되므로, 실제 `Command`에 넘기는 args와
> `render_*_flags`가 렌더링하는 문자열이 **동일한 순서로 조립되도록 헬퍼 함수를 공유**한다(문서 문자열과
> 실제 실행 커맨드가 어긋나는 것은 매니페스트 신뢰성을 깨는 흔한 실수 — 위 코드에서 `common_conn_args`
> 하나만 양쪽이 재사용하는 이유).

### 9. 오류 타입 (`PgSourceError`)

```rust
#[derive(thiserror::Error, Debug)]
pub enum PgSourceError {
    #[error("DB 접속 실패({database}): {source}")]
    ConnectionFailed { database: String, source: tokio_postgres::Error },
    #[error("버전 조회 쿼리 실패: {0}")]
    VersionQueryFailed(#[source] tokio_postgres::Error),
    #[error("버전 문자열 파싱 실패: {0:?}")]
    VersionParseFailed(String),
    #[error("카탈로그 조회 실패: {0}")]
    CatalogQueryFailed(#[source] tokio_postgres::Error),
    #[error("서버 메이저 버전 {server_major}을(를) 지원하는 번들 클라이언트가 없음(PG13~17 매트릭스 확인)")]
    NoBundledClientForServer { server_major: u32 },
    #[error("클라이언트 메이저({client_major}) < 서버 메이저({server_major}) — 하드 실패(DT §1.6)")]
    ClientTooOld { client_major: u32, server_major: u32 },
    #[error("덤프 도구 실행 실패({path}): {source}")]
    DumpToolSpawnFailed { path: std::path::PathBuf, source: std::io::Error },
}
```

### 10. 복원 순서 계약 (globals → db, R12)

PostgresSource는 복원을 실행하지 않는다(S09 소관). 이 섹션이 보장하는 건 **순서를 결정할 수 있는
데이터**뿐이다.

- 매니페스트의 `pairs_with_globals_backup_id`(DB 파트에만 존재)가 globals 백업의 `backup_id`를
  가리킨다.
- S09 Restorer는 복원 대상 DB 파트를 고를 때 이 필드로 짝 globals 백업을 카탈로그/매니페스트에서
  조회하고, **globals 복원(psql로 SQL 재생) → 해당 DB 복원(pg_restore)** 순서를 강제해야 한다.
  순서가 바뀌면 role 없는 상태에서 스키마 복원이 실패한다(DT §1.2). 이 강제 자체는 S09 파일의 책임이며,
  본 섹션은 강제에 필요한 필드만 채운다.

## Test Scenarios

| # | 시나리오 | 기대 | 검증 포인트 |
|---|---------|------|------------|
| T1 | `plan()` 정상 경로: registered_databases=[app,billing], 둘 다 서버에 존재 | globals_part 1개 + db_parts 2개, 각 db_part.pairs_with_globals_backup_id == globals.backup_id | R3/R12 |
| T2 | 버전 프로브: `server_version_num=160003`, 선택된 pg_dump 실제 버전도 16.x | `probe_and_select` 성공, `ClusterVersionProbe.detected_at` 채워짐 | R4 |
| T3 | 버전 프로브: 매트릭스 오설정으로 선택된 바이너리가 실제로는 major=15인데 서버는 16 | `ClientTooOld` Err, `plan()` 전체가 Err(어떤 DumpPlanPart도 생성 안 됨) | R5 defense-in-depth |
| T4 | 클라이언트 major(17) > 서버 major(15) | 성공(하위호환은 허용, 위험한 방향 아님) | R4/R5 대칭성 |
| T5 | `BundledToolMatrix`: server_major=16, 정확히 일치하는 `pg16/` 디렉토리 존재 | `pg16/pg_dump[.exe]` 선택 | R8 |
| T6 | `BundledToolMatrix`: server_major=15, `pg15/` 없고 `pg16/`,`pg17/`만 존재 | `pg16/`(서버 이상 중 최소) 선택 | R8 fallback |
| T7 | `BundledToolMatrix`: server_major=18(매트릭스가 13~17만 커버) | `NoBundledClientForServer` Err | R8 하드실패 |
| T8 | `list_live_databases`: 템플릿 DB(`template0/1`) 및 `datallowconn=false` DB 존재 | 결과에서 제외됨 | R7 쿼리 정확성 |
| T9 | 등록되지 않은 DB가 서버에 신규 발견됨 | `PlanWarning::UnregisteredDatabase`, db_parts에는 미포함(자동 편입 없음) | R7 |
| T10 | 등록된 DB가 서버에서 삭제됨(오탈자 포함) | `PlanWarning::RegisteredDatabaseMissing`, 해당 파트 생성 스킵, 나머지 DB는 정상 진행 | R7 |
| T11 | `capture_extensions`: 대상 DB에 `postgis 3.4`, `pg_trgm 1.6` 설치 | `DumpPlanPart.extensions == [{postgis,3.4},{pg_trgm,1.6}]`, 정렬 결정적(extname ORDER BY) | R6 |
| T12 | `capture_extensions`를 관리용(admin_database) 커넥션으로 잘못 조회하는 회귀 방지 | 유닛 테스트로 "항상 대상 DB 커넥션 사용" 어서션(코드 리뷰용 가드) | R6 gotcha |
| T13 | `render_db_flags`/`render_globals_flags` 출력에 비밀번호 문자열 포함 여부 | 포함되지 않음(비밀번호는 env로만 전달 확인) | R9 |
| T14 | `spawn()` DB 파트: 실제(or fake) `pg_dump -Fc -f -` 실행 | `DumpArtifact::Stream`, `reader`에서 custom 포맷 헤더(`PGDMP`) 확인 가능 | R2 |
| T15 | `spawn()` globals 파트: `pg_dumpall --globals-only` 실행 | stdout에 `CREATE ROLE`/`ALTER ROLE` 류 plain SQL | R3 |
| T16 | stderr 폭주(수천 줄 NOTICE) 자식 프로세스 | 데드락 없이 정상 종료, `stderr_tail`은 마지막 200줄만 | R10 |
| T17 | 존재하지 않는 `dump_tool_path`로 `spawn()` 호출 | 즉시 `Err`(자식 spawn 실패), `DumpArtifact` 생성 안 됨 | 오류 전파 |
| T18 | 자식이 stdout에 일부만 쓰고 exit≠0으로 종료 | `DumpStream.wait`가 `success=false`를 반환(S05가 이를 소비해 스풀 폐기) — 본 섹션은 값만 정확히 채움 | R2/S05 경계 |

## Implementation Strategy

1. **버전 프로브 + 매트릭스 먼저(T2~T7)** — 순수 로직(문자열/정수 파싱, `BTreeMap` 선택)은 실제
   서버 없이 유닛 테스트 가능. 하드 실패 게이트를 가장 먼저 증명해 "덤프가 시작되기 전에 막힌다"를
   확정한다.
2. **카탈로그 조회(T8~T12)** — `tokio-postgres`로 로컬 PG 컨테이너(CI)에 대해 DB 목록/확장/인코딩
   조회. `capture_extensions`가 DB별 재접속을 강제하는지 회귀 테스트로 고정(T12, 흔한 실수 지점).
3. **`plan()` 통합(T1, T9, T10)** — 위 두 계층을 조립. globals/db 파트 짝짓기와 경고 표면화를
   검증.
4. **`spawn()` + 자식 프로세스 드레인(T14~T18)** — 실제 `pg_dump`/`pg_dumpall` 바이너리(CI 이미지에
   설치) 또는 테스트용 fake 스크립트로 stdout/stderr/exit 시나리오를 재현. S05가 이미 갖춘 fake
   자식 하네스(`tests/support/fake_dump.rs`)를 재사용해 인터페이스 정합성을 이중 확인.
5. **`dump_flags` 렌더링 회귀(T13)** — 커맨드 조립 헬퍼(`common_conn_args`)를 실행 경로와
   렌더링 경로가 공유하는지 코드 리뷰 + 테스트로 고정(문서-실행 드리프트 방지).

## Quality Gate

- [ ] `cargo test -p source-postgres` 전 시나리오(T1~T18) 통과.
- [ ] `client_major < server_major`인 어떤 경로에서도 `DumpPlanPart`가 생성되지 않는다(T3, R5).
- [ ] `capture_extensions`/`capture_encoding`이 admin 커넥션이 아니라 대상 DB 커넥션에서만
      호출된다는 정적 검사 또는 회귀 테스트가 존재한다(T12).
- [ ] `render_*_flags` 출력 문자열에 `PGPASSWORD`/비밀번호가 포함되지 않는다(grep 기반 CI 어서션, T13).
- [ ] DB 파트는 전부 `dump_format="custom"`, globals 파트는 `dump_format="plain"`으로 고정된다(R2/R3).
- [ ] `BundledToolMatrix`가 PG13~17 커버, 미지원 메이저는 명시적 `Err`(경고 아님)로 실패한다(T7).
- [ ] `cargo clippy -- -D warnings`, `cargo fmt --check`.

## Risk & Rollback

| 리스크 | 영향 | 완화 |
|-------|------|------|
| `plan()`과 `spawn()` 사이 시간차 동안 서버가 재기동/업그레이드됨(드묾) | 선택된 클라이언트가 새 서버 메이저에 비해 낡아짐 | v1은 수용(같은 `run` 안에서 두 시점 간격이 짧음). 장기 실행 잡이면 `spawn()` 직전 재확인 훅을 추가할 수 있는 구조(버전 프로브 함수가 이미 순수 함수로 분리돼 있어 재호출 비용 낮음). 롤백: 재확인 추가는 `dump.rs`에 국소적 |
| `tokio-postgres` 커넥션이 관리용 목적으로 DB마다(등록 DB 수만큼) 열림 | DB 수가 많으면 `plan()` 1회에 커넥션 N+1개 순차 오픈 — 지연 누적 | v1은 순차(정확성 우선), DB 수가 많아지면(Open Q: 실제 대수 확정 후) 소규모 커넥션 풀 또는 병렬 조회로 전환 가능(구조 변경 없이 `plan.rs` 루프만 교체) |
| 번들 바이너리 공급망 치환(H6) | 매트릭스가 가리키는 파일이 신뢰할 수 없는 바이너리로 대체됨 | `probe_client_version`이 실제 실행 결과를 항상 재확인(R4가 부수적으로 이 방어도 제공). 완전한 방어(해시 핀)는 S11 소관 |
| `pg_dumpall --globals-only`이 plain SQL만 지원 → 파이프라인이 custom 포맷을 기대하는 코드가 있으면 오작동 | globals 검증(L2, `pg_restore --list`)이 globals 파트에는 적용 불가 | S09 Verifier가 `dump_format` 필드로 분기(plain은 L2를 "psql --set ON_ERROR_STOP dry-run 불가, 헤더 존재 확인"으로 대체) — 본 섹션은 `dump_format="plain"`을 정확히 표기해 그 분기를 가능하게 함 |
| `NoTls` 기본값 | 관리용 조회 트래픽이 평문(동일 호스트/사설망 가정이 깨지면 위험) | `[확인 필요]` S01 `sslmode` 설정 확정 시 `postgres-native-tls` 추가, 인터페이스(`connect`) 변경 없이 내부만 교체 |

## Acceptance Criteria

- AC1: `plan()`은 매 호출마다 서버 버전을 재조회하고, `client_major < server_major`면 어떤
  `DumpPlanPart`도 만들지 않고 하드 실패한다(T2/T3, DT §1.6).
- AC2: PostgreSQL 클러스터 1대당 `pg_dumpall --globals-only` 산출물이 DB 덤프와 별개의 1급
  `DumpPlanPart`로 생성되고, 모든 DB 파트가 `pairs_with_globals_backup_id`로 이를 가리킨다(T1, DT §1.2).
- AC3: DB 덤프는 기본적으로 `pg_dump -Fc -f -`(custom, stdout 스트림)로 생성되며 S05 파이프라인이
  그대로 소비할 수 있는 `DumpArtifact::Stream`을 반환한다(T14, DT §1.1).
- AC4: 각 DB 파트에 `SELECT extname, extversion FROM pg_extension`(대상 DB 커넥션)의 결과가
  `extensions` 필드로 정확히 채워진다(T11/T12, DT §1.4).
- AC5: 서버에 존재하지만 미등록인 DB, 등록됐지만 서버에 없는 DB가 각각 경고로 표면화되고
  자동으로 백업 대상에 편입되지 않는다(T9/T10, DT §1.5).
- AC6: PG13~17 번들 매트릭스가 서버 메이저에 맞는 클라이언트를 선택하며, 커버되지 않는
  메이저는 명시적으로 실패한다(T5~T7, DT §1.6).
- AC7: DB 접속 비밀번호가 자식 프로세스 `argv`나 `dump_flags` 매니페스트 필드 어디에도 노출되지
  않는다(T13, R9).
- AC8: 자식 stderr가 전량 드레인되어 파이프 백프레셔로 인한 데드락이 없고, 마지막 N줄만
  `DumpExit.stderr_tail`에 보존된다(T16, R10).

## Files

| 파일 | 내용 |
|------|------|
| `src/source/mod.rs` | `BackupSource` trait, `DumpPlan`, `DumpPlanPart`, `DumpPartKind`, `PlanWarning`, `ExtensionInfo`(S04와 공유) |
| `src/source/postgres/mod.rs` | `PostgresSource`, `PgConnCfg`, `PgSourceError`, `connect()` |
| `src/source/postgres/version.rs` | `ToolVersion`, `ClusterVersionProbe`, `probe_server_version`, `probe_client_version`, `probe_and_select`, `BundledToolMatrix` |
| `src/source/postgres/catalog_probe.rs` | `list_live_databases`, `capture_extensions`, `capture_encoding` |
| `src/source/postgres/plan.rs` | `BackupSource::plan()` 구현(globals/db 파트 조립, 경고 생성) |
| `src/source/postgres/dump.rs` | `BackupSource::spawn()` 구현, `render_db_flags`/`render_globals_flags`, `drain_stderr_tail` |
| `tests/postgres_source_version.rs` | T2~T7(버전 프로브/매트릭스) |
| `tests/postgres_source_catalog.rs` | T8~T12(DB목록/확장/인코딩, CI PG 컨테이너 필요) |
| `tests/postgres_source_plan.rs` | T1, T9, T10(plan 통합) |
| `tests/postgres_source_spawn.rs` | T13~T18(spawn/stderr드레인/flags 렌더링) |
| `tools/pg/pg13..pg17/{pg_dump,pg_dumpall}[.exe]` | 번들 바이너리 매트릭스(배포 산출물, 리포 밖 or LFS — S11 정책과 정합) |

---
`#tags: backupsource, postgressource, pg-dump, pg-dumpall, globals, version-check, extensions, tokio-process, tokio-postgres, rust, section-s03`
