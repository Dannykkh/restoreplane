use dbbackup_core::{
    BackupScopeMode, BackupScopeSelection, BackupScopeSelectionError, SelectedTable,
    MAX_SELECTED_TABLES, MAX_SELECTION_IDENTIFIER_BYTES,
};

fn table(schema: &str, name: &str) -> SelectedTable {
    SelectedTable::new(schema, name).expect("valid selected table")
}

#[test]
fn whole_database_has_no_selected_tables() {
    let selection = BackupScopeSelection::whole_database();

    assert_eq!(selection.mode(), BackupScopeMode::WholeDatabase);
    assert!(selection.tables().is_empty());
    assert_eq!(
        serde_json::to_string(&selection).expect("serialize selection"),
        r#"{"mode":"whole_database"}"#
    );
}

#[test]
fn explicit_tables_are_stored_in_canonical_identifier_order() {
    let selection = BackupScopeSelection::explicit_tables(vec![
        table("sales", "zebra"),
        table("audit", "orders"),
        table("sales", "accounts"),
    ])
    .expect("explicit selection");

    assert_eq!(selection.mode(), BackupScopeMode::ExplicitTables);
    assert_eq!(
        selection
            .tables()
            .iter()
            .map(|item| (item.schema_name(), item.table_name()))
            .collect::<Vec<_>>(),
        vec![
            ("audit", "orders"),
            ("sales", "accounts"),
            ("sales", "zebra"),
        ]
    );
}

#[test]
fn explicit_tables_reject_an_empty_selection() {
    assert_eq!(
        BackupScopeSelection::explicit_tables(Vec::new()),
        Err(BackupScopeSelectionError::EmptyExplicitTables)
    );
}

#[test]
fn explicit_tables_reject_duplicate_exact_identifiers() {
    assert_eq!(
        BackupScopeSelection::explicit_tables(vec![
            table("public", "orders"),
            table("public", "orders"),
        ]),
        Err(BackupScopeSelectionError::DuplicateTable {
            schema_name: "public".to_owned(),
            table_name: "orders".to_owned(),
        })
    );
}

#[test]
fn explicit_tables_reject_more_than_the_supported_table_count() {
    let tables = (0..=MAX_SELECTED_TABLES)
        .map(|index| table("public", &format!("table_{index}")))
        .collect();

    assert_eq!(
        BackupScopeSelection::explicit_tables(tables),
        Err(BackupScopeSelectionError::TooManyTables {
            max: MAX_SELECTED_TABLES,
        })
    );
}

#[test]
fn explicit_tables_reject_an_oversized_combined_identifier_payload() {
    let table_count = MAX_SELECTION_IDENTIFIER_BYTES / 256 + 1;
    let tables = (0..table_count)
        .map(|index| {
            let prefix = format!("{index:04}_");
            table("s", &format!("{prefix}{}", "x".repeat(256 - prefix.len())))
        })
        .collect();

    assert_eq!(
        BackupScopeSelection::explicit_tables(tables),
        Err(BackupScopeSelectionError::SelectionTooLarge {
            max_bytes: MAX_SELECTION_IDENTIFIER_BYTES,
        })
    );
}

#[test]
fn selected_table_rejects_empty_or_control_bearing_identifiers() {
    assert!(matches!(
        SelectedTable::new("", "orders"),
        Err(BackupScopeSelectionError::InvalidIdentifier {
            field: "schema_name"
        })
    ));
    assert!(matches!(
        SelectedTable::new("public", "orders\narchive"),
        Err(BackupScopeSelectionError::InvalidIdentifier {
            field: "table_name"
        })
    ));
}

#[test]
fn deserialization_revalidates_and_canonicalizes_explicit_tables() {
    let selection: BackupScopeSelection = serde_json::from_str(
        r#"{
            "mode":"explicit_tables",
            "tables":[
                {"schema_name":"public","table_name":"zebra"},
                {"schema_name":"audit","table_name":"orders"}
            ]
        }"#,
    )
    .expect("deserialize selection");

    assert_eq!(
        selection
            .tables()
            .iter()
            .map(|item| (item.schema_name(), item.table_name()))
            .collect::<Vec<_>>(),
        vec![("audit", "orders"), ("public", "zebra")]
    );
}

#[test]
fn deserialization_rejects_an_invalid_explicit_selection() {
    let result =
        serde_json::from_str::<BackupScopeSelection>(r#"{"mode":"explicit_tables","tables":[]}"#);

    assert!(result.is_err());
}

#[test]
fn scope_mode_deserializes_from_its_stable_wire_name() {
    let mode: BackupScopeMode =
        serde_json::from_str(r#""explicit_tables""#).expect("deserialize mode");

    assert_eq!(mode, BackupScopeMode::ExplicitTables);
}
