use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use dbbackup_application::{
    BuildIdentityV1, CutoverMode, ExecutionAuthority, VerifiedG1EvidenceV1,
};
use dbbackup_catalog::{ConfigCutoverSpec, G1EvidenceBindingV1};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

use crate::host::{HostPhase, ServiceHealthProbe};

const MAX_CONTROL_DOCUMENT_BYTES: usize = 1024 * 1024;
const REQUIRED_BUNDLE_ITEM_KINDS: [&str; 4] = [
    "legacy_catalog",
    "legacy_keystore",
    "legacy_toml",
    "scheduler_inventory",
];

pub trait LegacySchedulerProbeV1: Send + Sync {
    fn task_enabled(&self, task_name: &str) -> Option<bool>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsLegacySchedulerProbeV1;

impl LegacySchedulerProbeV1 for WindowsLegacySchedulerProbeV1 {
    fn task_enabled(&self, task_name: &str) -> Option<bool> {
        query_windows_task_enabled(task_name)
    }
}

pub trait LegacySchedulerControllerV1: Send + Sync {
    fn task_enabled(&self, task_name: &str) -> Option<bool>;

    fn task_definition_matches(
        &self,
        task_name: &str,
        task_xml: &str,
        xml_digest_blake3: &str,
    ) -> bool;

    fn set_task_enabled(
        &self,
        task_name: &str,
        enabled: bool,
    ) -> Result<(), LegacySchedulerControlError>;
}

#[derive(Clone, Copy, Debug, Default)]
pub struct WindowsLegacySchedulerControllerV1;

impl LegacySchedulerControllerV1 for WindowsLegacySchedulerControllerV1 {
    fn task_enabled(&self, task_name: &str) -> Option<bool> {
        query_windows_task_enabled(task_name)
    }

    fn task_definition_matches(
        &self,
        task_name: &str,
        task_xml: &str,
        xml_digest_blake3: &str,
    ) -> bool {
        windows_task_definition_matches(task_name, task_xml, xml_digest_blake3)
    }

