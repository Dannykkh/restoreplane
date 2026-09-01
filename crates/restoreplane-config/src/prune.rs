// crates/restoreplane-config/src/prune.rs
//
// prune 전용 프로파일 — 삭제 권한을 가진 격리 신원을 담는다. plan.md/team-review.md의
// H1/H2("prune는 백업 호스트에서 실행 금지, 별도 프로파일, 무인 run이 로드 안 함")를
// 구조적으로 강제하기 위해 schema.rs의 Config와 완전히 별도 타입으로 둔다.
// restoreplane-cli의 `run` 커맨드 모듈은 이 모듈을 import하지 않는다(코드 리뷰 체크 항목).
use std::path::Path;

use restoreplane_core::EncField;
use serde::{Deserialize, Serialize};

use crate::{schema::Meta, ConfigError};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneProfile {
    pub meta: Meta,
    /// 이 신원이 prune을 수행할 대상 Destination.name (main config 참조는 이름 문자열로만,
    /// Config 타입을 직접 참조하지 않는다).
    pub destination: String,
    pub identity: PruneIdentity,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PruneIdentity {
    pub access_key_id: EncField,
    pub secret_access_key: EncField,
}

impl PruneProfile {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| ConfigError::Io(path.to_path_buf(), e))?;
        let raw = raw.strip_prefix('\u{FEFF}').unwrap_or(&raw);
        toml::from_str(raw).map_err(ConfigError::Parse)
    }
}
