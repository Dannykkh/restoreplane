use std::path::PathBuf;

use restoreplane_catalog::{LocalManifestSource, ManifestSource};
use restoreplane_config::{schema::Destination, Config};
use restoreplane_core::DestKind;
use restoreplane_dest_s3::{S3ManifestSource, S3ReaderCfg};
use restoreplane_dest_ssh::{SshAuth, SshManifestSource, SshReaderCfg};
use restoreplane_secrets::DekEncryptor;
use zeroize::Zeroizing;

#[derive(Debug)]
pub enum ManifestSourceSpec {
    Local {
        name: String,
        root: PathBuf,
    },
    S3(Destination),
    Ssh {
        destination: Destination,
        host: String,
        port: u16,
    },
}

impl ManifestSourceSpec {
    pub fn name(&self) -> &str {
        match self {
            Self::Local { name, .. } => name,
            Self::S3(destination) | Self::Ssh { destination, .. } => &destination.name,
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local { .. } => "local",
            Self::S3(_) => "s3",
            Self::Ssh { .. } => "ssh",
        }
    }

    pub fn requires_secret_decryption(&self) -> bool {
        match self {
            Self::Local { .. } => false,
            Self::S3(destination) => destination.reader.is_some(),
            Self::Ssh { destination, .. } => destination.reader.as_ref().is_some_and(|reader| {
                reader.password.is_some() || reader.private_key_passphrase.is_some()
            }),
        }
    }
}

/// Resolve a configured destination name first, then an explicitly existing local directory.
/// Unknown names never fall back to policy.spool_dir.
pub fn select_manifest_source(
    config: &Config,
    selector: &str,
) -> anyhow::Result<ManifestSourceSpec> {
    if let Some(destination) = config.destinations.iter().find(|d| d.name == selector) {
        return match destination.kind {
            DestKind::Local => Ok(ManifestSourceSpec::Local {
                name: destination.name.clone(),
                root: destination.path.clone().ok_or_else(|| {
                    anyhow::anyhow!("local 목적지 '{}'에 path가 없습니다", destination.name)
                })?,
            }),
            DestKind::S3 => Ok(ManifestSourceSpec::S3(destination.clone())),
            DestKind::Ssh => {
                let endpoint = if let Some(host) = &destination.host {
                    Some((host.clone(), destination.port.unwrap_or(22)))
                } else {
                    destination.tunnel.as_ref().and_then(|name| {
                        config
                            .tunnels
                            .iter()
                            .find(|tunnel| &tunnel.name == name)
                            .map(|tunnel| {
                                (tunnel.host.clone(), destination.port.unwrap_or(tunnel.port))
                            })
                    })
                };
                let (host, port) = endpoint.ok_or_else(|| {
                    anyhow::anyhow!("ssh 목적지 '{}'에 host가 없습니다", destination.name)
                })?;
                Ok(ManifestSourceSpec::Ssh {
                    destination: destination.clone(),
                    host,
                    port,
                })
            }
        };
    }

    let path = PathBuf::from(selector);
    if path.is_dir() {
        return Ok(ManifestSourceSpec::Local {
            name: selector.to_string(),
            root: path,
        });
    }

    anyhow::bail!(
        "알 수 없는 목적지 '{selector}'입니다. config의 destination 이름 또는 존재하는 로컬 디렉터리를 지정하세요"
    )
}

