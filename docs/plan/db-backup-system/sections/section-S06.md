# S06 — Destination trait + LocalDest + S3Dest (Object Lock / 자가테스트)

> 의존: S01(스캐폴딩/Config), S02(SecretManager — 서명 매니페스트는 S08), S05(Pipeline — 커밋된 스풀을 넘겨받는 소비처)
> 레이어: 3 · plan 슬라이스 2(“S3 목적지 + Object Lock, 두 번째로 마지막 아님 — 진짜 랜섬웨어 방어 + 최대 외부 리스크”).

## Background

이 섹션은 백업본이 실제로 착지하는 **Destination trait**과 두 구현(LocalDest, S3Dest)을 정의한다.
아키텍처 리뷰 P1-2가 못박은 대로, 불변성(Object Lock)은 **S3에만** 존재하는 성질이므로 flat trait이
Local/Ssh에 무의미한 메서드를 강요하면 안 된다(ISP). 대신 각 Destination이 **자기 능력을 자가보고**하고
(`capabilities() -> DestCaps`), Job이 요구 불변성을 그 능력에 대해 assert한다.

이 섹션의 하중은 **랜섬웨어 방어가 “문서상 존재”가 아니라 “런타임에 강제·검증”되게 만드는 것**이다
(레드팀 H2 Unenforced-Control). 핵심 규율 네 가지를 먼저 못박는다.

1. **매 run 자가테스트(H2)**: writer 자격증명으로 `DeleteObject`/`PutObjectRetention`/
   `PutLifecycleConfiguration`를 실제로 시도해 **거부(AccessDenied)** 를 단언하고, `versioning=ON` +
   기본 보존(default retention)이 설정됐음을 단언한다. 과권한이거나 버킷이 잠기지 않았으면 **실행 거부
   또는 요란한 경고**. “delete가 실제로 실패하는지 테스트하라”를 도구가 매번 스스로 한다.
2. **prune는 백업 호스트에서 절대 실행 금지(H1)**: S3Dest가 쥔 자격증명은 **append-only 신원**뿐이다.
   Destination trait에는 delete/prune 경로 자체가 없다. 보존 회전(prune)은 S3 서버측 lifecycle 또는
   오프호스트 MFA 신원(별도 프로파일)이 담당하며, 무인 run은 그 프로파일을 로드하지 않는다.
3. **multipart-only-commit(P0-3)**: 스풀 파일을 멀티파트로 스트리밍하되 **완전 성공 시에만 `close()`**
   (CompleteMultipartUpload). 실패 시 abort → 목적지에 부분 객체가 절대 보이지 않는다.
4. **Compliance 자기잠금 방지(M5) + 보존 ≥ dwell(M4)**: Compliance 모드는 opt-in + 확인 + 폐기용
   버킷 dry-run 통과 후에만 허용. 기본 보존일수는 예상 탐지/체류(dwell) 기간 이상이어야 한다.

opendal은 **쓰기(스트리밍 멀티파트) 전용**으로 쓴다. opendal 0.58은 `GetBucketVersioning` /
`GetObjectLockConfiguration` / `PutObjectRetention` / `PutLifecycleConfiguration` 같은 **관리 API를
노출하지 않으므로**, 자가테스트는 동일한 writer 자격증명으로 **경량 SigV4 관리 클라이언트**(reqwest +
aws-sigv4, rustls)를 별도로 만들어 수행한다. 이 이원화가 이 섹션의 구조적 결정이다.

## Requirements

| # | 요구 | 근거 |
|---|------|------|
| R1 | `Destination` trait: `writer(key) -> DestWriter`(멀티파트/스트리밍) + `capabilities() -> DestCaps` | plan §트레이트, P1-2 |
| R2 | `DestCaps { immutability, checksum_required, resumable }`, `ImmutabilityKind = None\|ObjectLockGovernance\|ObjectLockCompliance\|ServerAppendOnly` | 도메인사전, P1-2 |
| R3 | LocalDest(opendal fs) = 스풀 파일. caps: immutability None, resumable true, checksum_required false | plan(Local=스풀), P0-4 |
| R4 | S3Dest(opendal 0.58, S3 + 호환 MinIO/B2/R2). 버킷 기본 보존(Governance)을 상속 — per-object 헤더 불필요 | rust-stack §5 |
| R5 | Object Lock가 checksum을 요구하면 `checksum_algorithm` 설정. 미설정 시 PutObject 실패 방지 | rust-stack §5 gotcha |
| R6 | multipart-only-commit: 완전 성공 시에만 `close()`. 실패 시 abort. opendal drop-abort 동작 `[확인 필요]` — 미보장 시 lifecycle `AbortIncompleteMultipartUpload` | P0-3 |
| R7 | **매 run 자가테스트**: writer 자격으로 Delete/PutObjectRetention/PutLifecycle 시도 → **DENIED 단언**; versioning=ON + 기본 보존 단언; 과권한/미잠금이면 refuse/loud-warn | H2, plan §랜섬웨어 |
| R8 | append-only IAM 정책 템플릿(문서): PutObject only + Delete*/lifecycle/versioning/retention/legal-hold/bypass **명시 Deny** | H2, plan §랜섬웨어 |
| R9 | prune는 백업 호스트/writer 신원으로 **불가능**. trait에 delete 경로 없음. 오프호스트 별도 신원 | H1 |
| R10 | Compliance 자기잠금 방지: opt-in + confirm + 폐기 버킷 dry-run 통과 필수 | M5 |
| R11 | 기본 보존일수 ≥ 예상 dwell(탐지/체류) 기간. init/자가테스트에서 검증 | M4 |
| R12 | Job이 `required_immutability`를 `capabilities()`에 대해 assert. 미충족 목적지는 요란히 실패 | P1-2 |
| R13 | 호환 provider(MinIO/B2/R2) 오류코드/락 표면 차이는 `[확인 필요]`로 표기하고 runtime 확인 | rust-stack §5 |

