// S09 9.6 — Verifier. Restorer의 복호/복원 기계 위에 레벨별 단언을 얹는다.
//   L1 파일 무결성: 재복호+재압축해제 + blake3 == manifest.plaintext_blake3 (Docker 불필요, 매 백업).
//   L2 포맷 유효성: pg_restore --list로 custom TOC 파싱(데이터 미전개). (pg 도구 필요)
//   L3 실제 복원: 샌드박스 복원 후 스키마 해시/row count 대조. (Docker 필요)
//
// "verified"는 절대 단독으로 쓰지 않는다 — 항상 레벨과 함께 기록한다(예: "verified(L1)").

use dbbackup_core::Engine;

use crate::asserts::{compare_row_counts, normalize_and_hash_schema, version_gap};
use crate::decrypt::decrypt_and_decompress_bytes;
use crate::inputs::RestoreInputs;
use crate::restorer::{RestoreTarget, Restorer};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VerifyLevel {
    L1 = 1,
    L2 = 2,
    L3 = 3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VerifyResult {
    Pass,
    Fail,
}

pub struct VerifyOutcome {
    pub level: VerifyLevel,
    pub result: VerifyResult,
    pub restore_target_version: Option<String>,
    pub restored_rows: Option<u64>,
    pub schema_hash: Option<String>,
    pub notes: String,
}

impl VerifyOutcome {
    /// "verified(Ln)" 규율 준수 라벨.
    pub fn label(&self) -> String {
        let tag = match self.result {
            VerifyResult::Pass => "verified",
            VerifyResult::Fail => "FAILED",
        };
        format!("{tag}(L{})", self.level as u8)
    }
}

pub struct Verifier<'a> {
    pub restorer: Restorer<'a>,
}

impl<'a> Verifier<'a> {
    pub async fn verify(
        &self,
        inputs: &RestoreInputs,
        level: VerifyLevel,
    ) -> anyhow::Result<VerifyOutcome> {
        match level {
            VerifyLevel::L1 => self.l1_file_integrity(inputs).await,
            VerifyLevel::L2 => self.l2_format_validity(inputs).await,
            VerifyLevel::L3 => self.l3_full_restore(inputs).await,
        }
    }

    /// L1: 재복호+재압축해제가 끝까지 되고 재계산 blake3 == 매니페스트 plaintext_blake3. Docker 불필요.
    pub async fn l1_file_integrity(&self, inputs: &RestoreInputs) -> anyhow::Result<VerifyOutcome> {
        let m = &inputs.db_manifest;
        let expected = m.plaintext_blake3.clone();
        let cipher = self.restorer.read_cipher(m).await?;

        // 복호/압축해제 실패(변조/절단)는 곧 무결성 실패 → Fail(에러 전파가 아니라 결과로 기록).
        let (result, notes) = match decrypt_and_decompress_bytes(&cipher, &self.restorer.dek) {
            Ok(plain) => {
                let got = blake3::hash(&plain).to_hex().to_string();
                if got == expected {
                    (
                        VerifyResult::Pass,
                        "재복호+재압축해제 성공, 평문 blake3 일치".to_string(),
                    )
                } else {
                    (
                        VerifyResult::Fail,
                        format!("평문 blake3 불일치: expected={expected} got={got}"),
                    )
                }
            }
            Err(e) => (
                VerifyResult::Fail,
                format!("복호/압축해제 실패(변조/절단 의심): {e}"),
            ),
        };

        Ok(VerifyOutcome {
            level: VerifyLevel::L1,
            result,
            restore_target_version: None,
            restored_rows: None,
            schema_hash: None,
            notes,
        })
    }

