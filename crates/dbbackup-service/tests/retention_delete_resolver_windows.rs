#![cfg(windows)]

use std::process::Command;
use std::sync::Arc;

use dbbackup_catalog::{
    rusqlite::Connection, DestinationSecretBindingSpec, DestinationSecretRoleV2, DestinationSpec,
    OperationalStore,
};
use dbbackup_domain::{DestinationId, SecretRefId};
use dbbackup_runtime::RetentionDeleteOperatorResolverV2;
use dbbackup_service::ipc::WindowsSid;
use dbbackup_service::retention::ServiceRetentionDeleteResolverV2;
use dbbackup_service::secrets::{AclPolicy, SecretBuffer, SecretPurpose, ServiceSecretStore};
use tempfile::TempDir;

#[test]
fn resolver_uses_only_the_delete_binding_and_never_falls_back_to_writer_or_reader() {
    let fixture = Fixture::new();
    let destination_id = DestinationId::new();
    fixture.insert_s3_destination(&destination_id, "primary");
    let writer = fixture.create_secret(SecretPurpose::BackupWriter, b"revoked-writer", 100);
    let reader = fixture.create_secret(SecretPurpose::RestoreReader, b"revoked-reader", 101);
    let delete = fixture.create_secret(
        SecretPurpose::RetentionDelete,
        br#"{"schemaVersion":1,"accessKeyId":"DELETE_KEY","secretAccessKey":"delete-secret"}"#,
        102,
    );
    fixture.bind(
        &destination_id,
        DestinationSecretRoleV2::BackupWriter,
        &writer,
        110,
    );
    fixture.bind(
        &destination_id,
        DestinationSecretRoleV2::RestoreReader,
        &reader,
        111,
    );
    fixture.bind(
        &destination_id,
        DestinationSecretRoleV2::DeleteOperator,
        &delete,
        112,
    );
    fixture.secrets.revoke(&writer, 200).expect("revoke writer");
    fixture.secrets.revoke(&reader, 201).expect("revoke reader");
    let resolver = fixture.resolver();

    resolver
        .resolve(
            &format!("s3:{}", destination_id.as_str()),
            Some(delete.as_str()),
        )
        .expect("delete role alone constructs the operator");
    let rotated_delete = fixture
        .secrets
        .rotate(
            &delete,
            SecretPurpose::RetentionDelete,
            SecretBuffer::new(
                br#"{"schemaVersion":1,"accessKeyId":"DELETE_KEY_V2","secretAccessKey":"delete-secret-v2"}"#
                    .to_vec(),
            )
            .expect("rotated delete secret"),
            202,
        )
        .expect("rotate delete credential")
        .id()
        .clone();
    Connection::open(&fixture.db_path)
        .expect("open binding rotation")
        .execute(
            "UPDATE destination_secret_bindings
             SET secret_ref_id = ?2
             WHERE destination_id = ?1 AND secret_role = 'delete_operator'",
            [destination_id.as_str(), rotated_delete.as_str()],
        )
        .expect("activate rotated delete credential");
    resolver
        .resolve(
            &format!("s3:{}", destination_id.as_str()),
            Some(delete.as_str()),
        )
        .expect("queued work continues to resolve its retired pinned secret version");
    let wrong_role = match resolver.resolve(
        &format!("s3:{}", destination_id.as_str()),
        Some(writer.as_str()),
    ) {
        Ok(_) => panic!("a writer Secret Ref must not satisfy the delete role"),
        Err(error) => error,
    };
    assert_eq!(wrong_role.to_string(), "SECRET_STORE_PURPOSE_MISMATCH");

    let local_id = DestinationId::new();
    fixture.insert_local_destination(&local_id, "local-vault");
    resolver
        .resolve(&format!("local:{}", local_id.as_str()), None)
        .expect("local deletion requires no credential");

    let ssh_id = DestinationId::new();
    fixture.insert_ssh_destination(&ssh_id, "sftp-vault");
    let ssh_delete = fixture.create_secret(
        SecretPurpose::RetentionDelete,
        br#"{"schemaVersion":1,"auth":{"kind":"password","password":"delete-password"}}"#,
        250,
    );
    fixture.bind(
        &ssh_id,
        DestinationSecretRoleV2::DeleteOperator,
        &ssh_delete,
        251,
    );
    resolver
        .resolve(
            &format!("ssh_sftp:{}", ssh_id.as_str()),
            Some(ssh_delete.as_str()),
        )
        .expect("SSH delete role constructs a host-key-pinned operator");

    let writer_only_id = DestinationId::new();
    fixture.insert_s3_destination(&writer_only_id, "writer-only");
    let fallback_writer =
        fixture.create_secret(SecretPurpose::BackupWriter, b"writer-only-secret", 300);
    fixture.bind(
        &writer_only_id,
        DestinationSecretRoleV2::BackupWriter,
        &fallback_writer,
        301,
    );
    let error = match resolver.resolve(&format!("s3:{}", writer_only_id.as_str()), None) {
        Ok(_) => panic!("writer credential must never be used for retention deletion"),
        Err(error) => error,
    };
    assert_eq!(error.to_string(), "RETENTION_DELETE_SECRET_BINDING_MISSING");
}

