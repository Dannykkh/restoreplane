use std::str::FromStr;

use dbbackup_catalog::{OperationalStore, WorkspaceSpec, DEFAULT_WORKSPACE_ID};
use dbbackup_domain::WorkspaceId;
use tempfile::TempDir;

#[test]
fn workspace_repository_lists_default_and_creates_an_isolated_scope() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("store");

    let initial = store.workspace().list().expect("initial workspaces");
    assert_eq!(initial.len(), 1);
    assert_eq!(initial[0].id, DEFAULT_WORKSPACE_ID);
    assert_eq!(initial[0].display_name, "Default Workspace");
    assert!(initial[0].is_default);

    let workspace_id =
        WorkspaceId::from_str("11111111-1111-4111-8111-111111111111").expect("workspace id");
    let created = store
        .workspace()
        .create(WorkspaceSpec::new(workspace_id.clone(), "Analytics", 1_000))
        .expect("create workspace");
    assert_eq!(created.id, workspace_id.as_str());
    assert_eq!(created.display_name, "Analytics");
    assert!(!created.is_default);
    assert_eq!(created.connection_count, 0);
    assert_eq!(created.destination_count, 0);
    assert_eq!(created.plan_count, 0);

    let listed = store.workspace().list().expect("list workspaces");
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].display_name, "Default Workspace");
    assert_eq!(listed[1].display_name, "Analytics");
}

#[test]
fn workspace_repository_exposes_a_strict_keyset_page_with_exact_completeness() {
    let temp = tempfile::TempDir::new().expect("temp directory");
    let store = dbbackup_catalog::OperationalStore::open(temp.path().join("operational.db"))
        .expect("open store");
    store
        .workspace()
        .create(dbbackup_catalog::WorkspaceSpec::new(
            dbbackup_domain::WorkspaceId::new(),
            "Second workspace",
            2_000,
        ))
        .expect("create second workspace");

    let first = store.workspace().page(None, 1).expect("first page");
    assert_eq!(first.items.len(), 1);
    assert_eq!(first.total_count, 2);
    assert!(!first.complete);
    let cursor = first.next_cursor.expect("next cursor");

    let second = store
        .workspace()
        .page(Some(&cursor), 1)
        .expect("second page");
    assert_eq!(second.items.len(), 1);
    assert_eq!(second.total_count, 2);
    assert!(second.complete);
    assert!(second.next_cursor.is_none());
    assert_ne!(first.items[0].id, second.items[0].id);

    let rejected = store
        .workspace()
        .page(None, 101)
        .expect_err("oversized workspace page must be rejected");
    assert_eq!(rejected.code(), "OPERATIONAL_RUNTIME_STATE_INVALID");
}

#[test]
fn workspace_names_are_unique_after_normalization() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("store");
    let first =
        WorkspaceId::from_str("11111111-1111-4111-8111-111111111111").expect("workspace id");
    let second =
        WorkspaceId::from_str("22222222-2222-4222-8222-222222222222").expect("workspace id");

    store
        .workspace()
        .create(WorkspaceSpec::new(first, "Analytics", 1_000))
        .expect("first workspace");
    let error = store
        .workspace()
        .create(WorkspaceSpec::new(second, "  ANALYTICS  ", 1_001))
        .expect_err("normalized duplicate must fail");

    assert_eq!(error.code(), "WORKSPACE_NAME_CONFLICT");
}

#[test]
fn workspace_creation_rejects_invalid_operator_text() {
    let temp = TempDir::new().expect("temp directory");
    let store = OperationalStore::open(temp.path().join("operational.db")).expect("store");
    let workspace_id =
        WorkspaceId::from_str("11111111-1111-4111-8111-111111111111").expect("workspace id");

    let error = store
        .workspace()
        .create(WorkspaceSpec::new(workspace_id, "   ", 1_000))
        .expect_err("blank workspace name");

    assert_eq!(error.code(), "WORKSPACE_INPUT_INVALID");
}