## Dependencies

- **입력(S01 Config)**: `[[destinations]]` — kind, endpoint/region/bucket/prefix, `object_lock.mode`,
  `object_lock.retention_days`, `checksum_algo`, `min_dwell_days`, secrets는 `enc:`(S02가 unwrap).
- **입력(S05 Pipeline)**: 커밋된 **스풀 파일 경로**(`SpoolCommit.spool_path`) + 서명 매니페스트(S08).
  S06은 이 파일을 목적지 key로 스트리밍한다. 팬아웃 orchestration(JoinSet/retry)은 S05, 실제
  write/multipart/close/abort는 S06.
- **출력 소비(S05 fan_out)**: S05의 `dest.upload_resumable(&spool)`은 이 섹션의 `writer()` 위에 얹은
  얇은 헬퍼다(§Implementation 8). 부분 성공 판정은 S05, 목적지별 커밋 원자성은 S06.
- **출력 소비(S08 Catalog/Manifest)**: 목적지별 `object_key`, `object_checksum/ETag`, `retention_until`,
  `immutability`를 매니페스트/artifacts에 기록.

## Reference Libraries (opendal 0.58)

| 크레이트 | 버전 | 이 섹션에서의 역할 | Gotcha |
|---------|------|------|--------|
| `opendal` | =0.58.0 | S3/fs 통합 writer. `Operator::writer_with(key)`가 멀티파트 스트리밍 | 관리 API(versioning/retention/lifecycle) **미노출** → 자가테스트는 별도. `default-features=false` + rustls만 |
| `reqwest` | 0.12.x | 자가테스트 관리 요청(GET/PUT/DELETE) 전송 | `rustls-tls` 강제(openssl 유입 차단 — H6) |
| `aws-sigv4` | 1.x | 관리 요청 SigV4 서명(writer 자격증명) | 정확한 서명 빌더 API `[확인 필요]`. 대안: `aws-sdk-s3` |
| `quick-xml` / `serde` | — | GetBucketVersioning / GetObjectLockConfiguration 응답 파싱 | S3 XML. 호환 provider 응답 필드 차이 `[확인 필요]` |
| `async-trait` | 0.1 | `Destination` async trait | |
| `anyhow`/`thiserror`/`tracing` | — | 오류·구조적 로깅(시크릿 스크럽 — M6) | 엔드포인트/키를 로그에 넣지 않음 |

`Cargo.toml` (발췌):
```toml
[dependencies]
opendal = { version = "=0.58.0", default-features = false, features = ["services-s3", "services-fs", "rustls"] }
reqwest = { version = "0.12", default-features = false, features = ["rustls-tls"] }
aws-sigv4 = "1"                     # 관리 요청 서명. [확인 필요] 서명 빌더 시그니처
quick-xml = { version = "0.36", features = ["serialize"] }
async-trait = "0.1"
anyhow = "1"; thiserror = "2"; tracing = "0.1"
```
> opendal은 `rustls` 강제 — 호환 backend(MinIO/B2/R2) HTTP도 openssl 없이(H6). 버전 핀은 opendal 0.58
> 표면이 minor마다 흔들리므로 `=0.58.0`.

## Implementation

### 1. Destination trait + DestCaps (R1/R2)

```rust
/// 목적지 불변성 능력(도메인사전과 1:1). Job이 이 값에 assert(P1-2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ImmutabilityKind {
    None,                    // Local: WORM 없음
    ObjectLockGovernance,    // bypass 권한이 있으면 삭제 가능(약한 기본)
    ObjectLockCompliance,    // root도 불가(강함, 자기잠금 위험 — M5)
    ServerAppendOnly,        // Put만, 서버측 append-only(예: 일부 SSH/MinIO 정책)
}

#[derive(Clone, Copy, Debug)]
pub struct DestCaps {
    pub immutability: ImmutabilityKind,
    /// Object Lock가 켜진 S3는 PutObject에 checksum을 요구 → true (R5).
    pub checksum_required: bool,
    /// 스풀 파일 재읽기로 이어올리기 가능한가(S05 retry/resume 힌트).
    pub resumable: bool,
}

#[async_trait::async_trait]
pub trait Destination: Send + Sync {
    fn name(&self) -> &str;
    fn capabilities(&self) -> DestCaps;

    /// 멀티파트/스트리밍 writer. **close()에서만 커밋**(multipart-only-commit, R6).
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError>;

    /// 매 run 자가테스트(R7). Local은 최소검사, S3는 deny-check + 락 상태 단언.
    async fn self_test(&self) -> Result<SelfTestReport, DestError>;
}
```