    fn set_task_enabled(
        &self,
        task_name: &str,
        enabled: bool,
    ) -> Result<(), LegacySchedulerControlError> {
        set_windows_task_enabled(task_name, enabled)
    }
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum LegacySchedulerControlError {
    #[error("CONFIG_CUTOVER_SCHEDULER_CONTROL_FAILED")]
    CommandFailed,
}

impl LegacySchedulerControlError {
    pub const fn code(self) -> &'static str {
        match self {
            Self::CommandFailed => "CONFIG_CUTOVER_SCHEDULER_CONTROL_FAILED",
        }
    }
}

pub trait CutoverReadinessVerifierV1: Send + Sync {
    fn verify(
        &self,
        input: &CutoverReadinessInputV1,
        accepted_at_ms: i64,
    ) -> Result<VerifiedCutoverReadinessV1, CutoverReadinessError>;
}

#[derive(Clone, Debug)]
pub struct CutoverReadinessInputV1 {
    shadow_import_id: Uuid,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    g1_evidence_path: String,
    recovery_bundle_manifest_path: String,
    freeze_receipt_path: String,
}

impl CutoverReadinessInputV1 {
    pub fn new(
        shadow_import_id: Uuid,
        source_checksum_blake3: impl Into<String>,
        relation_digest_blake3: impl Into<String>,
        g1_evidence_path: impl Into<String>,
        recovery_bundle_manifest_path: impl Into<String>,
        freeze_receipt_path: impl Into<String>,
    ) -> Self {
        Self {
            shadow_import_id,
            source_checksum_blake3: source_checksum_blake3.into(),
            relation_digest_blake3: relation_digest_blake3.into(),
            g1_evidence_path: g1_evidence_path.into(),
            recovery_bundle_manifest_path: recovery_bundle_manifest_path.into(),
            freeze_receipt_path: freeze_receipt_path.into(),
        }
    }
}

pub struct FileCutoverReadinessVerifierV1 {
    evidence_root: PathBuf,
    build_identity: BuildIdentityV1,
    health: ServiceHealthProbe,
    scheduler: Arc<dyn LegacySchedulerProbeV1>,
    g1_requirements_json: String,
    g1_matrix_json: String,
}

impl FileCutoverReadinessVerifierV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        evidence_root: PathBuf,
        build_identity: BuildIdentityV1,
        health: ServiceHealthProbe,
        scheduler: Arc<dyn LegacySchedulerProbeV1>,
        g1_requirements_json: impl Into<String>,
        g1_matrix_json: impl Into<String>,
    ) -> Result<Self, CutoverReadinessError> {
        let evidence_root = evidence_root
            .canonicalize()
            .map_err(|_| CutoverReadinessError::Invalid)?;
        if !evidence_root.is_dir() {
            return Err(CutoverReadinessError::Invalid);
        }
        let g1_requirements_json = g1_requirements_json.into();
        let g1_matrix_json = g1_matrix_json.into();
        if g1_requirements_json.is_empty() || g1_matrix_json.is_empty() {
            return Err(CutoverReadinessError::Invalid);
        }
        Ok(Self {
            evidence_root,
            build_identity,
            health,
            scheduler,
            g1_requirements_json,
            g1_matrix_json,
        })
    }

    pub fn verify(
        &self,
        input: &CutoverReadinessInputV1,
        accepted_at_ms: i64,
    ) -> Result<VerifiedCutoverReadinessV1, CutoverReadinessError> {
        validate_input(input, accepted_at_ms)?;
        let g1_path = resolve_file(&self.evidence_root, &input.g1_evidence_path)?;
        let g1_json = read_utf8_limited(&g1_path, MAX_CONTROL_DOCUMENT_BYTES)?;
        let g1_evidence = VerifiedG1EvidenceV1::verify(
            &g1_json,
            &self.g1_requirements_json,
            &self.g1_matrix_json,
            &self.build_identity,
        )
        .map_err(|_| CutoverReadinessError::Invalid)?;

        let manifest_path =
            resolve_file(&self.evidence_root, &input.recovery_bundle_manifest_path)?;
        let manifest_bytes = read_limited(&manifest_path, MAX_CONTROL_DOCUMENT_BYTES)?;
        let recovery_bundle_digest_blake3 = digest_bytes(&manifest_bytes);
        let manifest: RecoveryBundleManifestV1 =
            serde_json::from_slice(&manifest_bytes).map_err(|_| CutoverReadinessError::Invalid)?;
        let scheduler_inventory = self.verify_recovery_bundle(input, accepted_at_ms, &manifest)?;

        let freeze_path = resolve_file(&self.evidence_root, &input.freeze_receipt_path)?;
        let freeze_bytes = read_limited(&freeze_path, MAX_CONTROL_DOCUMENT_BYTES)?;
        let legacy_scheduler_freeze_digest_blake3 = digest_bytes(&freeze_bytes);
        let freeze: LegacyFreezeReceiptV1 =
            serde_json::from_slice(&freeze_bytes).map_err(|_| CutoverReadinessError::Invalid)?;
        self.verify_freeze(
            input,
            accepted_at_ms,
            &manifest,
            &recovery_bundle_digest_blake3,
            &scheduler_inventory,
            &freeze,
        )?;

        let service_health_digest_blake3 = self.verify_live_health()?;
        Ok(VerifiedCutoverReadinessV1 {
            shadow_import_id: input.shadow_import_id,
            source_checksum_blake3: input.source_checksum_blake3.clone(),
            relation_digest_blake3: input.relation_digest_blake3.clone(),
            recovery_bundle_digest_blake3,
            legacy_scheduler_freeze_digest_blake3,
            service_health_digest_blake3,
            g1_evidence,
        })
    }

    fn verify_recovery_bundle(
        &self,
        input: &CutoverReadinessInputV1,
        accepted_at_ms: i64,
        manifest: &RecoveryBundleManifestV1,
    ) -> Result<SchedulerInventoryV1, CutoverReadinessError> {
        verify_recovery_bundle_files(
            &self.evidence_root,
            manifest,
            Some(&input.source_checksum_blake3),
            Some(accepted_at_ms),
        )
    }

    fn verify_freeze(
        &self,
        input: &CutoverReadinessInputV1,
        accepted_at_ms: i64,
        manifest: &RecoveryBundleManifestV1,
        recovery_bundle_digest_blake3: &str,
        inventory: &SchedulerInventoryV1,
        freeze: &LegacyFreezeReceiptV1,
    ) -> Result<(), CutoverReadinessError> {
        if freeze.schema_version != 1
            || freeze.source_checksum_blake3 != input.source_checksum_blake3
            || freeze.recovery_bundle_digest_blake3 != recovery_bundle_digest_blake3
            || freeze.frozen_at_ms < manifest.created_at_ms
            || freeze.frozen_at_ms > accepted_at_ms
            || freeze.tasks.len() != inventory.tasks.len()
        {
            return Err(CutoverReadinessError::Invalid);
        }
        let inventory_by_name = inventory
            .tasks
            .iter()
            .map(|task| (task.task_name.as_str(), task))
            .collect::<BTreeMap<_, _>>();
        let mut seen = BTreeSet::new();
        for task in &freeze.tasks {
            let inventory_task = inventory_by_name
                .get(task.task_name.as_str())
                .ok_or(CutoverReadinessError::Invalid)?;
            if !seen.insert(task.task_name.as_str())
                || task.before_enabled != inventory_task.enabled
                || task.after_enabled
                || task.xml_digest_blake3 != inventory_task.xml_digest_blake3
                || self.scheduler.task_enabled(&task.task_name) != Some(false)
            {
                return Err(CutoverReadinessError::Invalid);
            }
        }
        Ok(())
    }

    fn verify_live_health(&self) -> Result<String, CutoverReadinessError> {
        let health = self.health.snapshot();
        if health.phase() != HostPhase::RehearsalReady
            || health.schema_version() != Some(4)
            || health.application_table_count() != Some(67)
            || health.execution_authority() != ExecutionAuthority::LegacyDirect
            || health.cutover_mode() != CutoverMode::RehearsalOnly
            || health.claims_enabled()
            || !health.accepts_ipc()
            || health.diagnostic_code().is_some()
        {
            return Err(CutoverReadinessError::Invalid);
        }
        let document = LiveServiceHealthEvidenceV1 {
            schema_version: 1,
            boot_id: health.boot_id().to_string(),
            phase: "rehearsal_ready",
            operational_schema_version: 4,
            application_table_count: 67,
            execution_authority: "legacy_direct",
            cutover_mode: "rehearsal_only",
            claims_enabled: false,
            accepts_ipc: true,
        };
        let canonical =
            serde_json::to_vec(&document).map_err(|_| CutoverReadinessError::Invalid)?;
        Ok(digest_bytes(&canonical))
    }
}

