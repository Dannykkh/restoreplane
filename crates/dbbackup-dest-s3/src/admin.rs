// crates/dbbackup-dest-s3/src/admin.rs
//! S06 §6 — 자가테스트(deny-check + 락 상태 단언). 이 크레이트의 하중(H2 해소).
//!
//! opendal이 관리 API를 노출하지 않으므로, **동일 writer 자격증명**으로 SigV4 서명한 관리 요청을
//! reqwest(rustls)로 직접 보낸다. 파괴 없이 권한만 판별하기 위해 "오류코드 분류" 기법을 쓴다:
//!   - DENIED(정상) ⟺ HTTP 403 / AccessDenied.
//!   - 과권한(위험) ⟺ 2xx 또는 인증 통과 후 검증 실패(400 MalformedXML / 404 NoSuchKey 등).
//!     즉 403이 아닌 결과는 "이 신원이 그 동작을 할 수 있다"는 신호.
//!
//! probe key는 존재하지 않는 키, 본문은 고의 malformed → 허용돼도 파괴 부작용 0(S06 §6 "파괴 0").
//!
//! 편차: aws-sigv4 크레이트(빌더 API churn) 대신 hmac-sha256 수동 SigV4. path-style 주소만 사용
//! (호환 provider 정합). XML 파싱은 경량 substring(호환 provider 필드 차이에 관대, R13).

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::OffsetDateTime;

use crate::{ImmutabilityKind, S3Cfg, S3Error};

type HmacSha256 = Hmac<Sha256>;

/// 자가테스트 관리 클라이언트(writer 자격증명으로 SigV4 서명).
pub struct S3Admin {
    client: reqwest::Client,
    access_key_id: String,
    secret_access_key: String,
    region: String,
    bucket: String,
    endpoint: String, // path-style 기준 base URL(스킴+호스트[:포트]). 비면 AWS 기본.
    min_dwell_days: u32,
}

/// 자가테스트 결과 영수증(→ S08 artifacts/로그). 시크릿은 담지 않는다(M6).
#[derive(Clone, Debug)]
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
    pub over_privileged: Vec<String>, // 403이 아니었던(=할 수 있는) 동작들
    pub warnings: Vec<String>,
}

enum HttpErr {
    Status(u16),
    Transport(String),
}

impl S3Admin {
    pub fn new(cfg: &S3Cfg) -> Self {
        let endpoint = if cfg.endpoint.is_empty() {
            format!("https://s3.{}.amazonaws.com", cfg.region)
        } else {
            cfg.endpoint.trim_end_matches('/').to_string()
        };
        S3Admin {
            client: reqwest::Client::new(),
            access_key_id: cfg.access_key_id.clone(),
            secret_access_key: cfg.secret_access_key.clone(),
            region: cfg.region.clone(),
            bucket: cfg.bucket.clone(),
            endpoint,
            min_dwell_days: cfg.min_dwell_days,
        }
    }

    /// 매 run 자가테스트(R7). 읽기 단언(versioning/락) + deny-check(5개 파괴동작) + enforce.
    pub async fn run_self_test(&self, cfg: &S3Cfg) -> Result<SelfTestReport, S3Error> {
        let probe = format!("{}/.selftest/{}", cfg.prefix.trim_end_matches('/'), uuid::Uuid::new_v4());

        // (A) 읽기 단언: versioning ON + Object Lock 기본 보존(M4/H2).
        let versioning_on = self.get_versioning_enabled().await?;
        let (lock_mode, default_retention_days) = self.get_object_lock().await?;

        // (B) deny-check: 403이면 DENIED(정상), 그 외(2xx/400/404)면 과권한.
        let denied_delete = self.expect_denied(Method::Delete, &format!("/{}", probe), "", b"").await?;
        let denied_put_retention = self
            .expect_denied(Method::Put, &format!("/{}", probe), "retention=", b"<x/>")
            .await?;
        let denied_put_lifecycle = self
            .expect_denied(Method::Put, "/", "lifecycle=", b"<x/>")
            .await?;
        let denied_put_versioning = self
            .expect_denied(Method::Put, "/", "versioning=", b"<x/>")
            .await?;
        let denied_legal_hold = self
            .expect_denied(Method::Put, &format!("/{}", probe), "legal-hold=", b"<x/>")
            .await?;

        let mut over = Vec::new();
        if !denied_delete {
            over.push("DeleteObject".to_string());
        }
        if !denied_put_retention {
            over.push("PutObjectRetention".to_string());
        }
        if !denied_put_lifecycle {
            over.push("PutLifecycleConfiguration".to_string());
        }
        if !denied_put_versioning {
            over.push("PutBucketVersioning".to_string());
        }
        if !denied_legal_hold {
            over.push("PutObjectLegalHold".to_string());
        }

        let mut warnings = Vec::new();
        if !versioning_on {
            warnings.push("versioning이 ON이 아님 — Object Lock가 아무 것도 보호 못 함(M4)".to_string());
        }
        match default_retention_days {
            None => warnings.push("기본 보존(default retention) 미설정 — 새 객체가 즉시 삭제 가능(H2)".to_string()),
            Some(d) if d < self.min_dwell_days => {
                warnings.push(format!("기본 보존 {d}일 < dwell {}일(M4)", self.min_dwell_days))
            }
            _ => {}
        }

        let report = SelfTestReport {
            dest: self.bucket.clone(),
            versioning_on,
            default_retention_days,
            lock_mode,
            denied_delete,
            denied_put_retention,
            denied_put_lifecycle,
            denied_put_versioning,
            denied_legal_hold,
            over_privileged: over,
            warnings,
        };
        Self::enforce(&report)?;
        Ok(report)
    }

