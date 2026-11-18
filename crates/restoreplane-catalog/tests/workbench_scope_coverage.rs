use std::str::FromStr;

use restoreplane_catalog::{
    ConnectionProfileSpec, DatabaseObjectDiscoverySpec, DatabaseTargetSpec,
    DiscoveredDatabaseObjectSpec, ExplicitBackupScopeSpec, OperationalStore,
};
use restoreplane_domain::{BackupScopeId, ConnectionProfileId, DatabaseTargetId, OperationRunId};
use rusqlite::{params, Connection};
use serde_json::{json, Value};

#[test]
fn selected_inventory_is_bound_to_one_scope_and_never_replaces_full_inventory() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("catalog.db");
    let store = OperationalStore::open(path.clone()).unwrap();
    let profile = ConnectionProfileId::new();
    let target = DatabaseTargetId::new();
    store
        .configuration()
        .insert_connection(ConnectionProfileSpec::tcp(
            profile.clone(),
            "PG",
            "postgresql",
            "localhost",
            5432,
            1,
        ))
        .unwrap();
    store
        .configuration()
        .insert_target(DatabaseTargetSpec::new(
            target.clone(),
            profile.clone(),
            "database:app",
            "app",
            "app",
            "db-fingerprint",
            2,
        ))
        .unwrap();
    let discover = |at: i64, object: &str, selected: bool| {
        let id = OperationRunId::new();
        Connection::open(&path).unwrap().execute(
            "INSERT INTO operation_runs(id,operation_kind,subject_type,subject_id,request_json,state,created_at_ms,updated_at_ms) VALUES(?1,'connection_discovery','database_target',?2,'{}','running',?3,?3)",
            params![id.as_str(),target.as_str(),at],
        ).unwrap();
        let spec = DatabaseObjectDiscoverySpec::new(
            id.clone(),
            target.clone(),
            "a".repeat(64),
            vec![DiscoveredDatabaseObjectSpec::eligible_table(
                object,
                "public",
                object,
                "b".repeat(64),
            )],
            at,
        );
        let spec = if selected {
            spec.with_selected_coverage()
        } else {
            spec
        };
        store.backup_scopes().record_object_discovery(spec).unwrap();
        let request = json!({"workbenchAuthority":{"scope":{"workspaceId":restoreplane_catalog::DEFAULT_WORKSPACE_ID,"connectionProfileId":profile,"databaseTargetId":target}}});
        Connection::open(&path)
            .unwrap()
            .execute(
                "UPDATE operation_runs SET state='succeeded',request_json=?2 WHERE id=?1",
                params![id.as_str(), request.to_string()],
            )
            .unwrap();
        id
    };
    let old_full = discover(10, "old_table", false);
    let own = discover(20, "selected_table", true);
    assert_eq!(
        store
            .backup_scopes()
            .object_page(&target, None, 100)
            .unwrap()
            .discovery_run_id(),
        &old_full
    );
    let scope = BackupScopeId::from_str(own.as_str()).unwrap();
    let spec = || {
        ExplicitBackupScopeSpec::new(
            scope.clone(),
            target.clone(),
            "선택 보호",
            own.clone(),
            vec!["selected_table".into()],
            21,
        )
    };
    assert!(
        store.backup_scopes().create_explicit(spec()).is_err(),
        "selected discovery requires the trusted proof path"
    );
    let own_for_record = own.clone();
    let created = store.backup_scopes().create_explicit_checked(
        spec().with_workbench_selection_proof(), |_|Ok(()), move|tx,scope,fingerprint,selector| {
            let raw:String=tx.query_row("SELECT result_json FROM operation_runs WHERE id=?1",[own_for_record.as_str()],|r|r.get(0))?;
            let mut result:Value=serde_json::from_str(&raw).unwrap();
            result["workbenchSelection"]=json!({"selectionDigest":"c".repeat(64)});
            result["workbenchScopeLinks"]=json!([{"scopeId":scope,"scopeFingerprint":fingerprint,"selectorHash":selector,"selectionDigest":"c".repeat(64),"linkedAtMs":"21"}]);
            tx.execute("UPDATE operation_runs SET result_json=?2 WHERE id=?1",params![own_for_record.as_str(),result.to_string()])?;
            Ok(())
        },
    ).unwrap();
    assert_eq!(created.id(), &scope);
    let health = || {
        store
            .console_query()
            .backup_scopes_in_workspace(
                restoreplane_catalog::DEFAULT_WORKSPACE_ID,
                Some(target.as_str()),
                100,
            )
            .unwrap()
            .into_iter()
            .find(|s| s.id == scope.as_str())
            .unwrap()
    };
    assert_eq!(
        health().drift_state,
        "current",
        "fresh exact selected proof overrides an older full inventory"
    );
    discover(30, "different_selection", true);
    assert_eq!(
        health().drift_state,
        "current",
        "another partial selection cannot replace this Scope's proof"
    );
    assert!(
        store
            .backup_scopes()
            .create_explicit_checked(
                spec().with_workbench_selection_proof(),
                |_| Ok(()),
                |_, _, _, _| Ok(())
            )
            .is_err(),
        "one discovery owns one Scope"
    );
    let db = Connection::open(&path).unwrap();
    let original: String = db
        .query_row(
            "SELECT result_json FROM operation_runs WHERE id=?1",
            [own.as_str()],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("UPDATE operation_runs SET result_json=json_set(result_json,'$.workbenchScopeLinks[0].selectorHash','changed') WHERE id=?1",[own.as_str()]).unwrap();
    assert_eq!(
        health().drift_state,
        "unknown",
        "a missing exact link cannot fall back to an older full inventory"
    );
    db.execute(
        "UPDATE operation_runs SET result_json=?2 WHERE id=?1",
        params![own.as_str(), original],
    )
    .unwrap();
    discover(40, "new_full_without_selected", false);
    assert_eq!(
        health().drift_state,
        "drifted",
        "only a later full observation can prove selected objects missing"
    );
    assert_eq!(store.writer_diagnostics().unwrap().user_version, 4);
}