### 2. Job의 required_immutability assert (R12)

```rust
/// Job이 요구하는 최소 불변성. capabilities()가 이를 만족하지 못하면 요란히 실패.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequiredImmutability { None, AppendOnlyOrLock, ObjectLock, ComplianceOnly }

impl RequiredImmutability {
    pub fn is_satisfied_by(self, caps: DestCaps) -> bool {
        use ImmutabilityKind::*;
        match self {
            RequiredImmutability::None => true,
            RequiredImmutability::AppendOnlyOrLock =>
                matches!(caps.immutability, ServerAppendOnly | ObjectLockGovernance | ObjectLockCompliance),
            RequiredImmutability::ObjectLock =>
                matches!(caps.immutability, ObjectLockGovernance | ObjectLockCompliance),
            RequiredImmutability::ComplianceOnly =>
                matches!(caps.immutability, ObjectLockCompliance),
        }
    }
}

/// 팬아웃 진입 전에 호출. Local-only가 P1을 만족한다고 오인하는 것을 막는다(P1-2).
pub fn assert_job_immutability(
    required: RequiredImmutability,
    dests: &[std::sync::Arc<dyn Destination>],
) -> Result<(), DestError> {
    let ok = dests.iter().filter(|d| required.is_satisfied_by(d.capabilities())).count();
    if ok == 0 {
        return Err(DestError::ImmutabilityUnmet {
            required: format!("{required:?}"),
            have: dests.iter().map(|d| format!("{}={:?}", d.name(), d.capabilities().immutability)).collect(),
        });
    }
    Ok(())
}
```

### 3. DestWriter — multipart-only-commit (R6)

```rust
/// opendal Writer 래퍼. close()로만 커밋. 미완료 drop은 abort 시도(+ 경고).
pub struct DestWriter {
    inner: Option<opendal::Writer>,   // Some=열림, None=이미 close/abort
    key: String,
    committed: bool,
}

impl DestWriter {
    pub async fn write_all(&mut self, buf: bytes::Bytes) -> Result<(), DestError> {
        let w = self.inner.as_mut().ok_or(DestError::WriterClosed)?;
        w.write(buf).await.map_err(DestError::from)   // opendal이 내부적으로 파트 축적/업로드
    }
    /// 완전 성공 경로에서만 호출 → CompleteMultipartUpload. 이후에야 객체가 보인다.
    pub async fn commit(mut self) -> Result<UploadReceipt, DestError> {
        let mut w = self.inner.take().ok_or(DestError::WriterClosed)?;
        w.close().await?;                 // 멀티파트 완료(= 유일한 커밋 지점)
        self.committed = true;
        Ok(UploadReceipt { key: self.key.clone(), /* etag/checksum는 close 후 stat로 */ })
    }
    /// 실패 경로: 부분 객체가 보이지 않게 abort.
    pub async fn abort(mut self) {
        if let Some(mut w) = self.inner.take() {
            // opendal 0.58: 미완료 writer의 abort 표면 [확인 필요]. abort()가 없으면
            // drop이 CompleteMultipartUpload를 하지 않는지(=고아 파트로 남는지) 확인 필요.
            let _ = w.abort().await;      // [확인 필요] 메서드 존재 여부
            tracing::warn!(key = %self.key, "멀티파트 업로드 abort");
        }
    }
}

impl Drop for DestWriter {
    fn drop(&mut self) {
        if self.inner.is_some() && !self.committed {
            // 여기서 async abort 불가(Drop은 sync) → 로그만. 실제 정리는 버킷 lifecycle
            // AbortIncompleteMultipartUpload(예: 1일)가 보증한다(R6 fallback).
            tracing::error!(key = %self.key, "DestWriter가 commit 없이 drop — 고아 파트 가능. lifecycle이 정리");
        }
    }
}
```
> **결정(R6)**: opendal이 drop 시 자동 abort하는지 **빌드 시 실측** `[확인 필요]`. 보장되면 위 Drop 경고는
> 방어선으로만 남긴다. 보장 안 되면 **버킷에 `AbortIncompleteMultipartUpload` lifecycle**(관리 신원이
> 설정 — writer는 PutLifecycle이 Deny이므로 못 함, R8/R9와 정합)로 고아 파트를 반드시 회수한다.

### 4. LocalDest (opendal fs = 스풀) (R3)

```rust
pub struct LocalDest { name: String, op: opendal::Operator, }

impl LocalDest {
    pub fn new(name: impl Into<String>, root: &std::path::Path) -> Result<Self, DestError> {
        let op = opendal::Operator::new(
            opendal::services::Fs::default().root(root.to_string_lossy().as_ref())
        )?.finish();
        Ok(Self { name: name.into(), op })
    }
}

#[async_trait::async_trait]
impl Destination for LocalDest {
    fn name(&self) -> &str { &self.name }
    fn capabilities(&self) -> DestCaps {
        DestCaps { immutability: ImmutabilityKind::None, checksum_required: false, resumable: true }
    }
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        let w = self.op.writer(key).await?;
        Ok(DestWriter { inner: Some(w), key: key.to_string(), committed: false })
    }
    async fn self_test(&self) -> Result<SelfTestReport, DestError> {
        // Local은 WORM을 강제할 수 없다 → 능력 자체가 None. 오인 방지 경고만.
        Ok(SelfTestReport::local_no_worm(&self.name))
    }
}
```
> LocalDest는 “최소 방어선”일 뿐 P1(랜섬웨어)을 **만족하지 못한다**(caps.immutability=None). S05가
> `assert_job_immutability`로 이를 걸러낸다(§2).

