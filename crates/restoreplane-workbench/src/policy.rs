//! Bounded syntactic policy over exact user bytes. AST classification is never
//! proof of server read-only authority, transactional effects or harmless UDFs.
use crate::WorkbenchError;
use restoreplane_domain::workbench::{EffectClass, Engine, Mode};
use sqlparser::ast::{Expr, ObjectName, Query, Select, Statement, Visit, Visitor};
use sqlparser::dialect::{Dialect, MySqlDialect, PostgreSqlDialect, SQLiteDialect};
use sqlparser::parser::{Parser, ParserError};
use sqlparser::tokenizer::{Location, Token, TokenWithSpan, Tokenizer};
use std::collections::BTreeSet;
use std::ops::{ControlFlow, Range};

pub const MAX_SQL_BYTES: usize = 1024 * 1024;
pub const MAX_SQL_TOKENS: usize = 32_768;
pub const MAX_STATEMENTS: usize = 256;
pub const MAX_PARSE_DEPTH: usize = 32;
/// Bounds left-associated AST depth before allocation/visitor/recursive Drop.
/// Flat literal lists still have the separate total-token/node limits.
pub const MAX_SHAPE_TOKENS_PER_UNIT: usize = 64;
const MAX_AST_NODES: usize = 16_384;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueryPurpose {
    Run,
    ExplainPlan,
    ExplainAnalyze,
}