pub fn build_manifest_source(
    spec: &ManifestSourceSpec,
    field_enc: Option<&DekEncryptor>,
) -> anyhow::Result<Box<dyn ManifestSource>> {
    match spec {
        ManifestSourceSpec::Local { root, .. } => {
            Ok(Box::new(LocalManifestSource::new(root.clone())))
        }
        ManifestSourceSpec::S3(destination) => {
            let reader = destination.reader.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "s3 목적지 '{}'에 [destination.reader] 읽기 신원이 없습니다; writer 신원으로 폴백하지 않습니다",
                    destination.name
                )
            })?;
            let field_enc = require_field_encryptor(field_enc, &destination.name)?;
            let access_key_id = decrypt_reader_secret(
                field_enc,
                reader.access_key_id.as_ref(),
                &destination.name,
                "access_key_id",
            )?;
            let secret_access_key = decrypt_reader_secret(
                field_enc,
                reader.secret_access_key.as_ref(),
                &destination.name,
                "secret_access_key",
            )?;
            let source = S3ManifestSource::new(S3ReaderCfg {
                endpoint: destination.endpoint.clone().unwrap_or_default(),
                region: destination
                    .region
                    .clone()
                    .unwrap_or_else(|| "us-east-1".to_string()),
                bucket: destination.bucket.clone().ok_or_else(|| {
                    anyhow::anyhow!("s3 목적지 '{}'에 bucket이 없습니다", destination.name)
                })?,
                prefix: destination.prefix.clone(),
                access_key_id: access_key_id.to_string(),
                secret_access_key: secret_access_key.to_string(),
            })?;
            Ok(Box::new(source))
        }
        ManifestSourceSpec::Ssh {
            destination,
            host,
            port,
        } => {
            let reader = destination.reader.as_ref().ok_or_else(|| {
                anyhow::anyhow!(
                    "ssh 목적지 '{}'에 [destination.reader] 읽기 신원이 없습니다; writer 신원으로 폴백하지 않습니다",
                    destination.name
                )
            })?;
            let user = reader.user.clone().ok_or_else(|| {
                anyhow::anyhow!("ssh 목적지 '{}'.reader에 user가 없습니다", destination.name)
            })?;
            let auth = if let Some(key_path) = &reader.private_key_path {
                let pem = std::fs::read_to_string(key_path).map_err(|e| {
                    anyhow::anyhow!("SSH reader 개인키 읽기 실패({}): {e}", key_path.display())
                })?;
                let passphrase = match reader.private_key_passphrase.as_ref() {
                    Some(field) => Some(
                        require_field_encryptor(field_enc, &destination.name)?
                            .unwrap_field(field)
                            .map_err(|e| {
                                anyhow::anyhow!(
                                    "ssh 목적지 '{}'.reader private_key_passphrase 복호 실패: {e}",
                                    destination.name
                                )
                            })?,
                    ),
                    None => None,
                };
                SshAuth::PublicKey {
                    identity_pem: Zeroizing::new(pem),
                    passphrase,
                }
            } else {
                SshAuth::Password {
                    password: decrypt_reader_secret(
                        require_field_encryptor(field_enc, &destination.name)?,
                        reader.password.as_ref(),
                        &destination.name,
                        "password",
                    )?,
                }
            };
            let source = SshManifestSource::new(SshReaderCfg {
                host: host.clone(),
                port: *port,
                user,
                host_key_fingerprint: destination.host_key_fingerprint.clone().ok_or_else(
                    || {
                        anyhow::anyhow!(
                            "ssh 목적지 '{}'에 host_key_fingerprint가 없습니다",
                            destination.name
                        )
                    },
                )?,
                auth,
                remote_dir: destination
                    .path
                    .as_ref()
                    .ok_or_else(|| {
                        anyhow::anyhow!("ssh 목적지 '{}'에 path가 없습니다", destination.name)
                    })?
                    .to_string_lossy()
                    .into_owned(),
            });
            Ok(Box::new(source))
        }
    }
}

fn require_field_encryptor<'a>(
    field_enc: Option<&'a DekEncryptor>,
    destination: &str,
) -> anyhow::Result<&'a DekEncryptor> {
    field_enc.ok_or_else(|| {
        anyhow::anyhow!(
            "원격 목적지 '{destination}'의 reader 시크릿을 복호화하려면 KEK unlock이 필요합니다"
        )
    })
}

fn decrypt_reader_secret(
    field_enc: &DekEncryptor,
    field: Option<&restoreplane_core::EncField>,
    destination: &str,
    field_name: &str,
) -> anyhow::Result<Zeroizing<String>> {
    let field = field.ok_or_else(|| {
        anyhow::anyhow!("목적지 '{destination}'.reader에 {field_name}가 없습니다")
    })?;
    let plaintext = field_enc.unwrap_field(field).map_err(|e| {
        anyhow::anyhow!(
            "목적지 '{destination}'.reader {field_name} 복호 실패(키스토어/wrap 세대 확인): {e}"
        )
    })?;
    anyhow::ensure!(
        !plaintext.is_empty(),
        "목적지 '{destination}'.reader {field_name}가 비어 있습니다; provider 자격증명 폴백을 허용하지 않습니다"
    );
    Ok(plaintext)
}

#[cfg(test)]
mod tests {
    use restoreplane_config::{schema::ReaderIdentity, Config};
    use restoreplane_core::SecretEncryptor;
    use restoreplane_secrets::{Dek, DekEncryptor};

    use super::{build_manifest_source, select_manifest_source, ManifestSourceSpec};