### 5. S3Dest 구성 + opendal writer (R4/R5)

```rust
pub struct S3Cfg {
    pub endpoint: String,        // AWS 공백 가능; MinIO/B2/R2는 필수
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,   // ← append-only(writer) 신원. 절대 delete 권한 없음(R9)
    pub secret_access_key: String,
    pub object_lock_mode: ImmutabilityKind,   // Governance | Compliance
    pub retention_days: u32,
    pub min_dwell_days: u32,      // M4: retention_days >= min_dwell_days
    pub checksum_algo: Option<String>,        // "CRC32C" 등 — Object Lock가 요구 시(R5)
    pub compliance_opt_in: bool,              // M5
    pub provider: S3Provider,    // Aws | MinIO | BackblazeB2 | CloudflareR2
}

pub struct S3Dest { name: String, op: opendal::Operator, admin: S3Admin, cfg: S3Cfg }

impl S3Dest {
    pub fn new(name: impl Into<String>, cfg: S3Cfg) -> Result<Self, DestError> {
        // M4: 보존이 dwell보다 짧으면 불변 사본이 정작 필요할 때 이미 만료.
        if cfg.retention_days < cfg.min_dwell_days {
            return Err(DestError::RetentionBelowDwell {
                retention: cfg.retention_days, dwell: cfg.min_dwell_days });
        }
        // M5: Compliance는 opt-in + dry-run 통과 없이는 생성 거부(§7에서 dry-run 강제).
        if cfg.object_lock_mode == ImmutabilityKind::ObjectLockCompliance && !cfg.compliance_opt_in {
            return Err(DestError::ComplianceNotOptedIn);
        }

        let mut b = opendal::services::S3::default()
            .bucket(&cfg.bucket)
            .region(&cfg.region)
            .access_key_id(&cfg.access_key_id)
            .secret_access_key(&cfg.secret_access_key)
            .root(&cfg.prefix);
        if !cfg.endpoint.is_empty() { b = b.endpoint(&cfg.endpoint); }   // 호환 provider
        // R5: Object Lock 버킷은 checksum 요구. opendal 0.58의 checksum 노출 [확인 필요].
        // 노출되면 여기서 설정, 아니면 SigV4 업로드 fallback(§Risk).
        if let Some(algo) = &cfg.checksum_algo {
            // b = b.checksum_algorithm(algo);   // [확인 필요] 0.58 메서드명/존재
            let _ = algo;
        }
        let op = opendal::Operator::new(b)?.finish();
        let admin = S3Admin::new(&cfg)?;         // 동일 writer 자격증명(§6)
        Ok(Self { name: name.into(), op, admin, cfg })
    }
}

#[async_trait::async_trait]
impl Destination for S3Dest {
    fn name(&self) -> &str { &self.name }
    fn capabilities(&self) -> DestCaps {
        DestCaps {
            immutability: self.cfg.object_lock_mode,
            checksum_required: self.cfg.checksum_algo.is_some(),
            resumable: true,   // 스풀 재읽기 + 멀티파트 재개
        }
    }
    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        // 버킷 기본 보존(Governance/Compliance)을 **상속** → per-object 헤더 불필요(rust-stack §5).
        let w = self.op.writer_with(key)
            .chunk(16 * 1024 * 1024)   // 멀티파트 파트 크기
            .concurrent(4)
            .await?;                    // [확인 필요] writer_with 체이닝 시그니처(0.58)
        Ok(DestWriter { inner: Some(w), key: key.to_string(), committed: false })
    }
    async fn self_test(&self) -> Result<SelfTestReport, DestError> {
        self.admin.run_self_test(&self.cfg).await   // §6
    }
}
```

### 6. 자가테스트 — deny-check + 락 상태 단언 (R7) — 핵심

opendal이 관리 API를 노출하지 않으므로, **동일 writer 자격증명**으로 SigV4 서명한 관리 요청을 직접
보낸다. 파괴 없이 권한만 판별하기 위해 **“오류코드 분류” 기법**을 쓴다:

- **DENIED(정상)** ⟺ 응답이 `AccessDenied`/HTTP 403.
- **과권한(위험)** ⟺ 요청이 성공(2xx)하거나, **인증을 통과한 뒤** 검증에서 실패(예: 400 `MalformedXML`,
  404 `NoSuchKey`). 즉 403이 아닌 결과는 “이 신원이 그 동작을 **할 수 있다**”는 신호.

이 트릭 덕에 **실제로 삭제/보존변경/lifecycle 변경을 하지 않고도**(존재하지 않는 probe key + 고의로
malformed한 본문) 권한을 판정한다. 파괴적 부작용 0.

