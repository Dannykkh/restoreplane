//! Native metadata projection for existing protection commands. No backup or
//! restore authority, service secret or user SQL is implemented here.
use crate::metadata::{MetadataCatalog, ScopeKey};
use crate::result_stream::{encoded_size, MemoryReservation};
use crate::types::{read, text};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{AuthorizePurpose, Engine, Scope, SessionRef, TypedCell};
use restoreplane_domain::workbench_metadata::ObjectRef;
use restoreplane_domain::workbench_protection::{
    WorkbenchSelection, WorkbenchSelectionItem, MAX_SELECTED_TABLES, MAX_SELECTION_BYTES,
};
use restoreplane_domain::workbench_structure::{postgresql_relation_signature, PgRelationAlias};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SelectionProjection {
    pub scope: Scope,
    pub workbench_selection: WorkbenchSelection,
    #[serde(skip)]
    _memory: MemoryReservation,
}
pub struct ProtectionLinks {
    runtime: Arc<WorkbenchState>,
    metadata: Arc<MetadataCatalog>,
}
impl ProtectionLinks {
    pub fn new(runtime: Arc<WorkbenchState>, metadata: Arc<MetadataCatalog>) -> Self {
        Self { runtime, metadata }
    }
    pub async fn selection(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        objects: Vec<ObjectRef>,
    ) -> Result<SelectionProjection, WorkbenchError> {
        if objects.is_empty()
            || objects.len() > MAX_SELECTED_TABLES
            || objects
                .iter()
                .map(|object| object.catalog_id)
                .collect::<BTreeSet<_>>()
                .len()
                != objects.len()
        {
            return Err(WorkbenchError::InvalidInput);
        }
        let view = self.runtime.get(context, session)?;
        let scoped = ScopeKey::current(&self.runtime, context, session)?;
        let nodes = objects
            .iter()
            .map(|object| self.metadata.node(&scoped, object))
            .collect::<Result<Vec<_>, _>>()?;
        if nodes.iter().any(|node| node.kind != "table") {
            return Err(WorkbenchError::Unsupported);
        }
        let names = nodes
            .iter()
            .map(|node| {
                (
                    (node.schema.clone(), node.name.clone()),
                    node.item.object.schema_version.clone(),
                )
            })
            .collect::<BTreeMap<_, _>>();
        if names.len() != objects.len() {
            return Err(WorkbenchError::InvalidInput);
        }
        let expected = names.clone();
        let memory = self.metadata.budget.reserve(MAX_SELECTION_BYTES * 4)?;
        let budget = self.metadata.budget.clone();
        let observed=self.runtime.with_connection(context,session,AuthorizePurpose::Read,move |connection|Box::pin(async move {
            if connection.facts().engine!=Engine::Postgresql { return Err(WorkbenchError::Unsupported); }
            let mut parameters=Vec::with_capacity(names.len()*2);
            let mut conditions=Vec::with_capacity(names.len());
            for (schema,name) in names.keys() {
                parameters.push(TypedCell::Text {value:schema.clone()}); let schema_index=parameters.len();
                parameters.push(TypedCell::Text {value:name.clone()}); let name_index=parameters.len();
                conditions.push(format!("(n.nspname=${schema_index} AND c.relname=${name_index})"));
            }
            let sql=format!("SELECT c.oid::pg_catalog.text,n.nspname::pg_catalog.text,c.relname::pg_catalog.text,{} FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind='r' AND ({}) ORDER BY n.nspname,c.relname,c.oid LIMIT 101",postgresql_relation_signature(PgRelationAlias::Relation),conditions.join(" OR "));
            read(connection,&budget,&sql,&parameters,101).await
        })).await?;
        if observed.rows.len() != expected.len() {
            return Err(WorkbenchError::Unsupported);
        }
        let mut items = Vec::with_capacity(expected.len());
        for row in &observed.rows {
            if row.len() != 4 {
                return Err(WorkbenchError::Unsupported);
            }
            let schema = text(&row[1])?.to_owned();
            let name = text(&row[2])?.to_owned();
            let signature = text(&row[3])?.to_owned();
            if expected.get(&(schema.clone(), name.clone())) != Some(&signature) {
                return Err(WorkbenchError::CursorInvalid);
            }
            items.push(WorkbenchSelectionItem {
                engine_object_id: format!("relation:{}", text(&row[0])?),
                schema_name: schema,
                object_name: name,
                structure_signature: signature,
            });
        }
        items.sort();
        let workbench_selection = WorkbenchSelection {
            signature_version: 1,
            expected_descriptor_revision: view.descriptor_revision,
            items,
        };
        if !workbench_selection.is_well_formed() {
            return Err(WorkbenchError::InvalidInput);
        }
        encoded_size(&workbench_selection, MAX_SELECTION_BYTES)?;
        Ok(SelectionProjection {
            scope: view.scope,
            workbench_selection,
            _memory: memory,
        })
    }
}
