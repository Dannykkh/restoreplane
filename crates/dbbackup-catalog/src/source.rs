// S08 8.4 — ManifestSource. S08은 S06/S07를 직접 의존하면 순환이 생기므로 자체적으로 작은
// 트레이트를 정의하고, 로컬 파일시스템 구현체(LocalManifestSource)만 여기서 제공한다.
// S6/S7 완성 시 S3Dest/SshDest가 같은 트레이트를 구현하면 rebuild 로직을 그대로 재사용한다.
//
// 섹션 원본은 async_trait이지만 카탈로그/rebuild 경로는 동기로 충분하므로 동기 트레이트로 둔다.

use std::path::PathBuf;

/// 목적지에서 매니페스트/객체 바이트를 읽는 최소 계약. key는 목적지 상대 경로.
pub trait ManifestSource: Send + Sync {
    /// job_filter가 Some이면 해당 job 접두 키만, None이면 전부. 반환은 `*.manifest.json` 상대 키.
    fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>>;
    /// 임의 객체(매니페스트 JSON 또는 스풀 암호문) 바이트를 읽는다.
    fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>>;
}

/// 1차 walking skeleton(PG→Local)용 구현. root 아래를 재귀 탐색한다.
pub struct LocalManifestSource {
    pub root: PathBuf,
}

impl LocalManifestSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 표준 라이브러리만으로 재귀 수집(walkdir 의존 회피 — 이 한 곳에서만 필요).
    fn collect(dir: &std::path::Path, out: &mut Vec<PathBuf>) -> std::io::Result<()> {
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() {
                Self::collect(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
}

impl ManifestSource for LocalManifestSource {
    fn list_manifests(&self, job_filter: Option<&str>) -> anyhow::Result<Vec<String>> {
        let mut files = Vec::new();
        if self.root.exists() {
            Self::collect(&self.root, &mut files)?;
        }
        let mut out = Vec::new();
        for p in files {
            let name = p.to_string_lossy();
            if !name.ends_with(".manifest.json") {
                continue;
            }
            let rel = p
                .strip_prefix(&self.root)
                .unwrap_or(&p)
                .to_string_lossy()
                .replace('\\', "/");
            if let Some(job) = job_filter {
                if !rel.starts_with(job) {
                    continue;
                }
            }
            out.push(rel);
        }
        out.sort();
        Ok(out)
    }

    fn read_bytes(&self, key: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.root.join(key))?)
    }
}