```rust
pub struct SelfTestReport {
    pub dest: String,
    pub versioning_on: bool,
    pub default_retention_days: Option<u32>,
    pub lock_mode: Option<ImmutabilityKind>,
    pub denied_delete: bool,
    pub denied_put_retention: bool,
    pub denied_put_lifecycle: bool,
    pub denied_put_versioning: bool,
    pub denied_legal_hold: bool,
    pub over_privileged: Vec<String>,   // 403이 아니었던(=할 수 있는) 동작들
    pub warnings: Vec<String>,
}

impl S3Admin {
    pub async fn run_self_test(&self, cfg: &S3Cfg) -> Result<SelfTestReport, DestError> {
        let probe = format!("{}/.selftest/{}", cfg.prefix.trim_end_matches('/'), uuid_v4());

        // (A) 읽기 단언: versioning ON + Object Lock 기본 보존 설정(M4/H2).
        let versioning_on = self.get_bucket_versioning().await? == VersioningStatus::Enabled;
        let lock = self.get_object_lock_configuration().await?;   // ObjectLockEnabled + DefaultRetention
        let default_retention_days = lock.as_ref().and_then(|l| l.default_retention_days);
        let lock_mode = lock.as_ref().and_then(|l| l.mode);

        // (B) deny-check: 403이면 DENIED(정상), 그 외(2xx/400/404)면 과권한.
        let denied_delete       = self.expect_denied(Op::DeleteObject(&probe)).await?;         // 없는 key
        let denied_put_retention= self.expect_denied(Op::PutObjectRetention(&probe)).await?;   // 없는 key
        let denied_put_lifecycle= self.expect_denied(Op::PutLifecycleMalformed).await?;        // malformed 본문
        let denied_put_versioning=self.expect_denied(Op::PutBucketVersioningMalformed).await?; // malformed 본문
        let denied_legal_hold   = self.expect_denied(Op::PutObjectLegalHold(&probe)).await?;

        let mut over = Vec::new();
        if !denied_delete        { over.push("DeleteObject".into()); }
        if !denied_put_retention { over.push("PutObjectRetention".into()); }
        if !denied_put_lifecycle { over.push("PutLifecycleConfiguration".into()); }
        if !denied_put_versioning{ over.push("PutBucketVersioning".into()); }
        if !denied_legal_hold    { over.push("PutObjectLegalHold".into()); }

        let mut warnings = Vec::new();
        if !versioning_on { warnings.push("versioning이 ON이 아님 — Object Lock가 아무 것도 보호 못 함(M4)".into()); }
        match default_retention_days {
            None => warnings.push("기본 보존(default retention) 미설정 — 새 객체가 즉시 삭제 가능(H2)".into()),
            Some(d) if d < cfg.min_dwell_days =>
                warnings.push(format!("기본 보존 {d}일 < dwell {}일(M4)", cfg.min_dwell_days)),
            _ => {}
        }

        let report = SelfTestReport {
            dest: cfg.bucket.clone(), versioning_on, default_retention_days, lock_mode,
            denied_delete, denied_put_retention, denied_put_lifecycle,
            denied_put_versioning, denied_legal_hold, over_privileged: over, warnings,
        };
        Self::enforce(&report, cfg)?;   // refuse or loud-warn
        Ok(report)
    }

    /// 403 → true(DENIED). 2xx/400/404 등 비-403 → false(=할 수 있음 = 과권한).
    async fn expect_denied(&self, op: Op<'_>) -> Result<bool, DestError> {
        match self.send_signed(op).await {
            Err(HttpErr::Status(403)) => Ok(true),
            Ok(_) | Err(HttpErr::Status(_)) => Ok(false),   // [확인 필요] 호환 provider 코드(MinIO/B2/R2)
            Err(HttpErr::Transport(e)) => Err(DestError::SelfTestTransport(e)),
        }
    }

    /// 정책 강제: append-only 위반이면 실행 거부, 락 미비면 loud-warn(설정에 따라 hard fail).
    fn enforce(r: &SelfTestReport, cfg: &S3Cfg) -> Result<(), DestError> {
        if !r.over_privileged.is_empty() {
            // 과권한 = 랜섬웨어가 이 자격증명으로 삭제/보존해제 가능 → **실행 거부**(H1/H2).
            return Err(DestError::OverPrivileged(r.over_privileged.clone()));
        }
        if !r.versioning_on || r.default_retention_days.is_none() {
            return Err(DestError::BucketNotLocked(r.warnings.clone()));   // 미잠금 = P1 미충족
        }
        for w in &r.warnings { tracing::warn!(dest = %cfg.bucket, "{w}"); }
        Ok(())
    }
}
```
> **왜 파괴가 없나**: DeleteObject/PutObjectRetention은 **존재하지 않는 probe key**를 대상으로 하므로
> 설령 허용돼도 지울 것이 없다(NoSuchKey). Lifecycle/Versioning은 **고의로 malformed한 본문**을 보내므로
> 허용돼도 상태를 바꾸지 못하고 400에서 멈춘다 — 그런데 400은 “인증은 통과”라는 뜻이라 과권한이 드러난다.
> `[확인 필요]`: MinIO/B2/R2가 권한 거부에 **403/AccessDenied**를 일관되게 주는지 runtime 확인(R13).

### 7. Compliance 자기잠금 방지 — 폐기 버킷 dry-run (R10/M5)