impl CutoverReadinessVerifierV1 for FileCutoverReadinessVerifierV1 {
    fn verify(
        &self,
        input: &CutoverReadinessInputV1,
        accepted_at_ms: i64,
    ) -> Result<VerifiedCutoverReadinessV1, CutoverReadinessError> {
        Self::verify(self, input, accepted_at_ms)
    }
}

#[derive(Clone, Debug)]
pub struct VerifiedCutoverReadinessV1 {
    shadow_import_id: Uuid,
    source_checksum_blake3: String,
    relation_digest_blake3: String,
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_freeze_digest_blake3: String,
    service_health_digest_blake3: String,
    g1_evidence: VerifiedG1EvidenceV1,
}

impl VerifiedCutoverReadinessV1 {
    pub const fn shadow_import_id(&self) -> Uuid {
        self.shadow_import_id
    }

    pub fn source_checksum_blake3(&self) -> &str {
        &self.source_checksum_blake3
    }

    pub fn relation_digest_blake3(&self) -> &str {
        &self.relation_digest_blake3
    }

    pub fn recovery_bundle_digest_blake3(&self) -> &str {
        &self.recovery_bundle_digest_blake3
    }

    pub fn legacy_scheduler_freeze_digest_blake3(&self) -> &str {
        &self.legacy_scheduler_freeze_digest_blake3
    }

    pub fn service_health_digest_blake3(&self) -> &str {
        &self.service_health_digest_blake3
    }

    pub const fn g1_evidence(&self) -> &VerifiedG1EvidenceV1 {
        &self.g1_evidence
    }

    pub fn config_cutover_spec(
        &self,
        cutover_run_id: Uuid,
        committed_at_ms: i64,
    ) -> ConfigCutoverSpec {
        ConfigCutoverSpec::new(
            cutover_run_id,
            self.shadow_import_id,
            self.source_checksum_blake3.clone(),
            self.relation_digest_blake3.clone(),
            self.recovery_bundle_digest_blake3.clone(),
            self.legacy_scheduler_freeze_digest_blake3.clone(),
            self.service_health_digest_blake3.clone(),
            G1EvidenceBindingV1::new(
                self.g1_evidence.source_revision(),
                self.g1_evidence.source_tree_digest(),
                self.g1_evidence.build_id(),
                self.g1_evidence.evidence_digest_blake3(),
            ),
            committed_at_ms,
        )
    }
}

pub struct FileCutoverRecoveryVerifierV1 {
    evidence_root: PathBuf,
    controller: Arc<dyn LegacySchedulerControllerV1>,
}

impl FileCutoverRecoveryVerifierV1 {
    pub fn new(
        evidence_root: PathBuf,
        controller: Arc<dyn LegacySchedulerControllerV1>,
    ) -> Result<Self, CutoverReadinessError> {
        let evidence_root = evidence_root
            .canonicalize()
            .map_err(|_| CutoverReadinessError::Invalid)?;
        if !evidence_root.is_dir() {
            return Err(CutoverReadinessError::Invalid);
        }
        Ok(Self {
            evidence_root,
            controller,
        })
    }

