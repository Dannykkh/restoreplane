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
        Self {
            wrap_gen,
            ciphertext,
        }
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
        let rest = s
            .strip_prefix(Self::PREFIX)
            .ok_or(EncFieldError::MissingPrefix)?;
        let (wrap_gen_str, b64) = rest.split_once(':').ok_or(EncFieldError::Malformed)?;
        let wrap_gen: u32 = wrap_gen_str
            .parse()
            .map_err(EncFieldError::InvalidWrapGen)?;
        let ciphertext = STANDARD.decode(b64).map_err(EncFieldError::InvalidBase64)?;
        Ok(EncField {
            wrap_gen,
            ciphertext,
        })
    }
}

impl fmt::Display for EncField {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}{}:{}",
            Self::PREFIX,
            self.wrap_gen,
            STANDARD.encode(&self.ciphertext)
        )
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
        assert!(matches!(
            "3:aGVsbG8=".parse::<EncField>(),
            Err(EncFieldError::MissingPrefix)
        ));
    }

    #[test]
    fn not_yet_implemented_encryptor_fails_loudly() {
        let e = NotYetImplementedEncryptor;
        assert!(e.wrap("secret").is_err());
    }
}
