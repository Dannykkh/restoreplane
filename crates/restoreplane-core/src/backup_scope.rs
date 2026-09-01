use serde::{de::Error as _, Deserialize, Deserializer, Serialize};
use thiserror::Error;

pub const MAX_SELECTED_TABLES: usize = 1_000;
pub const MAX_SELECTED_IDENTIFIER_BYTES: usize = 256;
pub const MAX_SELECTION_IDENTIFIER_BYTES: usize = 12 * 1_024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackupScopeMode {
    WholeDatabase,
    ExplicitTables,
}

#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct SelectedTable {
    schema_name: String,
    table_name: String,
}

impl SelectedTable {
    pub fn new(
        schema_name: impl Into<String>,
        table_name: impl Into<String>,
    ) -> Result<Self, BackupScopeSelectionError> {
        let value = Self {
            schema_name: schema_name.into(),
            table_name: table_name.into(),
        };
        validate_identifier("schema_name", &value.schema_name)?;
        validate_identifier("table_name", &value.table_name)?;
        Ok(value)
    }

    pub fn schema_name(&self) -> &str {
        &self.schema_name
    }

    pub fn table_name(&self) -> &str {
        &self.table_name
    }
}

impl<'de> Deserialize<'de> for SelectedTable {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct WireSelectedTable {
            schema_name: String,
            table_name: String,
        }

        let wire = WireSelectedTable::deserialize(deserializer)?;
        Self::new(wire.schema_name, wire.table_name).map_err(D::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BackupScopeSelection {
    WholeDatabase,
    ExplicitTables { tables: Vec<SelectedTable> },
}

impl BackupScopeSelection {
    pub const fn whole_database() -> Self {
        Self::WholeDatabase
    }

    pub fn explicit_tables(
        mut tables: Vec<SelectedTable>,
    ) -> Result<Self, BackupScopeSelectionError> {
        if tables.is_empty() {
            return Err(BackupScopeSelectionError::EmptyExplicitTables);
        }
        if tables.len() > MAX_SELECTED_TABLES {
            return Err(BackupScopeSelectionError::TooManyTables {
                max: MAX_SELECTED_TABLES,
            });
        }
        tables.sort();
        if let Some(duplicate) = tables
            .windows(2)
            .find_map(|pair| (pair[0] == pair[1]).then(|| pair[0].clone()))
        {
            return Err(BackupScopeSelectionError::DuplicateTable {
                schema_name: duplicate.schema_name,
                table_name: duplicate.table_name,
            });
        }
        let identifier_bytes = tables.iter().try_fold(0usize, |total, table| {
            total
                .checked_add(table.schema_name.len())
                .and_then(|value| value.checked_add(table.table_name.len()))
        });
        if identifier_bytes.is_none_or(|bytes| bytes > MAX_SELECTION_IDENTIFIER_BYTES) {
            return Err(BackupScopeSelectionError::SelectionTooLarge {
                max_bytes: MAX_SELECTION_IDENTIFIER_BYTES,
            });
        }
        Ok(Self::ExplicitTables { tables })
    }

    pub const fn mode(&self) -> BackupScopeMode {
        match self {
            Self::WholeDatabase => BackupScopeMode::WholeDatabase,
            Self::ExplicitTables { .. } => BackupScopeMode::ExplicitTables,
        }
    }

    pub fn tables(&self) -> &[SelectedTable] {
        match self {
            Self::WholeDatabase => &[],
            Self::ExplicitTables { tables } => tables,
        }
    }

    pub const fn is_whole_database(&self) -> bool {
        matches!(self, Self::WholeDatabase)
    }
}

impl<'de> Deserialize<'de> for BackupScopeSelection {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(tag = "mode", rename_all = "snake_case", deny_unknown_fields)]
        enum WireSelection {
            WholeDatabase,
            ExplicitTables { tables: Vec<SelectedTable> },
        }

        match WireSelection::deserialize(deserializer)? {
            WireSelection::WholeDatabase => Ok(Self::whole_database()),
            WireSelection::ExplicitTables { tables } => {
                Self::explicit_tables(tables).map_err(D::Error::custom)
            }
        }
    }
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum BackupScopeSelectionError {
    #[error("explicit table selection cannot be empty")]
    EmptyExplicitTables,
    #[error("explicit table selection exceeds the maximum of {max} tables")]
    TooManyTables { max: usize },
    #[error("selected table identifier is invalid: {field}")]
    InvalidIdentifier { field: &'static str },
    #[error("selected table identifier exceeds {max_bytes} bytes: {field}")]
    IdentifierTooLong {
        field: &'static str,
        max_bytes: usize,
    },
    #[error("selected table appears more than once: {schema_name}.{table_name}")]
    DuplicateTable {
        schema_name: String,
        table_name: String,
    },
    #[error("explicit table identifier payload exceeds {max_bytes} bytes")]
    SelectionTooLarge { max_bytes: usize },
}

fn validate_identifier(field: &'static str, value: &str) -> Result<(), BackupScopeSelectionError> {
    if value.is_empty() || value.contains(['\0', '\r', '\n']) {
        return Err(BackupScopeSelectionError::InvalidIdentifier { field });
    }
    if value.len() > MAX_SELECTED_IDENTIFIER_BYTES {
        return Err(BackupScopeSelectionError::IdentifierTooLong {
            field,
            max_bytes: MAX_SELECTED_IDENTIFIER_BYTES,
        });
    }
    Ok(())
}