    pub fn verify(
        &self,
        recovery_bundle_manifest_path: &str,
        resume_after_catalog_commit: bool,
    ) -> Result<VerifiedCutoverRecoveryPlanV1, CutoverReadinessError> {
        let manifest_path = resolve_file(&self.evidence_root, recovery_bundle_manifest_path)?;
        let manifest_bytes = read_limited(&manifest_path, MAX_CONTROL_DOCUMENT_BYTES)?;
        let recovery_bundle_digest_blake3 = digest_bytes(&manifest_bytes);
        let manifest: RecoveryBundleManifestV1 =
            serde_json::from_slice(&manifest_bytes).map_err(|_| CutoverReadinessError::Invalid)?;
        let inventory = verify_recovery_bundle_files(&self.evidence_root, &manifest, None, None)?;

        let mut tasks = inventory
            .tasks
            .into_iter()
            .map(|task| LegacySchedulerRestoreTaskV1 {
                task_name: task.task_name,
                restore_enabled: task.enabled,
                task_xml: task.task_xml,
                xml_digest_blake3: task.xml_digest_blake3,
            })
            .collect::<Vec<_>>();
        tasks.sort_by(|left, right| left.task_name.cmp(&right.task_name));
        for task in &tasks {
            let Some(live_enabled) = self.controller.task_enabled(&task.task_name) else {
                return Err(CutoverReadinessError::Invalid);
            };
            if !self.controller.task_definition_matches(
                &task.task_name,
                &task.task_xml,
                &task.xml_digest_blake3,
            ) {
                return Err(CutoverReadinessError::Invalid);
            }
            let permitted = if resume_after_catalog_commit {
                !live_enabled || live_enabled == task.restore_enabled
            } else {
                !live_enabled
            };
            if !permitted {
                return Err(CutoverReadinessError::Invalid);
            }
        }

        let digest_document = LegacySchedulerRestorePlanDocumentV1 {
            schema_version: 1,
            recovery_bundle_digest_blake3: &recovery_bundle_digest_blake3,
            tasks: tasks
                .iter()
                .map(|task| LegacySchedulerRestoreTaskDocumentV1 {
                    task_name: &task.task_name,
                    restore_enabled: task.restore_enabled,
                    xml_digest_blake3: &task.xml_digest_blake3,
                })
                .collect(),
        };
        let canonical =
            serde_json::to_vec(&digest_document).map_err(|_| CutoverReadinessError::Invalid)?;
        Ok(VerifiedCutoverRecoveryPlanV1 {
            recovery_bundle_digest_blake3,
            legacy_scheduler_restore_digest_blake3: digest_bytes(&canonical),
            tasks,
        })
    }

    pub fn restore(
        &self,
        plan: &VerifiedCutoverRecoveryPlanV1,
    ) -> Result<(), LegacySchedulerControlError> {
        for task in &plan.tasks {
            if !self.controller.task_definition_matches(
                &task.task_name,
                &task.task_xml,
                &task.xml_digest_blake3,
            ) {
                return Err(LegacySchedulerControlError::CommandFailed);
            }
            let current = self
                .controller
                .task_enabled(&task.task_name)
                .ok_or(LegacySchedulerControlError::CommandFailed)?;
            if current == task.restore_enabled {
                continue;
            }
            if current || !task.restore_enabled {
                return Err(LegacySchedulerControlError::CommandFailed);
            }
            self.controller
                .set_task_enabled(&task.task_name, task.restore_enabled)?;
            if self.controller.task_enabled(&task.task_name) != Some(task.restore_enabled)
                || !self.controller.task_definition_matches(
                    &task.task_name,
                    &task.task_xml,
                    &task.xml_digest_blake3,
                )
            {
                return Err(LegacySchedulerControlError::CommandFailed);
            }
        }
        Ok(())
    }
}

impl std::fmt::Debug for FileCutoverRecoveryVerifierV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FileCutoverRecoveryVerifierV1")
            .field("evidence_root", &self.evidence_root)
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedCutoverRecoveryPlanV1 {
    recovery_bundle_digest_blake3: String,
    legacy_scheduler_restore_digest_blake3: String,
    tasks: Vec<LegacySchedulerRestoreTaskV1>,
}

impl VerifiedCutoverRecoveryPlanV1 {
    pub fn recovery_bundle_digest_blake3(&self) -> &str {
        &self.recovery_bundle_digest_blake3
    }

    pub fn legacy_scheduler_restore_digest_blake3(&self) -> &str {
        &self.legacy_scheduler_restore_digest_blake3
    }

