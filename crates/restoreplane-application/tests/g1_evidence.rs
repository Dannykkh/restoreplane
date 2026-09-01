use restoreplane_application::{BuildIdentityV1, VerifiedG1EvidenceV1};
use serde_json::{json, Value};

const REQUIREMENTS_JSON: &str =
    include_str!("../../../release/gates/g1-evidence-requirements.json");
const MATRIX_JSON: &str = include_str!("../../../release/gates/g1-fault-matrix.json");
const SOURCE_REVISION: &str = "1111111111111111111111111111111111111111";
const SOURCE_TREE_DIGEST: &str = "2222222222222222222222222222222222222222222222222222222222222222";
const BUILD_ID: &str = "3333333333333333333333333333333333333333333333333333333333333333";

#[test]
fn exact_current_build_and_complete_g1_matrix_create_an_opaque_proof() {
    let evidence = valid_evidence();
    let proof = verify(&evidence).expect("verified current-build G1 evidence");

    assert_eq!(proof.source_revision(), SOURCE_REVISION);
    assert_eq!(proof.source_tree_digest(), SOURCE_TREE_DIGEST);
    assert_eq!(proof.build_id(), BUILD_ID);
    assert_eq!(proof.required_evidence_count(), 9);
    assert_eq!(proof.runner_os(), "windows-test-runner");
    assert_eq!(proof.evidence_digest_blake3().len(), 64);
}

#[test]
fn stale_incomplete_failed_duplicated_or_output_bearing_evidence_is_rejected() {
    let mut cases = Vec::new();

    let mut stale_revision = valid_evidence();
    stale_revision["sourceRevision"] = json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    cases.push(stale_revision);

    let mut stale_tree = valid_evidence();
    stale_tree["sourceTreeDigest"] =
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    cases.push(stale_tree);

    let mut stale_build = valid_evidence();
    stale_build["buildId"] =
        json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
    cases.push(stale_build);

    let mut missing = valid_evidence();
    missing["evidence"]
        .as_array_mut()
        .expect("evidence rows")
        .pop();
    cases.push(missing);

    let mut failed = valid_evidence();
    failed["evidence"][0]["status"] = json!("fail");
    failed["evidence"][0]["exitCode"] = json!(7);
    cases.push(failed);

    let mut duplicated = valid_evidence();
    let duplicate_id = duplicated["evidence"][0]["id"].clone();
    duplicated["evidence"][1]["id"] = duplicate_id;
    cases.push(duplicated);

    let mut wrong_command = valid_evidence();
    wrong_command["evidence"][0]["command"] = json!("cargo test forged");
    cases.push(wrong_command);

    let mut raw_output = valid_evidence();
    raw_output["evidence"][0]["stdout"] = json!("must never be durable evidence");
    cases.push(raw_output);

    for evidence in cases {
        let error = verify(&evidence).expect_err("invalid G1 evidence must fail closed");
        assert_eq!(error.code(), "G1_EVIDENCE_INVALID");
    }
}

fn verify(
    evidence: &Value,
) -> Result<VerifiedG1EvidenceV1, restoreplane_application::G1EvidenceError> {
    let identity = BuildIdentityV1::new(SOURCE_REVISION, SOURCE_TREE_DIGEST, BUILD_ID)
        .expect("valid build identity");
    VerifiedG1EvidenceV1::verify(
        &serde_json::to_string(evidence).expect("evidence JSON"),
        REQUIREMENTS_JSON,
        MATRIX_JSON,
        &identity,
    )
}

fn valid_evidence() -> Value {
    let matrix: Value = serde_json::from_str(MATRIX_JSON).expect("G1 matrix");
    let rows = matrix["commands"]
        .as_array()
        .expect("matrix commands")
        .iter()
        .enumerate()
        .map(|(index, command)| {
            json!({
                "id": command["id"],
                "command": command["display"],
                "status": "pass",
                "exitCode": 0,
                "startedAt": format!("2026-07-29T00:00:{index:02}+09:00"),
                "finishedAt": format!("2026-07-29T00:01:{index:02}+09:00"),
                "durationMs": 1.25,
                "sourceRevision": SOURCE_REVISION,
                "sourceTreeDigest": SOURCE_TREE_DIGEST,
                "buildId": BUILD_ID,
                "runnerOs": "windows-test-runner"
            })
        })
        .collect::<Vec<_>>();
    json!({
        "schemaVersion": 1,
        "gateId": "G1",
        "sourceRevision": SOURCE_REVISION,
        "sourceTreeDigest": SOURCE_TREE_DIGEST,
        "buildId": BUILD_ID,
        "startedAt": "2026-07-29T00:00:00+09:00",
        "finishedAt": "2026-07-29T00:02:00+09:00",
        "runner": {
            "os": "windows-test-runner",
            "architecture": "x86_64",
            "powershell": "5.1",
            "rustc": "rustc test",
            "cargo": "cargo test"
        },
        "evidence": rows,
        "failureCount": 0,
        "status": "pass"
    })
}
