//! Actual artifact/restore/proof producer chain. Clock, catalog bootstrap and
//! token/integrity inspection are fixture seams; this is not named-pipe or
//! standard-integrity OS E2E. No restore proof field is hand-authored.
use super::*;
use crate::application::ServiceApplication;
use crate::backup::{
    BackupWorkerTick, NativeBackupExecutor, NativeBackupLimits, ServiceBackupWorker,
};
use crate::host::{HostConfig, HostRuntime, ServiceHost, ShutdownReason};
use crate::ipc::{
    authorize_and_decode, encode_request, AuthorizationPolicy, DurableIpcDispatcher,
    RequestEnvelope, TokenIdentity, WindowsSid,
};
use crate::secrets::AclPolicy;
use restoreplane_application::{
    ApplicationCommand, ApplicationRequest, ApplicationUseCases, AuthoritySnapshot, CutoverMode,
    ExecutionAuthority,
};
use restoreplane_catalog::{
    rusqlite::Connection, BackupWorkSpec, CertificationCapability, ConnectionProbeSpec,
    ConnectionProfileSpec, DatabaseTargetSpec, DestinationSpec, DiscoverySnapshotSpec,
    EngineCertificationSpec, OccurrenceSpec, PlanDraftSpec, SafeRestoreSpec, DEFAULT_WORKSPACE_ID,
};
use restoreplane_domain::workbench::{
    AuthorizePurpose, BindingReceipt, Column, Mode, Scope, TargetPurpose, TypedCell,
};
use restoreplane_domain::workbench_protection::WorkbenchRestoreProof;
use restoreplane_domain::{
    BackupPlanId, BackupRunId, ConnectionProfileId, DatabaseTargetId, DestinationId,
    DestinationRequirement, IpcRequestId, OccurrenceIdentity, OccurrenceOrigin, PlanRevisionId,
    RecoveryPointId, RestoreRunId, SecretRefId, WorkItemId, WorkspaceId,
};
use restoreplane_pipeline::SignedSetManifestV2;
use restoreplane_workbench::credentials::CredentialProvider;
use restoreplane_workbench::driver::{DriverLogin, NativeDriverFactory, QueryLimits, RowSink};
use restoreplane_workbench::session::{RestoredSessionCheck, RuntimeLimits};
use restoreplane_workbench::{
    BindingAuthorizer, IntegrityLevel, InvocationContext, NativePrincipal, WorkbenchError,
    WorkbenchState,
};
use std::str::FromStr;