    pub fn tasks(&self) -> &[LegacySchedulerRestoreTaskV1] {
        &self.tasks
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct LegacySchedulerRestoreTaskV1 {
    task_name: String,
    restore_enabled: bool,
    task_xml: String,
    xml_digest_blake3: String,
}

impl LegacySchedulerRestoreTaskV1 {
    pub fn task_name(&self) -> &str {
        &self.task_name
    }

    pub const fn restore_enabled(&self) -> bool {
        self.restore_enabled
    }

    pub fn xml_digest_blake3(&self) -> &str {
        &self.xml_digest_blake3
    }
}

impl std::fmt::Debug for LegacySchedulerRestoreTaskV1 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LegacySchedulerRestoreTaskV1")
            .field("task_name", &self.task_name)
            .field("restore_enabled", &self.restore_enabled)
            .field("task_xml", &"[redacted]")
            .field("xml_digest_blake3", &self.xml_digest_blake3)
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacySchedulerRestorePlanDocumentV1<'a> {
    schema_version: u32,
    recovery_bundle_digest_blake3: &'a str,
    tasks: Vec<LegacySchedulerRestoreTaskDocumentV1<'a>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LegacySchedulerRestoreTaskDocumentV1<'a> {
    task_name: &'a str,
    restore_enabled: bool,
    xml_digest_blake3: &'a str,
}

#[derive(Debug, Error)]
pub enum CutoverReadinessError {
    #[error("CONFIG_CUTOVER_READINESS_INVALID")]
    Invalid,
}

impl CutoverReadinessError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Invalid => "CONFIG_CUTOVER_READINESS_INVALID",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecoveryBundleManifestV1 {
    schema_version: u32,
    source_checksum_blake3: String,
    created_at_ms: i64,
    bundle_directory: String,
    items: Vec<RecoveryBundleItemV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct RecoveryBundleItemV1 {
    kind: String,
    relative_path: String,
    size_bytes: u64,
    digest_blake3: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SchedulerInventoryV1 {
    schema_version: u32,
    tasks: Vec<SchedulerInventoryTaskV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SchedulerInventoryTaskV1 {
    task_name: String,
    enabled: bool,
    task_xml: String,
    xml_digest_blake3: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LegacyFreezeReceiptV1 {
    schema_version: u32,
    source_checksum_blake3: String,
    recovery_bundle_digest_blake3: String,
    frozen_at_ms: i64,
    tasks: Vec<LegacyFreezeTaskV1>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct LegacyFreezeTaskV1 {
    task_name: String,
    before_enabled: bool,
    after_enabled: bool,
    xml_digest_blake3: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LiveServiceHealthEvidenceV1 {
    schema_version: u32,
    boot_id: String,
    phase: &'static str,
    operational_schema_version: i64,
    application_table_count: i64,
    execution_authority: &'static str,
    cutover_mode: &'static str,
    claims_enabled: bool,
    accepts_ipc: bool,
}

fn verify_recovery_bundle_files(
    evidence_root: &Path,
    manifest: &RecoveryBundleManifestV1,
    expected_source_checksum_blake3: Option<&str>,
    maximum_created_at_ms: Option<i64>,
) -> Result<SchedulerInventoryV1, CutoverReadinessError> {
    if manifest.schema_version != 1
        || !is_digest(&manifest.source_checksum_blake3)
        || expected_source_checksum_blake3
            .is_some_and(|expected| manifest.source_checksum_blake3 != expected)
        || manifest.created_at_ms < 0
        || maximum_created_at_ms.is_some_and(|maximum| manifest.created_at_ms > maximum)
        || manifest.items.len() != REQUIRED_BUNDLE_ITEM_KINDS.len()
    {
        return Err(CutoverReadinessError::Invalid);
    }
    let bundle_root = resolve_directory(evidence_root, &manifest.bundle_directory)?;
    let mut kinds = BTreeSet::new();
    let mut paths = BTreeSet::new();
    let mut scheduler_inventory_path = None;
    for item in &manifest.items {
        if !REQUIRED_BUNDLE_ITEM_KINDS.contains(&item.kind.as_str())
            || !kinds.insert(item.kind.as_str())
            || !paths.insert(item.relative_path.as_str())
            || !is_digest(&item.digest_blake3)
        {
            return Err(CutoverReadinessError::Invalid);
        }
        let item_path = resolve_file(&bundle_root, &item.relative_path)?;
        let metadata = item_path
            .metadata()
            .map_err(|_| CutoverReadinessError::Invalid)?;
        if metadata.len() != item.size_bytes || digest_file(&item_path)? != item.digest_blake3 {
            return Err(CutoverReadinessError::Invalid);
        }
        if item.kind == "scheduler_inventory" {
            scheduler_inventory_path = Some(item_path);
        }
    }
    let expected_kinds = REQUIRED_BUNDLE_ITEM_KINDS
        .into_iter()
        .collect::<BTreeSet<_>>();
    if kinds != expected_kinds {
        return Err(CutoverReadinessError::Invalid);
    }
    let scheduler_inventory_path =
        scheduler_inventory_path.ok_or(CutoverReadinessError::Invalid)?;
    let bytes = read_limited(&scheduler_inventory_path, MAX_CONTROL_DOCUMENT_BYTES)?;
    let inventory: SchedulerInventoryV1 =
        serde_json::from_slice(&bytes).map_err(|_| CutoverReadinessError::Invalid)?;
    validate_scheduler_inventory(&inventory)?;
    Ok(inventory)
}

fn validate_input(
    input: &CutoverReadinessInputV1,
    accepted_at_ms: i64,
) -> Result<(), CutoverReadinessError> {
    if accepted_at_ms < 0
        || !is_digest(&input.source_checksum_blake3)
        || !is_digest(&input.relation_digest_blake3)
        || !is_safe_relative_path(&input.g1_evidence_path)
        || !is_safe_relative_path(&input.recovery_bundle_manifest_path)
        || !is_safe_relative_path(&input.freeze_receipt_path)
    {
        return Err(CutoverReadinessError::Invalid);
    }
    Ok(())
}

fn validate_scheduler_inventory(
    inventory: &SchedulerInventoryV1,
) -> Result<(), CutoverReadinessError> {
    if inventory.schema_version != 1 || inventory.tasks.is_empty() {
        return Err(CutoverReadinessError::Invalid);
    }
    let mut names = BTreeSet::new();
    for task in &inventory.tasks {
        if !is_task_name(&task.task_name)
            || !names.insert(task.task_name.as_str())
            || task.task_xml.is_empty()
            || task.task_xml.len() > MAX_CONTROL_DOCUMENT_BYTES
            || !is_digest(&task.xml_digest_blake3)
            || digest_bytes(task.task_xml.as_bytes()) != task.xml_digest_blake3
            || enabled_from_task_xml(&task.task_xml) != Some(task.enabled)
        {
            return Err(CutoverReadinessError::Invalid);
        }
    }
    Ok(())
}

fn resolve_file(root: &Path, relative: &str) -> Result<PathBuf, CutoverReadinessError> {
    let path = resolve_existing(root, relative)?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| CutoverReadinessError::Invalid)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(CutoverReadinessError::Invalid);
    }
    Ok(path)
}

fn resolve_directory(root: &Path, relative: &str) -> Result<PathBuf, CutoverReadinessError> {
    let path = resolve_existing(root, relative)?;
    let metadata = fs::symlink_metadata(&path).map_err(|_| CutoverReadinessError::Invalid)?;
    if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
        return Err(CutoverReadinessError::Invalid);
    }
    Ok(path)
}

fn resolve_existing(root: &Path, relative: &str) -> Result<PathBuf, CutoverReadinessError> {
    if !is_safe_relative_path(relative) {
        return Err(CutoverReadinessError::Invalid);
    }
    let candidate = root.join(relative);
    let resolved = candidate
        .canonicalize()
        .map_err(|_| CutoverReadinessError::Invalid)?;
    if !resolved.starts_with(root) {
        return Err(CutoverReadinessError::Invalid);
    }
    Ok(resolved)
}

fn is_safe_relative_path(value: &str) -> bool {
    let path = Path::new(value);
    !value.is_empty()
        && value.len() <= 512
        && !path.is_absolute()
        && path.components().all(|component| {
            matches!(component, Component::Normal(_))
                && component
                    .as_os_str()
                    .to_str()
                    .is_some_and(|part| !part.is_empty())
        })
}

fn read_utf8_limited(path: &Path, limit: usize) -> Result<String, CutoverReadinessError> {
    String::from_utf8(read_limited(path, limit)?).map_err(|_| CutoverReadinessError::Invalid)
}

fn read_limited(path: &Path, limit: usize) -> Result<Vec<u8>, CutoverReadinessError> {
    let metadata = path
        .metadata()
        .map_err(|_| CutoverReadinessError::Invalid)?;
    if metadata.len() > limit as u64 {
        return Err(CutoverReadinessError::Invalid);
    }
    let file = File::open(path).map_err(|_| CutoverReadinessError::Invalid)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take((limit + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|_| CutoverReadinessError::Invalid)?;
    if bytes.len() > limit {
        return Err(CutoverReadinessError::Invalid);
    }
    Ok(bytes)
}

fn digest_file(path: &Path) -> Result<String, CutoverReadinessError> {
    let mut file = File::open(path).map_err(|_| CutoverReadinessError::Invalid)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|_| CutoverReadinessError::Invalid)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
        buffer[..count].fill(0);
    }
    buffer.fill(0);
    Ok(hasher.finalize().to_hex().to_string())
}

fn digest_bytes(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_task_name(value: &str) -> bool {
    value.starts_with(r"\DBBackup\")
        && value.len() > r"\DBBackup\".len()
        && value.len() <= 256
        && !value.chars().any(char::is_control)
}

#[cfg(windows)]
fn query_windows_task_enabled(task_name: &str) -> Option<bool> {
    enabled_from_task_xml(&query_windows_task_xml(task_name)?)
}

#[cfg(windows)]
fn query_windows_task_xml(task_name: &str) -> Option<String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if !is_task_name(task_name) {
        return None;
    }
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Query", "/TN", task_name, "/XML"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() || !output.stderr.is_empty() {
        return None;
    }
    decode_command_text(&output.stdout)
}

#[cfg(not(windows))]
fn query_windows_task_enabled(_task_name: &str) -> Option<bool> {
    None
}

#[cfg(windows)]
fn windows_task_definition_matches(
    task_name: &str,
    task_xml: &str,
    xml_digest_blake3: &str,
) -> bool {
    if !is_digest(xml_digest_blake3) || digest_bytes(task_xml.as_bytes()) != xml_digest_blake3 {
        return false;
    }
    let Some(expected) = canonical_task_definition(task_xml) else {
        return false;
    };
    query_windows_task_xml(task_name)
        .and_then(|xml| canonical_task_definition(&xml))
        .is_some_and(|live| live == expected)
}

#[cfg(not(windows))]
fn windows_task_definition_matches(
    _task_name: &str,
    _task_xml: &str,
    _xml_digest_blake3: &str,
) -> bool {
    false
}

#[cfg(windows)]
fn set_windows_task_enabled(
    task_name: &str,
    enabled: bool,
) -> Result<(), LegacySchedulerControlError> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    if !is_task_name(task_name) {
        return Err(LegacySchedulerControlError::CommandFailed);
    }
    let operation = if enabled { "/ENABLE" } else { "/DISABLE" };
    let output = std::process::Command::new("schtasks.exe")
        .args(["/Change", "/TN", task_name, operation])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|_| LegacySchedulerControlError::CommandFailed)?;
    if !output.status.success() || !output.stderr.is_empty() {
        return Err(LegacySchedulerControlError::CommandFailed);
    }
    Ok(())
}

#[cfg(not(windows))]
fn set_windows_task_enabled(
    _task_name: &str,
    _enabled: bool,
) -> Result<(), LegacySchedulerControlError> {
    Err(LegacySchedulerControlError::CommandFailed)
}

fn decode_command_text(bytes: &[u8]) -> Option<String> {
    if let Some(body) = bytes.strip_prefix(&[0xff, 0xfe]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).ok();
    }
    if let Some(body) = bytes.strip_prefix(&[0xfe, 0xff]) {
        if body.len() % 2 != 0 {
            return None;
        }
        let units = body
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<_>>();
        return String::from_utf16(&units).ok();
    }
    String::from_utf8(bytes.to_vec()).ok()
}

fn enabled_from_task_xml(xml: &str) -> Option<bool> {
    settings_enabled_span(xml).map(|(enabled, _)| enabled)
}

fn canonical_task_definition(xml: &str) -> Option<String> {
    let (_, enabled_span) = settings_enabled_span(xml)?;
    let without_task_enabled = if let Some(span) = enabled_span {
        let mut value = String::with_capacity(xml.len() - span.len());
        value.push_str(&xml[..span.start]);
        value.push_str(&xml[span.end..]);
        value
    } else {
        xml.to_owned()
    };
    Some(remove_inter_element_whitespace(&without_task_enabled))
}

fn settings_enabled_span(xml: &str) -> Option<(bool, Option<std::ops::Range<usize>>)> {
    const SETTINGS: &str = "<Settings";
    const SETTINGS_CLOSE: &str = "</Settings>";
    const ENABLED: &str = "<Enabled>true</Enabled>";
    const DISABLED: &str = "<Enabled>false</Enabled>";

    let settings_starts = xml
        .match_indices(SETTINGS)
        .filter_map(|(offset, _)| {
            xml.as_bytes()
                .get(offset + SETTINGS.len())
                .filter(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'>'))
                .map(|_| offset)
        })
        .collect::<Vec<_>>();
    if settings_starts.len() != 1 {
        return None;
    }
    let settings_start = settings_starts[0];
    let opening_end = settings_start + xml[settings_start..].find('>')?;
    let opening = &xml[settings_start..=opening_end];
    if opening[..opening.len() - 1].trim_end().ends_with('/') {
        return (!xml.contains(SETTINGS_CLOSE)).then_some((true, None));
    }