/// Constructed from a native observation on the same physical connection. The
/// coordinator compares this again before dispatch; JS cannot certify settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VerifiedLexicalSettings {
    Unknown,
    Postgresql {
        standard_conforming_strings: bool,
        client_encoding: String,
    },
    Mysql {
        ansi_quotes: bool,
        no_backslash_escapes: bool,
        character_set_client: String,
    },
    Sqlite,
}
#[derive(Clone, Debug)]
pub struct PolicyContext {
    pub purpose: QueryPurpose,
    pub manual_transaction: bool,
    pub lexical_settings: VerifiedLexicalSettings,
}
impl Default for PolicyContext {
    fn default() -> Self {
        Self {
            purpose: QueryPurpose::Run,
            manual_transaction: false,
            lexical_settings: VerifiedLexicalSettings::Unknown,
        }
    }
}
#[derive(Clone, Debug)]
pub struct StatementPolicy {
    pub source_range: Range<usize>,
    pub digest: String,
    pub effect: EffectClass,
    pub boundary_verified: bool,
    pub read_only_candidate: bool,
    /// Deliberately false: only the independently certified driver/server may
    /// supply a read authority proof. This field cannot authorize a lease.
    pub certified_read_only: bool,
    pub may_implicit_commit: bool,
    /// Identifier components, never SQL fragments; quoted dots stay in one
    /// component. PostgreSQL unquoted ASCII names are folded by its rules.
    /// CTE aliases can occur here: callers must conservatively refuse an
    /// unresolvable relation rather than guess a physical table identity.
    pub relations: Vec<Vec<String>>,
    pub has_function_calls: bool,
}
impl StatementPolicy {
    pub fn source<'a>(&self, sql: &'a str) -> Result<&'a str, WorkbenchError> {
        let value = sql
            .get(self.source_range.clone())
            .ok_or(WorkbenchError::InvalidInput)?;
        if digest(value) != self.digest {
            return Err(WorkbenchError::IdempotencyConflict);
        }
        Ok(value)
    }
}
#[derive(Clone, Debug)]
pub struct AnalyzedScript {
    pub digest: String,
    pub statements: Vec<StatementPolicy>,
    pub warnings: Vec<String>,
    pub required_mode: Mode,
    pub retire_after_run: bool,
}
impl AnalyzedScript {
    pub fn verify_source(&self, sql: &str) -> Result<(), WorkbenchError> {
        if digest(sql) == self.digest {
            Ok(())
        } else {
            Err(WorkbenchError::IdempotencyConflict)
        }
    }
}
pub struct SqlPolicy;
impl SqlPolicy {
    pub fn analyze(
        engine: Engine,
        mode: Mode,
        sql: &str,
        context: &PolicyContext,
    ) -> Result<AnalyzedScript, WorkbenchError> {
        preflight(sql)?;
        lexical_gate(engine, sql, &context.lexical_settings)?;
        let dialect: &dyn Dialect = match engine {
            Engine::Postgresql => &PostgreSqlDialect {},
            Engine::Mysql | Engine::Mariadb => &MySqlDialect {},
            Engine::Sqlite => &SQLiteDialect {},
        };
        let tokens = Tokenizer::new(dialect, sql)
            .tokenize_with_location()
            .map_err(|_| WorkbenchError::InvalidInput)?;
        if tokens.len() > MAX_SQL_TOKENS {
            return Err(WorkbenchError::Limit);
        }
        let meaningful = tokens
            .iter()
            .filter(|token| {
                !matches!(
                    token.token,
                    Token::Whitespace(_) | Token::SemiColon | Token::EOF
                )
            })
            .collect::<Vec<_>>();
        if meaningful.is_empty() {
            return Err(WorkbenchError::InvalidInput);
        }
        shape_gate(&tokens)?;
        let procedural = meaningful.iter().any(|token| {
            word(token).is_some_and(|word| {
                matches!(
                    word.as_str(),
                    "DO" | "PROCEDURE" | "TRIGGER" | "FUNCTION" | "DECLARE"
                )
            })
        });
        // Check every lexical unit even when the parser cannot classify the
        // script. This is not a regex splitter, and quoted/commented words are
        // never interpreted as native transaction/session commands.
        let mut first = true;
        for token in &tokens {
            if matches!(token.token, Token::Whitespace(_)) {
                continue;
            }
            if matches!(token.token, Token::SemiColon) {
                first = true;
                continue;
            }
            if first && word(token).is_some_and(|value| forbidden_start(&value)) {
                return Err(WorkbenchError::Forbidden);
            }
            first = false;
        }
        if procedural {
            // A procedure body is opaque, including to recursive visitor/Drop.
            // Do not attempt to build a partly understood procedural AST.
            return unknown(sql, mode, context);
        }
        let mut boundaries = if procedural {
            vec![sql.len()]
        } else {
            let mut ends = Vec::new();
            let lines = line_starts(sql);
            for token in &tokens {
                if matches!(token.token, Token::SemiColon) {
                    let end = byte_offset(sql, &lines, token.span.end)?;
                    // Independently verify that the lexer end points directly
                    // after a literal ASCII delimiter in the original bytes.
                    if end == 0 || sql.as_bytes().get(end - 1) != Some(&b';') {
                        return Err(WorkbenchError::InvalidInput);
                    }
                    ends.push(end);
                    if ends.len() > MAX_STATEMENTS {
                        return Err(WorkbenchError::Limit);
                    }
                }
            }
            if ends.last().copied() != Some(sql.len()) {
                ends.push(sql.len());
            }
            ends
        };
        boundaries.dedup();
        let mut statements = Vec::new();
        let mut warnings = Vec::new();
        let mut required = Mode::ReadOnly;
        let mut retire = false;
        let mut start = 0;
        for end in boundaries {
            let source = sql.get(start..end).ok_or(WorkbenchError::InvalidInput)?;
            let parsed = Parser::new(dialect)
                .with_recursion_limit(MAX_PARSE_DEPTH)
                .try_with_sql(source)
                .and_then(|mut parser| parser.parse_statements());
            let ast = match parsed {
                Ok(ast) if ast.is_empty() => continue,
                Ok(mut ast) if ast.len() == 1 => ast.remove(0),
                Err(ParserError::RecursionLimitExceeded) => return Err(WorkbenchError::Limit),
                _ => return unknown(sql, mode, context),
            };
            let mut inspected = Inspection {
                engine: Some(engine),
                ..Inspection::default()
            };
            if let ControlFlow::Break(error) = ast.visit(&mut inspected) {
                return Err(error);
            }
            let mut effect = classify(&ast, source, &mut inspected);
            let analyze = matches!(&ast, Statement::Explain { analyze: true, .. })
                || matches!(&ast, Statement::Explain { options: Some(options), .. } if options.iter().any(|option| option.name.value.eq_ignore_ascii_case("analyze")));
            match context.purpose {
                QueryPurpose::ExplainPlan
                    if !matches!(&ast, Statement::Explain { .. }) || analyze =>
                {
                    return Err(WorkbenchError::InvalidInput)
                }
                QueryPurpose::ExplainAnalyze
                    if !matches!(&ast, Statement::Explain { .. }) || !analyze =>
                {
                    return Err(WorkbenchError::InvalidInput)
                }
                _ => {}
            }
            if let Statement::Explain { .. } = &ast {
                if analyze {
                    effect = EffectClass::Unknown;
                    push(&mut warnings, "explain_analyze_executes");
                } else {
                    effect = EffectClass::Read;
                    push(&mut warnings, "plan_is_not_effect_proof");
                }
            }
            if inspected.function {
                push(&mut warnings, "function_effects_unproven");
            }
            if inspected.mutating_function {
                effect = EffectClass::Unknown;
            }
            if inspected.lock {
                effect = merge(effect, EffectClass::Write);
                push(&mut warnings, "locking_read");
            }
            if inspected.into {
                effect = EffectClass::Ddl;
            }
            let statement_mode = match effect {
                EffectClass::Read => Mode::ReadOnly,
                EffectClass::Write => Mode::ReadWrite,
                _ => Mode::Full,
            };
            required = stricter(required, statement_mode);
            let implicit = matches!(engine, Engine::Mysql | Engine::Mariadb)
                && matches!(effect, EffectClass::Ddl | EffectClass::Unknown);
            match effect {
                EffectClass::Write => push(&mut warnings, "data_change"),
                EffectClass::Ddl => {
                    push(&mut warnings, "ddl_change");
                    retire = true;
                }
                EffectClass::Unknown => {
                    push(&mut warnings, "unknown_effects");
                    retire = true;
                }
                _ => {}
            }
            if implicit {
                push(&mut warnings, "implicit_commit_possible");
            }
            if context.manual_transaction
                && (implicit || matches!(effect, EffectClass::Unknown | EffectClass::Ddl))
            {
                return Err(WorkbenchError::Unsupported);
            }
            statements.push(StatementPolicy {
                source_range: start..end,
                digest: digest(source),
                effect,
                boundary_verified: true,
                read_only_candidate: effect == EffectClass::Read,
                certified_read_only: false,
                may_implicit_commit: implicit,
                relations: inspected.relations.into_iter().collect(),
                has_function_calls: inspected.function,
            });
            start = end;
        }
        if statements.is_empty() {
            return Err(WorkbenchError::InvalidInput);
        }
        // Retain trailing whitespace/comments exactly with the final statement.
        if start != sql.len() {
            let last = statements.last_mut().ok_or(WorkbenchError::InvalidInput)?;
            last.source_range.end = sql.len();
            last.digest = digest(&sql[last.source_range.clone()]);
        }
        if statements.len() > MAX_STATEMENTS {
            return Err(WorkbenchError::Limit);
        }
        if statements.len() > 1 {
            if context.purpose != QueryPurpose::Run {
                return Err(WorkbenchError::InvalidInput);
            }
            push(&mut warnings, "multiple_statements_partial_effects");
        }
        if !allows(mode, required) {
            return Err(WorkbenchError::Forbidden);
        }
        if required == Mode::ReadOnly {
            push(&mut warnings, "server_readonly_authority_required");
        }
        Ok(AnalyzedScript {
            digest: digest(sql),
            statements,
            warnings,
            required_mode: required,
            retire_after_run: retire,
        })
    }
}

