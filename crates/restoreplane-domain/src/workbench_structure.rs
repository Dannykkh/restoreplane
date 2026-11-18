//! Fixed catalog signature expressions shared by native readers. These are
//! not user SQL generators: aliases and catalog attributes are closed constants.
//! PostgreSQL callers must hold pg_catalog-only lookup while executing them.
#[derive(Clone, Copy, Debug)]
pub enum PgRelationAlias {
    Relation,
    ForeignTarget,
}

pub fn postgresql_relation_signature(alias: PgRelationAlias) -> String {
    let (relation, namespace) = match alias {
        PgRelationAlias::Relation => ("c", "n"),
        PgRelationAlias::ForeignTarget => ("rc", "rn"),
    };
    format!("pg_catalog.encode(pg_catalog.sha256(pg_catalog.convert_to(pg_catalog.concat_ws('|',{relation}.oid::text,{namespace}.nspname,{relation}.relname,{relation}.relkind::text,COALESCE((SELECT pg_catalog.string_agg(pg_catalog.jsonb_build_array(a.attnum,a.attname,a.atttypid,a.atttypmod,a.attnotnull,a.attgenerated,a.attidentity,a.attcollation,pg_catalog.pg_get_expr(d.adbin,d.adrelid))::text,E'\\n' ORDER BY a.attnum) FROM pg_catalog.pg_attribute a LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid=a.attrelid AND d.adnum=a.attnum WHERE a.attrelid={relation}.oid AND a.attnum>0 AND NOT a.attisdropped),''),COALESCE((SELECT pg_catalog.string_agg(x.oid::text||pg_catalog.pg_get_constraintdef(x.oid),E'\\n' ORDER BY x.oid) FROM pg_catalog.pg_constraint x WHERE x.conrelid={relation}.oid),''),COALESCE((SELECT pg_catalog.string_agg(i.indexrelid::text||pg_catalog.pg_get_indexdef(i.indexrelid),E'\\n' ORDER BY i.indexrelid) FROM pg_catalog.pg_index i WHERE i.indrelid={relation}.oid),''),COALESCE((SELECT pg_catalog.string_agg(t.oid::text||pg_catalog.pg_get_triggerdef(t.oid),E'\\n' ORDER BY t.oid) FROM pg_catalog.pg_trigger t WHERE t.tgrelid={relation}.oid AND NOT t.tgisinternal),'')),'UTF8')),'hex')")
}
pub fn mysql_relation_signature() -> String {
    let parts=[("COLUMNS","c","JSON_ARRAY(c.ORDINAL_POSITION,c.COLUMN_NAME,c.COLUMN_TYPE,c.IS_NULLABLE,c.COLUMN_DEFAULT,c.EXTRA,c.COLLATION_NAME,c.GENERATION_EXPRESSION)","c.ORDINAL_POSITION"),("STATISTICS","i","JSON_ARRAY(i.INDEX_NAME,i.SEQ_IN_INDEX,i.COLUMN_NAME,i.NON_UNIQUE,i.SUB_PART,i.COLLATION)","i.INDEX_NAME,i.SEQ_IN_INDEX"),("KEY_COLUMN_USAGE","k","JSON_ARRAY(k.CONSTRAINT_NAME,k.ORDINAL_POSITION,k.COLUMN_NAME,k.REFERENCED_TABLE_SCHEMA,k.REFERENCED_TABLE_NAME,k.REFERENCED_COLUMN_NAME)","k.CONSTRAINT_NAME,k.ORDINAL_POSITION"),("TRIGGERS","g","JSON_ARRAY(g.TRIGGER_NAME,g.ACTION_TIMING,g.EVENT_MANIPULATION,g.ACTION_STATEMENT)","g.TRIGGER_NAME")];
    let mut values = Vec::new();
    let mut lengths = Vec::new();
    for (table, alias, value, order) in parts {
        let (schema_col, name_col) = if table == "TRIGGERS" {
            ("EVENT_OBJECT_SCHEMA", "EVENT_OBJECT_TABLE")
        } else {
            ("TABLE_SCHEMA", "TABLE_NAME")
        };
        let condition =
            format!("BINARY {alias}.{schema_col}=BINARY t.TABLE_SCHEMA AND BINARY {alias}.{name_col}=BINARY t.TABLE_NAME");
        values.push(format!("COALESCE((SELECT GROUP_CONCAT({value} ORDER BY {order} SEPARATOR ',') FROM information_schema.{table} {alias} WHERE {condition}),'')"));
        lengths.push(format!("COALESCE((SELECT SUM(OCTET_LENGTH({value}))+COUNT(*) FROM information_schema.{table} {alias} WHERE {condition}),0)"));
    }
    format!("CONCAT(SHA2(CONCAT_WS('|',t.TABLE_SCHEMA,t.TABLE_NAME,t.TABLE_TYPE,COALESCE(t.ENGINE,''),{}),256),':',{})",values.join(","),lengths.join("+"))
}
