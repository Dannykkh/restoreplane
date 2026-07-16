// crates/dbbackup-dest-s3/src/lib.rs
//! S06 — S3Dest: opendal 0.58 기반 S3(및 호환 MinIO/B2/R2) 목적지.
//!
//! 이 크레이트의 하중은 "랜섬웨어 방어가 문서상 존재가 아니라 런타임에 강제·검증되게" 하는 것이다
//! (레드팀 H2 Unenforced-Control). 네 가지 규율:
//!   1. 매 run 자가테스트(H2): writer 자격으로 Delete/PutObjectRetention/PutLifecycle를 실제 시도해
//!      거부(AccessDenied)를 단언하고, versioning=ON + 기본 보존이 설정됐음을 단언한다(→ [`admin`]).
//!   2. prune 금지(H1): S3Dest가 쥔 자격은 append-only뿐 — trait에 delete 경로 자체가 없다.
//!   3. multipart-only-commit(P0-3): 완전 성공 시에만 opendal `close()`. 실패 시 `abort()`.
//!   4. Compliance 자기잠금 방지(M5) + 보존 >= dwell(M4).
//!
//! 섹션 대비 편차(API 현실):
//! - opendal 0.58 `Writer::write`는 `impl Into<Buffer>`를 받는다(섹션의 `bytes::Bytes` 시그니처와 정합).
//!   `Writer::abort()`가 **존재**하므로(docs.rs 실측) 섹션의 `[확인 필요]`는 해소 — abort 경로에서 명시 호출.
//! - opendal 0.58 S3 빌더에 `checksum_algorithm(&str)`가 존재(실측) → SigV4 fallback 불필요, R5 해소.
//! - 자가테스트 SigV4는 aws-sigv4 크레이트(빌더 API churn) 대신 hmac-sha256 수동 서명으로 구현 —
//!   의존 표면 축소 + 결정성. XML 파싱은 경량 substring(호환 provider 필드 차이에 관대).

pub mod admin;

use async_trait::async_trait;
use dbbackup_pipeline::{
    DestCaps, DestError, DestWriter, Destination, ImmutabilityKind, UploadReceipt, WriterBackend,
};

pub use admin::{S3Admin, SelfTestReport};

/// append-only(무인 백업) writer 신원에 붙이는 IAM 정책 템플릿(R8).
/// PutObject/멀티파트 관련만 Allow, 파괴 동작(Delete*/lifecycle/versioning/retention/legal-hold/bypass)은
/// **명시 Deny**. 명시 Deny가 Allow를 이기므로 실수로 넓은 정책을 덧붙여도 파괴 동작은 계속 막힌다(H2).
/// prune는 이 신원에 절대 없다(H1) — 별도 오프호스트 신원/서버측 lifecycle 소관.
/// `BACKUP-BUCKET`을 실제 버킷명으로 치환해 사용. 호환 provider(MinIO/B2/R2)는 등가 매핑 필요(R13).
pub const APPEND_ONLY_IAM_POLICY: &str = r#"{
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
}"#;

/// S3 호환 provider. 오류코드/락 표면 차이는 runtime 확인 대상(R13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum S3Provider {
    Aws,
    MinIO,
    BackblazeB2,
    CloudflareR2,
}

