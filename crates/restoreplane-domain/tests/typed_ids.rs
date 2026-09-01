use std::str::FromStr;

use restoreplane_domain::{
    ConnectionProfileId, DatabaseTargetId, DomainError, PlanRevisionId, WorkItemId,
};
use uuid::{uuid, Variant, Version};

#[test]
fn generated_ids_are_canonical_typed_uuid_strings() {
    let connection_id = ConnectionProfileId::new();
    let target_id = DatabaseTargetId::new();
    let revision_id = PlanRevisionId::new();
    let work_item_id = WorkItemId::new();

    for value in [
        connection_id.to_string(),
        target_id.to_string(),
        revision_id.to_string(),
        work_item_id.to_string(),
    ] {
        assert_eq!(value.len(), 36);
        assert_eq!(value, value.to_ascii_lowercase());
        assert_eq!(value.matches('-').count(), 4);
    }

    let json = serde_json::to_string(&revision_id).expect("serialize typed ID");
    assert_eq!(json, format!("\"{revision_id}\""));
    assert_eq!(
        serde_json::from_str::<PlanRevisionId>(&json).expect("deserialize typed ID"),
        revision_id
    );
}

#[test]
fn deterministic_ids_are_stable_and_use_uuid_v5() {
    let namespace = uuid!("f8a62129-c0d6-5b43-89de-540a57a44f31");
    let first = DatabaseTargetId::from_stable_name(namespace, b"postgres://db-1");
    let repeated = DatabaseTargetId::from_stable_name(namespace, b"postgres://db-1");
    let different = DatabaseTargetId::from_stable_name(namespace, b"postgres://db-2");

    assert_eq!(first, repeated);
    assert_ne!(first, different);
    assert_eq!(first.as_uuid().get_version(), Some(Version::Sha1));
    assert_eq!(first.as_uuid().get_variant(), Variant::RFC4122);
}

#[test]
fn boundary_parsing_rejects_non_uuid_and_noncanonical_text() {
    let malformed = WorkItemId::from_str("work-1").expect_err("raw labels are not typed IDs");
    assert_eq!(malformed.code(), "DOMAIN_ID_INVALID");
    assert!(matches!(malformed, DomainError::InvalidId { .. }));

    let uppercase = "550E8400-E29B-41D4-A716-446655440000";
    let noncanonical =
        WorkItemId::from_str(uppercase).expect_err("ID text must use canonical lowercase form");
    assert_eq!(noncanonical.code(), "DOMAIN_ID_NONCANONICAL");

    let canonical = "550e8400-e29b-41d4-a716-446655440000";
    assert_eq!(
        WorkItemId::from_str(canonical)
            .expect("canonical UUID")
            .as_str(),
        canonical
    );
}
