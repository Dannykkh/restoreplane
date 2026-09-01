// crates/restoreplane-dest-s3/src/admin.rs
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

use std::time::Duration;

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;
use zeroize::Zeroizing;

use crate::{ImmutabilityKind, S3Cfg, S3Error};

type HmacSha256 = Hmac<Sha256>;

const S3_HTTP_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
const S3_HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(8);
const MAX_S3_HTTP_RESPONSE_BODY_BYTES: usize = 64 * 1024;

/// 자가테스트 관리 클라이언트(writer 자격증명으로 SigV4 서명).
pub struct S3Admin {
    client: reqwest::Client,
    access_key_id: Zeroizing<String>,
    secret_access_key: Zeroizing<String>,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObjectLockStatus {
    Unlocked,
    LegalHold,
    RetainedUntil(i64),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObjectLockProbeError {
    pub message: String,
    pub retryable: bool,
}

impl S3Admin {
    pub fn new(cfg: &S3Cfg) -> Self {
        Self::from_credentials(
            &cfg.endpoint,
            &cfg.region,
            &cfg.bucket,
            &cfg.access_key_id,
            &cfg.secret_access_key,
            cfg.min_dwell_days,
        )
    }

    pub(crate) fn from_credentials(
        endpoint: &str,
        region: &str,
        bucket: &str,
        access_key_id: &str,
        secret_access_key: &str,
        min_dwell_days: u32,
    ) -> Self {
        let endpoint = if endpoint.is_empty() {
            format!("https://s3.{region}.amazonaws.com")
        } else {
            endpoint.trim_end_matches('/').to_string()
        };
        S3Admin {
            client: bounded_s3_http_client(),
            access_key_id: Zeroizing::new(access_key_id.to_owned()),
            secret_access_key: Zeroizing::new(secret_access_key.to_owned()),
            region: region.to_owned(),
            bucket: bucket.to_owned(),
            endpoint,
            min_dwell_days,
        }
    }

    /// 매 run 자가테스트(R7). 읽기 단언(versioning/락) + deny-check(5개 파괴동작) + enforce.
    pub async fn run_self_test(&self, cfg: &S3Cfg) -> Result<SelfTestReport, S3Error> {
        let probe = format!(
            "{}/.selftest/{}",
            cfg.prefix.trim_end_matches('/'),
            uuid::Uuid::new_v4()
        );

        // (A) 읽기 단언: versioning ON + Object Lock 기본 보존(M4/H2).
        let versioning_on = self.get_versioning_enabled().await?;
        let (lock_mode, default_retention_days) = self.get_object_lock().await?;

        // (B) deny-check: 403이면 DENIED(정상), 그 외(2xx/400/404)면 과권한.
        let denied_delete = self
            .expect_denied(Method::Delete, &format!("/{}", probe), "", b"")
            .await?;
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
            warnings.push(
                "versioning이 ON이 아님 — Object Lock가 아무 것도 보호 못 함(M4)".to_string(),
            );
        }
        match default_retention_days {
            None => warnings.push(
                "기본 보존(default retention) 미설정 — 새 객체가 즉시 삭제 가능(H2)".to_string(),
            ),
            Some(d) if d < self.min_dwell_days => warnings.push(format!(
                "기본 보존 {d}일 < dwell {}일(M4)",
                self.min_dwell_days
            )),
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
    async fn expect_denied(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Result<bool, S3Error> {
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
            Err(HttpErr::Status(s)) => {
                Err(S3Error::SelfTest(format!("GetBucketVersioning HTTP {s}")))
            }
        }
    }

    async fn get_object_lock(&self) -> Result<(Option<ImmutabilityKind>, Option<u32>), S3Error> {
        match self
            .send_signed(Method::Get, "/", "object-lock=", b"")
            .await
        {
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

    pub(crate) async fn inspect_object_lock(
        &self,
        object_key: &str,
        version: &str,
    ) -> Result<ObjectLockStatus, ObjectLockProbeError> {
        let path = format!("/{object_key}");
        let encoded_version = uri_encode(version, true);
        let legal_hold_query = format!("legal-hold=&versionId={encoded_version}");
        match self
            .send_signed(Method::Get, &path, &legal_hold_query, b"")
            .await
        {
            Ok((_, body)) if body.contains("<Status>ON</Status>") => {
                return Ok(ObjectLockStatus::LegalHold);
            }
            Ok(_) | Err(HttpErr::Status(404)) => {}
            Err(error) => return Err(object_probe_error("GetObjectLegalHold", error)),
        }

        let retention_query = format!("retention=&versionId={encoded_version}");
        match self
            .send_signed(Method::Get, &path, &retention_query, b"")
            .await
        {
            Ok((_, body)) => {
                let Some(retain_until_ms) =
                    parse_retain_until_ms(&body).map_err(|message| ObjectLockProbeError {
                        message,
                        retryable: false,
                    })?
                else {
                    return Ok(ObjectLockStatus::Unlocked);
                };
                let now_ms = OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000;
                if i128::from(retain_until_ms) > now_ms {
                    Ok(ObjectLockStatus::RetainedUntil(retain_until_ms))
                } else {
                    Ok(ObjectLockStatus::Unlocked)
                }
            }
            Err(HttpErr::Status(404)) => Ok(ObjectLockStatus::Unlocked),
            Err(error) => Err(object_probe_error("GetObjectRetention", error)),
        }
    }

    /// SigV4(AWS4-HMAC-SHA256) path-style 서명 요청 전송. (status, body)를 반환.
    async fn send_signed(
        &self,
        method: Method,
        path: &str,
        query: &str,
        body: &[u8],
    ) -> Result<(u16, String), HttpErr> {
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
        let datestamp = format!(
            "{:04}{:02}{:02}",
            now.year(),
            u8::from(now.month()),
            now.day()
        );

        // path-style: canonical URI = /{bucket}{path}
        let canonical_uri = uri_encode(&format!("/{}{}", self.bucket, path), false);
        let host = host_of(&self.endpoint);
        let payload_hash = hex::encode(Sha256::digest(body));

        // canonical headers (host, x-amz-content-sha256, x-amz-date) — 정렬됨.
        let canonical_headers =
            format!("host:{host}\nx-amz-content-sha256:{payload_hash}\nx-amz-date:{amz_date}\n");
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
            self.access_key_id.as_str()
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

        let mut resp = req.send().await.map_err(http_transport_error)?;
        let status = resp.status().as_u16();
        if resp
            .content_length()
            .is_some_and(|length| length > MAX_S3_HTTP_RESPONSE_BODY_BYTES as u64)
        {
            return Err(HttpErr::Transport(
                "S3_HTTP_RESPONSE_BODY_TOO_LARGE".to_owned(),
            ));
        }
        let mut response_body = Vec::with_capacity(
            resp.content_length()
                .unwrap_or(0)
                .min(MAX_S3_HTTP_RESPONSE_BODY_BYTES as u64) as usize,
        );
        while let Some(chunk) = resp.chunk().await.map_err(http_transport_error)? {
            append_bounded_response_chunk(&mut response_body, &chunk)?;
        }
        let text = String::from_utf8_lossy(&response_body).into_owned();
        if (200..300).contains(&status) {
            Ok((status, text))
        } else {
            // 상태코드를 그대로 실어 호출자가 403/기타를 분류하게 한다.
            let _ = text;
            Err(HttpErr::Status(status))
        }
    }

    fn signing_key(&self, datestamp: &str) -> Vec<u8> {
        let k_date = hmac(
            format!("AWS4{}", self.secret_access_key.as_str()).as_bytes(),
            datestamp.as_bytes(),
        );
        let k_region = hmac(&k_date, self.region.as_bytes());
        let k_service = hmac(&k_region, b"s3");
        hmac(&k_service, b"aws4_request")
    }
}

fn bounded_s3_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(S3_HTTP_CONNECT_TIMEOUT)
        .timeout(S3_HTTP_REQUEST_TIMEOUT)
        .build()
        .expect("static bounded S3 HTTP client configuration")
}

fn http_transport_error(error: reqwest::Error) -> HttpErr {
    let message = if error.is_timeout() && error.is_connect() {
        "S3_HTTP_CONNECT_TIMEOUT".to_owned()
    } else if error.is_timeout() {
        "S3_HTTP_REQUEST_TIMEOUT".to_owned()
    } else {
        error.to_string()
    };
    HttpErr::Transport(message)
}

fn append_bounded_response_chunk(body: &mut Vec<u8>, chunk: &[u8]) -> Result<(), HttpErr> {
    let next_len = body
        .len()
        .checked_add(chunk.len())
        .ok_or_else(|| HttpErr::Transport("S3_HTTP_RESPONSE_BODY_TOO_LARGE".to_owned()))?;
    if next_len > MAX_S3_HTTP_RESPONSE_BODY_BYTES {
        return Err(HttpErr::Transport(
            "S3_HTTP_RESPONSE_BODY_TOO_LARGE".to_owned(),
        ));
    }
    body.extend_from_slice(chunk);
    Ok(())
}

fn object_probe_error(operation: &str, error: HttpErr) -> ObjectLockProbeError {
    match error {
        HttpErr::Transport(message) => ObjectLockProbeError {
            message: format!("{operation} transport failed: {message}"),
            retryable: true,
        },
        HttpErr::Status(status) => ObjectLockProbeError {
            message: format!("{operation} returned HTTP {status}"),
            retryable: status == 408 || status == 429 || (500..=599).contains(&status),
        },
    }
}

fn parse_retain_until_ms(body: &str) -> Result<Option<i64>, String> {
    let Some(value) = extract_xml_value(body, "RetainUntilDate") else {
        return Ok(None);
    };
    let parsed = OffsetDateTime::parse(value, &Rfc3339)
        .map_err(|error| format!("invalid GetObjectRetention date: {error}"))?;
    let millis = parsed.unix_timestamp_nanos() / 1_000_000;
    i64::try_from(millis)
        .map(Some)
        .map_err(|_| "GetObjectRetention date is outside the supported range".to_owned())
}

fn extract_xml_value<'a>(body: &'a str, field: &str) -> Option<&'a str> {
    let start_tag = format!("<{field}>");
    let end_tag = format!("</{field}>");
    let start = body.find(&start_tag)? + start_tag.len();
    let end = body[start..].find(&end_tag)? + start;
    Some(body[start..end].trim())
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
        assert_eq!(
            host_of("https://s3.us-east-1.amazonaws.com"),
            "s3.us-east-1.amazonaws.com"
        );
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

    #[test]
    fn retention_admin_http_has_finite_connect_and_request_bounds() {
        assert!(S3_HTTP_CONNECT_TIMEOUT > Duration::ZERO);
        assert!(S3_HTTP_REQUEST_TIMEOUT > S3_HTTP_CONNECT_TIMEOUT);
    }

    #[test]
    fn response_body_accumulator_rejects_a_chunk_before_exceeding_its_allocation_bound() {
        let mut body = vec![b'a'; MAX_S3_HTTP_RESPONSE_BODY_BYTES];

        let error = append_bounded_response_chunk(&mut body, b"b")
            .expect_err("response above the retention probe cap");

        assert!(matches!(
            error,
            HttpErr::Transport(message) if message == "S3_HTTP_RESPONSE_BODY_TOO_LARGE"
        ));
        assert_eq!(body.len(), MAX_S3_HTTP_RESPONSE_BODY_BYTES);
    }
}