    let closing_starts = xml
        .match_indices(SETTINGS_CLOSE)
        .map(|(offset, _)| offset)
        .collect::<Vec<_>>();
    if closing_starts.len() != 1 || closing_starts[0] <= opening_end {
        return None;
    }
    let body_start = opening_end + 1;
    let body_end = closing_starts[0];
    let body = &xml[body_start..body_end];
    let enabled = body.match_indices(ENABLED).collect::<Vec<_>>();
    let disabled = body.match_indices(DISABLED).collect::<Vec<_>>();
    let recognized_count = enabled.len() + disabled.len();
    if body.matches("<Enabled").count() != recognized_count
        || body.matches("</Enabled>").count() != recognized_count
    {
        return None;
    }
    match (enabled.as_slice(), disabled.as_slice()) {
        ([], []) => Some((true, None)),
        ([(offset, _)], []) => Some((
            true,
            Some(body_start + *offset..body_start + *offset + ENABLED.len()),
        )),
        ([], [(offset, _)]) => Some((
            false,
            Some(body_start + *offset..body_start + *offset + DISABLED.len()),
        )),
        _ => None,
    }
}

fn remove_inter_element_whitespace(xml: &str) -> String {
    let mut output = String::with_capacity(xml.len());
    let mut whitespace = String::new();
    for character in xml.chars() {
        if character.is_whitespace() {
            whitespace.push(character);
            continue;
        }
        if character == '<' && (output.is_empty() || output.ends_with('>')) {
            whitespace.clear();
        } else {
            output.push_str(&whitespace);
            whitespace.clear();
        }
        output.push(character);
    }
    if !output.ends_with('>') {
        output.push_str(&whitespace);
    }
    output
}