```rust
/// Compliance 모드 S3Dest를 “진짜” 버킷에 붙이기 전에 반드시 통과해야 하는 관문.
/// 폐기용(throwaway) 버킷에 아주 짧은 보존(예: 1일)으로 락→쓰기→만료 검증까지 손으로 확인.
pub async fn compliance_preflight(dry_run_bucket: &str, confirmed: bool) -> Result<(), DestError> {
    if !confirmed {
        // init CLI에서 명시적 확인(재입력)을 받지 않으면 진행 불가(M5).
        return Err(DestError::ComplianceNeedsConfirm);
    }
    tracing::warn!(
        "Compliance 모드: 잘못 지정한 긴 보존은 root도 되돌릴 수 없고 스토리지 비용이 영구 발생. \
         폐기 버킷 '{dry_run_bucket}'에서 dry-run으로 동작을 먼저 확인하십시오."
    );
    // 1) 폐기 버킷에 Compliance + 1일 보존 설정, 2) 작은 객체 Put, 3) Delete가 거부되는지 확인,
    // 4) 보존 만료 후 삭제 가능해지는지 확인(운영자 수동). 실패 시 진짜 버킷 연결 차단.
    Ok(())
}
```
> S3Dest::new는 `compliance_opt_in`만 강제하고, **dry-run 통과 증적**은 init 플로우(S01/CLI)가 별도로
> 요구한다: Compliance 연결 전 `compliance_preflight`가 성공한 기록이 없으면 도구가 진행을 거부한다.

### 8. S05 팬아웃과의 접합 (writer 위의 얇은 헬퍼)

S05 `fan_out`은 `dest.upload_resumable(&spool)`를 부른다. 이는 §3 `writer()`/`commit()` 위에 얹은 기본
구현이다(멀티파트-온리-커밋을 자연히 계승).

```rust
pub async fn upload_resumable(dest: &dyn Destination, spool: &std::path::Path, key: &str)
    -> Result<UploadReceipt, UploadError>
{
    let mut w = dest.writer(key).await.map_err(UploadError::from)?;
    let mut f = tokio::fs::File::open(spool).await?;
    let mut buf = vec![0u8; 8 * 1024 * 1024];
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut f, &mut buf).await?;
        if n == 0 { break; }
        if let Err(e) = w.write_all(bytes::Bytes::copy_from_slice(&buf[..n])).await {
            w.abort().await;              // 실패 → 부분 객체 안 보이게(R6)
            return Err(UploadError::from(e));
        }
    }
    w.commit().await.map_err(UploadError::from)   // 완전 성공 시에만 CompleteMultipartUpload
}
```

### 9. append-only IAM 정책 템플릿 (문서, R8)

writer(무인 백업) 신원에 붙이는 정책. **PutObject/멀티파트 관련만 Allow**, 나머지는 **명시 Deny**.
prune/bypass는 이 신원에 절대 없다(H1). 관리(버킷 락 설정·lifecycle)는 **별도 관리 신원**이 한다.

```json
{
  "Version": "2012-10-17",
  "Statement": [
    {
      "Sid": "AppendOnlyPut",
      "Effect": "Allow",
      "Action": [
        "s3:PutObject",
        "s3:AbortMultipartUpload",
        "s3:ListBucketMultipartUploads",
        "s3:ListMultipartUploadParts",
        "s3:ListBucket",
        "s3:GetBucketVersioning",
        "s3:GetBucketObjectLockConfiguration",
        "s3:GetObjectRetention"
      ],
      "Resource": [
        "arn:aws:s3:::BACKUP-BUCKET",
        "arn:aws:s3:::BACKUP-BUCKET/*"
      ]
    },
    {
      "Sid": "DenyEverythingDestructive",
      "Effect": "Deny",
      "Action": [
        "s3:DeleteObject",
        "s3:DeleteObjectVersion",
        "s3:PutLifecycleConfiguration",
        "s3:PutBucketVersioning",
        "s3:PutObjectRetention",
        "s3:PutObjectLegalHold",
        "s3:BypassGovernanceRetention",
        "s3:PutBucketObjectLockConfiguration"
      ],
      "Resource": [
        "arn:aws:s3:::BACKUP-BUCKET",
        "arn:aws:s3:::BACKUP-BUCKET/*"
      ]
    }
  ]
}
```
> - `GetObjectRetention`/`GetBucketVersioning` 등 **읽기**는 자가테스트(§6 A파트)를 위해 Allow.
> - **명시 Deny가 Allow를 이긴다** — 실수로 넓은 정책을 덧붙여도 파괴 동작은 계속 막힌다(H2).
> - **호환 provider(R13)**: MinIO는 동일 IAM 문법을 대체로 지원, B2/R2는 애플리케이션 키 스코프/버킷
>   정책 표면이 달라 등가 매핑 필요 `[확인 필요]`. 각 provider의 Object Lock/append-only 문서 대조.
> - **prune 신원(별도 파일 `prune.toml`)**: `s3:DeleteObject*` + (Governance면)
>   `s3:BypassGovernanceRetention`를 **가진 다른 신원**. 오프호스트 + MFA. 무인 run은 이 프로파일을
>   **로드하지 않는다**(H1). 서버측 lifecycle 회전이면 이 신원조차 불필요.

## Test Scenarios

