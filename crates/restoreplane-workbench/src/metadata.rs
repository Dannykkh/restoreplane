//! Native, lazy catalog reads. Object names never come from a SQL text field.
use crate::driver::DriverConnection;
use crate::result_stream::{encoded_size, MemoryReservation, NativeBudget};
use crate::types::{
    boolean, integer, now_ms, optional_text, placeholder, quote_identifier, read_catalog as read,
    text,
};
use crate::{InvocationContext, WorkbenchError, WorkbenchState};
use restoreplane_domain::workbench::{Column, Engine, SessionRef, TypedCell};
use restoreplane_domain::workbench_metadata::{
    CatalogDetail, CatalogItem, CatalogKind, Completeness, Count, DescribePart, Diagram, ObjectRef,
    Page, PageInput,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScopeKey {
    owner: String,
    window: Uuid,
    session: Uuid,
    generation: u64,
    revision: String,
}
impl ScopeKey {
    pub(crate) fn matches_window(&self, context: &InvocationContext) -> bool {
        self.owner == context.principal().owner_id() && self.window == context.window_instance_id()
    }
    pub(crate) fn window_id(&self) -> Uuid {
        self.window
    }
    pub(crate) fn matches(&self, context: &InvocationContext, session: SessionRef) -> bool {
        self.owner == context.principal().owner_id()
            && self.window == context.window_instance_id()
            && self.session == session.session_id
            && self.generation == session.connection_generation.value()
    }
    pub(crate) fn current(
        runtime: &WorkbenchState,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<Self, WorkbenchError> {
        let view = runtime.get(context, session)?;
        if view.state == restoreplane_domain::workbench::SessionState::Retiring {
            return Err(WorkbenchError::SessionStale);
        }
        Ok(Self {
            owner: context.principal().owner_id().into(),
            window: context.window_instance_id(),
            session: session.session_id,
            generation: session.connection_generation.value(),
            revision: view.descriptor_revision,
        })
    }
}
#[derive(Clone, Debug, Serialize)]
pub struct ColumnMetadata {
    pub column: Column,
    pub ordinal: usize,
    pub generated: bool,
    pub declared_type: String,
    pub type_detail: String,
    pub defaulted: bool,
    /// MariaDB exposes its implicit SQL NULL as the text NULL. This is a
    /// certified constant, distinct from quoted 'NULL' or any expression.
    pub null_default: bool,
    pub native_type_id: Option<u32>,
    pub native_type_schema: Option<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct KeyMetadata {
    pub key_id: Uuid,
    pub name: String,
    pub columns: Vec<Uuid>,
    pub primary: bool,
    pub unique: bool,
    pub nullable: bool,
    pub partial: bool,
    pub expression: bool,
}
#[derive(Debug, Serialize)]
pub struct RelationMetadata {
    #[serde(skip)]
    scope: ScopeKey,
    pub object: ObjectRef,
    pub engine: Engine,
    pub schema: String,
    pub name: String,
    pub kind: String,
    pub storage_engine: Option<String>,
    pub columns: Vec<ColumnMetadata>,
    pub keys: Vec<KeyMetadata>,
    pub indexes: Vec<CatalogDetail>,
    pub foreign_keys: Vec<CatalogDetail>,
    #[serde(skip)]
    foreign_targets: Vec<ForeignTarget>,
    #[serde(skip)]
    _memory: MemoryReservation,
}
async fn editing_eligible(
    connection: &mut dyn DriverConnection,
    budget: Arc<NativeBudget>,
    relation: Arc<RelationMetadata>,
) -> Result<bool, WorkbenchError> {
    match crate::generated_sql::verify_transactional_table(connection, &budget, &relation).await {
        Ok(()) => Ok(true),
        Err(WorkbenchError::Unsupported) => Ok(false),
        Err(error) => Err(error),
    }
}
fn logical_relation_result(
    result: Result<RelationMetadata, WorkbenchError>,
) -> Result<Result<RelationMetadata, WorkbenchError>, WorkbenchError> {
    match result {
        Err(WorkbenchError::CursorInvalid) => Ok(Err(WorkbenchError::CursorInvalid)),
        Err(error) => Err(error),
        Ok(relation) => Ok(Ok(relation)),
    }
}
#[derive(Clone, Debug, Serialize)]
struct ForeignTarget {
    schema: String,
    name: String,
    kind: String,
    version: String,
}
impl RelationMetadata {
    pub fn qualified_name(&self) -> Result<String, WorkbenchError> {
        Ok(format!(
            "{}.{}",
            quote_identifier(self.engine, &self.schema)?,
            quote_identifier(self.engine, &self.name)?
        ))
    }
    pub fn stable_key(&self) -> Option<&KeyMetadata> {
        self.keys
            .iter()
            .filter(|key| {
                key.unique
                    && !key.nullable
                    && !key.partial
                    && !key.expression
                    && !key.columns.is_empty()
            })
            .min_by_key(|key| (!key.primary, key.columns.len()))
    }
}
pub(crate) struct Node {
    pub scope: ScopeKey,
    pub item: CatalogItem,
    pub schema: String,
    pub name: String,
    pub kind: String,
    touched: AtomicU64,
    _memory: MemoryReservation,
}
struct CatalogCursor {
    scope: ScopeKey,
    parent: Option<ObjectRef>,
    kind: CatalogKind,
    search: String,
    last: String,
    touched: Instant,
    _memory: MemoryReservation,
}
pub struct MetadataCatalog {
    pub(crate) runtime: Arc<WorkbenchState>,
    pub(crate) budget: Arc<NativeBudget>,
    nodes: Mutex<HashMap<Uuid, Arc<Node>>>,
    cursors: Mutex<HashMap<Uuid, CatalogCursor>>,
    relations: Mutex<HashMap<Uuid, Arc<RelationMetadata>>>,
}
impl MetadataCatalog {
    pub fn retire_session(
        &self,
        context: &InvocationContext,
        session: SessionRef,
    ) -> Result<(), WorkbenchError> {
        self.retire_scope(context, Some(session))
    }
    pub fn retire_window(&self, context: &InvocationContext) -> Result<(), WorkbenchError> {
        self.retire_scope(context, None)
    }
    fn retire_scope(
        &self,
        context: &InvocationContext,
        session: Option<SessionRef>,
    ) -> Result<(), WorkbenchError> {
        if context.principal() != self.runtime.principal() {
            return Err(WorkbenchError::Forbidden);
        }
        let matches = |scope: &ScopeKey| {
            scope.matches_window(context)
                && session.is_none_or(|session| scope.matches(context, session))
        };
        self.nodes
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, node| !matches(&node.scope));
        self.cursors
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, cursor| !matches(&cursor.scope));
        self.relations
            .lock()
            .map_err(|_| WorkbenchError::Io)?
            .retain(|_, relation| !matches(&relation.scope));
        Ok(())
    }
    pub fn retained_counts(&self) -> Result<(usize, usize, usize), WorkbenchError> {
        Ok((
            self.nodes.lock().map_err(|_| WorkbenchError::Io)?.len(),
            self.cursors.lock().map_err(|_| WorkbenchError::Io)?.len(),
            self.relations.lock().map_err(|_| WorkbenchError::Io)?.len(),
        ))
    }
    pub fn new(runtime: Arc<WorkbenchState>, budget: Arc<NativeBudget>) -> Self {
        Self {
            runtime,
            budget,
            nodes: Mutex::new(HashMap::new()),
            cursors: Mutex::new(HashMap::new()),
            relations: Mutex::new(HashMap::new()),
        }
    }
    pub fn budget(&self) -> Arc<NativeBudget> {
        self.budget.clone()
    }
    pub async fn page(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        parent: Option<ObjectRef>,
        kind: CatalogKind,
        search: String,
        page: PageInput,
    ) -> Result<Page<CatalogItem>, WorkbenchError> {
        let limit = page_limit(page.limit)?;
        if search.len() > 256 || search.contains('\0') {
            return Err(WorkbenchError::InvalidInput);
        }
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        let schema = match &parent {
            Some(object) => {
                let node = self.node(&scope, object)?;
                if node.kind != "schema" {
                    return Err(WorkbenchError::InvalidInput);
                }
                Some(node.name.clone())
            }
            None => None,
        };
        let last = if let Some(cursor) = page.cursor {
            let id = Uuid::parse_str(&cursor).map_err(|_| WorkbenchError::CursorInvalid)?;
            let cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            let cursor = cursors.get(&id).ok_or(WorkbenchError::CursorInvalid)?;
            if cursor.scope != scope
                || cursor.parent != parent
                || cursor.kind != kind
                || cursor.search != search
                || cursor.touched.elapsed() > Duration::from_secs(120)
            {
                return Err(WorkbenchError::CursorInvalid);
            }
            cursor.last.clone()
        } else {
            String::new()
        };
        let budget = self.budget.clone();
        let schema_query = schema.clone();
        let search_query = search.clone();
        let rows = self
            .runtime
            .with_connection(
                context,
                session,
                restoreplane_domain::workbench::AuthorizePurpose::Read,
                move |connection| {
                    Box::pin(async move {
                        let engine = connection.facts().engine;
                        if engine == Engine::Sqlite && kind == CatalogKind::Routines {
                            return Ok((
                                engine,
                                crate::types::BoundedRows::new(&budget, limit + 1, 512 * 1024)?,
                            ));
                        }
                        let (sql, params) = listing_sql(
                            engine,
                            kind,
                            schema_query.as_deref(),
                            &search_query,
                            &last,
                            limit + 1,
                        )?;
                        let rows = read(connection, &budget, &sql, &params, limit + 1).await?;
                        Ok((engine, rows))
                    })
                },
            )
            .await?;
        let (engine, mut rows) = rows;
        let more = rows.rows.len() > limit;
        rows.rows.truncate(limit);
        let mut items = Vec::with_capacity(rows.rows.len());
        let mut final_name = String::new();
        for row in rows.rows.drain(..) {
            let name = text(&row[0])?.to_owned();
            let kind_name = text(&row[1])?.to_owned();
            let schema_name = if kind == CatalogKind::Schemas {
                name.clone()
            } else {
                text(&row[2])?.to_owned()
            };
            let version = if engine == Engine::Sqlite {
                digest(&[
                    &schema_name,
                    &name,
                    &kind_name,
                    text(&row[3])?,
                    text(&row[4])?,
                ])
            } else {
                checked_fingerprint(text(&row[3])?)?
            };
            let storage_engine = if engine == Engine::Sqlite {
                None
            } else {
                optional_text(&row[4])?.map(str::to_owned)
            };
            let item = self.register(
                scope.clone(),
                schema_name,
                name.clone(),
                kind_name,
                storage_engine,
                version,
            )?;
            final_name = name;
            items.push(item);
        }
        let cursor = if more {
            let mut cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            cursors.retain(|_, cursor| cursor.touched.elapsed() < Duration::from_secs(120));
            if cursors.len() >= 1024 {
                return Err(WorkbenchError::Limit);
            }
            let id = Uuid::new_v4();
            cursors.insert(
                id,
                CatalogCursor {
                    scope,
                    parent,
                    kind,
                    search,
                    last: final_name,
                    touched: Instant::now(),
                    _memory: self.budget.reserve(2048)?,
                },
            );
            Some(id.to_string())
        } else {
            None
        };
        let result = Page {
            items,
            next_cursor: cursor,
            completeness: if more {
                Completeness::Limited
            } else {
                Completeness::Complete
            },
            count: Count::Unknown,
            as_of_ms: now_ms().into(),
        };
        encoded_size(&result, 512 * 1024)?;
        Ok(result)
    }
    pub(crate) fn node(
        &self,
        scope: &ScopeKey,
        object: &ObjectRef,
    ) -> Result<Arc<Node>, WorkbenchError> {
        let nodes = self.nodes.lock().map_err(|_| WorkbenchError::Io)?;
        let node = nodes
            .get(&object.catalog_id)
            .ok_or(WorkbenchError::CursorInvalid)?;
        if node.scope != *scope
            || node.item.object != *object
            || now_ms().saturating_sub(node.touched.load(Ordering::Relaxed)) > 120_000
        {
            return Err(WorkbenchError::CursorInvalid);
        }
        node.touched.store(now_ms(), Ordering::Relaxed);
        Ok(node.clone())
    }
    fn register(
        &self,
        scope: ScopeKey,
        schema: String,
        name: String,
        kind: String,
        _storage_engine: Option<String>,
        version: String,
    ) -> Result<CatalogItem, WorkbenchError> {
        let id = stable_id(&[
            &scope.session.to_string(),
            &scope.revision,
            &schema,
            &name,
            &kind,
            &version,
        ]);
        let item = CatalogItem {
            object: ObjectRef {
                catalog_id: id,
                schema_version: version,
            },
            name: name.clone(),
            kind: kind.clone(),
        };
        let mut nodes = self.nodes.lock().map_err(|_| WorkbenchError::Io)?;
        nodes.retain(|_, node| {
            now_ms().saturating_sub(node.touched.load(Ordering::Relaxed)) < 120_000
        });
        if !nodes.contains_key(&id) {
            if nodes.len() >= 2048 {
                let mut oldest = nodes
                    .iter()
                    .map(|(id, node)| (*id, node.touched.load(Ordering::Relaxed)))
                    .collect::<Vec<_>>();
                oldest.sort_unstable_by_key(|(_, time)| *time);
                for (id, _) in oldest.into_iter().take(256) {
                    nodes.remove(&id);
                }
            }
            let memory = self
                .budget
                .reserve(encoded_size(&item, 512 * 1024)? * 3 + schema.len() + 512)?;
            nodes.insert(
                id,
                Arc::new(Node {
                    scope,
                    item: item.clone(),
                    schema,
                    name,
                    kind,
                    touched: AtomicU64::new(now_ms()),
                    _memory: memory,
                }),
            );
        }
        Ok(item)
    }
    pub async fn resolve_relation(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        object: &ObjectRef,
    ) -> Result<Arc<RelationMetadata>, WorkbenchError> {
        self.resolve_with_lease(context, session, object, None)
            .await
    }
    /// Resolve parser-observed, fully qualified identifiers through the same
    /// native catalog and scope registry as the explorer. No SQL is accepted.
    pub(crate) async fn resolve_named(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        schema: &str,
        name: &str,
        lease: Option<&crate::session::ConnectionLease>,
    ) -> Result<Arc<RelationMetadata>, WorkbenchError> {
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        let schema_query = schema.to_owned();
        let name_query = name.to_owned();
        let budget = self.budget.clone();
        let (engine, listed) = if let Some(lease) = lease {
            self.runtime
                .with_lease(
                    context,
                    lease,
                    restoreplane_domain::workbench::AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(named_rows(connection, budget, schema_query, name_query))
                    },
                )
                .await?
        } else {
            self.runtime
                .with_connection(
                    context,
                    session,
                    restoreplane_domain::workbench::AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(named_rows(connection, budget, schema_query, name_query))
                    },
                )
                .await?
        };
        let row = listed
            .rows
            .iter()
            .find(|row| {
                text(&row[0]).is_ok_and(|value| value == name)
                    && text(&row[2]).is_ok_and(|value| value == schema)
            })
            .ok_or(WorkbenchError::Unsupported)?;
        let kind = text(&row[1])?;
        let version = if engine == Engine::Sqlite {
            digest(&[schema, name, kind, text(&row[3])?, text(&row[4])?])
        } else {
            checked_fingerprint(text(&row[3])?)?
        };
        let storage = if engine == Engine::Sqlite {
            None
        } else {
            optional_text(&row[4])?.map(str::to_owned)
        };
        let item = self.register(
            scope,
            schema.into(),
            name.into(),
            kind.into(),
            storage,
            version,
        )?;
        self.resolve_with_lease(context, session, &item.object, lease)
            .await
    }
    /// Revalidate an already registered native object while its physical lease
    /// is held. The caller establishes the engine's transaction/schema lock.
    pub(crate) async fn reload_on_connection(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        object: &ObjectRef,
        connection: &mut dyn DriverConnection,
    ) -> Result<Arc<RelationMetadata>, WorkbenchError> {
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        let node = self.node(&scope, object)?;
        Ok(Arc::new(
            load_relation(
                connection,
                &self.budget,
                object.clone(),
                &scope,
                &node.schema,
                &node.name,
                &object.schema_version,
            )
            .await?,
        ))
    }
    pub async fn resolve_with_lease(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        object: &ObjectRef,
        lease: Option<&crate::session::ConnectionLease>,
    ) -> Result<Arc<RelationMetadata>, WorkbenchError> {
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        if self.budget.retained_bytes() > 40 * 1024 * 1024 {
            self.relations
                .lock()
                .map_err(|_| WorkbenchError::Io)?
                .clear();
        }
        let node = self.node(&scope, object)?;
        if !matches!(node.kind.as_str(), "table" | "view") {
            return Err(WorkbenchError::Unsupported);
        }
        let budget = self.budget.clone();
        let schema = node.schema.clone();
        let name = node.name.clone();
        let expected = object.schema_version.clone();
        let object_copy = object.clone();
        let load_scope = scope.clone();
        let mut relation = if let Some(lease) = lease {
            self.runtime
                .with_lease(
                    context,
                    lease,
                    restoreplane_domain::workbench::AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            logical_relation_result(
                                load_relation(
                                    connection,
                                    &budget,
                                    object_copy,
                                    &load_scope,
                                    &schema,
                                    &name,
                                    &expected,
                                )
                                .await,
                            )
                        })
                    },
                )
                .await??
        } else {
            self.runtime
                .with_connection(
                    context,
                    session,
                    restoreplane_domain::workbench::AuthorizePurpose::Read,
                    move |connection| {
                        Box::pin(async move {
                            logical_relation_result(
                                load_relation(
                                    connection,
                                    &budget,
                                    object_copy,
                                    &load_scope,
                                    &schema,
                                    &name,
                                    &expected,
                                )
                                .await,
                            )
                        })
                    },
                )
                .await??
        };
        let certified = self
            .runtime
            .get(context, session)?
            .capabilities
            .get("verified_editing")
            .is_some_and(|cap| cap.supported);
        let basic = certified
            && relation.kind == "table"
            && relation.stable_key().is_some_and(|key| {
                key.columns.iter().all(|id| {
                    relation
                        .columns
                        .iter()
                        .find(|c| c.column.column_id == *id)
                        .is_some_and(|c| {
                            crate::generated_sql::column_supports_editing(relation.engine, c, true)
                        })
                })
            })
            && relation.columns.iter().all(|c| {
                !c.generated
                    && (!c.defaulted || c.null_default)
                    && crate::generated_sql::column_supports_editing(relation.engine, c, false)
            });
        let mut eligible = basic;
        if basic {
            let candidate = Arc::new(relation);
            let native = candidate.clone();
            let budget = self.budget.clone();
            eligible = if let Some(lease) = lease {
                self.runtime
                    .with_lease(
                        context,
                        lease,
                        restoreplane_domain::workbench::AuthorizePurpose::Read,
                        move |connection| Box::pin(editing_eligible(connection, budget, native)),
                    )
                    .await?
            } else {
                self.runtime
                    .with_connection(
                        context,
                        session,
                        restoreplane_domain::workbench::AuthorizePurpose::Read,
                        move |connection| Box::pin(editing_eligible(connection, budget, native)),
                    )
                    .await?
            };
            relation = Arc::try_unwrap(candidate).map_err(|_| WorkbenchError::Io)?;
        }
        for column in &mut relation.columns {
            column.column.editable = eligible;
            if eligible {
                column.column.readonly_reason = None;
            } else if certified {
                column.column.readonly_reason = Some(
                    if basic {
                        "transactional_effects_unproven"
                    } else {
                        "unsupported_type"
                    }
                    .into(),
                );
            }
        }
        for target in &relation.foreign_targets {
            self.register(
                scope.clone(),
                target.schema.clone(),
                target.name.clone(),
                target.kind.clone(),
                None,
                target.version.clone(),
            )?;
        }
        let relation = Arc::new(relation);
        let mut cache = self.relations.lock().map_err(|_| WorkbenchError::Io)?;
        if cache.len() >= 64 {
            cache.clear();
        }
        cache.insert(object.catalog_id, relation.clone());
        Ok(relation)
    }
    pub async fn describe(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        object: ObjectRef,
        part: DescribePart,
        page: PageInput,
    ) -> Result<Page<CatalogDetail>, WorkbenchError> {
        let limit = page_limit(page.limit)?;
        let relation = self.resolve_relation(context, session, &object).await?;
        let all = match part {
            DescribePart::Columns => relation
                .columns
                .iter()
                .map(|value| CatalogDetail::Column {
                    column: value.column.clone(),
                    ordinal: value.ordinal,
                })
                .collect::<Vec<_>>(),
            DescribePart::Keys => relation
                .keys
                .iter()
                .map(|key| CatalogDetail::Key {
                    key_id: key.key_id,
                    columns: key.columns.clone(),
                    primary: key.primary,
                    unique: key.unique,
                    nullable: key.nullable,
                    partial: key.partial,
                    expression: key.expression,
                })
                .collect(),
            DescribePart::Indexes => relation.indexes.clone(),
            DescribePart::ForeignKeys => relation.foreign_keys.clone(),
        };
        // Describe cursors bind the complete request digest and native context.
        let marker = format!("describe:{}:{part:?}", object.catalog_id);
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        let start = if let Some(value) = page.cursor {
            let id = Uuid::parse_str(&value).map_err(|_| WorkbenchError::CursorInvalid)?;
            let cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            let cursor = cursors.get(&id).ok_or(WorkbenchError::CursorInvalid)?;
            if cursor.scope != scope
                || cursor.parent.as_ref() != Some(&object)
                || cursor.search != marker
                || cursor.touched.elapsed() > Duration::from_secs(120)
            {
                return Err(WorkbenchError::CursorInvalid);
            }
            cursor
                .last
                .parse::<usize>()
                .map_err(|_| WorkbenchError::CursorInvalid)?
        } else {
            0
        };
        if start > all.len() {
            return Err(WorkbenchError::CursorInvalid);
        }
        let end = start.saturating_add(limit).min(all.len());
        let next = if end < all.len() {
            let mut cursors = self.cursors.lock().map_err(|_| WorkbenchError::Io)?;
            if cursors.len() >= 1024 {
                return Err(WorkbenchError::Limit);
            }
            let id = Uuid::new_v4();
            cursors.insert(
                id,
                CatalogCursor {
                    scope,
                    parent: Some(object),
                    kind: CatalogKind::Relations,
                    search: marker,
                    last: end.to_string(),
                    touched: Instant::now(),
                    _memory: self.budget.reserve(2048)?,
                },
            );
            Some(id.to_string())
        } else {
            None
        };
        let result = Page {
            items: all[start..end].to_vec(),
            next_cursor: next,
            completeness: if end < all.len() {
                Completeness::Limited
            } else {
                Completeness::Complete
            },
            count: Count::Exact {
                value: (all.len() as u64).into(),
            },
            as_of_ms: now_ms().into(),
        };
        encoded_size(&result, 512 * 1024)?;
        Ok(result)
    }
    pub async fn diagram(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        objects: &[ObjectRef],
    ) -> Result<Diagram, WorkbenchError> {
        if objects.len() > 100 {
            return Err(WorkbenchError::Limit);
        }
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut fingerprints = Vec::new();
        let mut limited = false;
        for object in objects {
            let relation = self.resolve_relation(context, session, object).await?;
            nodes.push(CatalogItem {
                object: object.clone(),
                name: relation.name.clone(),
                kind: relation.kind.clone(),
            });
            fingerprints.push(relation.object.schema_version.clone());
            for edge in &relation.foreign_keys {
                if edges.len() == 200 {
                    limited = true;
                    break;
                }
                edges.push(edge.clone());
            }
        }
        let version = digest(&fingerprints.iter().map(String::as_str).collect::<Vec<_>>());
        let result = Diagram {
            nodes,
            edges,
            schema_version: version,
            limited,
        };
        encoded_size(&result, 512 * 1024)?;
        Ok(result)
    }
    pub async fn graph(
        &self,
        context: &InvocationContext,
        session: SessionRef,
        schema: ObjectRef,
        page: PageInput,
    ) -> Result<Page<restoreplane_domain::workbench_metadata::GraphNode>, WorkbenchError> {
        use restoreplane_domain::workbench_metadata::{GraphEdge, GraphNode};
        let scope = ScopeKey::current(&self.runtime, context, session)?;
        let namespace = self.node(&scope, &schema)?;
        if namespace.kind != "schema" {
            return Err(WorkbenchError::InvalidInput);
        }
        let listed = self
            .page(
                context,
                session,
                Some(schema),
                CatalogKind::Relations,
                String::new(),
                PageInput {
                    cursor: page.cursor,
                    limit: page_limit(page.limit)?.min(100),
                },
            )
            .await?;
        let mut nodes = listed
            .items
            .iter()
            .map(|item| GraphNode {
                node_id: item.object.catalog_id,
                relation: item.object.clone(),
                name: item.name.clone(),
                edges: Vec::new(),
            })
            .collect::<Vec<_>>();
        let names = nodes
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>();
        let budget = self.budget.clone();
        let schema_name = namespace.name.clone();
        let mut limited = listed.completeness == Completeness::Limited;
        if !names.is_empty() {
            let raw=self.runtime.with_connection(context,session,restoreplane_domain::workbench::AuthorizePurpose::Read,move|connection|Box::pin(async move {
                let engine=connection.facts().engine;
                let mut params=if engine==Engine::Sqlite {Vec::new()}else{vec![TypedCell::Text {value:schema_name.clone()}]};
                let places=names.iter().map(|name|{params.push(TypedCell::Text {value:name.clone()});placeholder(engine,params.len())}).collect::<Vec<_>>().join(",");
                let sql=match engine {
                    Engine::Postgresql=>format!("SELECT c.relname::text,rn.nspname::text,rc.relname::text,k.conname::text FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_class rc ON rc.oid=k.confrelid JOIN pg_catalog.pg_namespace rn ON rn.oid=rc.relnamespace WHERE k.contype='f' AND n.nspname=$1 AND c.relname IN({places}) ORDER BY c.relname,k.conname LIMIT 201"),
                    Engine::Sqlite=>format!("SELECT DISTINCT t.name,'main',f.\"table\",CAST(f.id AS TEXT) FROM sqlite_schema t JOIN pragma_foreign_key_list(t.name) f WHERE t.name IN({places}) ORDER BY t.name,f.id LIMIT 201"),
                    _=>format!("SELECT DISTINCT TABLE_NAME,REFERENCED_TABLE_SCHEMA,REFERENCED_TABLE_NAME,CONSTRAINT_NAME FROM information_schema.KEY_COLUMN_USAGE WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME IN({places}) AND REFERENCED_TABLE_NAME IS NOT NULL ORDER BY TABLE_NAME,CONSTRAINT_NAME LIMIT 201"),
                };
                let edges=read(connection,&budget,&sql,&params,201).await?;
                let (sql,mut verify_params)=listing_sql(engine,CatalogKind::Relations,Some(&schema_name),"","",201)?;
                let places=names.iter().map(|name|{verify_params.push(TypedCell::Text {value:name.clone()});placeholder(engine,verify_params.len())}).collect::<Vec<_>>().join(",");
                let column=match engine {Engine::Postgresql=>"c.relname",Engine::Sqlite=>"s.name",_=>"t.TABLE_NAME"};
                let boundary=sql.rfind(" ORDER BY ").ok_or(WorkbenchError::InvalidInput)?;
                let verify_sql=format!("{} AND {column} IN({places}){}",&sql[..boundary],&sql[boundary..]);
                let verified=read(connection,&budget,&verify_sql,&verify_params,201).await?;
                Ok((engine,schema_name,edges,verified))
            })).await?;
            let (engine, schema_name, edges, verified) = raw;
            for node in &nodes {
                let row = verified
                    .rows
                    .iter()
                    .find(|row| text(&row[0]).is_ok_and(|name| name == node.name))
                    .ok_or(WorkbenchError::CursorInvalid)?;
                let version = if engine == Engine::Sqlite {
                    digest(&[
                        &schema_name,
                        &node.name,
                        text(&row[1])?,
                        text(&row[3])?,
                        text(&row[4])?,
                    ])
                } else {
                    checked_fingerprint(text(&row[3])?)?
                };
                if version != node.relation.schema_version {
                    return Err(WorkbenchError::CursorInvalid);
                }
            }
            limited |= edges.rows.len() > 200;
            for row in edges.rows.iter().take(200) {
                let from = text(&row[0])?;
                let target_schema = text(&row[1])?;
                let to = text(&row[2])?;
                let name = text(&row[3])?;
                let target = nodes
                    .iter()
                    .find(|node| target_schema == schema_name && node.name == to)
                    .map(|node| node.node_id);
                if let Some(target) = target {
                    if let Some(source) = nodes.iter_mut().find(|node| node.name == from) {
                        source.edges.push(GraphEdge {
                            to: target,
                            name: name.into(),
                        });
                    }
                } else {
                    limited = true;
                }
            }
        }
        let result = Page {
            items: nodes,
            next_cursor: listed.next_cursor,
            completeness: if limited {
                Completeness::Limited
            } else {
                Completeness::Complete
            },
            count: Count::Unknown,
            as_of_ms: now_ms().into(),
        };
        encoded_size(&result, 512 * 1024)?;
        Ok(result)
    }
}
pub(crate) fn stable_id(parts: &[&str]) -> Uuid {
    let hash = digest(parts);
    let mut bytes = [0u8; 16];
    for (index, chunk) in hash.as_bytes().chunks_exact(2).take(16).enumerate() {
        bytes[index] =
            u8::from_str_radix(std::str::from_utf8(chunk).unwrap_or("00"), 16).unwrap_or(0);
    }
    Uuid::from_bytes(bytes)
}
pub(crate) fn digest(parts: &[&str]) -> String {
    let mut hash = blake3::Hasher::new();
    for part in parts {
        hash.update(&(part.len() as u64).to_le_bytes());
        hash.update(part.as_bytes());
    }
    hash.finalize().to_hex().to_string()
}
fn page_limit(limit: usize) -> Result<usize, WorkbenchError> {
    if limit == 0 {
        Ok(100)
    } else if limit <= 200 {
        Ok(limit)
    } else {
        Err(WorkbenchError::Limit)
    }
}
fn checked_fingerprint(value: &str) -> Result<String, WorkbenchError> {
    let (hash, size) = value.split_once(':').unwrap_or((value, "0"));
    if hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
        || size
            .parse::<usize>()
            .map_err(|_| WorkbenchError::Unsupported)?
            > 1024 * 1024
    {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(hash.to_ascii_lowercase())
}

fn pg_fingerprint(relation: &str, namespace: &str) -> String {
    use restoreplane_domain::workbench_structure::{
        postgresql_relation_signature, PgRelationAlias,
    };
    postgresql_relation_signature(match (relation, namespace) {
        ("c", "n") => PgRelationAlias::Relation,
        ("rc", "rn") => PgRelationAlias::ForeignTarget,
        _ => unreachable!("native catalog aliases are fixed"),
    })
}
fn mysql_fingerprint() -> String {
    restoreplane_domain::workbench_structure::mysql_relation_signature()
}
fn listing_sql(
    engine: Engine,
    kind: CatalogKind,
    schema: Option<&str>,
    search: &str,
    last: &str,
    limit: usize,
) -> Result<(String, Vec<TypedCell>), WorkbenchError> {
    let pattern = format!(
        "{}%",
        search
            .replace('!', "!!")
            .replace('%', "!%")
            .replace('_', "!_")
    );
    let mut params = vec![
        TypedCell::Text { value: pattern },
        TypedCell::Text { value: last.into() },
    ];
    let p1 = placeholder(engine, 1);
    let p2 = placeholder(engine, 2);
    let sql=match (engine,kind) {
        (Engine::Postgresql,CatalogKind::Schemas)=>format!("SELECT n.nspname::text,'schema'::text,n.nspname::text,pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(n.oid::text||n.nspname,'UTF8')),'hex'),NULL::text FROM pg_catalog.pg_namespace n WHERE n.nspname LIKE {p1} ESCAPE '!' AND n.nspname::text COLLATE \"C\">{p2} COLLATE \"C\" ORDER BY n.nspname::text COLLATE \"C\" LIMIT {limit}"),
        (Engine::Postgresql,CatalogKind::Relations)=>{params.push(TypedCell::Text {value:schema.unwrap_or("public").into()});format!("SELECT c.relname::text,CASE WHEN c.relkind IN('v','m') THEN 'view' ELSE 'table' END,n.nspname::text,{},NULL::text FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace WHERE c.relkind IN('r','p','v','m') AND c.relname LIKE {p1} ESCAPE '!' AND c.relname::text COLLATE \"C\">{p2} COLLATE \"C\" AND n.nspname=$3 ORDER BY c.relname::text COLLATE \"C\" LIMIT {limit}",pg_fingerprint("c","n"))},
        (Engine::Postgresql,CatalogKind::Routines)=>{params.push(TypedCell::Text {value:schema.unwrap_or("public").into()});format!("SELECT (p.proname||'('||p.oid::text||')')::text,'routine'::text,n.nspname::text,pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(p.oid::text||p.prosrc,'UTF8')),'hex'),NULL::text FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid=p.pronamespace WHERE p.proname LIKE {p1} ESCAPE '!' AND (p.proname||'('||p.oid::text||')') COLLATE \"C\">{p2} COLLATE \"C\" AND n.nspname=$3 ORDER BY (p.proname||'('||p.oid::text||')') COLLATE \"C\" LIMIT {limit}")},
        (Engine::Sqlite,CatalogKind::Schemas)=>format!("SELECT 'main','schema','main','main','' WHERE 'main' LIKE {p1} ESCAPE '!' AND 'main'>{p2} LIMIT {limit}"),
        (Engine::Sqlite,_)=>format!("SELECT s.name,s.type,'main',s.sql,COALESCE((SELECT group_concat(x.sql,char(10) ORDER BY x.name) FROM main.sqlite_schema x WHERE x.tbl_name=s.name AND x.type IN('index','trigger')),'') FROM main.sqlite_schema s WHERE s.type IN('table','view') AND s.name NOT LIKE 'sqlite_%' AND s.name LIKE {p1} ESCAPE '!' AND s.name COLLATE BINARY>{p2} COLLATE BINARY ORDER BY s.name COLLATE BINARY LIMIT {limit}"),
        (_,CatalogKind::Schemas)=>format!("SELECT DATABASE(),'schema',DATABASE(),SHA2(DATABASE(),256),NULL WHERE DATABASE() LIKE ? ESCAPE '!' AND BINARY DATABASE()>BINARY ? LIMIT {limit}"),
        (_,CatalogKind::Relations)=>{params.push(TypedCell::Text {value:schema.unwrap_or("").into()});format!("SELECT t.TABLE_NAME,CASE WHEN t.TABLE_TYPE='VIEW' THEN 'view' ELSE 'table' END,t.TABLE_SCHEMA,{},t.ENGINE FROM information_schema.TABLES t WHERE BINARY t.TABLE_NAME LIKE BINARY ? ESCAPE '!' AND BINARY t.TABLE_NAME>BINARY ? AND BINARY t.TABLE_SCHEMA=BINARY COALESCE(NULLIF(?,''),DATABASE()) ORDER BY BINARY t.TABLE_NAME LIMIT {limit}",mysql_fingerprint())},
        (_,CatalogKind::Routines)=>{params.push(TypedCell::Text {value:schema.unwrap_or("").into()});format!("SELECT ROUTINE_NAME,'routine',ROUTINE_SCHEMA,SHA2(CONCAT(ROUTINE_NAME,COALESCE(ROUTINE_DEFINITION,'')),256),NULL FROM information_schema.ROUTINES WHERE ROUTINE_NAME LIKE ? ESCAPE '!' AND BINARY ROUTINE_NAME>BINARY ? AND BINARY ROUTINE_SCHEMA=BINARY COALESCE(NULLIF(?,''),DATABASE()) ORDER BY BINARY ROUTINE_NAME LIMIT {limit}")},
    };
    Ok((sql, params))
}

async fn named_rows(
    connection: &mut dyn DriverConnection,
    budget: Arc<NativeBudget>,
    schema: String,
    name: String,
) -> Result<(Engine, crate::types::BoundedRows), WorkbenchError> {
    let engine = connection.facts().engine;
    let (sql, params) = listing_sql(engine, CatalogKind::Relations, Some(&schema), &name, "", 2)?;
    let listed = read(connection, &budget, &sql, &params, 2).await?;
    Ok((engine, listed))
}
async fn load_relation(
    connection: &mut dyn DriverConnection,
    budget: &Arc<NativeBudget>,
    object: ObjectRef,
    scope: &ScopeKey,
    schema: &str,
    name: &str,
    expected: &str,
) -> Result<RelationMetadata, WorkbenchError> {
    let engine = connection.facts().engine;
    let (sql, params) = listing_sql(engine, CatalogKind::Relations, Some(schema), name, "", 2)?;
    let listed = read(connection, budget, &sql, &params, 2).await?;
    let row = listed
        .rows
        .iter()
        .find(|row| text(&row[0]).is_ok_and(|actual| actual == name))
        .ok_or(WorkbenchError::CursorInvalid)?;
    let version = if engine == Engine::Sqlite {
        digest(&[schema, name, text(&row[1])?, text(&row[3])?, text(&row[4])?])
    } else {
        checked_fingerprint(text(&row[3])?)?
    };
    if version != expected {
        return Err(WorkbenchError::CursorInvalid);
    }
    let kind = text(&row[1])?.to_owned();
    let storage_engine = if engine == Engine::Sqlite {
        None
    } else {
        optional_text(&row[4])?.map(str::to_owned)
    };
    let params = vec![
        TypedCell::Text {
            value: schema.into(),
        },
        TypedCell::Text { value: name.into() },
    ];
    let columns_sql:String=match engine {
        Engine::Postgresql=>"SELECT a.attname::text,t.typname::text,a.attnum::bigint,a.attnotnull,(a.attgenerated<>'' OR a.attidentity<>''),pg_catalog.format_type(a.atttypid,a.atttypmod),a.atthasdef,false,t.oid::bigint,tn.nspname::text FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid=a.attrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_type t ON t.oid=a.atttypid JOIN pg_catalog.pg_namespace tn ON tn.oid=t.typnamespace WHERE n.nspname=$1 AND c.relname=$2 AND a.attnum>0 AND NOT a.attisdropped ORDER BY a.attnum LIMIT 513".into(),
        Engine::Sqlite=>"SELECT name,type,cid+1,\"notnull\",hidden,type,dflt_value IS NOT NULL,0,NULL,NULL FROM pragma_table_xinfo(?2) ORDER BY cid LIMIT 513".into(),
        _=>format!("SELECT COLUMN_NAME,DATA_TYPE,ORDINAL_POSITION,IS_NULLABLE='NO',EXTRA LIKE '%GENERATED%',COLUMN_TYPE,(COLUMN_DEFAULT IS NOT NULL OR EXTRA LIKE '%auto_increment%'),{},NULL,NULL FROM information_schema.COLUMNS WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ? ORDER BY ORDINAL_POSITION LIMIT 513",if engine==Engine::Mariadb{"(COLUMN_DEFAULT='NULL' AND EXTRA NOT LIKE '%auto_increment%')"}else{"0"}),
    };
    let raw_columns = read(connection, budget, &columns_sql, &params, 513).await?;
    if raw_columns.rows.len() > 512 {
        return Err(WorkbenchError::Unsupported);
    }
    let mut columns = Vec::new();
    for row in &raw_columns.rows {
        let column_name = text(&row[0])?.to_owned();
        let ordinal =
            usize::try_from(integer(&row[2])?).map_err(|_| WorkbenchError::Unsupported)?;
        let nullable = !boolean(&row[3])?;
        let generated = if engine == Engine::Sqlite {
            integer(&row[4])? != 0
        } else {
            boolean(&row[4])?
        };
        let declared_type = text(&row[1])?.to_owned();
        let type_detail = text(&row[5])?.to_owned();
        let defaulted = boolean(&row[6])?;
        let null_default = if matches!(row[7], TypedCell::Null) {
            false
        } else {
            boolean(&row[7])?
        };
        let native_type_id = if matches!(row[8], TypedCell::Null) {
            None
        } else {
            Some(u32::try_from(integer(&row[8])?).map_err(|_| WorkbenchError::Unsupported)?)
        };
        let native_type_schema = optional_text(&row[9])?.map(str::to_owned);
        columns.push(ColumnMetadata {
            column: Column {
                column_id: stable_id(&[&object.catalog_id.to_string(), &column_name]),
                name: column_name,
                type_name: declared_type.clone(),
                nullable,
                editable: false,
                readonly_reason: Some("verified_editing_not_certified".into()),
            },
            ordinal,
            generated,
            declared_type,
            type_detail,
            defaulted,
            null_default,
            native_type_id,
            native_type_schema,
        });
    }
    let keys_sql:String=match engine {
        Engine::Postgresql=>"SELECT ci.relname::text,(x.ordinality-1)::bigint,a.attname::text,i.indisunique,i.indisprimary,(i.indpred IS NOT NULL),(i.indexprs IS NOT NULL) FROM pg_catalog.pg_index i JOIN pg_catalog.pg_class c ON c.oid=i.indrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_class ci ON ci.oid=i.indexrelid CROSS JOIN LATERAL pg_catalog.unnest(i.indkey) WITH ORDINALITY x(attnum,ordinality) LEFT JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum=x.attnum WHERE n.nspname=$1 AND c.relname=$2 AND x.ordinality<=i.indnkeyatts ORDER BY ci.relname,x.ordinality LIMIT 1025".into(),
        Engine::Sqlite=>"SELECT i.name,x.seqno,x.name,i.\"unique\",i.origin='pk',i.partial,x.cid<0 FROM pragma_index_list(?2) i JOIN pragma_index_xinfo(i.name) x WHERE x.key=1 ORDER BY i.name,x.seqno LIMIT 1025".into(),
        _=>"SELECT INDEX_NAME,SEQ_IN_INDEX-1,COLUMN_NAME,NON_UNIQUE=0,INDEX_NAME='PRIMARY',SUB_PART IS NOT NULL,COLUMN_NAME IS NULL FROM information_schema.STATISTICS WHERE BINARY TABLE_SCHEMA=BINARY ? AND BINARY TABLE_NAME=BINARY ? ORDER BY INDEX_NAME,SEQ_IN_INDEX LIMIT 1025".into(),
    };
    let raw_keys = read(connection, budget, &keys_sql, &params, 1024).await?;
    let mut grouped: BTreeMap<String, KeyMetadata> = BTreeMap::new();
    for row in &raw_keys.rows {
        let key_name = text(&row[0])?.to_owned();
        let key = grouped.entry(key_name.clone()).or_insert(KeyMetadata {
            key_id: stable_id(&[&object.catalog_id.to_string(), &key_name]),
            name: key_name,
            columns: Vec::new(),
            primary: boolean(&row[4])?,
            unique: boolean(&row[3])?,
            nullable: false,
            partial: boolean(&row[5])?,
            expression: boolean(&row[6])?,
        });
        if let Some(name) = optional_text(&row[2])? {
            if let Some(column) = columns.iter().find(|column| column.column.name == name) {
                key.columns.push(column.column.column_id);
                key.nullable |= column.column.nullable;
            } else {
                key.expression = true;
            }
        } else {
            key.expression = true;
        }
    }
    if engine == Engine::Sqlite {
        let primary = read(
            connection,
            budget,
            "SELECT name,pk FROM pragma_table_xinfo(?2) WHERE pk>0 ORDER BY pk LIMIT 513",
            &params,
            513,
        )
        .await?;
        if !primary.rows.is_empty() && !grouped.values().any(|key| key.primary) {
            let mut key = KeyMetadata {
                key_id: stable_id(&[&object.catalog_id.to_string(), "PRIMARY"]),
                name: "PRIMARY".into(),
                columns: Vec::new(),
                primary: true,
                unique: true,
                nullable: false,
                partial: false,
                expression: false,
            };
            for row in &primary.rows {
                let name = text(&row[0])?;
                let column = columns
                    .iter_mut()
                    .find(|column| column.column.name == name)
                    .ok_or(WorkbenchError::Unsupported)?;
                if primary.rows.len() == 1 && column.declared_type.eq_ignore_ascii_case("INTEGER") {
                    column.column.nullable = false;
                }
                key.nullable |= column.column.nullable;
                key.columns.push(column.column.column_id);
            }
            grouped.insert("PRIMARY".into(), key);
        }
    }
    if grouped.len() > 128 {
        return Err(WorkbenchError::Unsupported);
    }
    let keys = grouped.into_values().collect::<Vec<_>>();
    let indexes = keys
        .iter()
        .map(|key| CatalogDetail::Index {
            index_id: key.key_id,
            columns: key.columns.clone(),
            unique: key.unique,
            predicate: if key.partial {
                Some("partial_or_prefix_index".into())
            } else {
                None
            },
        })
        .collect::<Vec<_>>();
    let fk_sql=match engine {
        Engine::Postgresql=>format!("SELECT k.conname::text,x.ordinality::bigint,a.attname::text,rn.nspname::text,rc.relname::text,ra.attname::text,{},'table'::text,''::text FROM pg_catalog.pg_constraint k JOIN pg_catalog.pg_class c ON c.oid=k.conrelid JOIN pg_catalog.pg_namespace n ON n.oid=c.relnamespace JOIN pg_catalog.pg_class rc ON rc.oid=k.confrelid JOIN pg_catalog.pg_namespace rn ON rn.oid=rc.relnamespace CROSS JOIN LATERAL ROWS FROM (pg_catalog.unnest(k.conkey),pg_catalog.unnest(k.confkey)) WITH ORDINALITY x(localnum,remotenum,ordinality) JOIN pg_catalog.pg_attribute a ON a.attrelid=c.oid AND a.attnum=x.localnum JOIN pg_catalog.pg_attribute ra ON ra.attrelid=rc.oid AND ra.attnum=x.remotenum WHERE k.contype='f' AND n.nspname=$1 AND c.relname=$2 ORDER BY k.conname,x.ordinality LIMIT 1025",pg_fingerprint("rc","rn")),
        Engine::Sqlite=>"SELECT CAST(f.id AS TEXT),f.seq+1,f.\"from\",'main',f.\"table\",COALESCE(f.\"to\",(SELECT p.name FROM pragma_table_xinfo(f.\"table\") p WHERE p.pk=f.seq+1)),COALESCE(t.sql,''),COALESCE(t.type,'missing'),COALESCE((SELECT group_concat(x.sql,char(10) ORDER BY x.name) FROM sqlite_schema x WHERE x.tbl_name=f.\"table\" AND x.type IN('index','trigger')),'') FROM pragma_foreign_key_list(?2) f LEFT JOIN sqlite_schema t ON t.name=f.\"table\" ORDER BY f.id,f.seq LIMIT 1025".into(),
        _=>format!("SELECT k.CONSTRAINT_NAME,k.ORDINAL_POSITION,k.COLUMN_NAME,k.REFERENCED_TABLE_SCHEMA,k.REFERENCED_TABLE_NAME,k.REFERENCED_COLUMN_NAME,{},'table','' FROM information_schema.KEY_COLUMN_USAGE k JOIN information_schema.TABLES t ON BINARY t.TABLE_SCHEMA=BINARY k.REFERENCED_TABLE_SCHEMA AND BINARY t.TABLE_NAME=BINARY k.REFERENCED_TABLE_NAME WHERE BINARY k.TABLE_SCHEMA=BINARY ? AND BINARY k.TABLE_NAME=BINARY ? AND k.REFERENCED_TABLE_NAME IS NOT NULL ORDER BY k.CONSTRAINT_NAME,k.ORDINAL_POSITION LIMIT 1025",mysql_fingerprint()),
    };
    let raw_fk = read(connection, budget, &fk_sql, &params, 1024).await?;
    let mut foreign: BTreeMap<String, (Vec<Uuid>, ForeignTarget, Vec<String>)> = BTreeMap::new();
    for row in &raw_fk.rows {
        let target_schema = text(&row[3])?;
        let target_name = text(&row[4])?;
        let target_kind = text(&row[7])?;
        let target_version = if engine == Engine::Sqlite {
            digest(&[
                target_schema,
                target_name,
                target_kind,
                text(&row[6])?,
                text(&row[8])?,
            ])
        } else {
            checked_fingerprint(text(&row[6])?)?
        };
        let entry = foreign.entry(text(&row[0])?.into()).or_insert((
            Vec::new(),
            ForeignTarget {
                schema: target_schema.into(),
                name: target_name.into(),
                kind: target_kind.into(),
                version: target_version,
            },
            Vec::new(),
        ));
        let name = text(&row[2])?;
        entry.0.push(
            columns
                .iter()
                .find(|column| column.column.name == name)
                .ok_or(WorkbenchError::Unsupported)?
                .column
                .column_id,
        );
        entry.2.push(optional_text(&row[5])?.unwrap_or("").into());
    }
    let mut foreign_targets = Vec::new();
    let foreign_keys = foreign
        .into_iter()
        .map(|(name, (local, target_info, remote))| {
            let target = stable_id(&[
                &scope.session.to_string(),
                &scope.revision,
                &target_info.schema,
                &target_info.name,
                &target_info.kind,
                &target_info.version,
            ]);
            let target_version = target_info.version.clone();
            foreign_targets.push(target_info);
            CatalogDetail::ForeignKey {
                key_id: stable_id(&[&object.catalog_id.to_string(), &name]),
                columns: local,
                referenced_relation: ObjectRef {
                    catalog_id: target,
                    schema_version: target_version,
                },
                referenced_columns: remote
                    .iter()
                    .map(|column| stable_id(&[&target.to_string(), column]))
                    .collect(),
            }
        })
        .collect::<Vec<_>>();
    let memory = budget.reserve(
        encoded_size(&columns, 512 * 1024)?
            .checked_mul(4)
            .ok_or(WorkbenchError::Limit)?
            + encoded_size(&keys, 512 * 1024)? * 4
            + encoded_size(&indexes, 512 * 1024)? * 4
            + encoded_size(&foreign_keys, 512 * 1024)? * 4
            + encoded_size(&foreign_targets, 512 * 1024)? * 4
            + 4096,
    )?;
    Ok(RelationMetadata {
        scope: scope.clone(),
        object,
        engine,
        schema: schema.into(),
        name: name.into(),
        kind,
        storage_engine,
        columns,
        keys,
        indexes,
        foreign_keys,
        foreign_targets,
        _memory: memory,
    })
}