#[cfg(test)]
mod tests {
    use super::{
        canonical_task_definition, decode_command_text, enabled_from_task_xml,
        set_windows_task_enabled, LegacySchedulerControlError,
    };

    #[test]
    fn scheduler_probe_decodes_utf8_and_utf16_without_guessing_ambiguous_state() {
        let disabled = "<Task><Settings><Enabled>false</Enabled></Settings></Task>";
        assert_eq!(
            enabled_from_task_xml(&decode_command_text(disabled.as_bytes()).expect("UTF-8 XML")),
            Some(false)
        );

        let enabled = "<Task><Settings><Enabled>true</Enabled></Settings></Task>";
        let mut utf16 = vec![0xff, 0xfe];
        for unit in enabled.encode_utf16() {
            utf16.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(
            enabled_from_task_xml(&decode_command_text(&utf16).expect("UTF-16LE XML")),
            Some(true)
        );
        assert_eq!(
            enabled_from_task_xml(
                "<Task><Settings><Enabled>true</Enabled><Enabled>false</Enabled></Settings></Task>"
            ),
            None
        );
        assert_eq!(
            enabled_from_task_xml("<Task><Settings /></Task>"),
            Some(true)
        );
    }

    #[test]
    fn scheduler_probe_reads_task_settings_without_confusing_trigger_enablement() {
        let xml = concat!(
            "<Task>",
            "<Triggers><CalendarTrigger><Enabled>true</Enabled></CalendarTrigger></Triggers>",
            "<Settings><Enabled>false</Enabled></Settings>",
            "</Task>"
        );
        assert_eq!(enabled_from_task_xml(xml), Some(false));
    }

    #[test]
    fn scheduler_controller_rejects_non_dbbackup_task_without_spawning_a_command() {
        assert_eq!(
            set_windows_task_enabled(r"\Microsoft\Windows\Update", true),
            Err(LegacySchedulerControlError::CommandFailed)
        );
        assert_eq!(
            set_windows_task_enabled(r"\DBBackup\", true),
            Err(LegacySchedulerControlError::CommandFailed)
        );
    }

    #[test]
    fn task_definition_canonicalization_ignores_enabled_state_and_serialization_whitespace() {
        let implicit = concat!(
            "<Task>\r\n",
            "  <Triggers><CalendarTrigger><Enabled>true</Enabled></CalendarTrigger></Triggers>\r\n",
            "  <Settings>\r\n  </Settings>\r\n",
            "</Task>\r\n"
        );
        let disabled = concat!(
            "<Task>\n",
            "<Triggers><CalendarTrigger><Enabled>true</Enabled></CalendarTrigger></Triggers>\n",
            "<Settings>\n<Enabled>false</Enabled>\n</Settings>\n",
            "</Task>"
        );
        assert_eq!(
            canonical_task_definition(implicit),
            canonical_task_definition(disabled)
        );
        let trigger_drift = disabled.replacen(
            "<CalendarTrigger><Enabled>true</Enabled>",
            "<CalendarTrigger><Enabled>false</Enabled>",
            1,
        );
        assert_ne!(
            canonical_task_definition(disabled),
            canonical_task_definition(&trigger_drift)
        );
        assert_eq!(
            canonical_task_definition(
                "<Task><Settings><Enabled>true</Enabled><Enabled>false</Enabled></Settings></Task>"
            ),
            None
        );
    }
}