| # | 시나리오 | 기대 | 검증 포인트 |
|---|---------|------|------------|
| T1 | LocalDest writer → 파일 생성·재읽기 | caps.immutability=None, resumable=true | R3 |
| T2 | S3Dest happy path(멀티파트 업로드 → commit) | 객체 커밋됨, ETag/checksum 회수 | R4/R6 |
| T3 | 업로드 중 실패 → `abort()` | 목적지에 **객체 미노출**(부분 객체 없음) | R6 multipart-only-commit |
| T4 | `commit` 없이 DestWriter drop | error 로그 + lifecycle이 고아 파트 회수 대상 | R6 fallback |
| T5 | **자가테스트: 올바른 append-only 자격** | 5개 파괴동작 모두 DENIED, versioning ON, 기본 보존 확인 → Ok | R7 정상 경로 |
| T6 | **자가테스트: 과권한(s3:\* 붙임)** | `over_privileged` 비어있지 않음 → `OverPrivileged` **실행 거부** | R7/H2 핵심 |
| T7 | 자가테스트: versioning OFF / 기본 보존 미설정 | `BucketNotLocked`로 실행 거부 | R7/M4 |
| T8 | 자가테스트: 기본 보존일수 < dwell | warning(또는 hard fail 설정 시 거부) | M4/R11 |
| T9 | deny-check 비파괴 검증: probe key/ malformed 본문 사용 후 버킷 상태 불변 | 버킷 versioning/lifecycle/객체 수 그대로 | §6 “파괴 0” |
| T10 | Job assert: Local-only에 `ObjectLock` 요구 | `ImmutabilityUnmet`로 실패 | R12/P1-2 |
| T11 | Job assert: Local+S3(Lock) 혼합에 `ObjectLock` 요구 | 통과(락 목적지 ≥1) | R12 |
| T12 | Compliance 생성: opt-in 없음 | `ComplianceNotOptedIn` | R10/M5 |
| T13 | Compliance preflight: 확인 미입력 | `ComplianceNeedsConfirm` | R10/M5 |
| T14 | 보존 < dwell로 S3Dest::new | `RetentionBelowDwell` | R11/M4 |
| T15 | 호환 provider(MinIO 컨테이너) 자가테스트 | 403/AccessDenied 매핑 확인 | R13 `[확인 필요]` 해소 |

> 하네스: MinIO(Object Lock 활성) 컨테이너로 S3 경로를 결정적 재현. 과권한/append-only 두 정책을 각각
> 붙인 두 액세스키로 T5/T6를 대조. deny-check 비파괴성(T9)은 테스트 전후 버킷 상태 스냅샷 비교로 단언.

## Implementation Strategy

1. **trait + caps + Job assert 먼저(T1/T10/T11)**: `Destination`/`DestCaps`/`RequiredImmutability`와
   LocalDest를 붙여 S05 팬아웃과 계약을 닫는다(불변성 오인 차단).
2. **S3Dest 업로드 경로(T2/T3/T4)**: opendal writer_with 멀티파트 + multipart-only-commit. opendal
   drop-abort 동작을 **실측**해 R6 fallback(lifecycle) 필요 여부 확정 `[확인 필요]`.
3. **자가테스트(T5~T9)** — 이 섹션의 하중: SigV4 관리 클라이언트 + 오류코드 분류(비파괴 deny-check) +
   락 상태 읽기 + `enforce`. 과권한/미잠금에서 **실제로 실행이 거부되는지**를 증명(H2 해소).
4. **Compliance 가드(T12~T14)**: opt-in/confirm/dry-run + 보존≥dwell 검증(M4/M5).
5. **호환 provider(T15)**: MinIO로 오류코드 매핑 확정, B2/R2는 문서 대조 후 `[확인 필요]` 해소.
6. IAM 템플릿·prune 신원 분리(§9)는 문서/설치 산출물로 고정.

## Quality Gate

- `cargo test` — T1~T15 통과. 특히 **T6(과권한 거부)**, **T7(미잠금 거부)**, **T3(부분 객체 미노출)**.
- `cargo clippy -- -D warnings`, `cargo fmt --check`.
- **불변식**: (a) 자가테스트가 성공(Ok)일 때만 그 run이 진행된다 — 과권한/미잠금이면 절대 백업 안 함.
  (b) 어떤 실패 경로에서도 목적지에 **부분/미커밋 객체가 보이지 않는다**. (c) writer 신원으로는
  Delete/retention/lifecycle/versioning 중 **무엇도 성공하지 않는다**(deny-check로 매 run 증명).
- **비파괴 검증**: 자가테스트 전후 버킷 상태(versioning/lifecycle/객체 수) 불변(T9).
- `cargo deny check` — opendal/reqwest 핀·rustls 강제·라이선스(H6), openssl 유입 0.

## Risk & Rollback