    /// 정책 강제(S06 §6 enforce): 과권한이면 실행 거부, 락 미비면 실행 거부, 그 외 경고.
    fn enforce(r: &SelfTestReport) -> Result<(), S3Error> {
        if !r.over_privileged.is_empty() {
            return Err(S3Error::OverPrivileged(r.over_privileged.clone()));
        }
        if !r.versioning_on || r.default_retention_days.is_none() {
            return Err(S3Error::BucketNotLocked(r.warnings.clone()));
        }
        for w in &r.warnings {
            tracing::warn!(dest = %r.dest, "{w}");
        }
        Ok(())
    }

    /// 403 → true(DENIED). 2xx/400/404 등 비-403 → false(=할 수 있음 = 과권한).
    async fn expect_denied(&self, method: Method, path: &str, query: &str, body: &[u8]) -> Result<bool, S3Error> {
        match self.send_signed(method, path, query, body).await {
            Ok((status, _)) => Ok(status == 403),
            Err(HttpErr::Status(403)) => Ok(true),
            Err(HttpErr::Status(_)) => Ok(false),
            Err(HttpErr::Transport(e)) => Err(S3Error::SelfTestTransport(e)),
        }
    }

    async fn get_versioning_enabled(&self) -> Result<bool, S3Error> {
        match self.send_signed(Method::Get, "/", "versioning=", b"").await {
            Ok((_, body)) => Ok(body.contains("<Status>Enabled</Status>")),
            Err(HttpErr::Transport(e)) => Err(S3Error::SelfTestTransport(e)),
            Err(HttpErr::Status(s)) => Err(S3Error::SelfTest(format!("GetBucketVersioning HTTP {s}"))),
        }
    }

    async fn get_object_lock(&self) -> Result<(Option<ImmutabilityKind>, Option<u32>), S3Error> {
        match self.send_signed(Method::Get, "/", "object-lock=", b"").await {
            Ok((_, body)) => {
                let mode = if body.contains("<Mode>COMPLIANCE</Mode>") {
                    Some(ImmutabilityKind::ObjectLockCompliance)
                } else if body.contains("<Mode>GOVERNANCE</Mode>") {
                    Some(ImmutabilityKind::ObjectLockGovernance)
                } else {
                    None
                };
                let days = extract_days(&body);
                Ok((mode, days))
            }
            Err(HttpErr::Transport(e)) => Err(S3Error::SelfTestTransport(e)),
            // 락 미설정 버킷은 404(ObjectLockConfigurationNotFoundError) 등 → None으로 해석.
            Err(HttpErr::Status(_)) => Ok((None, None)),
        }
    }

    /// SigV4(AWS4-HMAC-SHA256) path-style 서명 요청 전송. (status, body)를 반환.
    async fn send_signed(&self, method: Method, path: &str, query: &str, body: &[u8]) -> Result<(u16, String), HttpErr> {
        let now = OffsetDateTime::now_utc();
        let amz_date = format!(
            "{:04}{:02}{:02}T{:02}{:02}{:02}Z",
            now.year(),
            u8::from(now.month()),
            now.day(),
            now.hour(),
            now.minute(),
            now.second()
        );
        let datestamp = format!("{:04}{:02}{:02}", now.year(), u8::from(now.month()), now.day());

        // path-style: canonical URI = /{bucket}{path}
        let canonical_uri = uri_encode(&format!("/{}{}", self.bucket, path), false);
        let host = host_of(&self.endpoint);
        let payload_hash = hex::encode(Sha256::digest(body));

        // canonical headers (host, x-amz-content-sha256, x-amz-date) — 정렬됨.
        let canonical_headers = format!(
            "host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n"
        );
        let signed_headers = "host;x-amz-content-sha256;x-amz-date";

        let canonical_request = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            method.as_str(),
            canonical_uri,
            query, // 이미 정렬된 단일 파라미터(빈 값) 형태
            canonical_headers,
            signed_headers,
            payload_hash
        );

