use std::path::PathBuf;
use std::pin::Pin;
use std::task::{Context, Poll};

use async_trait::async_trait;
use restoreplane_pipeline::{
    BackupSource, DumpArtifact, DumpExit, DumpPartKind, DumpPlan, DumpPlanPart, DumpStream,
};
use tokio::io::{AsyncRead, ReadBuf};

use crate::{
    online_backup_to_path, probe_sqlite_file, BackupCancellationToken, OnlineBackupOptions,
};

pub struct SqliteSource {
    job: String,
    alias: String,
    file_path: PathBuf,
    options: OnlineBackupOptions,
    cancellation: BackupCancellationToken,
}

impl SqliteSource {
    pub fn new(
        job: impl Into<String>,
        alias: impl Into<String>,
        file_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            job: job.into(),
            alias: alias.into(),
            file_path: file_path.into(),
            options: OnlineBackupOptions::default(),
            cancellation: BackupCancellationToken::default(),
        }
    }

    pub fn with_options(mut self, options: OnlineBackupOptions) -> Self {
        self.options = options;
        self
    }

    pub fn with_cancellation_token(mut self, cancellation: BackupCancellationToken) -> Self {
        self.cancellation = cancellation;
        self
    }
}

#[async_trait]
impl BackupSource for SqliteSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        let probe = probe_sqlite_file(&self.file_path)?;
        let database_name = self
            .file_path
            .file_name()
            .and_then(|value| value.to_str())
            .unwrap_or("main")
            .to_owned();
        Ok(DumpPlan {
            globals_part: None,
            db_parts: vec![DumpPlanPart {
                backup_id: uuid::Uuid::new_v4(),
                job: self.job.clone(),
                source_alias: self.alias.clone(),
                kind: DumpPartKind::Database {
                    name: database_name,
                },
                pairs_with_globals_backup_id: None,
                dump_tool: "sqlite-online-backup",
                dump_tool_path: PathBuf::from("embedded://rusqlite/0.41.0"),
                dump_tool_sha256: None,
                dump_tool_dependencies: Vec::new(),
                dump_tool_version: format!("rusqlite 0.41.0 / SQLite {}", probe.sqlite_version()),
                dump_format: "sqlite-database",
                dump_flags: format!(
                    "online-backup pages-per-step={} busy-timeout-ms={}",
                    self.options.pages_per_step,
                    self.options.busy_timeout.as_millis()
                ),
                extensions: Vec::new(),
                charset: Some("UTF-8".to_owned()),
                collation: None,
            }],
            warnings: Vec::new(),
        })
    }

    fn spawn(&self, part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        if part.dump_tool != "sqlite-online-backup" {
            anyhow::bail!("SQLITE_PLAN_PART_INVALID");
        }
        let temp = tempfile::tempdir()?;
        let output = temp.path().join("backup.sqlite");
        online_backup_to_path(&self.file_path, &output, &self.options, &self.cancellation)?;
        let file = std::fs::File::open(output)?;
        let reader = OwnedTempReader {
            file: tokio::fs::File::from_std(file),
            _temp: temp,
        };
        Ok(DumpArtifact::Stream(DumpStream {
            reader: Box::new(reader),
            wait: Box::pin(async {
                Ok(DumpExit {
                    code: Some(0),
                    success: true,
                    stderr_tail: String::new(),
                })
            }),
            abort: None,
        }))
    }
}

struct OwnedTempReader {
    file: tokio::fs::File,
    _temp: tempfile::TempDir,
}

impl AsyncRead for OwnedTempReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.file).poll_read(context, buffer)
    }
}