const SERVICE_SID: &str = "S-1-5-80-1001";
const LOGON_SID: &str = "S-1-5-5-10-20";
fn fixed_now() -> i64 {
    6000
}
struct Clock;
impl RuntimeClock for Clock {
    fn now_ms(&self) -> i64 {
        fixed_now()
    }
}
struct Noop;
impl HostRuntime for Noop {
    fn started(&self, _: bool) -> Result<(), HostError> {
        Ok(())
    }
    fn stop_accepting_ipc(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn stop_claiming_work(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn drain_clients(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn checkpointed(&self) -> Result<(), HostError> {
        Ok(())
    }
    fn stopped(&self, _: ShutdownReason) {}
}
struct ServiceBindings(Arc<ServiceApplication>);
#[async_trait]
impl BindingAuthorizer for ServiceBindings {
    async fn authorize(
        &self,
        ctx: &InvocationContext,
        scope: &Scope,
        purpose: AuthorizePurpose,
        revision: Option<&str>,
    ) -> Result<BindingReceipt, WorkbenchError> {
        let accepted_at = i64::try_from(restoreplane_workbench::types::now_ms())
            .map_err(|_| WorkbenchError::InvalidInput)?;
        let response=self.0.authorize_workbench_binding(json!({"workspaceId":scope.workspace_id,"connectionProfileId":scope.connection_profile_id,"databaseTargetId":scope.database_target_id,"purpose":purpose,"expectedDescriptorRevision":revision,"appNonce":uuid::Uuid::new_v4()}),ctx.principal().owner_id(),accepted_at).map_err(|_|WorkbenchError::BindingUnproven)?;
        serde_json::from_value(response.body).map_err(|_| WorkbenchError::BindingUnproven)
    }
}
struct EmptySqliteCredentials;
#[async_trait]
impl CredentialProvider for EmptySqliteCredentials {
    async fn resolve(
        &self,
        _: &InvocationContext,
        _: &Scope,
        _: uuid::Uuid,
    ) -> Result<DriverLogin, WorkbenchError> {
        Ok(DriverLogin {
            username: String::new(),
            secret: Zeroizing::new(String::new()),
        })
    }
}
#[derive(Default)]
struct Rows(Vec<Vec<TypedCell>>);
#[async_trait]
impl RowSink for Rows {
    async fn columns(&mut self, _: Vec<Column>) -> Result<(), WorkbenchError> {
        Ok(())
    }
    async fn row(&mut self, row: Vec<TypedCell>) -> Result<(), WorkbenchError> {
        if self.0.len() >= 10 {
            return Err(WorkbenchError::Limit);
        }
        self.0.push(row);
        Ok(())
    }
}
fn current_user_sid() -> WindowsSid {
    let output = std::process::Command::new("whoami")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let value = String::from_utf8(output.stdout).unwrap();
    WindowsSid::parse(
        value
            .trim()
            .trim_matches('"')
            .rsplit("\",\"")
            .next()
            .unwrap()
            .trim_matches('"'),
    )
    .unwrap()
}
fn find_manifest(root: &Path) -> Option<PathBuf> {
    for entry in std::fs::read_dir(root).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_manifest(&path) {
                return Some(found);
            }
        } else if path
            .file_name()
            .is_some_and(|name| name == "set-manifest.json")
        {
            return Some(path);
        }
    }
    None
}
fn record_chain_evidence(value: &serde_json::Value) {
    if let Some(root) = std::env::var_os("RESTORE_CHAIN_EVIDENCE_DIR") {
        let root = PathBuf::from(root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("restore-chain-metadata.json"),
            serde_json::to_vec_pretty(value).unwrap(),
        )
        .unwrap();
    }
}

#[test]
fn actual_signed_backup_restore_completed_dispatch_proof_opens_only_the_final_file() {
    let temp = tempfile::tempdir().unwrap();
    let source_path = temp.path().join("source.sqlite");
    Connection::open(&source_path).unwrap().execute_batch("CREATE TABLE orders(id INTEGER PRIMARY KEY,value TEXT NOT NULL);INSERT INTO orders VALUES(1,'alpha'),(2,'beta')").unwrap();
    let catalog_path = temp.path().join("operational.db");
    let host = ServiceHost::start(
        HostConfig::new(
            format!("wb-restore-chain-{}", uuid::Uuid::new_v4()),
            catalog_path.clone(),
        )
        .unwrap(),
        Arc::new(Noop),
    )
    .unwrap();
    let store = host.operational_store().unwrap();
    let owner = current_user_sid();
    let secrets = Arc::new(
        ServiceSecretStore::open(
            temp.path().join("secrets"),
            store.secret_refs(),
            AclPolicy::new(WindowsSid::parse(SERVICE_SID).unwrap(), owner.clone()).unwrap(),
        )
        .unwrap(),
    );
    let destination_root = temp.path().join("destination");
    let spool_root = temp.path().join("spool");
    std::fs::create_dir_all(&destination_root).unwrap();
    std::fs::create_dir_all(&spool_root).unwrap();
    let source_profile = ConnectionProfileId::new();
    let source_target = DatabaseTargetId::new();
    let destination = DestinationId::new();
    let revision = PlanRevisionId::new();
    let backup_work = WorkItemId::new();
    let configuration = store.configuration();
    configuration
        .insert_connection(ConnectionProfileSpec::file(
            source_profile.clone(),
            "source",
            "sqlite",
            source_path.to_string_lossy(),
            1000,
        ))
        .unwrap();
    configuration
        .insert_target(DatabaseTargetSpec::new(
            source_target.clone(),
            source_profile.clone(),
            "file:source.sqlite",
            "source",
            "source",
            "source-fingerprint",
            1010,
        ))
        .unwrap();
    configuration
        .insert_destination(DestinationSpec::local(
            destination.clone(),
            "local-vault",
            json!({"path":destination_root}).to_string(),
            1020,
        ))
        .unwrap();
    configuration
        .insert_plan_draft(PlanDraftSpec::manual(
            BackupPlanId::new(),
            revision.clone(),
            "chain",
            1,
            "a".repeat(64),
            1030,
        ))
        .unwrap();
    configuration
        .add_revision_target(&revision, &source_target, 1031)
        .unwrap();
    configuration
        .add_revision_destination(
            &revision,
            &destination,
            DestinationRequirement::Required,
            1032,
        )
        .unwrap();
    configuration.publish_revision(&revision, 1040).unwrap();
    let occurrence =
        OccurrenceIdentity::scheduled(revision.clone(), OccurrenceOrigin::Scheduled, 5000)
            .unwrap()
            .deterministic_id(uuid::Uuid::NAMESPACE_URL);
    store
        .runtime()
        .upsert_occurrence(OccurrenceSpec::scheduled(
            occurrence.clone(),
            revision.clone(),
            OccurrenceOrigin::Scheduled,
            5000,
            4900,
        ))
        .unwrap();
    store
        .runtime()
        .enqueue_backup_work(BackupWorkSpec::new(
            BackupRunId::new(),
            backup_work.clone(),
            occurrence,
            revision,
            source_target,
            "sqlite",
            "source-fingerprint",
            5100,
        ))
        .unwrap();
    let executor = Arc::new(
        NativeBackupExecutor::with_clock_and_limits(
            store.clone(),
            secrets.clone(),
            temp.path().to_path_buf(),
            spool_root.clone(),
            "actual-chain",
            fixed_now,
            NativeBackupLimits::new(60_000, 1024 * 1024, 64 * 1024, 2, 1),
        )
        .unwrap(),
    );
    let worker = ServiceBackupWorker::with_clock(
        store.clone(),
        executor,
        "chain-backup",
        "chain-backup-boot",
        fixed_now,
    );
    assert_eq!(
        worker.run_once().unwrap(),
        BackupWorkerTick::Completed(backup_work.clone())
    );
    let signed_bytes = std::fs::read(find_manifest(&destination_root).unwrap()).unwrap();
    let signed: SignedSetManifestV2 = serde_json::from_slice(&signed_bytes).unwrap();
    let signing_ref = SecretRefId::from_str(&signed.manifest.signing_key_id).unwrap();
    let seed = secrets
        .resolve(&signing_ref, SecretPurpose::SigningKey)
        .unwrap();
    let signing_key = ed25519_dalek::SigningKey::from_bytes(&seed.expose().try_into().unwrap());
    signed.verify(&signing_key.verifying_key()).unwrap();
    let probe = Connection::open(&catalog_path).unwrap();
    let (recovery,replica):(String,String)=probe.query_row("SELECT b.recovery_point_id,r.id FROM backup_runs b JOIN work_items w ON w.backup_run_id=b.id JOIN catalog_artifact_replicas r ON r.recovery_point_id=b.recovery_point_id WHERE w.id=?1 AND w.state='succeeded' AND r.state='committed'",[backup_work.as_str()],|row|Ok((row.get(0)?,row.get(1)?))).unwrap();
    let manifest_plaintext = signed
        .manifest
        .parts
        .iter()
        .try_fold(0u64, |total, part| total.checked_add(part.plaintext_bytes))
        .unwrap();
    let (backup_plaintext,recovery_plaintext,parts_plaintext):(Option<i64>,Option<i64>,Option<i64>)=probe.query_row("SELECT b.plaintext_bytes,r.plaintext_bytes,(SELECT SUM(p.plaintext_bytes) FROM catalog_artifact_parts p WHERE p.recovery_point_id=r.id) FROM backup_runs b JOIN catalog_recovery_points r ON r.id=b.recovery_point_id WHERE r.id=?1",[&recovery],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).unwrap();
    let mut chain_evidence = json!({"producer":"NativeBackupExecutor + ServiceBackupWorker","setManifestBlake3":blake3::hash(&signed_bytes).to_hex().to_string(),"signatureVerified":true,"manifestPlaintextBytes":manifest_plaintext,"backupRunPlaintextBytes":backup_plaintext,"catalogPartPlaintextBytes":parts_plaintext,"catalogRecoveryPlaintextBytes":recovery_plaintext,"proofFieldsHandcrafted":false,"bootstrapSeams":["clock","native token principal","readiness probe/discovery","capability certification"]});
    let manifest_database_dump_format = signed
        .manifest
        .parts
        .iter()
        .find(|part| part.role == restoreplane_core::ArtifactPartRole::DatabaseDump)
        .unwrap()
        .dump_format
        .clone();
    chain_evidence["manifestDatabaseDumpFormat"] = json!(manifest_database_dump_format);
    let restored_path = temp.path().join("restored.sqlite");
    let restore_profile = ConnectionProfileId::new();
    configuration
        .insert_connection(ConnectionProfileSpec::file(
            restore_profile.clone(),
            "restore-destination",
            "sqlite",
            restored_path.to_string_lossy(),
            5200,
        ))
        .unwrap();
    // Bootstrap seams: current observed readiness/capability records. The
    // artifact, signature, native restore result and final file ID are real.
    configuration
        .record_connection_probe(ConnectionProbeSpec::passed(
            uuid::Uuid::new_v4().to_string(),
            restore_profile.clone(),
            restoreplane_catalog::rusqlite::version(),
            json!({"restorePreflight":true}).to_string(),
            5300,
            5310,
        ))
        .unwrap();
    configuration
        .record_discovery_snapshot(DiscoverySnapshotSpec::successful(
            uuid::Uuid::new_v4().to_string(),
            restore_profile.clone(),
            Vec::new(),
            5320,
            5330,
        ))
        .unwrap();
    let free = i64::try_from(fs2::available_space(&spool_root).unwrap()).unwrap();
    store
        .runtime()
        .register_spool_volume(SpoolVolumeSpec::ready(
            uuid::Uuid::new_v4().to_string(),
            "chain-spool",
            spool_root.to_string_lossy(),
            free,
            free,
            0,
            free,
            5340,
        ))
        .unwrap();
    let mut certification = EngineCertificationSpec::candidate(
        "chain-sqlite",
        "sqlite",
        ">=3,<4",
        "sqlite-online-backup",
        ">=3,<4",
        "windows",
        "x86_64",
        "sqlite",
        "application-consistent",
        "test://chain/bootstrap",
        "a".repeat(64),
        5350,
    );
    for capability in restoreplane_core::ProviderCapability::REQUIRED {
        certification = certification.with_capability(CertificationCapability::passed(
            capability.as_str(),
            "test://chain/bootstrap",
        ));
    }
    store
        .certification()
        .insert_candidate(certification)
        .unwrap();
    store
        .certification()
        .certify("chain-sqlite", 5360, i64::MAX - 1)
        .unwrap();
    let app = Arc::new(
        ServiceApplication::with_clock(host.health_probe(), Some(store.clone()), fixed_now)
            .with_secret_store(secrets.clone()),
    );
    let preflight=app.execute(ApplicationRequest{command:ApplicationCommand::RestorePreflight,payload:json!({"workspaceId":DEFAULT_WORKSPACE_ID,"snapshotId":recovery,"sourceReplicaId":replica,"targetConnectionId":restore_profile,"targetName":"restored","mode":"create_new","purpose":"incident_recovery"})}).unwrap().body;
    chain_evidence["preflightRequiredSpoolBytes"] = preflight["requiredSpoolBytes"].clone();
    chain_evidence["preflightAvailableSpoolBytes"] = preflight["availableSpoolBytes"].clone();
    chain_evidence["preflightEligible"] = preflight["eligible"].clone();
    record_chain_evidence(&chain_evidence);
    assert_eq!(
        manifest_database_dump_format, "sqlite_database",
        "the observed signed production format must match its canonical restore contract"
    );
    assert_eq!(backup_plaintext, i64::try_from(manifest_plaintext).ok());
    assert_eq!(parts_plaintext, i64::try_from(manifest_plaintext).ok());
    assert_eq!(
        recovery_plaintext,
        i64::try_from(manifest_plaintext).ok(),
        "the production projection must persist the actual signed part total"
    );
    assert_eq!(
        preflight["requiredSpoolBytes"].as_u64(),
        manifest_plaintext.checked_mul(3)
    );
    assert_eq!(
        preflight["eligible"], true,
        "actual preflight checks {preflight}"
    );
    let restore_run = RestoreRunId::new();
    let restore_work = WorkItemId::new();
    store
        .recovery()
        .enqueue_safe(SafeRestoreSpec::new(
            restore_run.clone(),
            restore_work.clone(),
            RecoveryPointId::from_str(&recovery).unwrap(),
            &replica,
            restore_profile.clone(),
            "restored",
            preflight["targetFingerprint"].as_str().unwrap(),
            preflight["preflightVersion"].as_str().unwrap(),
            preflight.to_string(),
            6000,
        ))
        .unwrap();
    let mut trusted = std::collections::BTreeMap::new();
    trusted.insert(
        signed.manifest.signing_key_id.clone(),
        signing_key.verifying_key(),
    );
    let resolver = Arc::new(
        RecoveryPointResolver::new(
            store.clone(),
            catalog_path.clone(),
            secrets.clone(),
            trusted,
            temp.path().to_path_buf(),
            temp.path().join("materialized"),
        )
        .unwrap(),
    );
    let restore_executor = Arc::new(ServiceRestoreExecutor::new(
        store.clone(),
        secrets,
        resolver,
        spool_root,
        Arc::new(AtomicBool::new(false)),
        Arc::new(Clock),
    ));
    let worker = RestoreWorker::new(
        store.runtime(),
        restore_executor,
        Arc::new(Clock),
        RestoreWorkerConfig::new("chain-restore", "chain-restore-boot", 30_000, 5000).unwrap(),
    );
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async {
        let result=worker.run_once().await.unwrap();
        assert!(matches!(result,restoreplane_runtime::RestoreWorkerOutcome::Succeeded{..}),"{result:?}");
        let completed:String=probe.query_row("SELECT validation_result_json FROM restore_runs WHERE id=?1 AND state='succeeded'",[restore_run.as_str()],|row|row.get(0)).unwrap();
        let completed:serde_json::Value=serde_json::from_str(&completed).unwrap();
        assert_eq!(completed["phase"],"completed");assert_eq!(completed["restore"]["evidence"]["workbenchRestoreIdentity"]["signatureVersion"],1);
        let policy=AuthorizationPolicy::new(WindowsSid::parse(SERVICE_SID).unwrap(),owner.clone(),WindowsSid::parse(LOGON_SID).unwrap(),10,Vec::new()).unwrap();
        let envelope=RequestEnvelope::new(IpcRequestId::new(),ApplicationCommand::WorkGet,None,6000,7000,json!({"workspaceId":DEFAULT_WORKSPACE_ID,"workItemId":restore_work,"includeWorkbenchRestoreProof":true})).unwrap();
        let frame=encode_request(&envelope).unwrap();
        let identity=TokenIdentity::new(owner,WindowsSid::parse(LOGON_SID).unwrap(),10,false,true);
        let authorized=authorize_and_decode(&policy,&identity,&frame,6100).unwrap();
        let owner_hash=authorized.principal().sid_hash().to_owned();
        let dispatcher=DurableIpcDispatcher::new(store.ipc(),app.clone(),AuthoritySnapshot::new(ExecutionAuthority::LegacyDirect,CutoverMode::RehearsalOnly,false,false));
        let response=dispatcher.dispatch(authorized).unwrap();assert!(response.is_success(),"{}",response.code());
        let proof:WorkbenchRestoreProof=serde_json::from_value(response.body()["body"]["workbenchRestoreProof"].clone()).unwrap();
        assert_eq!(proof.restore_work_item_id,restore_work);
        let actual=restoreplane_source_sqlite::probe_sqlite_file(&restored_path).unwrap().object_id().to_owned();
        assert_eq!(proof.actual_database_identity,format!("sqlite-file-object-id:{actual}"));
        chain_evidence["completedRestoreProofProduced"]=json!(true);
        chain_evidence["authenticatedDispatcherProofObserved"]=json!(true);
        chain_evidence["actualFinalFileIdentity"]=json!(proof.actual_database_identity);
        record_chain_evidence(&chain_evidence);
        let principal=NativePrincipal::new(owner_hash,10,IntegrityLevel::Standard).unwrap();
        let context=InvocationContext::new(principal.clone(),uuid::Uuid::new_v4());
        let state=WorkbenchState::new(principal.clone(),Arc::new(ServiceBindings(app)),Arc::new(EmptySqliteCredentials),Arc::new(NativeDriverFactory::default()),RuntimeLimits::default()).unwrap();
        let workspace=WorkspaceId::from_str(DEFAULT_WORKSPACE_ID).unwrap();
        let check=RestoredSessionCheck::from_authenticated_work(principal,workspace.clone(),proof).unwrap();
        // A new registration is allowed; actual final file ID supplies the
        // authority rather than equating its UUID to provenance profile UUID.
        let profile=ConnectionProfileId::new();let target=DatabaseTargetId::new();
        configuration.insert_connection(ConnectionProfileSpec::file(profile.clone(),"restored-inspection","sqlite",restored_path.to_string_lossy(),6200)).unwrap();
        configuration.insert_target(DatabaseTargetSpec::new(target.clone(),profile.clone(),format!("file:{actual}"),"restored","restored","final-file-registration",6210)).unwrap();
        let scope=Scope{workspace_id:workspace.clone(),connection_profile_id:profile.clone(),database_target_id:target};
        let view=state.open_checked(&context,scope.clone(),uuid::Uuid::new_v4(),None,check.clone()).await.unwrap();
        assert_eq!(view.actual_context.purpose,TargetPurpose::Restored);assert_eq!(view.mode,Mode::ReadOnly);
        let session=view.session;
        let rows=state.with_connection(&context,session,AuthorizePurpose::Read,|connection|Box::pin(async move{let mut rows=Rows::default();connection.execute("SELECT id,value FROM orders ORDER BY id",QueryLimits::default(),&mut rows).await?;Ok(rows.0)})).await.unwrap();
        assert_eq!(rows,vec![vec![TypedCell::I64{value:"1".into()},TypedCell::Text{value:"alpha".into()}],vec![TypedCell::I64{value:"2".into()},TypedCell::Text{value:"beta".into()}]]);
        assert!(state.set_mode(&context,session,Mode::ReadWrite).await.is_err());
        let reopened=state.reconnect(&context,session,&view.descriptor_revision).await.unwrap();assert_eq!(reopened.actual_context.purpose,TargetPurpose::Restored);assert_eq!(reopened.mode,Mode::ReadOnly);
        let wrong_profile=ConnectionProfileId::new();let wrong_target=DatabaseTargetId::new();
        let source_id=restoreplane_source_sqlite::probe_sqlite_file(&source_path).unwrap().object_id().to_owned();
        configuration.insert_connection(ConnectionProfileSpec::file(wrong_profile.clone(),"wrong-source-file","sqlite",source_path.to_string_lossy(),6300)).unwrap();
        configuration.insert_target(DatabaseTargetSpec::new(wrong_target.clone(),wrong_profile.clone(),format!("file:{source_id}"),"restored","restored","wrong-source-registration",6310)).unwrap();
        assert!(state.open_checked(&context,Scope{workspace_id:workspace,connection_profile_id:wrong_profile,database_target_id:wrong_target},uuid::Uuid::new_v4(),None,check).await.is_err());
        state.close_window(&context).await.unwrap();
        chain_evidence["restoredOpenAndReadVerified"]=json!(true);
        chain_evidence["reconnectPurposeReadOnlyPreserved"]=json!(true);
        chain_evidence["wrongFinalFileRejected"]=json!(true);
        record_chain_evidence(&chain_evidence);
    });
}