        let scope = format!("{datestamp}/{}/s3/aws4_request", self.region);
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{amz_date}\n{scope}\n{}",
            hex::encode(Sha256::digest(canonical_request.as_bytes()))
        );

        let signing_key = self.signing_key(&datestamp);
        let signature = hex::encode(hmac(&signing_key, string_to_sign.as_bytes()));

        let authorization = format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed_headers}, Signature={signature}",
            self.access_key_id
        );

        let url = if query.is_empty() {
            format!("{}/{}{}", self.endpoint, self.bucket, path)
        } else {
            format!("{}/{}{}?{}", self.endpoint, self.bucket, path, query)
        };

        let req = self
            .client
            .request(method.reqwest(), &url)
            .header("host", host)
            .header("x-amz-date", &amz_date)
            .header("x-amz-content-sha256", &payload_hash)
            .header("authorization", authorization)
            .body(body.to_vec());

        let resp = req.send().await.map_err(|e| HttpErr::Transport(e.to_string()))?;
        let status = resp.status().as_u16();
        let text = resp.text().await.unwrap_or_default();
        if (200..300).contains(&status) {
            Ok((status, text))
        } else {
            // 상태코드를 그대로 실어 호출자가 403/기타를 분류하게 한다.
            let _ = text;
            Err(HttpErr::Status(status))
        }
    }

    fn signing_key(&self, datestamp: &str) -> Vec<u8> {
        let k_date = hmac(format!("AWS4{}", self.secret_access_key).as_bytes(), datestamp.as_bytes());
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        hmac(&k_service, b"aws4_request")
    }
}

#[derive(Clone, Copy)]
enum Method {
    Get,
    Put,
    Delete,
}

impl Method {
    fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
        }
    }
    fn reqwest(self) -> reqwest::Method {
        match self {
            Method::Get => reqwest::Method::GET,
            Method::Put => reqwest::Method::PUT,
            Method::Delete => reqwest::Method::DELETE,
        }
    }
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

/// SigV4 canonical URI 인코딩. unreserved 문자 외 percent-encode. encode_slash=false면 '/'는 보존.
fn uri_encode(s: &str, encode_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let unreserved = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~');
        if unreserved || (b == b'/' && !encode_slash) {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

fn host_of(endpoint: &str) -> String {
    endpoint
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_end_matches('/')
        .to_string()
}

/// <Days>N</Days>에서 N을 추출(Object Lock 기본 보존일수).
fn extract_days(xml: &str) -> Option<u32> {
    let start = xml.find("<Days>")? + "<Days>".len();
    let end = xml[start..].find("</Days>")? + start;
    xml[start..end].trim().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_encode_preserves_slash_and_encodes_special() {
        assert_eq!(uri_encode("/bucket/a b", false), "/bucket/a%20b");
        assert_eq!(uri_encode("/a/b", true), "%2Fa%2Fb");
    }

    #[test]
    fn extract_days_parses_default_retention() {
        let xml = "<ObjectLockConfiguration><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>30</Days></DefaultRetention></Rule></ObjectLockConfiguration>";
        assert_eq!(extract_days(xml), Some(30));
        assert_eq!(extract_days("<none/>"), None);
    }

    #[test]
    fn host_of_strips_scheme() {
        assert_eq!(host_of("https://s3.us-east-1.amazonaws.com"), "s3.us-east-1.amazonaws.com");
        assert_eq!(host_of("http://127.0.0.1:9000/"), "127.0.0.1:9000");
    }

    #[test]
    fn signing_key_is_deterministic() {
        let cfg = S3Cfg {
            endpoint: "http://127.0.0.1:9000".into(),
            region: "us-east-1".into(),
            bucket: "b".into(),
            prefix: "p".into(),
            access_key_id: "AKIA".into(),
            secret_access_key: "secret".into(),
            object_lock_mode: ImmutabilityKind::ObjectLockGovernance,
            retention_days: 30,
            min_dwell_days: 7,
            checksum_algo: None,
            compliance_opt_in: false,
            provider: crate::S3Provider::MinIO,
        };
        let admin = S3Admin::new(&cfg);
        assert_eq!(admin.signing_key("20260716"), admin.signing_key("20260716"));
    }
}
