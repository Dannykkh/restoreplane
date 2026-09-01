use serde::{Deserialize, Serialize};

use crate::ApplicationError;

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AccessClass {
    Read,
    Operate,
    Admin,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MutationClass {
    ReadOnly,
    Mutate,
    Enqueue,
    Destructive,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfirmationAction {
    RestoreOverwrite,
    RetentionDelete,
    ConnectionArchive,
    DestinationArchive,
    SecretRotate,
    ConfigCutover,
    ControlStoreRestore,
    AclChange,
}

impl ConfirmationAction {
    pub const ALL: [Self; 8] = [
        Self::RestoreOverwrite,
        Self::RetentionDelete,
        Self::ConnectionArchive,
        Self::DestinationArchive,
        Self::SecretRotate,
        Self::ConfigCutover,
        Self::ControlStoreRestore,
        Self::AclChange,
    ];

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RestoreOverwrite => "restore_overwrite",
            Self::RetentionDelete => "retention_delete",
            Self::ConnectionArchive => "connection_archive",
            Self::DestinationArchive => "destination_archive",
            Self::SecretRotate => "secret_rotate",
            Self::ConfigCutover => "config_cutover",
            Self::ControlStoreRestore => "control_store_restore",
            Self::AclChange => "acl_change",
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicationCommand {
    ServiceHealth,
    ServiceDiagnostics,
    SchemaStatus,
    WorkspaceList,
    WorkspaceCreate,
    ConnectionList,
    TargetList,
    DiscoveryResultList,
    DestinationList,
    PlanList,
    EngineMatrix,
    SnapshotList,
    SnapshotGet,
    CatalogRebuildPreview,
    RetentionDryRun,
    RetentionControlSet,
    RetentionTombstone,
    ActivityList,
    WorkGet,
    SettingsRead,
    ConnectionSave,
    ConnectionProbe,
    DiscoveryRun,
    TargetSelect,
    BackupScopeList,
    BackupScopeDiscoveryRun,
    BackupScopeObjectList,
    BackupScopeSave,
    DestinationSave,
    DestinationProbe,
    PlanSave,
    PlanPublish,
    SettingsUpdate,
    BackupEnqueue,
    VerificationEnqueue,
    RestorePreflight,
    RestoreEnqueueSafe,
    RestoreEnqueueOverwrite,
    RepairEnqueue,
    WorkCancel,
    ConfirmationPrepare,
    RetentionDelete,
    ConnectionArchive,
    DestinationArchive,
    SecretCreate,
    SecretRotate,
    ConfigShadowImport,
    ConfigCutover,
    ControlStoreBackup,
    ControlStoreRestore,
    AclChange,
    ServiceDrain,
    ServiceUpdateCommit,
    SupportBundleCreate,
    ToolRescan,
}

impl ApplicationCommand {
    pub const ALL: [Self; 55] = [
        Self::ServiceHealth,
        Self::ServiceDiagnostics,
        Self::SchemaStatus,
        Self::WorkspaceList,
        Self::WorkspaceCreate,
        Self::ConnectionList,
        Self::TargetList,
        Self::DiscoveryResultList,
        Self::DestinationList,
        Self::PlanList,
        Self::EngineMatrix,
        Self::SnapshotList,
        Self::SnapshotGet,
        Self::CatalogRebuildPreview,
        Self::RetentionDryRun,
        Self::RetentionControlSet,
        Self::RetentionTombstone,
        Self::ActivityList,
        Self::WorkGet,
        Self::SettingsRead,
        Self::ConnectionSave,
        Self::ConnectionProbe,
        Self::DiscoveryRun,
        Self::TargetSelect,
        Self::BackupScopeList,
        Self::BackupScopeDiscoveryRun,
        Self::BackupScopeObjectList,
        Self::BackupScopeSave,
        Self::DestinationSave,
        Self::DestinationProbe,
        Self::PlanSave,
        Self::PlanPublish,
        Self::SettingsUpdate,
        Self::BackupEnqueue,
        Self::VerificationEnqueue,
        Self::RestorePreflight,
        Self::RestoreEnqueueSafe,
        Self::RestoreEnqueueOverwrite,
        Self::RepairEnqueue,
        Self::WorkCancel,
        Self::ConfirmationPrepare,
        Self::RetentionDelete,
        Self::ConnectionArchive,
        Self::DestinationArchive,
        Self::SecretCreate,
        Self::SecretRotate,
        Self::ConfigShadowImport,
        Self::ConfigCutover,
        Self::ControlStoreBackup,
        Self::ControlStoreRestore,
        Self::AclChange,
        Self::ServiceDrain,
        Self::ServiceUpdateCommit,
        Self::SupportBundleCreate,
        Self::ToolRescan,
    ];

    pub const fn descriptor(self) -> CommandDescriptor {
        match self {
            Self::ServiceHealth => read("service.health"),
            Self::ServiceDiagnostics => read("service.diagnostics"),
            Self::SchemaStatus => read("schema.status"),
            Self::WorkspaceList => read("workspace.list"),
            Self::WorkspaceCreate => mutate("workspace.create"),
            Self::ConnectionList => read("connection.list"),
            Self::TargetList => read("target.list"),
            Self::DiscoveryResultList => read("discovery.result_list"),
            Self::DestinationList => read("destination.list"),
            Self::PlanList => read("plan.list"),
            Self::EngineMatrix => read("engine.matrix"),
            Self::SnapshotList => read("snapshot.list"),
            Self::SnapshotGet => read("snapshot.get"),
            Self::CatalogRebuildPreview => read("catalog.rebuild_preview"),
            Self::RetentionDryRun => read("retention.dry_run"),
            Self::RetentionControlSet => mutate("retention.control_set"),
            Self::RetentionTombstone => mutate("retention.tombstone"),
            Self::ActivityList => read("activity.list"),
            Self::WorkGet => read("work.get"),
            Self::SettingsRead => read("settings.read"),
            Self::ConnectionSave => mutate("connection.save"),
            Self::ConnectionProbe => operate("connection.probe"),
            Self::DiscoveryRun => operate("discovery.run"),
            Self::TargetSelect => operate("target.select"),
            Self::BackupScopeList => read("backup_scope.list"),
            Self::BackupScopeDiscoveryRun => operate("backup_scope.discovery_run"),
            Self::BackupScopeObjectList => read("backup_scope.object_list"),
            Self::BackupScopeSave => mutate("backup_scope.save"),
            Self::DestinationSave => mutate("destination.save"),
            Self::DestinationProbe => operate("destination.probe"),
            Self::PlanSave => mutate("plan.save"),
            Self::PlanPublish => mutate("plan.publish"),
            Self::SettingsUpdate => mutate("settings.update"),
            Self::BackupEnqueue => enqueue("backup.enqueue", true),
            Self::VerificationEnqueue => enqueue("verification.enqueue", true),
            Self::RestorePreflight => read("restore.preflight"),
            Self::RestoreEnqueueSafe => enqueue("restore.enqueue_safe", true),
            Self::RestoreEnqueueOverwrite => destructive(
                "restore.enqueue_overwrite",
                ConfirmationAction::RestoreOverwrite,
                true,
            ),
            Self::RepairEnqueue => enqueue("repair.enqueue", true),
            Self::WorkCancel => operate("work.cancel"),
            Self::ConfirmationPrepare => mutate("confirmation.prepare"),
            Self::RetentionDelete => destructive(
                "retention.delete",
                ConfirmationAction::RetentionDelete,
                false,
            ),
            Self::ConnectionArchive => destructive(
                "connection.archive",
                ConfirmationAction::ConnectionArchive,
                false,
            ),
            Self::DestinationArchive => destructive(
                "destination.archive",
                ConfirmationAction::DestinationArchive,
                false,
            ),
            Self::SecretCreate => mutate("secret.create"),
            Self::SecretRotate => {
                destructive("secret.rotate", ConfirmationAction::SecretRotate, false)
            }
            Self::ConfigShadowImport => mutate("config.shadow_import"),
            Self::ConfigCutover => {
                destructive("config.cutover", ConfirmationAction::ConfigCutover, true)
            }
            Self::ControlStoreBackup => mutate("control_store.backup"),
            Self::ControlStoreRestore => destructive(
                "control_store.restore",
                ConfirmationAction::ControlStoreRestore,
                false,
            ),
            Self::AclChange => destructive("acl.change", ConfirmationAction::AclChange, false),
            Self::ServiceDrain => mutate("service.drain"),
            Self::ServiceUpdateCommit => mutate("service.update_commit"),
            Self::SupportBundleCreate => operate("support_bundle.create"),
            Self::ToolRescan => CommandDescriptor::new(
                "tool.rescan",
                AccessClass::Admin,
                MutationClass::Enqueue,
                None,
                false,
            ),
        }
    }
}

impl TryFrom<&str> for ApplicationCommand {
    type Error = ApplicationError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::ALL
            .into_iter()
            .find(|command| command.descriptor().name == value)
            .ok_or(ApplicationError::UnknownCommand)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CommandDescriptor {
    name: &'static str,
    access: AccessClass,
    mutation: MutationClass,
    confirmation: Option<ConfirmationAction>,
    requires_g1: bool,
}

impl CommandDescriptor {
    pub const fn new(
        name: &'static str,
        access: AccessClass,
        mutation: MutationClass,
        confirmation: Option<ConfirmationAction>,
        requires_g1: bool,
    ) -> Self {
        Self {
            name,
            access,
            mutation,
            confirmation,
            requires_g1,
        }
    }

    pub const fn name(self) -> &'static str {
        self.name
    }

    pub const fn access(self) -> AccessClass {
        self.access
    }

    pub const fn mutation(self) -> MutationClass {
        self.mutation
    }

    pub const fn confirmation(self) -> Option<ConfirmationAction> {
        self.confirmation
    }

    pub const fn requires_g1(self) -> bool {
        self.requires_g1
    }

    pub const fn requires_idempotency_key(self) -> bool {
        !matches!(self.mutation, MutationClass::ReadOnly)
    }

    pub const fn is_long_running(self) -> bool {
        matches!(
            self.mutation,
            MutationClass::Enqueue | MutationClass::Destructive
        ) && self.requires_g1
    }
}

const fn read(name: &'static str) -> CommandDescriptor {
    CommandDescriptor::new(
        name,
        AccessClass::Read,
        MutationClass::ReadOnly,
        None,
        false,
    )
}

const fn operate(name: &'static str) -> CommandDescriptor {
    CommandDescriptor::new(
        name,
        AccessClass::Operate,
        MutationClass::Mutate,
        None,
        false,
    )
}

const fn mutate(name: &'static str) -> CommandDescriptor {
    CommandDescriptor::new(name, AccessClass::Admin, MutationClass::Mutate, None, false)
}

const fn enqueue(name: &'static str, requires_g1: bool) -> CommandDescriptor {
    CommandDescriptor::new(
        name,
        AccessClass::Operate,
        MutationClass::Enqueue,
        None,
        requires_g1,
    )
}

const fn destructive(
    name: &'static str,
    confirmation: ConfirmationAction,
    requires_g1: bool,
) -> CommandDescriptor {
    CommandDescriptor::new(
        name,
        AccessClass::Admin,
        MutationClass::Destructive,
        Some(confirmation),
        requires_g1,
    )
}