    /// L2: PG custom은 pg_restore --list로 TOC 파싱(데이터 미전개, 실행 위험 없음). 그 외는 헤더 검사.
    pub async fn l2_format_validity(
        &self,
        inputs: &RestoreInputs,
    ) -> anyhow::Result<VerifyOutcome> {
        let m = &inputs.db_manifest;
        let cipher = self.restorer.read_cipher(m).await?;
        let plain = match decrypt_and_decompress_bytes(&cipher, &self.restorer.dek) {
            Ok(p) => p,
            Err(e) => {
                return Ok(VerifyOutcome {
                    level: VerifyLevel::L2,
                    result: VerifyResult::Fail,
                    restore_target_version: None,
                    restored_rows: None,
                    schema_hash: None,
                    notes: format!("복호 실패: {e}"),
                });
            }
        };

        let is_pg = matches!(m.engine, Engine::Postgresql);
        let (pass, notes) = if is_pg && m.dump_format == "custom" {
            pg_restore_list_ok(&plain)
        } else {
            plain_dump_l2_result(m.engine, &plain)
        };

        Ok(VerifyOutcome {
            level: VerifyLevel::L2,
            result: if pass {
                VerifyResult::Pass
            } else {
                VerifyResult::Fail
            },
            restore_target_version: None,
            restored_rows: None,
            schema_hash: None,
            notes,
        })
    }

    /// L3: 샌드박스 실제 복원 → 스키마 해시/row count 대조(매니페스트 캡처값 기준). Docker 필요.
    pub async fn l3_full_restore(&self, inputs: &RestoreInputs) -> anyhow::Result<VerifyOutcome> {
        let outcome = self
            .restorer
            .restore(inputs, RestoreTarget::Sandbox)
            .await?;
        let sandbox = outcome
            .sandbox
            .ok_or_else(|| anyhow::anyhow!("L3는 샌드박스 필수"))?;
        let target_version = outcome.restore_target_version.clone();

        let raw_schema = sandbox.dump_schema_only().await?;
        let restored_schema_hash = normalize_and_hash_schema(&raw_schema);
        let restored_counts = sandbox.count_rows_per_table().await?;
        let (pass, mut notes, total_restored) = evaluate_restore_oracle(
            inputs.schema_hash.as_deref(),
            &inputs.source_row_counts,
            &restored_schema_hash,
            &restored_counts,
        );
        if let Some(src_ver) = &inputs.server_version {
            if version_gap(src_ver, &target_version) {
                notes.push_str(&format!(
                    " [주의: 원본({src_ver})과 다른 버전({target_version})으로 검증됨 — 완전한 보증 아님]"
                ));
            }
        }

        Ok(VerifyOutcome {
            level: VerifyLevel::L3,
            result: if pass {
                VerifyResult::Pass
            } else {
                VerifyResult::Fail
            },
            restore_target_version: Some(target_version),
            restored_rows: Some(total_restored),
            schema_hash: Some(restored_schema_hash),
            notes,
        })
        // sandbox는 여기서 Drop → docker rm -f(봉쇄).
    }
}

fn evaluate_restore_oracle(
    expected_schema_hash: Option<&str>,
    expected_row_counts: &std::collections::BTreeMap<String, u64>,
    restored_schema_hash: &str,
    restored_row_counts: &std::collections::BTreeMap<String, u64>,
) -> (bool, String, u64) {
    let total_restored = restored_row_counts.values().sum();
    let Some(expected_schema_hash) = expected_schema_hash else {
        return (
            false,
            "VERIFY_L3_ORACLE_REQUIRED: backup-time schema hash is missing".to_owned(),
            total_restored,
        );
    };
    if expected_row_counts.is_empty() {
        return (
            false,
            "VERIFY_L3_ORACLE_REQUIRED: backup-time row-count oracle is missing".to_owned(),
            total_restored,
        );
    }

    let schema_pass = expected_schema_hash == restored_schema_hash;
    let (rows_pass, rows_note, total_restored) =
        compare_row_counts(expected_row_counts, restored_row_counts);
    let schema_note = if schema_pass {
        "schema hash matched".to_owned()
    } else {
        format!("schema hash mismatch: expected={expected_schema_hash} got={restored_schema_hash}")
    };
    (
        schema_pass && rows_pass,
        format!("{schema_note}; {rows_note}"),
        total_restored,
    )
}

