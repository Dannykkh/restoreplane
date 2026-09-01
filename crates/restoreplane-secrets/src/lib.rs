// crates/restoreplane-secrets/src/lib.rs
//! S02 SecretManager — 봉투암호화(DEK/KEK 3경로 wrap), 오프라인 ed25519 서명,
//! 복구코드 생성/검증을 제공한다.
//!
//! 설계 원칙(P0-5): "DEK를 wrap하는 계층"(chacha20poly1305 원샷 AEAD)과
//! "payload를 암호화하는 계층"(DEK 기반 chacha20poly1305 STREAM)을 타입/코드로 분리한다.
//! S02는 payload 데이터를 직접 스트리밍하지 않고 `Dek`와 STREAM primitive만 S05에 넘긴다.

pub mod secret;

pub use secret::dek::Dek;
pub use secret::error::SecretError;
pub use secret::field::DekEncryptor;
pub use secret::kdf::{generate_recovery_code, RecoveryVerifier};
pub use secret::keystore::{Keystore, KEYSTORE_VERSION};
pub use secret::rotate::RotateReport;
pub use secret::sign::{generate_signing_keypair, sign_manifest, verify_manifest};
pub use secret::wrap::{unwrap_dek_with_kek, wrap_dek_with_kek, WrapSet, WrapStanza};
pub use secret::{Credential, SecretManager};