struct Fixture {
    _temp: TempDir,
    db_path: std::path::PathBuf,
    store: OperationalStore,
    secrets: Arc<ServiceSecretStore>,
}

impl Fixture {
    fn new() -> Self {
        let temp = TempDir::new().expect("temp directory");
        let db_path = temp.path().join("operational.db");
        let store = OperationalStore::open(db_path.clone()).expect("operational store");
        let acl = AclPolicy::new(
            WindowsSid::parse("S-1-5-80-1001").expect("service SID"),
            current_user_sid(),
        )
        .expect("ACL");
        let secrets = Arc::new(
            ServiceSecretStore::open(temp.path().join("secret-store"), store.secret_refs(), acl)
                .expect("secret store"),
        );
        Self {
            _temp: temp,
            db_path,
            store,
            secrets,
        }
    }

    fn insert_s3_destination(&self, id: &DestinationId, display_name: &str) {
        self.store
            .configuration()
            .insert_destination(DestinationSpec::s3(
                id.clone(),
                display_name,
                r#"{"endpoint":"http://127.0.0.1:9000","region":"us-east-1","bucket":"backup-bucket","prefix":"objects"}"#,
                10,
            ))
            .expect("insert S3 destination");
        self.mark_ready(id);
    }

    fn insert_local_destination(&self, id: &DestinationId, display_name: &str) {
        let endpoint_json = serde_json::json!({
            "path": self._temp.path(),
        })
        .to_string();
        self.store
            .configuration()
            .insert_destination(DestinationSpec::local(
                id.clone(),
                display_name,
                endpoint_json,
                11,
            ))
            .expect("insert local destination");
        self.mark_ready(id);
    }

    fn insert_ssh_destination(&self, id: &DestinationId, display_name: &str) {
        self.store
            .configuration()
            .insert_destination(DestinationSpec::ssh_sftp(
                id.clone(),
                display_name,
                r#"{"host":"127.0.0.1","port":22,"user":"retention-delete","hostKeyFingerprint":"SHA256:AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA","path":"/backup"}"#,
                12,
            ))
            .expect("insert SSH destination");
        self.mark_ready(id);
    }

    fn mark_ready(&self, id: &DestinationId) {
        Connection::open(&self.db_path)
            .expect("open destination state")
            .execute(
                "UPDATE destinations SET state = 'ready' WHERE id = ?1",
                [id.as_str()],
            )
            .expect("mark destination ready");
    }

    fn create_secret(
        &self,
        purpose: SecretPurpose,
        value: &[u8],
        created_at_ms: i64,
    ) -> SecretRefId {
        self.secrets
            .create(
                purpose,
                SecretBuffer::new(value.to_vec()).expect("secret buffer"),
                created_at_ms,
            )
            .expect("create secret")
            .id()
            .clone()
    }

    fn bind(
        &self,
        destination_id: &DestinationId,
        role: DestinationSecretRoleV2,
        secret_ref_id: &SecretRefId,
        created_at_ms: i64,
    ) {
        self.store
            .configuration()
            .bind_destination_secret(DestinationSecretBindingSpec::new(
                destination_id.clone(),
                role,
                secret_ref_id.clone(),
                created_at_ms,
            ))
            .expect("bind destination secret");
    }

    fn resolver(&self) -> ServiceRetentionDeleteResolverV2 {
        ServiceRetentionDeleteResolverV2::new(self.store.configuration(), Arc::clone(&self.secrets))
    }
}

fn current_user_sid() -> WindowsSid {
    let output = Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .expect("whoami");
    assert!(output.status.success());
    let line = String::from_utf8(output.stdout).expect("whoami UTF-8");
    let sid = line
        .trim()
        .trim_matches('"')
        .rsplit("\",\"")
        .next()
        .expect("SID")
        .trim_matches('"');
    WindowsSid::parse(sid).expect("current user SID")
}