    #[test]
    fn unknown_selector_never_falls_back_to_spool() {
        let mut config = Config::empty();
        config.policy.spool_dir = "fallback-spool".into();

        let err = select_manifest_source(&config, "missing-destination").unwrap_err();

        assert!(err.to_string().contains("알 수 없는 목적지"), "{err}");
        assert!(!err.to_string().contains("fallback-spool"), "{err}");
    }

    #[test]
    fn s3_reader_never_reuses_writer_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        std::fs::write(
            &config_path,
            r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"

[[destination]]
name = "s3-primary"
kind = "s3"
endpoint = "https://s3.example.test"
region = "ap-northeast-2"
bucket = "backups"
access_key_id = "enc:1:d3JpdGVy"
secret_access_key = "enc:1:d3JpdGVy"
"#,
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let spec = select_manifest_source(&config, "s3-primary").unwrap();
        let field_enc = DekEncryptor::new(Dek::generate(), super::super::run::FIELD_WRAP_GEN);

        let err = match build_manifest_source(&spec, Some(&field_enc)) {
            Ok(_) => panic!("writer credentials must not be accepted as reader credentials"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("destination.reader"), "{err}");
        assert!(err.to_string().contains("폴백하지 않습니다"), "{err}");
    }

    #[test]
    fn ssh_destination_inherits_tunnel_host_and_port() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        std::fs::write(
            &config_path,
            r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"

[[ssh]]
name = "archive-gateway"
host = "archive.example.test"
port = 2222
user = "writer"
private_key_path = "writer-key"
host_key_fingerprint = "SHA256:writer-pin"

[[destination]]
name = "ssh-archive"
kind = "ssh"
tunnel = "archive-gateway"
path = "/srv/backups"
user = "writer"
private_key_path = "writer-key"
host_key_fingerprint = "SHA256:archive-pin"
"#,
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();

        let spec = select_manifest_source(&config, "ssh-archive").unwrap();

        match spec {
            ManifestSourceSpec::Ssh { host, port, .. } => {
                assert_eq!(host, "archive.example.test");
                assert_eq!(port, 2222);
            }
            other => panic!("expected SSH source, got {other:?}"),
        }
    }

    #[test]
    fn unencrypted_ssh_reader_key_does_not_require_kek_unlock() {
        let dir = tempfile::tempdir().unwrap();
        let key_path = dir.path().join("reader-key.pem");
        std::fs::write(&key_path, "test key is parsed only when connecting").unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        let key_path = key_path.to_string_lossy().replace('\\', "/");
        std::fs::write(
            &config_path,
            format!(
                r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"

[[destination]]
name = "ssh-archive"
kind = "ssh"
host = "archive.example.test"
path = "/srv/backups"
user = "writer"
private_key_path = "writer-key"
host_key_fingerprint = "SHA256:archive-pin"

[destination.reader]
user = "restore-reader"
private_key_path = "{key_path}"
"#
            ),
        )
        .unwrap();
        let config = Config::load(&config_path).unwrap();
        let spec = select_manifest_source(&config, "ssh-archive").unwrap();

        assert!(build_manifest_source(&spec, None).is_ok());
    }

    #[test]
    fn empty_s3_reader_credential_is_rejected_before_opendal_can_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("restoreplane.toml");
        std::fs::write(
            &config_path,
            r#"
[meta]
schema_version = 1

[policy]
spool_dir = "spool"

[[destination]]
name = "s3-primary"
kind = "s3"
endpoint = "https://s3.example.test"
region = "ap-northeast-2"
bucket = "backups"
access_key_id = "enc:1:d3JpdGVy"
secret_access_key = "enc:1:d3JpdGVy"
"#,
        )
        .unwrap();
        let mut config = Config::load(&config_path).unwrap();
        let field_enc = DekEncryptor::new(Dek::generate(), super::super::run::FIELD_WRAP_GEN);
        config.destinations[0].reader = Some(ReaderIdentity {
            access_key_id: Some(field_enc.wrap("").unwrap()),
            secret_access_key: Some(field_enc.wrap("reader-secret").unwrap()),
            user: None,
            private_key_path: None,
            private_key_passphrase: None,
            password: None,
        });
        let spec = select_manifest_source(&config, "s3-primary").unwrap();

        let err = match build_manifest_source(&spec, Some(&field_enc)) {
            Ok(_) => panic!("empty credentials must not reach the provider credential chain"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("비어 있습니다"), "{err}");
    }
}