#[derive(Debug, thiserror::Error)]
pub enum S3Error {
    #[error("opendal 오류: {0}")]
    Opendal(#[from] opendal::Error),
    #[error("보존일수({retention})가 dwell({dwell})보다 짧음 — 불변 사본이 필요할 때 이미 만료(M4)")]
    RetentionBelowDwell { retention: u32, dwell: u32 },
    #[error("Compliance 모드는 opt-in 없이 생성 불가(M5)")]
    ComplianceNotOptedIn,
    #[error("Compliance preflight는 명시적 확인(confirm) 없이 진행 불가(M5)")]
    ComplianceNeedsConfirm,
    #[error("과권한 신원 — 이 자격증명으로 다음 파괴 동작이 가능함(랜섬웨어 위험, 실행 거부): {0:?}")]
    OverPrivileged(Vec<String>),
    #[error("버킷이 잠기지 않음(versioning/기본보존 미비, P1 미충족, 실행 거부): {0:?}")]
    BucketNotLocked(Vec<String>),
    #[error("자가테스트 전송 오류: {0}")]
    SelfTestTransport(String),
    #[error("자가테스트 서명/구성 오류: {0}")]
    SelfTest(String),
}

/// S3Dest 구성. access/secret은 **append-only(writer) 신원** — delete 권한 없음(R9).
#[derive(Clone, Debug)]
pub struct S3Cfg {
    pub endpoint: String, // AWS는 공백 가능, MinIO/B2/R2는 필수
    pub region: String,
    pub bucket: String,
    pub prefix: String,
    pub access_key_id: String,
    pub secret_access_key: String,
    pub object_lock_mode: ImmutabilityKind, // ObjectLockGovernance | ObjectLockCompliance | None
    pub retention_days: u32,
    pub min_dwell_days: u32,
    pub checksum_algo: Option<String>, // "crc32c" 등 — Object Lock가 요구 시(R5)
    pub compliance_opt_in: bool,        // M5
    pub provider: S3Provider,
}

/// prefix + key를 단일 슬래시로 합쳐 S3 object key를 만든다(중복/선행 슬래시 정규화).
pub fn object_key(prefix: &str, key: &str) -> String {
    let p = prefix.trim_matches('/');
    let k = key.trim_start_matches('/');
    if p.is_empty() {
        k.to_string()
    } else {
        format!("{p}/{k}")
    }
}

pub struct S3Dest {
    name: String,
    op: opendal::Operator,
    cfg: S3Cfg,
}

impl S3Dest {
    /// M4/M5 가드를 통과해야만 생성된다. Compliance는 opt-in 필수(dry-run 통과 증적은 init 플로우 소관).
    pub fn new(name: impl Into<String>, cfg: S3Cfg) -> Result<Self, S3Error> {
        // M4: 보존이 dwell보다 짧으면 불변 사본이 정작 필요할 때 이미 만료.
        if cfg.retention_days < cfg.min_dwell_days {
            return Err(S3Error::RetentionBelowDwell {
                retention: cfg.retention_days,
                dwell: cfg.min_dwell_days,
            });
        }
        // M5: Compliance는 opt-in 없이 생성 거부.
        if cfg.object_lock_mode == ImmutabilityKind::ObjectLockCompliance && !cfg.compliance_opt_in {
            return Err(S3Error::ComplianceNotOptedIn);
        }

        let mut builder = opendal::services::S3::default()
            .bucket(&cfg.bucket)
            .region(&cfg.region)
            .access_key_id(&cfg.access_key_id)
            .secret_access_key(&cfg.secret_access_key);
        if !cfg.endpoint.is_empty() {
            builder = builder.endpoint(&cfg.endpoint); // 호환 provider
        }
        // R5: Object Lock 버킷은 PutObject에 checksum을 요구. opendal 0.58에 checksum_algorithm 존재(실측).
        if let Some(algo) = &cfg.checksum_algo {
            builder = builder.checksum_algorithm(algo);
        }
        // opendal 0.58: Operator::new(builder)?는 Operator를 직접 반환(구버전의 .finish() 불필요 — 실측).
        let op = opendal::Operator::new(builder)?;
        Ok(Self { name: name.into(), op, cfg })
    }

    /// 매 run 자가테스트(R7). 동일 writer 자격증명으로 SigV4 관리 요청을 보내 deny-check + 락 상태 단언.
    /// Ok를 돌려줄 때만 그 run이 진행돼야 한다(과권한/미잠금이면 Err로 실행 거부).
    pub async fn self_test(&self) -> Result<SelfTestReport, S3Error> {
        let admin = S3Admin::new(&self.cfg);
        admin.run_self_test(&self.cfg).await
    }

    pub fn config(&self) -> &S3Cfg {
        &self.cfg
    }
}

#[async_trait]
impl Destination for S3Dest {
    fn name(&self) -> &str {
        &self.name
    }

    fn capabilities(&self) -> DestCaps {
        DestCaps {
            immutability: self.cfg.object_lock_mode,
            checksum_required: self.cfg.checksum_algo.is_some(),
            resumable: true, // 스풀 재읽기 + 멀티파트 재개
        }
    }

    async fn writer(&self, key: &str) -> Result<DestWriter, DestError> {
        let full_key = object_key(&self.cfg.prefix, key);
        // 버킷 기본 보존(Governance/Compliance)을 상속 → per-object retention 헤더 불필요(rust-stack §5).
        let w = self
            .op
            .writer_with(&full_key)
            .chunk(16 * 1024 * 1024) // 멀티파트 파트 크기
            .concurrent(4)
            .await
            .map_err(|e| DestError::backend(&full_key, e))?;
        Ok(DestWriter::from_backend(Box::new(S3Backend {
            writer: Some(w),
            key: full_key,
            bytes: 0,
        })))
    }
}

/// S3Dest의 WriterBackend. opendal 멀티파트 writer를 감싸 multipart-only-commit을 구현(R6).
struct S3Backend {
    writer: Option<opendal::Writer>,
    key: String,
    bytes: u64,
}

#[async_trait]
impl WriterBackend for S3Backend {
    async fn write_all(&mut self, buf: &[u8]) -> Result<(), DestError> {
        let w = self.writer.as_mut().ok_or(DestError::WriterClosed)?;
        // opendal이 내부적으로 파트 축적/업로드. Buffer는 Vec<u8>에서 변환.
        w.write(buf.to_vec())
            .await
            .map_err(|e| DestError::backend(&self.key, e))?;
        self.bytes += buf.len() as u64;
        Ok(())
    }