fn digest(source: &str) -> String {
    blake3::hash(source.as_bytes()).to_hex().to_string()
}
fn push(warnings: &mut Vec<String>, value: &str) {
    if !warnings.iter().any(|item| item == value) {
        warnings.push(value.into());
    }
}
fn stricter(a: Mode, b: Mode) -> Mode {
    if a == Mode::Full || b == Mode::Full {
        Mode::Full
    } else if a == Mode::ReadWrite || b == Mode::ReadWrite {
        Mode::ReadWrite
    } else {
        Mode::ReadOnly
    }
}
fn allows(actual: Mode, required: Mode) -> bool {
    stricter(actual, required) == actual
}
fn merge(a: EffectClass, b: EffectClass) -> EffectClass {
    if a == EffectClass::Unknown || b == EffectClass::Unknown {
        EffectClass::Unknown
    } else if a == EffectClass::Ddl || b == EffectClass::Ddl {
        EffectClass::Ddl
    } else if a == EffectClass::Write || b == EffectClass::Write {
        EffectClass::Write
    } else {
        EffectClass::Read
    }
}
fn word(token: &TokenWithSpan) -> Option<String> {
    match &token.token {
        Token::Word(word) if word.quote_style.is_none() => Some(word.value.to_ascii_uppercase()),
        _ => None,
    }
}
fn forbidden_start(word: &str) -> bool {
    matches!(
        word,
        "SET"
            | "RESET"
            | "USE"
            | "BEGIN"
            | "START"
            | "COMMIT"
            | "END"
            | "ROLLBACK"
            | "ABORT"
            | "SAVEPOINT"
            | "RELEASE"
            | "ATTACH"
            | "DETACH"
            | "PRAGMA"
            | "COPY"
            | "LOAD"
            | "INSTALL"
            | "DELIMITER"
            | "PREPARE"
            | "EXECUTE"
            | "DEALLOCATE"
            | "DISCARD"
            | "LOCK"
            | "UNLOCK"
            | "KILL"
            | "CONNECT"
            | "DISCONNECT"
    )
}
fn classify(ast: &Statement, source: &str, inspected: &mut Inspection) -> EffectClass {
    match ast {
        Statement::Query(_) => {
            if inspected.write {
                EffectClass::Write
            } else {
                EffectClass::Read
            }
        }
        Statement::Insert(_)
        | Statement::Update(_)
        | Statement::Delete(_)
        | Statement::Merge(_) => EffectClass::Write,
        Statement::Explain { .. } => EffectClass::Read,
        _ => {
            // This first keyword only refines an already parsed AST; all other
            // accepted ASTs stay Full/unknown. It never authorizes a read.
            let head = source
                .trim_start()
                .split(|ch: char| !ch.is_ascii_alphabetic())
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if matches!(
                head.as_str(),
                "CREATE"
                    | "ALTER"
                    | "DROP"
                    | "TRUNCATE"
                    | "REINDEX"
                    | "VACUUM"
                    | "ANALYZE"
                    | "GRANT"
                    | "REVOKE"
                    | "COMMENT"
            ) {
                EffectClass::Ddl
            } else {
                EffectClass::Unknown
            }
        }
    }
}
fn unknown(
    sql: &str,
    mode: Mode,
    context: &PolicyContext,
) -> Result<AnalyzedScript, WorkbenchError> {
    if mode != Mode::Full || context.manual_transaction || context.purpose != QueryPurpose::Run {
        return Err(WorkbenchError::Unsupported);
    }
    Ok(AnalyzedScript {
        digest: digest(sql),
        statements: vec![StatementPolicy {
            source_range: 0..sql.len(),
            digest: digest(sql),
            effect: EffectClass::Unknown,
            boundary_verified: false,
            read_only_candidate: false,
            certified_read_only: false,
            may_implicit_commit: true,
            relations: Vec::new(),
            has_function_calls: true,
        }],
        warnings: vec![
            "unknown_effects".into(),
            "unverified_boundary_single_unit".into(),
            "session_retired_after_execution".into(),
        ],
        required_mode: Mode::Full,
        retire_after_run: true,
    })
}

