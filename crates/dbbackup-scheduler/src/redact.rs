// src/redact.rs — 시크릿 리댁션(FR8, M6). 2중 방어: (a) 명시적 scrub() 호출지점, (b) RedactingWriter.
use regex::Regex;
use std::sync::OnceLock;

fn patterns() -> &'static Vec<(Regex, &'static str)> {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        vec![
            // enc:<wrap_gen>:<base64> 필드(config 시크릿, S01 EncField 와이어 포맷).
            (
                Regex::new(r"enc:\d+:[A-Za-z0-9+/=]+").unwrap(),
                "enc:[REDACTED]",
            ),
            // DSN/연결문자열의 user:pass@host 부분(postgres://, mysql://, s3://, sftp:// 등 공통).
            (
                Regex::new(r"://[^/\s:@]+:[^/\s@]+@").unwrap(),
                "://[REDACTED]:[REDACTED]@",
            ),
            // key=value / key: value 형태의 흔한 시크릿 키워드(대소문자 무관).
            (
                Regex::new(
                    r"(?i)\b(password|passwd|secret|token|dek|kek|recovery[_-]?code|access[_-]?key|api[_-]?key|private[_-]?key)\s*[:=]\s*\S+",
                )
                .unwrap(),
                "$1=[REDACTED]",
            ),
            // schtasks /RP 인자가 실수로 로그 문자열에 섞여 들어온 경우.
            (Regex::new(r"(?i)/RP\s+\S+").unwrap(), "/RP [REDACTED]"),
        ]
    })
}

/// 로그/알림에 실어 보내기 직전 반드시 통과시킨다(FR8). 명시적 호출 지점.
pub fn scrub(input: &str) -> String {
    let mut out = input.to_string();
    for (re, replacement) in patterns() {
        out = re.replace_all(&out, *replacement).to_string();
    }
    out
}

/// 파일 writer를 감싸는 방어적 레이어(2중 방어). 호출부가 scrub()을 빠뜨려도 마지막 관문에서
/// 걸러진다 — 특히 서드파티 크레이트(DB 드라이버/S3 SDK)의 Display 구현이 DSN을 그대로
/// 노출하는 경우를 잡기 위함(M6).
pub struct RedactingWriter<W: std::io::Write> {
    inner: W,
}

impl<W: std::io::Write> RedactingWriter<W> {
    pub fn new(inner: W) -> Self {
        RedactingWriter { inner }
    }
}

impl<W: std::io::Write> std::io::Write for RedactingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let text = String::from_utf8_lossy(buf);
        let scrubbed = scrub(&text);
        self.inner.write_all(scrubbed.as_bytes())?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}