    /// 완전 성공 경로에서만 호출 → CompleteMultipartUpload(유일한 커밋 지점).
    async fn commit(mut self: Box<Self>) -> Result<UploadReceipt, DestError> {
        let mut w = self.writer.take().ok_or(DestError::WriterClosed)?;
        w.close()
            .await
            .map_err(|e| DestError::backend(&self.key, e))?;
        Ok(UploadReceipt { key: self.key.clone(), bytes: self.bytes })
    }

    /// 실패 경로: 멀티파트 abort → 부분 객체가 절대 보이지 않음. abort 실패 시 lifecycle이 회수(R6 fallback).
    async fn abort(mut self: Box<Self>) {
        if let Some(mut w) = self.writer.take() {
            if let Err(e) = w.abort().await {
                tracing::warn!(key = %self.key, "S3 멀티파트 abort 실패(고아 파트 가능, lifecycle이 회수): {e}");
            }
        }
    }

    fn key_for_log(&self) -> String {
        self.key.clone()
    }
}

impl Drop for S3Backend {
    fn drop(&mut self) {
        if self.writer.is_some() {
            // async abort 불가(Drop은 sync) → 로그만. 고아 파트는 버킷 lifecycle
            // AbortIncompleteMultipartUpload(관리 신원 설정, writer는 PutLifecycle Deny)가 회수(R6).
            tracing::error!(key = %self.key, "S3Backend가 commit/abort 없이 drop — 고아 파트 가능(lifecycle이 정리)");
        }
    }
}

/// Compliance 모드를 진짜 버킷에 붙이기 전 반드시 통과해야 하는 관문(R10/M5).
/// 폐기용 버킷에 짧은 보존으로 락→쓰기→만료를 손으로 검증한다. 여기서는 confirm 게이트만 강제하고,
/// 실제 dry-run 절차는 운영자/ init 플로우가 수행한다(잘못된 긴 보존은 root도 되돌릴 수 없음).
pub async fn compliance_preflight(dry_run_bucket: &str, confirmed: bool) -> Result<(), S3Error> {
    if !confirmed {
        return Err(S3Error::ComplianceNeedsConfirm);
    }
    tracing::warn!(
        "Compliance 모드: 잘못 지정한 긴 보존은 root도 되돌릴 수 없고 스토리지 비용이 영구 발생. \
         폐기 버킷 '{dry_run_bucket}'에서 dry-run으로 동작을 먼저 확인하십시오."
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iam_policy_is_well_formed_json_with_explicit_deny() {
        let v: serde_json::Value = serde_json::from_str(APPEND_ONLY_IAM_POLICY).expect("IAM 템플릿은 유효한 JSON이어야 함");
        let stmts = v["Statement"].as_array().expect("Statement 배열");
        // Allow + Deny 두 문장.
        assert_eq!(stmts.len(), 2);
        let deny = stmts
            .iter()
            .find(|s| s["Effect"] == "Deny")
            .expect("명시 Deny 문장이 있어야 함(H2)");
        let actions: Vec<&str> = deny["Action"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        // 파괴 동작들이 Deny에 포함돼야 함.
        for must in [
            "s3:DeleteObject",
            "s3:PutLifecycleConfiguration",
            "s3:PutBucketVersioning",
            "s3:PutObjectRetention",
            "s3:PutObjectLegalHold",
            "s3:BypassGovernanceRetention",
        ] {
            assert!(actions.contains(&must), "Deny에 {must}가 있어야 함");
        }
        // Allow에는 PutObject만(파괴 동작 없음).
        let allow = stmts.iter().find(|s| s["Effect"] == "Allow").unwrap();
        let allow_actions: Vec<&str> = allow["Action"].as_array().unwrap().iter().map(|a| a.as_str().unwrap()).collect();
        assert!(allow_actions.contains(&"s3:PutObject"));
        assert!(!allow_actions.iter().any(|a| a.starts_with("s3:Delete")));
    }

    #[test]
    fn object_key_joins_prefix_and_key() {
        assert_eq!(object_key("backups", "db.spool"), "backups/db.spool");
        assert_eq!(object_key("/backups/", "/db.spool"), "backups/db.spool");
        assert_eq!(object_key("", "db.spool"), "db.spool");
        assert_eq!(object_key("a/b", "c/d.spool"), "a/b/c/d.spool");
    }

    #[test]
    fn new_rejects_retention_below_dwell() {
        let cfg = base_cfg(ImmutabilityKind::ObjectLockGovernance, 3, 7, false);
        assert!(matches!(
            S3Dest::new("s3", cfg).err(),
            Some(S3Error::RetentionBelowDwell { retention: 3, dwell: 7 })
        ));
    }

    #[test]
    fn new_rejects_compliance_without_opt_in() {
        let cfg = base_cfg(ImmutabilityKind::ObjectLockCompliance, 30, 7, false);
        assert!(matches!(S3Dest::new("s3", cfg).err(), Some(S3Error::ComplianceNotOptedIn)));
    }

    #[test]
    fn new_accepts_compliance_with_opt_in() {
        let cfg = base_cfg(ImmutabilityKind::ObjectLockCompliance, 30, 7, true);
        assert!(S3Dest::new("s3", cfg).is_ok());
    }

    #[tokio::test]
    async fn compliance_preflight_requires_confirm() {
        assert!(matches!(
            compliance_preflight("throwaway", false).await,
            Err(S3Error::ComplianceNeedsConfirm)
        ));
        assert!(compliance_preflight("throwaway", true).await.is_ok());
    }

    #[test]
    fn governance_dest_reports_object_lock_capability() {
        let cfg = base_cfg(ImmutabilityKind::ObjectLockGovernance, 30, 7, false);
        let dest = S3Dest::new("s3", cfg).unwrap();
        assert_eq!(dest.capabilities().immutability, ImmutabilityKind::ObjectLockGovernance);
    }

    #[test]
    fn s3_governance_satisfies_object_lock_but_local_does_not() {
        use dbbackup_pipeline::{assert_job_immutability, LocalDest, RequiredImmutability};
        use std::sync::Arc;

        let s3 = Arc::new(S3Dest::new("s3", base_cfg(ImmutabilityKind::ObjectLockGovernance, 30, 7, false)).unwrap())
            as Arc<dyn Destination>;
        let local = Arc::new(LocalDest::new("local", std::env::temp_dir())) as Arc<dyn Destination>;

        // ObjectLock 요구: S3(Governance)는 통과, Local-only는 실패(R12/P1-2).
        assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &[s3.clone()]).is_ok());
        assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &[local.clone()]).is_err());
        // 혼합(Local+S3)이면 락 목적지 >=1이므로 통과.
        assert!(assert_job_immutability(RequiredImmutability::ObjectLock, &[local, s3]).is_ok());
    }

    #[test]
    fn s3_compliance_only_requires_compliance_mode() {
        use dbbackup_pipeline::{assert_job_immutability, RequiredImmutability};
        use std::sync::Arc;

        let gov = Arc::new(S3Dest::new("s3g", base_cfg(ImmutabilityKind::ObjectLockGovernance, 30, 7, false)).unwrap())
            as Arc<dyn Destination>;
        let comp = Arc::new(S3Dest::new("s3c", base_cfg(ImmutabilityKind::ObjectLockCompliance, 30, 7, true)).unwrap())
            as Arc<dyn Destination>;
        // ComplianceOnly는 Governance로는 불충분, Compliance만 통과.
        assert!(assert_job_immutability(RequiredImmutability::ComplianceOnly, &[gov]).is_err());
        assert!(assert_job_immutability(RequiredImmutability::ComplianceOnly, &[comp]).is_ok());
    }

    fn base_cfg(mode: ImmutabilityKind, retention: u32, dwell: u32, opt_in: bool) -> S3Cfg {
        S3Cfg {
            endpoint: "http://127.0.0.1:9000".to_string(),
            region: "us-east-1".to_string(),
            bucket: "backup-bucket".to_string(),
            prefix: "backups".to_string(),
            access_key_id: "AKIA_TEST".to_string(),
            secret_access_key: "secret_test".to_string(),
            object_lock_mode: mode,
            retention_days: retention,
            min_dwell_days: dwell,
            checksum_algo: Some("crc32c".to_string()),
            compliance_opt_in: opt_in,
            provider: S3Provider::MinIO,
        }
    }
}
