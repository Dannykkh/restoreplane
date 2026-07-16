// crates/dbbackup-pipeline/src/stub_source.rs
//! StubSource — DB 없이 파이프라인을 end-to-end로 구동하기 위한 BackupSource 구현.
//! 결정적 바이트를 in-memory reader(Cursor)로 방출하고, 자식 종료(exit)를 설정 가능하게 흉내낸다.
//! 이것이 "라이브 DB 없이 전체 백업 경로를 검증"하게 해주는 핵심 테스트 이중체다.

use async_trait::async_trait;
use uuid::Uuid;

use crate::artifact::{DumpArtifact, DumpExit, DumpStream};
use crate::source::{BackupSource, DumpPartKind, DumpPlan, DumpPlanPart};

pub struct StubSource {
    pub job: String,
    pub alias: String,
    /// 방출할 결정적 평문 바이트(예: echo 같은 고정 데이터).
    pub payload: Vec<u8>,
    /// 자식 exit 성공 여부. false면 스트림 EOF에 도달해도 S05가 스풀을 폐기해야 한다(R2).
    pub exit_success: bool,
    /// 진단/치명 패턴 검사에 쓰이는 stderr tail.
    pub stderr_tail: String,
}

impl StubSource {
    /// 성공 소스(결정적 payload).
    pub fn ok(job: impl Into<String>, alias: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            job: job.into(),
            alias: alias.into(),
            payload,
            exit_success: true,
            stderr_tail: String::new(),
        }
    }

    /// 실패 소스(EOF는 정상 도달하지만 exit≠0 — truncation-as-success 버그 재현 방지 테스트용).
    pub fn failing(job: impl Into<String>, alias: impl Into<String>, payload: Vec<u8>) -> Self {
        Self {
            job: job.into(),
            alias: alias.into(),
            payload,
            exit_success: false,
            stderr_tail: "stub source signalled failure".to_string(),
        }
    }

    /// 테스트가 plan() 없이도 파트를 만들 수 있게 하는 헬퍼.
    pub fn make_part(&self) -> DumpPlanPart {
        DumpPlanPart {
            backup_id: Uuid::new_v4(),
            job: self.job.clone(),
            source_alias: self.alias.clone(),
            kind: DumpPartKind::Database {
                name: self.alias.clone(),
            },
            pairs_with_globals_backup_id: None,
            dump_tool: "stub",
            dump_tool_path: std::path::PathBuf::from("stub"),
            dump_tool_version: "stub 0.0.0".to_string(),
            dump_format: "custom",
            dump_flags: "stub --deterministic".to_string(),
            extensions: Vec::new(),
            charset: None,
            collation: None,
        }
    }
}

#[async_trait]
impl BackupSource for StubSource {
    async fn plan(&self) -> anyhow::Result<DumpPlan> {
        Ok(DumpPlan {
            globals_part: None,
            db_parts: vec![self.make_part()],
            warnings: Vec::new(),
        })
    }

    fn spawn(&self, _part: &DumpPlanPart) -> anyhow::Result<DumpArtifact> {
        // std::io::Cursor<Vec<u8>>는 tokio::io::AsyncRead를 구현한다(in-memory reader).
        let reader = std::io::Cursor::new(self.payload.clone());
        let success = self.exit_success;
        let code = if success { Some(0) } else { Some(1) };
        let stderr_tail = self.stderr_tail.clone();
        let wait = Box::pin(async move {
            Ok(DumpExit {
                code,
                success,
                stderr_tail,
            })
        });
        Ok(DumpArtifact::Stream(DumpStream {
            reader: Box::new(reader),
            wait,
        }))
    }
}