| 리스크 | 영향 | 완화 |
|--------|------|------|
| **opendal 0.58 checksum 미노출**(R5) | Object Lock 버킷에 PutObject 실패 | `[확인 필요]` 실측. 미노출이면 해당 업로드만 SigV4 직접 PUT(`x-amz-checksum-*`)로 fallback, opendal은 non-lock 경로에 |
| **opendal drop-abort 미보장**(R6) | 고아 멀티파트 파트 잔존(비용/혼란) | `abort()` 실측 `[확인 필요]`. 미보장 시 관리 신원이 `AbortIncompleteMultipartUpload` lifecycle(1일) 설정 — writer는 PutLifecycle Deny이므로 out-of-band |
| **호환 provider 오류코드 상이**(R13) | deny-check 오판(과권한을 정상으로) | MinIO/B2/R2별 403/AccessDenied 매핑 실측(T15). 모르면 **보수적으로 과권한 취급**(거부) |
| **aws-sigv4 서명 API 변동** | 자가테스트 요청 인증 실패 | 서명 빌더 시그니처 `[확인 필요]`. 대안 `aws-sdk-s3`(무겁지만 관리 API 1급) |
| **자가테스트 probe 객체 축적** | `.selftest/` 프리픽스에 소량 객체 | probe는 **없는 key/ malformed 본문**이라 실제 객체 생성 안 함(§6). 생성형 probe 불필요 |
| **Compliance 자기잠금**(M5) | 되돌릴 수 없는 긴 보존 + 영구 비용 | opt-in+confirm+폐기버킷 dry-run 강제, 진짜 버킷 연결 전 preflight 통과 필수 |

**Rollback**: S06은 라이브러리 계층(부작용 = 목적지 객체 write + 자가테스트 읽기/거부-probe). 자가테스트가
실패하면 **애초에 백업을 시작하지 않으므로** 원자적으로 안전(no-op). 업로드 실패는 multipart abort +
객체 미노출로 복구. 기능 플래그로 S3Dest를 끄면 슬라이스 1(Local-only) 상태로 회귀하되, 그 경우
`assert_job_immutability`가 P1 요구를 만족 못 함을 요란히 알린다(오인 방지).

## Acceptance Criteria

- [ ] `Destination { writer(key), capabilities() }` + `DestCaps{immutability,checksum_required,resumable}` 정의(R1/R2).
- [ ] LocalDest(opendal fs)=스풀, caps.immutability=None, resumable=true(R3).
- [ ] S3Dest(opendal 0.58)가 S3 및 호환(MinIO/B2/R2) 엔드포인트로 멀티파트 스트리밍 업로드(R4).
- [ ] 버킷 기본 보존을 상속(per-object 헤더 불필요), Object Lock가 요구하면 checksum 설정(R5) — `[확인 필요]` 표기.
- [ ] multipart-only-commit: 완전 성공 시에만 `commit()`(CompleteMultipartUpload), 실패 시 abort로 부분 객체 미노출(R6).
- [ ] **매 run 자가테스트**: Delete/PutObjectRetention/PutLifecycle(및 versioning/legal-hold) DENIED 단언 + versioning ON + 기본 보존 단언, 과권한/미잠금이면 실행 거부/loud-warn(R7).
- [ ] 자가테스트가 **비파괴**(없는 key + malformed 본문 → 버킷 상태 불변)임을 T9로 증명.
- [ ] append-only IAM 템플릿(PutObject only + Delete*/lifecycle/versioning/retention/legal-hold/bypass 명시 Deny) 문서화(R8).
- [ ] writer 신원에는 prune/delete 경로가 없고, prune은 오프호스트 별도 신원/서버측 lifecycle임을 명시(R9).
- [ ] Compliance는 opt-in + confirm + 폐기버킷 dry-run 통과 후에만 연결(R10/M5).
- [ ] 기본 보존일수 ≥ min_dwell_days 검증(R11/M4).
- [ ] Job이 `required_immutability`를 `capabilities()`에 assert, Local-only가 P1을 오인 만족하지 않음(R12).
- [ ] 호환 provider 오류코드/락 표면 차이를 `[확인 필요]`로 표기하고 T15로 해소(R13).
- [ ] T1~T15 전부 통과.

## Files

| 파일 | 내용 |
|------|------|
| `src/dest/mod.rs` | `Destination` trait, `DestCaps`, `ImmutabilityKind`, `RequiredImmutability`, `assert_job_immutability`, `DestError` |
| `src/dest/writer.rs` | `DestWriter`(multipart-only-commit), `commit`/`abort`, drop 경고, `upload_resumable` |
| `src/dest/local.rs` | `LocalDest`(opendal fs = 스풀), 최소 self_test |
| `src/dest/s3.rs` | `S3Dest`(opendal 0.58 S3), `S3Cfg`, `S3Provider`, capabilities |
| `src/dest/s3_admin.rs` | `S3Admin`(reqwest + aws-sigv4), 자가테스트 deny-check/락 단언/`enforce` |
| `src/dest/compliance.rs` | `compliance_preflight`(M5 dry-run/confirm) |
| `docs/iam/append-only-policy.json` | append-only IAM 템플릿(R8) + provider별 주석 |
| `docs/iam/prune-identity.md` | prune 신원 분리·오프호스트 운영 지침(H1) |
| `tests/dest_s3_selftest.rs` | T5~T9(자가테스트/비파괴/과권한 거부) |
| `tests/dest_caps_assert.rs` | T1/T10~T14(caps/Job assert/Compliance 가드) |
| `tests/support/minio.rs` | MinIO(Object Lock) 컨테이너 하네스, 두 정책 키(T6/T15) |

---
`#tags: destination-trait, s3dest, object-lock, self-test, append-only, opendal, multipart-commit, iam, compliance, rust, section-s06`