fn plain_dump_l2_result(engine: Engine, plain: &[u8]) -> (bool, String) {
    let head = String::from_utf8_lossy(&plain[..plain.len().min(256)]);
    let header_recognized = match engine {
        Engine::Postgresql => head.contains("PostgreSQL database dump"),
        Engine::Mysql => head.contains("-- MySQL dump"),
        Engine::Mariadb => head.contains("-- MariaDB dump") || head.contains("sandbox mode"),
        Engine::Sqlserver => head.starts_with("PK"),
        Engine::Sqlite => head.starts_with("SQLite format 3\0"),
    };
    let header_note = if header_recognized {
        "header recognized"
    } else {
        "header not recognized"
    };
    (
        false,
        format!(
            "VERIFY_L2_NATIVE_VALIDATOR_UNAVAILABLE: {header_note}; \
             a header-only check is not L2 and L1 was not substituted"
        ),
    )
}

/// pg_restore --list를 임시 파일에 대해 실행(데이터 미전개 → 실행 위험 없음).
fn pg_restore_list_ok(custom_dump: &[u8]) -> (bool, String) {
    let tmp = std::env::temp_dir().join(format!("dbbackup-l2-{}.dump", uuid::Uuid::new_v4()));
    if std::fs::write(&tmp, custom_dump).is_err() {
        return (false, "임시 파일 쓰기 실패".to_string());
    }
    let out = std::process::Command::new("pg_restore")
        .arg("--list")
        .arg(&tmp)
        .output();
    let _ = std::fs::remove_file(&tmp);
    match out {
        Ok(o) if o.status.success() && !o.stdout.is_empty() => (
            true,
            format!("pg_restore --list OK, TOC {} bytes", o.stdout.len()),
        ),
        Ok(o) => (
            false,
            format!(
                "pg_restore --list 실패: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
        ),
        Err(e) => (false, format!("pg_restore 실행 불가: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn mysql_header_alone_never_counts_as_native_l2() {
        let (passed, note) = plain_dump_l2_result(
            Engine::Mysql,
            b"-- MySQL dump 10.13\nCREATE TABLE t(id int);",
        );
        assert!(!passed);
        assert!(note.contains("VERIFY_L2_NATIVE_VALIDATOR_UNAVAILABLE"));
    }

    #[test]
    fn mariadb_header_alone_never_counts_as_native_l2() {
        let (passed, note) = plain_dump_l2_result(
            Engine::Mariadb,
            b"/*M!999999\\- enable the sandbox mode */\n-- MariaDB dump",
        );
        assert!(!passed);
        assert!(note.contains("VERIFY_L2_NATIVE_VALIDATOR_UNAVAILABLE"));
    }

    #[test]
    fn l3_without_backup_time_oracle_is_never_a_pass() {
        let (passed, note, rows) =
            evaluate_restore_oracle(None, &BTreeMap::new(), "restored-schema", &BTreeMap::new());
        assert!(!passed);
        assert_eq!(rows, 0);
        assert!(note.contains("VERIFY_L3_ORACLE_REQUIRED"));
    }

    #[test]
    fn l3_requires_both_schema_and_row_oracles_to_match() {
        let expected = BTreeMap::from([("items".to_owned(), 2)]);
        let restored = BTreeMap::from([("items".to_owned(), 2)]);
        let (passed, note, rows) =
            evaluate_restore_oracle(Some("schema-a"), &expected, "schema-a", &restored);
        assert!(passed, "{note}");
        assert_eq!(rows, 2);

        let (passed, note, _) =
            evaluate_restore_oracle(Some("schema-a"), &expected, "schema-b", &restored);
        assert!(!passed);
        assert!(note.contains("schema hash mismatch"));
    }
}