fn preflight(sql: &str) -> Result<(), WorkbenchError> {
    if sql.is_empty() || sql.len() > MAX_SQL_BYTES || sql.contains('\0') {
        return Err(WorkbenchError::InvalidInput);
    }
    // A conservative upper bound before the tokenizer allocates its Vec. Each
    // ASCII letter/underscore run and digit run can yield at most one token;
    // every other Unicode scalar counts individually (even inside literals).
    // This may reject punctuation-rich large literals, never relax the cap.
    let mut estimate = 0usize;
    let mut previous = 0u8;
    for ch in sql.chars() {
        let group = if ch.is_ascii_alphabetic() || ch == '_' {
            1
        } else if ch.is_ascii_digit() {
            2
        } else {
            0
        };
        if group == 0 || group != previous {
            estimate += 1;
            if estimate > MAX_SQL_TOKENS {
                return Err(WorkbenchError::Limit);
            }
        }
        previous = group;
    }
    Ok(())
}
fn shape_gate(tokens: &[TokenWithSpan]) -> Result<(), WorkbenchError> {
    let mut shape = 0usize;
    for token in tokens {
        match &token.token {
            Token::SemiColon => shape = 0,
            Token::Whitespace(_)
            | Token::Comma
            | Token::Number(..)
            | Token::SingleQuotedString(_)
            | Token::DollarQuotedString(_) => {}
            _ => {
                shape += 1;
                if shape > MAX_SHAPE_TOKENS_PER_UNIT {
                    return Err(WorkbenchError::Limit);
                }
            }
        }
    }
    Ok(())
}
fn lexical_gate(
    engine: Engine,
    sql: &str,
    observed: &VerifiedLexicalSettings,
) -> Result<(), WorkbenchError> {
    let mysql = matches!(engine, Engine::Mysql | Engine::Mariadb);
    if mysql && (sql.contains("/*!") || sql.to_ascii_uppercase().contains("/*M!")) {
        return Err(WorkbenchError::Unsupported);
    }
    if mysql
        && sql
            .as_bytes()
            .windows(3)
            .any(|bytes| bytes[0..2] == *b"--" && !bytes[2].is_ascii_whitespace())
    {
        return Err(WorkbenchError::Unsupported);
    }
    match (engine, observed) {
        (
            Engine::Postgresql,
            VerifiedLexicalSettings::Postgresql {
                standard_conforming_strings,
                client_encoding,
            },
        ) => {
            if !client_encoding.eq_ignore_ascii_case("UTF8")
                || (!standard_conforming_strings && sql.contains('\\'))
            {
                return Err(WorkbenchError::Unsupported);
            }
        }
        (
            Engine::Mysql | Engine::Mariadb,
            VerifiedLexicalSettings::Mysql {
                ansi_quotes,
                no_backslash_escapes,
                character_set_client,
            },
        ) => {
            if !character_set_client.eq_ignore_ascii_case("utf8mb4")
                || (*ansi_quotes && sql.contains('"'))
                || (*no_backslash_escapes && sql.contains('\\'))
            {
                return Err(WorkbenchError::Unsupported);
            }
        }
        (Engine::Sqlite, VerifiedLexicalSettings::Sqlite) => {}
        (_, VerifiedLexicalSettings::Unknown) => {
            if sql.contains('\\') || (mysql && sql.contains('"')) {
                return Err(WorkbenchError::Unsupported);
            }
        }
        _ => return Err(WorkbenchError::BindingStale),
    }
    Ok(())
}
fn line_starts(sql: &str) -> Vec<usize> {
    std::iter::once(0)
        .chain(sql.match_indices('\n').map(|(index, _)| index + 1))
        .collect()
}
fn byte_offset(sql: &str, lines: &[usize], location: Location) -> Result<usize, WorkbenchError> {
    let line = usize::try_from(location.line)
        .ok()
        .and_then(|line| line.checked_sub(1))
        .and_then(|line| lines.get(line))
        .copied()
        .ok_or(WorkbenchError::InvalidInput)?;
    let column = usize::try_from(location.column)
        .ok()
        .and_then(|column| column.checked_sub(1))
        .ok_or(WorkbenchError::InvalidInput)?;
    if column == sql[line..].chars().count() {
        return Ok(sql.len());
    }
    sql[line..]
        .char_indices()
        .nth(column)
        .map(|(offset, _)| line + offset)
        .ok_or(WorkbenchError::InvalidInput)
}

#[derive(Default)]
struct Inspection {
    nodes: usize,
    write: bool,
    into: bool,
    lock: bool,
    function: bool,
    mutating_function: bool,
    engine: Option<Engine>,
    relations: BTreeSet<Vec<String>>,
}
impl Inspection {
    fn step(&mut self) -> ControlFlow<WorkbenchError> {
        self.nodes += 1;
        if self.nodes > MAX_AST_NODES {
            ControlFlow::Break(WorkbenchError::Limit)
        } else {
            ControlFlow::Continue(())
        }
    }
}
impl Visitor for Inspection {
    type Break = WorkbenchError;
    fn pre_visit_statement(&mut self, statement: &Statement) -> ControlFlow<Self::Break> {
        self.step()?;
        match statement {
            Statement::Set(_)
            | Statement::Use(_)
            | Statement::StartTransaction { .. }
            | Statement::Commit { .. }
            | Statement::Rollback { .. }
            | Statement::Savepoint { .. }
            | Statement::ReleaseSavepoint { .. }
            | Statement::AttachDatabase { .. }
            | Statement::AttachDuckDBDatabase { .. }
            | Statement::DetachDuckDBDatabase { .. }
            | Statement::Pragma { .. }
            | Statement::Copy { .. }
            | Statement::Install { .. }
            | Statement::Load { .. }
            | Statement::Prepare { .. }
            | Statement::Execute { .. }
            | Statement::Deallocate { .. }
            | Statement::Discard { .. }
            | Statement::AlterSession { .. } => {
                return ControlFlow::Break(WorkbenchError::Forbidden)
            }
            Statement::Insert(_)
            | Statement::Update(_)
            | Statement::Delete(_)
            | Statement::Merge(_) => self.write = true,
            _ => {}
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_query(&mut self, query: &Query) -> ControlFlow<Self::Break> {
        self.step()?;
        self.lock |= !query.locks.is_empty();
        ControlFlow::Continue(())
    }
    fn pre_visit_select(&mut self, select: &Select) -> ControlFlow<Self::Break> {
        self.step()?;
        self.into |= select.into.is_some();
        ControlFlow::Continue(())
    }
    fn pre_visit_expr(&mut self, expression: &Expr) -> ControlFlow<Self::Break> {
        self.step()?;
        if let Expr::Function(function) = expression {
            self.function = true;
            if let Some(name) = function
                .name
                .0
                .last()
                .and_then(|part| part.as_ident())
                .map(|name| name.value.to_ascii_lowercase())
            {
                if matches!(
                    name.as_str(),
                    "load_extension"
                        | "readfile"
                        | "writefile"
                        | "pg_read_file"
                        | "pg_read_binary_file"
                        | "lo_import"
                        | "lo_export"
                ) {
                    return ControlFlow::Break(WorkbenchError::Forbidden);
                }
                self.mutating_function |= matches!(
                    name.as_str(),
                    "nextval"
                        | "setval"
                        | "set_config"
                        | "pg_advisory_lock"
                        | "pg_advisory_xact_lock"
                        | "pg_try_advisory_lock"
                        | "pg_advisory_unlock"
                        | "pg_advisory_unlock_all"
                        | "get_lock"
                        | "release_lock"
                        | "release_all_locks"
                );
            } else {
                self.mutating_function = true;
            }
        }
        ControlFlow::Continue(())
    }
    fn pre_visit_relation(&mut self, relation: &ObjectName) -> ControlFlow<Self::Break> {
        self.step()?;
        if relation.0.is_empty() || relation.0.len() > 4 {
            return ControlFlow::Break(WorkbenchError::Unsupported);
        }
        let mut parts = Vec::with_capacity(relation.0.len());
        for part in &relation.0 {
            let Some(identifier) = part.as_ident() else {
                return ControlFlow::Break(WorkbenchError::Unsupported);
            };
            if identifier.value.is_empty() || identifier.value.len() > 1024 {
                return ControlFlow::Break(WorkbenchError::Limit);
            }
            parts.push(
                if self.engine == Some(Engine::Postgresql) && identifier.quote_style.is_none() {
                    identifier.value.to_ascii_lowercase()
                } else {
                    identifier.value.clone()
                },
            );
        }
        self.relations.insert(parts);
        if self.relations.len() > 64 {
            return ControlFlow::Break(WorkbenchError::Limit);
        }
        ControlFlow::Continue(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn context() -> PolicyContext {
        PolicyContext::default()
    }
    #[test]
    fn exact_unicode_crlf_comments_quotes_and_dollar_body_ranges_are_preserved() {
        let sql = "-- 한글\r\n SELECT 'a;b', $$x;y$$; /* ; */\n SELECT \"한글;열\" FROM sample; -- tail\n";
        let analyzed =
            SqlPolicy::analyze(Engine::Postgresql, Mode::ReadOnly, sql, &context()).unwrap();
        assert_eq!(analyzed.statements.len(), 2);
        assert_eq!(
            analyzed
                .statements
                .iter()
                .map(|statement| statement.source(sql).unwrap())
                .collect::<String>(),
            sql
        );
        assert!(analyzed
            .statements
            .iter()
            .all(|statement| statement.boundary_verified && !statement.certified_read_only));
        assert_eq!(
            analyzed
                .verify_source(&sql.replace("sample", "changed"))
                .unwrap_err(),
            WorkbenchError::IdempotencyConflict
        );
    }
    #[test]
    fn nested_write_ctes_select_into_locks_and_destructive_modes_are_restrictive() {
        for sql in [
            "WITH changed AS (DELETE FROM sample RETURNING *) SELECT * FROM changed",
            "SELECT * FROM sample FOR UPDATE",
            "INSERT INTO sample VALUES (1)",
        ] {
            assert!(
                SqlPolicy::analyze(Engine::Postgresql, Mode::ReadOnly, sql, &context()).is_err()
            );
            let analyzed =
                SqlPolicy::analyze(Engine::Postgresql, Mode::ReadWrite, sql, &context()).unwrap();
            assert_eq!(analyzed.required_mode, Mode::ReadWrite);
        }
        for sql in [
            "DROP TABLE sample",
            "SELECT * INTO copied FROM sample",
            "ANALYZE sample",
        ] {
            assert!(
                SqlPolicy::analyze(Engine::Postgresql, Mode::ReadWrite, sql, &context()).is_err()
            );
            assert!(
                SqlPolicy::analyze(Engine::Postgresql, Mode::Full, sql, &context())
                    .unwrap()
                    .retire_after_run
            );
        }
    }
    #[test]
    fn native_transaction_and_session_escape_or_local_file_sql_are_rejected() {
        for sql in [
            "SELECT 1; COMMIT",
            "SET transaction_read_only=off",
            "BEGIN",
            "ROLLBACK",
            "SAVEPOINT named",
            "COPY sample TO STDOUT",
            "SELECT set_config('transaction_read_only','off',false)",
        ] {
            let result = SqlPolicy::analyze(Engine::Postgresql, Mode::Full, sql, &context());
            if sql.contains("set_config") {
                assert!(result.unwrap().retire_after_run);
            } else {
                assert!(result.is_err(), "{sql}");
            }
        }
        for sql in [
            "ATTACH DATABASE 'x' AS other",
            "PRAGMA query_only=OFF",
            "SELECT load_extension('x')",
        ] {
            assert!(SqlPolicy::analyze(Engine::Sqlite, Mode::Full, sql, &context()).is_err());
        }
    }
    #[test]
    fn unknown_and_procedural_text_stays_one_full_review_unit() {
        let sql = "DO $$ BEGIN PERFORM 1; PERFORM 2; END $$;";
        assert!(SqlPolicy::analyze(Engine::Postgresql, Mode::ReadWrite, sql, &context()).is_err());
        let analyzed = SqlPolicy::analyze(Engine::Postgresql, Mode::Full, sql, &context()).unwrap();
        assert_eq!(analyzed.statements.len(), 1);
        assert_eq!(analyzed.statements[0].source(sql).unwrap(), sql);
        assert!(analyzed.retire_after_run);
        let mut manual = context();
        manual.manual_transaction = true;
        assert!(SqlPolicy::analyze(Engine::Postgresql, Mode::Full, sql, &manual).is_err());
    }
    #[test]
    fn explain_analyze_and_purpose_cannot_silently_execute_in_read_mode() {
        let mut ctx = context();
        ctx.purpose = QueryPurpose::ExplainPlan;
        assert!(SqlPolicy::analyze(Engine::Postgresql, Mode::ReadOnly, "SELECT 1", &ctx).is_err());
        assert!(
            SqlPolicy::analyze(Engine::Postgresql, Mode::ReadOnly, "EXPLAIN SELECT 1", &ctx)
                .is_ok()
        );
        assert!(SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::Full,
            "EXPLAIN ANALYZE SELECT 1",
            &ctx
        )
        .is_err());
        ctx.purpose = QueryPurpose::ExplainAnalyze;
        assert!(SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::ReadWrite,
            "EXPLAIN ANALYZE SELECT 1",
            &ctx
        )
        .is_err());
        let result = SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::Full,
            "EXPLAIN (ANALYZE, BUFFERS) SELECT 1",
            &ctx,
        )
        .unwrap();
        assert!(result.warnings.contains(&"explain_analyze_executes".into()));
    }
    #[test]
    fn mysql_comments_modes_and_uncertain_lexing_cannot_bypass_the_gate() {
        for sql in [
            "SELECT 1 /*!; DROP TABLE sample */",
            "SELECT 1 /*M!; DROP TABLE sample */",
            "SELECT 'a\\'; COMMIT; -- '",
            "SELECT \"x\"",
            "SELECT 1--not_a_comment\n; COMMIT",
        ] {
            assert!(SqlPolicy::analyze(Engine::Mysql, Mode::Full, sql, &context()).is_err());
        }
        let mut ctx = context();
        ctx.lexical_settings = VerifiedLexicalSettings::Mysql {
            ansi_quotes: false,
            no_backslash_escapes: false,
            character_set_client: "utf8mb4".into(),
        };
        assert!(SqlPolicy::analyze(Engine::Mysql, Mode::ReadOnly, "SELECT 'a\\';b'", &ctx).is_ok());
        ctx.lexical_settings = VerifiedLexicalSettings::Mysql {
            ansi_quotes: true,
            no_backslash_escapes: true,
            character_set_client: "utf8mb4".into(),
        };
        assert!(SqlPolicy::analyze(Engine::Mysql, Mode::Full, "SELECT 'a\\';b'", &ctx).is_err());
    }
    #[test]
    fn token_depth_and_size_limits_precede_unbounded_work() {
        assert_eq!(
            SqlPolicy::analyze(
                Engine::Postgresql,
                Mode::Full,
                &" ".repeat(MAX_SQL_TOKENS + 1),
                &context()
            )
            .unwrap_err(),
            WorkbenchError::Limit
        );
        let sql = format!("SELECT {}1{}", "(".repeat(100), ")".repeat(100));
        assert_eq!(
            SqlPolicy::analyze(Engine::Postgresql, Mode::Full, &sql, &context()).unwrap_err(),
            WorkbenchError::Limit
        );
        assert!(SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::Full,
            &"x".repeat(MAX_SQL_BYTES + 1),
            &context()
        )
        .is_err());
        assert!(SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::ReadOnly,
            "-- only a comment",
            &context()
        )
        .is_err());
    }
    #[test]
    fn relation_components_preserve_quoted_identity_without_sql_interpolation() {
        let analyzed = SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::ReadWrite,
            "INSERT INTO \"Odd.Schema\".\"한글.Table\" VALUES (1)",
            &context(),
        )
        .unwrap();
        assert_eq!(
            analyzed.statements[0].relations,
            vec![vec!["Odd.Schema".to_owned(), "한글.Table".to_owned()]]
        );
        assert!(!analyzed.statements[0].has_function_calls);
        let analyzed = SqlPolicy::analyze(
            Engine::Postgresql,
            Mode::ReadOnly,
            "SELECT COUNT(*) FROM PUBLIC.Sample",
            &context(),
        )
        .unwrap();
        assert_eq!(
            analyzed.statements[0].relations,
            vec![vec!["public".to_owned(), "sample".to_owned()]]
        );
        assert!(analyzed.statements[0].has_function_calls);
    }

    #[test]
    fn long_left_deep_shapes_are_checked_in_an_isolated_process() {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "policy::tests::shape_probe_child",
                "--ignored",
                "--nocapture",
            ])
            .env("WORKBENCH_SQL_SHAPE_PROBE", "1");
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            command.creation_flags(0x0800_0000);
        }
        let output = command.output().unwrap();
        assert!(
            output.status.success(),
            "shape subprocess failed: {}",
            output.status
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("left_deep_shape_cases=4"));
    }

    #[test]
    #[ignore = "Invoked only by the isolated policy shape parent"]
    fn shape_probe_child() {
        assert_eq!(
            std::env::var("WORKBENCH_SQL_SHAPE_PROBE").as_deref(),
            Ok("1")
        );
        std::thread::Builder::new()
            .stack_size(1024 * 1024)
            .spawn(|| {
                let cases = [
                    format!("SELECT {}", vec!["1"; 4000].join("+")),
                    format!("SELECT {}", vec!["true"; 4000].join(" AND ")),
                    vec!["SELECT 1"; 2000].join(" UNION ALL "),
                    format!("SELECT 1{}", "::int".repeat(4000)),
                ];
                for source in cases {
                    assert_eq!(
                        SqlPolicy::analyze(Engine::Postgresql, Mode::Full, &source, &context())
                            .unwrap_err(),
                        WorkbenchError::Limit
                    );
                }
                let supported = format!("SELECT {}", vec!["1"; 63].join("+"));
                assert!(SqlPolicy::analyze(
                    Engine::Postgresql,
                    Mode::ReadOnly,
                    &supported,
                    &context()
                )
                .is_ok());
                println!(
                    "M3 left_deep_shape_cases=4 bounded_stack_bytes=1048576 boundary_violations=0"
                );
            })
            .unwrap()
            .join()
            .unwrap();
    }
}
