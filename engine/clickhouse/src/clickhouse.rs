//! The ClickHouse analysis. The server's `DESCRIBE` of a query is the type source: each result
//! column's name and its type as ClickHouse writes it, which says whether the column may be NULL
//! ([`crate::types`]). Each parameter states its type ([`crate::params`]), and the query is
//! described with a value of that type in the parameter's place. How many rows it returns is read
//! off a `sqlparser` tree of it ([`crate::rows`]).
//!
//! ClickHouse describes a SELECT alone. An `INSERT … VALUES` is read against the catalog: its
//! table and columns are checked there, and a parameter that is a whole value takes its column's
//! type. An `INSERT … SELECT` has its query described. Any other statement that returns no rows (a
//! mutation, DDL) has its syntax checked by `EXPLAIN AST` and is typed by what its parameters
//! state; one that returns rows the server does not describe (`SHOW`, `EXPLAIN`) is refused.

use crate::api::{Analysis, ClickhouseDescribe, Column, DbError, Diagnostic, Param};
use crate::catalog::{self, Aggregates, TableColumn};
use crate::host::{Host, Lazy};
use crate::{params, rows, types};
use reylo_engine_core::analysis::{bare, diag};
use reylo_engine_core::api::Scope;
use reylo_engine_core::dialect::Dialect;
use reylo_engine_core::lex::{self, Lexed, Rewritten};
use reylo_engine_core::types::{Cardinality, StatementKind};
use sqlparser::ast::Statement;

/// The statements whose first word names a query or a write the analysis reads; any other is a
/// command ([`lex::Statement`]).
const KINDS: &[&str] = &["select", "with", "insert", "update", "delete"];

/// The statements that return rows ClickHouse does not describe before they run.
const UNDESCRIBED: &[&str] = &["show", "describe", "desc", "explain", "exists", "check", "watch"];

/// The diagnostic code of a statement the analysis does not type.
const NOT_TYPED: u32 = 10;
/// The diagnostic code of an error the server answered with.
const REFUSED: u32 = 6;
/// The diagnostic code of a parameter that states no type the analysis reads.
const UNTYPED: u32 = 12;

/// The words of `sql` outside its strings, comments, parameters and parentheses, each lowercased
/// with its place, up to the statement's end.
fn top_words(sql: &str, lexed: &Lexed, end: usize) -> Vec<(String, usize)> {
    let b = sql.as_bytes();
    let skip = |i: usize| lexed.opaque.iter().find(|(s, e)| *s <= i && i < *e).map(|(_, e)| *e).or_else(|| lexed.placeholders.iter().find(|p| p.start <= i && i < p.end).map(|p| p.end));
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    let (mut words, mut depth, mut i) = (Vec::new(), 0usize, 0usize);
    while i < end {
        if let Some(past) = skip(i) {
            i = past;
            continue;
        }
        match b[i] {
            b'(' | b'[' => depth += 1,
            b')' | b']' => depth = depth.saturating_sub(1),
            c if is_name(c) => {
                let start = i;
                while i < end && is_name(b[i]) {
                    i += 1;
                }
                if depth == 0 {
                    words.push((sql[start..i].to_ascii_lowercase(), start));
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    words
}

/// Where the statement ends: its first semicolon outside text, after which only space and comments stand.
fn statement_end(sql: &str, lexed: &Lexed) -> usize {
    let b = sql.as_bytes();
    let in_text = |i: usize| lexed.opaque.iter().any(|&(s, e)| s <= i && i < e) || lexed.placeholders.iter().any(|p| p.start <= i && i < p.end);
    (0..b.len()).find(|&i| b[i] == b';' && !in_text(i)).unwrap_or(sql.len())
}

/// The offset in a rewritten statement of the offset `at` of the author's text, outside a parameter.
fn to_rewritten(rewritten: &Rewritten, at: usize) -> usize {
    let mut delta: isize = 0;
    for &(os, oe, rs, re) in &rewritten.spans {
        if oe <= at {
            delta += (re - rs) as isize - (oe - os) as isize;
        }
    }
    (at as isize + delta).max(0) as usize
}

/// The first place `name` stands as a word of the statement's code, outside its strings and comments.
fn word_at(sql: &str, lexed: &Lexed, name: &str) -> Option<usize> {
    if name.is_empty() {
        return None;
    }
    let b = sql.as_bytes();
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    sql.match_indices(name).map(|(at, _)| at).find(|&at| {
        let whole = (at == 0 || !is_name(b[at - 1])) && !b.get(at + name.len()).is_some_and(|c| is_name(*c));
        // A quoted name is text to the lexer, and code to the statement.
        let quoted = lexed.opaque.iter().any(|&(s, e)| s < at && at < e && !matches!(b[s], b'"' | b'`'));
        whole && !quoted
    })
}

/// The names an error's message quotes, in its order: `` `nope` `` and `'nope'`.
fn quoted_names(message: &str) -> Vec<&str> {
    let mut found = Vec::new();
    let mut open: Option<(usize, char)> = None;
    for (at, c) in message.char_indices() {
        match open {
            Some((start, quote)) if c == quote => {
                found.push(&message[start + 1..at]);
                open = None;
            }
            None if c == '`' || c == '\'' => open = Some((at, c)),
            _ => {}
        }
    }
    found
}

/// Whether the statement's code starts with a parenthesis, past its leading space and comments.
fn parenthesized(sql: &str, lexed: &Lexed) -> bool {
    sql[lex::past_comments(sql, lexed, 0)..].starts_with('(')
}

/// A statement as the server was given it: the described text, where it starts in the whole
/// rewritten statement, and the map back to the author's text.
struct Given<'a> {
    rewritten: &'a Rewritten,
    /// The offset of the given text in `rewritten.sql`.
    start: usize,
}

/// The analysis of a statement the server refused: its error at the place it names, a position
/// in the text it was given or a name the message quotes, else over the whole statement.
fn refused(sql: &str, lexed: &Lexed, given: &Given, e: &DbError, command: bool) -> Analysis {
    let message = if command { lex::refused_command(&e.message) } else { e.message.trim_end().to_string() };
    let word = |start: usize| sql[start..].chars().take_while(|c| c.is_alphanumeric() || *c == '_').map(char::len_utf8).sum::<usize>().max(1);
    let by_position = e.position.and_then(|position| {
        let text = &given.rewritten.sql[given.start..];
        let byte = text.char_indices().nth(position.checked_sub(1)?).map(|(i, _)| i)?;
        let start = given.rewritten.to_original(given.start + byte).min(sql.len().saturating_sub(1));
        Some((start, word(start)))
    });
    let by_name = || quoted_names(&e.message).into_iter().find_map(|name| word_at(sql, lexed, name).map(|at| (at, name.len())));
    let (start, length) = by_position.or_else(by_name).unwrap_or((0, sql.trim_end().len().max(1)));
    bare(sql, lexed, Dialect::Clickhouse, vec![Diagnostic { message, start, length, code: REFUSED, server_code: e.code.clone() }])
}

/// The analysis of a statement.
pub fn analyze(sql: &str, host: &Host) -> Lazy<Analysis> {
    let lexed = match reylo_engine_core::analysis::lexed(sql, Dialect::Clickhouse) {
        Ok(lexed) => lexed,
        Err(bare) => return Some(bare),
    };
    let failed = |diagnostics: Vec<Diagnostic>| Some(bare(sql, &lexed, Dialect::Clickhouse, diagnostics));
    let whole = |message: String, code: u32| failed(vec![diag(message, 0, sql.trim_end().len(), code)]);
    let statement = match lex::one_statement(sql, &lexed, KINDS) {
        Ok(statement) => statement,
        Err(message) => return whole(message, NOT_TYPED),
    };
    let end = statement_end(sql, &lexed);
    let words = top_words(sql, &lexed, end);
    // `(select …) union all (select …)` starts with no word of its own.
    let first = if parenthesized(sql, &lexed) { "" } else { words.first().map(|(w, _)| w.as_str()).unwrap_or_default() };
    let query = matches!(first, "select" | "with" | "");
    let insert = first == "insert";
    if UNDESCRIBED.contains(&first) {
        return whole(format!("ClickHouse describes the columns of a SELECT alone, and {} returns rows it does not describe before it runs: read the system tables with a SELECT instead, as in select name from system.tables", first.to_ascii_uppercase()), NOT_TYPED);
    }
    // The driver names the format a result is read in, and reads no file a statement writes.
    if query || insert {
        for pair in words.windows(2) {
            let ((word, at), (next, _)) = (&pair[0], &pair[1]);
            if word == "format" && next != "from" && (insert || words.last().is_some_and(|(_, last)| *last == pair[1].1) || words.iter().any(|(w, a)| *a > pair[1].1 && w == "settings")) {
                let message = if insert { "Reylo binds the values of an INSERT as parameters, and reads no data written after FORMAT: write the row as VALUES (:id, :name)" } else { "The driver names the format a result is read in: leave FORMAT out" };
                return failed(vec![diag(message, *at, "format".len(), NOT_TYPED)]);
            }
            if query && word == "into" && next == "outfile" {
                return failed(vec![diag("INTO OUTFILE writes the result to a file of the client's, which a driver has none of: leave it out", *at, "into".len(), NOT_TYPED)]);
            }
        }
    }

    // The type each parameter states, by its name.
    let placeholder = |name: &str| lexed.placeholders.iter().find(|p| p.name == name);
    let mut stated: Vec<Option<String>> = lexed.params.iter().map(|name| placeholder(name).and_then(|p| params::stated(sql, p))).collect();
    let mut diagnostics: Vec<Diagnostic> = Vec::new();
    for (name, ty) in lexed.params.iter().zip(&stated) {
        let (Some(ty), Some(p)) = (ty, placeholder(name)) else { continue };
        if !types::readable(ty) {
            let written = &sql[p.start..p.end];
            let message = if ty.trim() == "Identifier" {
                format!("{written} substitutes a name, not a value, and a statement's tables and columns are what its types are read from: write the name itself")
            } else {
                format!("{written} states the type {ty}, which does not read as a ClickHouse type")
            };
            diagnostics.push(diag(message, p.start, p.end - p.start, UNTYPED));
        }
    }
    if !diagnostics.is_empty() {
        return failed(diagnostics);
    }

    // The tree, read from the statement with a word in each parameter's place.
    let parsable = params::parsable(sql, &lexed);
    let parsed = rows::parse(&parsable.sql[..to_rewritten(&parsable, end)]);
    let mut comments: Vec<Option<String>> = vec![None; lexed.params.len()];

    // An INSERT's table, whose columns type the parameters its VALUES list writes whole.
    let written = if insert { parsed.as_ref().and_then(rows::insert) } else { None };
    // Asked with whatever else this round asks.
    let table = written.as_ref().map(|w| catalog::columns(host, &w.table));
    let mut select_width = None;
    if let (Some(w), Some(Some(columns))) = (&written, &table) {
        let name = w.table.join(".");
        let named = |n: &str| word_at(sql, &lexed, n).map_or((0, sql.trim_end().len()), |at| (at, n.len()));
        if columns.is_empty() {
            let (start, length) = named(w.table.last().map(String::as_str).unwrap_or_default());
            return failed(vec![Diagnostic { message: format!("ClickHouse has no table {name}"), start, length: length.max(1), code: REFUSED, server_code: Some("60".into()) }]);
        }
        let targets: Vec<&TableColumn> = if w.columns.is_empty() {
            columns.iter().filter(|c| c.written).collect()
        } else {
            let mut found = Vec::new();
            for column in &w.columns {
                match columns.iter().find(|c| &c.name == column) {
                    Some(c) => found.push(c),
                    None => {
                        let (start, length) = named(column);
                        diagnostics.push(Diagnostic { message: format!("The table {name} has no column {column}"), start, length: length.max(1), code: REFUSED, server_code: Some("16".into()) });
                    }
                }
            }
            found
        };
        if !diagnostics.is_empty() {
            return failed(diagnostics);
        }
        for row in w.values.iter().flatten() {
            if row.len() != targets.len() {
                return whole(format!("The INSERT writes {} column{} of {name} and a row of its VALUES has {} value{}", targets.len(), if targets.len() == 1 { "" } else { "s" }, row.len(), if row.len() == 1 { "" } else { "s" }), REFUSED);
            }
            for (value, column) in row.iter().zip(&targets) {
                let Some(k) = value.and_then(|n| n.checked_sub(1)).filter(|k| *k < stated.len()) else { continue };
                if stated[k].is_none() {
                    stated[k] = Some(column.ty.clone());
                }
                comments[k] = column.comment.clone();
            }
        }
        if w.selects {
            select_width = Some((targets.len(), name));
        }
    }

    // Every parameter states a type, or the statement is not one ClickHouse can be given.
    let untyped: Vec<Diagnostic> = lexed
        .params
        .iter()
        .zip(&stated)
        .filter(|(_, ty)| ty.is_none())
        .filter_map(|(name, _)| placeholder(name))
        .map(|p| {
            let written = &sql[p.start..p.end];
            diag(format!("ClickHouse types no parameter from where it stands, so each states its own type: cast {written}, as in {written}::UInt64, or write it as ClickHouse's own {{name:UInt64}}"), p.start, p.end - p.start, UNTYPED)
        })
        .collect();
    // The table's columns may yet type a parameter: its answer comes first.
    if !untyped.is_empty() {
        if matches!(table, Some(None)) {
            return None;
        }
        return failed(untyped);
    }

    let type_of = |name: &str| lexed.params.iter().position(|p| p == name).and_then(|i| stated[i].clone());
    let rewritten = params::rewritten(sql, &lexed, &type_of);
    let probe = params::probe(sql, &lexed, &type_of);
    let probe_end = to_rewritten(&probe, end);
    let command = matches!(statement, lex::Statement::Command(_));

    // What the server is asked: a describe of the query, of an INSERT's query, or a syntax check.
    let select_at = if insert { words.iter().find(|(w, _)| matches!(w.as_str(), "select" | "with")).map(|(_, at)| *at) } else { None };
    let (described, checked, given_start) = if query {
        (Some(host.describe(&probe.sql[..probe_end])), None, 0)
    } else if let (true, Some(at)) = (written.as_ref().is_some_and(|w| w.selects), select_at) {
        let start = to_rewritten(&probe, at);
        (Some(host.describe(&probe.sql[start..probe_end])), None, start)
    } else {
        (None, Some(host.query(&format!("EXPLAIN AST {}", &probe.sql[..probe_end]), Vec::new(), Scope::Statement)), 0)
    };
    let aggregates = match &parsed {
        Some(statement) if query && rows::asks_aggregates(statement) => catalog::aggregates(host),
        _ => Some(Aggregates::default()),
    };
    // Every need of this round is asked: the table's columns are one of them.
    if matches!(table, Some(None)) {
        return None;
    }
    let aggregates = aggregates?;
    let given = Given { rewritten: &probe, start: given_start };
    let described: Option<ClickhouseDescribe> = match described {
        Some(answer) => match answer? {
            Ok(described) => Some(described),
            Err(e) => return Some(refused(sql, &lexed, &given, &e, command)),
        },
        None => None,
    };
    if let Some(answer) = checked {
        if let Err(mut e) = answer? {
            // The position is in the text after `EXPLAIN AST `, which the statement starts after.
            e.position = e.position.and_then(|p| p.checked_sub("EXPLAIN AST ".len()));
            return Some(refused(sql, &lexed, &given, &e, command));
        }
    }

    let params: Vec<Param> = lexed
        .params
        .iter()
        .enumerate()
        .map(|(i, name)| {
            let position = stated[i].as_deref().map(types::of);
            match position {
                Some(position) => Param { nullable: position.nullable, comment: comments[i].clone(), ..Param::new(name.clone(), position.ty) },
                None => Param::new(name.clone(), reylo_engine_core::types::Type::unknown()),
            }
        })
        .collect();
    let columns: Vec<Column> = match (&described, query) {
        (Some(described), true) => described.columns.iter().map(|c| {
            let position = types::of(&c.ty);
            Column::new(c.name.clone(), position.ty, position.nullable)
        }).collect(),
        _ => Vec::new(),
    };
    // An INSERT's query gives a value to each column the INSERT writes.
    if let (Some(described), Some((width, name))) = (&described, &select_width) {
        if described.columns.len() != *width {
            return whole(format!("The INSERT writes {width} column{} of {name} and its query returns {}", if *width == 1 { "" } else { "s" }, described.columns.len()), REFUSED);
        }
    }
    let (cardinality, reason) = match (&parsed, query) {
        (_, false) => (Cardinality::None, None),
        (Some(Statement::Query(q)), true) => rows::cardinality(q, &aggregates, &parsable.sql),
        (_, true) => (Cardinality::Many, Some("the engine does not read this statement's syntax, so nothing bounds its rows".to_string())),
    };
    let kind = if query { StatementKind::Select } else { StatementKind::of(first) };
    let mut analysis = Analysis {
        kind: Some(kind),
        comment: None,
        params,
        columns,
        cardinality,
        cardinality_reason: reason,
        key: None,
        unique: Vec::new(),
        order: Vec::new(),
        ordered: false,
        value: false,
        rewritten,
        style: lexed.style,
        diagnostics: Vec::new(),
        unresolved: Vec::new(),
        structured: Vec::new(),
        fresh: None,
        errors: None,
        read_only: false,
        notes: Vec::new(),
        tables: parsed.as_ref().map(rows::tables),
        domains: Default::default(),
    };
    if query {
        let targets = parsed.as_ref().and_then(rows::targets);
        reylo_engine_core::analysis::names(sql, Dialect::Clickhouse, &mut analysis, targets.as_deref(), &|_| None);
    }
    Some(analysis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{Answer, Answers, Clickhouse};
    use reylo_engine_core::api::Need;
    use reylo_engine_core::database::Database;
    use reylo_engine_core::api::Response;
    use serde_json::{json, Value};

    /// The analysis of `sql` with each need answered by `answer`, a describe by the columns it
    /// gives or the error, and a catalog query by its rows.
    fn run(sql: &str, answer: &dyn Fn(&Need) -> Value) -> (Analysis, Vec<Need>) {
        let mut answers: Answers = Answers::new();
        let mut asked = Vec::new();
        for _ in 0..8 {
            match Clickhouse::round(sql, &answers) {
                Response::Analysis { analysis } => return (analysis, asked),
                Response::Need { needs } => {
                    for need in needs {
                        let (Need::Describe { id, .. } | Need::Query { id, .. }) = &need;
                        let given: Answer = serde_json::from_value(answer(&need)).unwrap();
                        answers.insert(id.clone(), given);
                        asked.push(need);
                    }
                }
                other => panic!("{other:?}"),
            }
        }
        panic!("unsettled: {sql}");
    }

    fn columns(pairs: &[(&str, &str)]) -> Value {
        json!({ "describe": { "columns": pairs.iter().map(|(name, ty)| json!({ "name": name, "type": ty })).collect::<Vec<_>>() } })
    }

    const AGGREGATES: &str = r#"{"rows":[["count","1","function"],["uniqExact","0","function"],["If","0","combinator"]]}"#;

    #[test]
    fn a_query_is_typed_from_its_describe() {
        let sql = "select id, name, tag from events where id = :id::UInt64 and tag = {tag:Nullable(String)};\n";
        let (analysis, asked) = run(sql, &|need| match need {
            Need::Describe { sql, .. } => {
                assert_eq!(sql, "select id, name, tag from events where id = defaultValueOfTypeName('UInt64')::UInt64 and tag = defaultValueOfTypeName('Nullable(String)')");
                columns(&[("id", "UInt64"), ("name", "LowCardinality(String)"), ("tag", "Nullable(String)")])
            }
            Need::Query { .. } => json!({ "rows": [["events", "id", "UInt64", "true", "true"], ["events", "name", "String", "true", "true"], ["events", "tag", "Nullable(String)", "false", "true"]] }),
        });
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
        assert_eq!(analysis.rewritten.sql, "select id, name, tag from events where id = {p1:UInt64}::UInt64 and tag = {p2:Nullable(String)};\n");
        assert_eq!(analysis.rewritten.order, ["id", "tag"]);
        assert_eq!(analysis.columns.iter().map(|c| format!("{}: {}{}", c.name, c.ty.short(), if c.nullable { "?" } else { "" })).collect::<Vec<_>>(), ["id: UInt64", "name: String(utf8)", "tag: String(utf8)?"]);
        assert_eq!(analysis.params.iter().map(|p| format!("{}: {}{}", p.name, p.ty.short(), if p.nullable { "?" } else { "" })).collect::<Vec<_>>(), ["id: UInt64", "tag: String(utf8)?"]);
        assert_eq!((analysis.kind, analysis.cardinality), (Some(StatementKind::Select), Cardinality::Many));
        assert!(analysis.read_only);
        assert_eq!(analysis.fresh.as_ref().map(|f| f.rows.len()), Some(3));
        assert_eq!(asked.len(), 2);
    }

    #[test]
    fn an_aggregate_with_no_group_by_is_one_row() {
        let (analysis, asked) = run("select count() as n, uniqExactIf(id, id > 1) as u from events", &|need| match need {
            Need::Describe { .. } => columns(&[("n", "UInt64"), ("u", "UInt64")]),
            Need::Query { sql, .. } if sql.contains("system.functions") => serde_json::from_str(AGGREGATES).unwrap(),
            Need::Query { .. } => json!({ "rows": [] }),
        });
        assert_eq!(analysis.cardinality, Cardinality::One);
        assert!(asked.iter().any(|n| matches!(n, Need::Query { scope: Scope::Session, .. })));
        // A query that calls no function asks for none.
        let (analysis, asked) = run("select id from events limit 1", &|need| match need {
            Need::Describe { .. } => columns(&[("id", "UInt64")]),
            Need::Query { .. } => json!({ "rows": [] }),
        });
        assert_eq!(analysis.cardinality, Cardinality::AtMostOne);
        assert!(!asked.iter().any(|n| matches!(n, Need::Query { scope: Scope::Session, .. })));
    }

    #[test]
    fn a_parameter_that_states_no_type_is_refused() {
        let (analysis, asked) = run("select id from events where id = :id and name = :name::String", &|_| panic!("nothing is asked"));
        assert!(asked.is_empty());
        assert_eq!(analysis.diagnostics.len(), 1);
        assert_eq!((analysis.diagnostics[0].start, analysis.diagnostics[0].length, analysis.diagnostics[0].code), (33, 3, UNTYPED));
        assert!(analysis.diagnostics[0].message.contains(":id::UInt64"), "{}", analysis.diagnostics[0].message);
        let (analysis, _) = run("select * from {t:Identifier}", &|_| panic!("nothing is asked"));
        assert!(analysis.diagnostics[0].message.contains("substitutes a name"));
    }

    #[test]
    fn an_insert_s_values_take_their_columns_types() {
        let table = json!({ "rows": [["id", "UInt64", "", ""], ["name", "String", "", "The event's name"], ["tag", "Nullable(String)", "", ""], ["day", "Date", "MATERIALIZED", ""]] });
        let sql = "insert into events (id, name, tag) values (:id, lower(:name::String), :tag)";
        let (analysis, _) = run(sql, &|need| match need {
            Need::Query { sql, .. } if sql.starts_with("EXPLAIN AST") => {
                assert_eq!(sql, "EXPLAIN AST insert into events (id, name, tag) values (defaultValueOfTypeName('UInt64'), lower(defaultValueOfTypeName('String')::String), defaultValueOfTypeName('Nullable(String)'))");
                json!({ "rows": [["InsertQuery"]] })
            }
            Need::Query { sql, .. } if sql.contains("fresh_table") => json!({ "rows": [] }),
            Need::Query { .. } => table.clone(),
            Need::Describe { .. } => panic!("an INSERT … VALUES is not described"),
        });
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
        assert_eq!(analysis.rewritten.sql, "insert into events (id, name, tag) values ({p1:UInt64}, lower({p2:String}::String), {p3:Nullable(String)})");
        assert_eq!(analysis.params.iter().map(|p| (p.ty.short(), p.nullable)).collect::<Vec<_>>(), [("UInt64".to_string(), false), ("String(utf8)".to_string(), false), ("String(utf8)".to_string(), true)]);
        assert_eq!((analysis.kind, analysis.cardinality, analysis.read_only), (Some(StatementKind::Insert), Cardinality::None, false));
        // With no column list, the columns an INSERT writes: not a MATERIALIZED one.
        let (analysis, _) = run("insert into events values (:id, :name, :tag)", &|need| match need {
            Need::Query { sql, .. } if sql.starts_with("EXPLAIN AST") => json!({ "rows": [] }),
            Need::Query { sql, .. } if sql.contains("fresh_table") => json!({ "rows": [] }),
            Need::Query { .. } => table.clone(),
            Need::Describe { .. } => panic!(),
        });
        assert_eq!(analysis.params.iter().map(|p| p.ty.short()).collect::<Vec<_>>(), ["UInt64", "String(utf8)", "String(utf8)"]);
        assert_eq!(analysis.params[1].comment.as_deref(), Some("The event's name"));
        let (analysis, _) = run("insert into events (id, nope) values (:id, 1)", &|_| table.clone());
        assert_eq!((analysis.diagnostics[0].message.as_str(), analysis.diagnostics[0].start, analysis.diagnostics[0].length), ("The table events has no column nope", 24, 4));
        let (analysis, _) = run("insert into events (id) values (:id, 1)", &|_| table.clone());
        assert!(analysis.diagnostics[0].message.contains("1 column of events and a row of its VALUES has 2 values"));
        let (analysis, _) = run("insert into missing (id) values (:id)", &|_| json!({ "rows": [] }));
        assert_eq!((analysis.diagnostics[0].message.as_str(), analysis.diagnostics[0].start), ("ClickHouse has no table missing", 12));
    }

    #[test]
    fn an_insert_s_query_is_described() {
        let table = json!({ "rows": [["id", "UInt64", "", ""], ["name", "String", "", ""]] });
        let (analysis, _) = run("insert into events (id, name) select id, name from staging where id > {after:UInt64}", &|need| match need {
            Need::Describe { sql, .. } => {
                assert_eq!(sql, "select id, name from staging where id > defaultValueOfTypeName('UInt64')");
                columns(&[("id", "UInt64"), ("name", "String")])
            }
            Need::Query { sql, .. } if sql.contains("fresh_table") => json!({ "rows": [] }),
            Need::Query { .. } => table.clone(),
        });
        assert!(analysis.diagnostics.is_empty(), "{:?}", analysis.diagnostics);
        assert!(analysis.columns.is_empty());
        assert_eq!(analysis.cardinality, Cardinality::None);
        let (analysis, _) = run("insert into events select id from staging", &|need| match need {
            Need::Describe { .. } => columns(&[("id", "UInt64")]),
            Need::Query { .. } => table.clone(),
        });
        assert!(analysis.diagnostics[0].message.contains("2 columns of events and its query returns 1"));
    }

    #[test]
    fn the_server_s_error_is_placed_where_it_points() {
        let unknown = json!({ "error": { "message": "Unknown expression identifier `nope` in scope SELECT nope FROM events. Maybe you meant: ['name']. (UNKNOWN_IDENTIFIER)", "code": "47" } });
        let (analysis, _) = run("select id,\n  nope from events", &|_| unknown.clone());
        let d = &analysis.diagnostics[0];
        assert_eq!((d.start, d.length, d.code, d.server_code.as_deref()), (13, 4, REFUSED, Some("47")));
        // A syntax error names a position in the text the server was given.
        let (analysis, _) = run("select {a:UInt8}, id frm events", &|need| match need {
            Need::Describe { sql, .. } => json!({ "error": { "message": "Syntax error: failed at position 44 (frm) (SYNTAX_ERROR)", "code": "62", "position": sql.find("frm").unwrap() + 1 } }),
            Need::Query { .. } => panic!("a query is described"),
        });
        let d = &analysis.diagnostics[0];
        assert_eq!(&"select {a:UInt8}, id frm events"[d.start..d.start + d.length], "frm");
        // A command's error says how to leave a migration out of the type check.
        let (analysis, _) = run("alter table events delete where nope = 1", &|_| unknown.clone());
        assert!(analysis.diagnostics[0].message.contains("exclude"), "{}", analysis.diagnostics[0].message);
    }

    #[test]
    fn a_statement_reylo_does_not_type_says_so() {
        let nothing = |_: &Need| -> Value { panic!("nothing is asked") };
        let (analysis, _) = run("show tables", &nothing);
        assert!(analysis.diagnostics[0].message.contains("SHOW returns rows it does not describe"));
        let (analysis, _) = run("select 1 as x format JSON", &nothing);
        assert_eq!((analysis.diagnostics[0].start, analysis.diagnostics[0].length), (14, 6));
        let (analysis, _) = run("insert into events format JSONEachRow {\"id\": 1}", &nothing);
        assert!(analysis.diagnostics[0].message.contains("VALUES"));
        let (analysis, _) = run("select 1; select 2", &nothing);
        assert!(analysis.diagnostics[0].message.contains("holds one statement"));
        // A column or a function named format is neither.
        let described = |need: &Need| match need {
            Need::Describe { .. } => columns(&[("format", "String")]),
            Need::Query { .. } => json!({ "rows": [] }),
        };
        assert!(run("select format from system.formats", &described).0.diagnostics.is_empty());
        assert!(run("select format('{}', name) as format from system.formats", &described).0.diagnostics.is_empty());
    }

    #[test]
    fn a_mutation_is_checked_and_returns_no_rows() {
        let (analysis, asked) = run("alter table events delete where id = {id:UInt64}", &|need| match need {
            Need::Query { sql, .. } => {
                assert_eq!(sql, "EXPLAIN AST alter table events delete where id = defaultValueOfTypeName('UInt64')");
                json!({ "rows": [["AlterQuery"]] })
            }
            Need::Describe { .. } => panic!("a mutation is not described"),
        });
        assert_eq!(asked.len(), 1);
        assert_eq!((analysis.kind, analysis.cardinality), (Some(StatementKind::Other), Cardinality::None));
        assert_eq!(analysis.rewritten.sql, "alter table events delete where id = {p1:UInt64}");
        let (analysis, _) = run("delete from events where id = :id::UInt64", &|_| json!({ "rows": [] }));
        assert_eq!(analysis.kind, Some(StatementKind::Delete));
    }

    #[test]
    fn a_column_with_no_name_is_refused_among_several() {
        let (analysis, _) = run("select count(), max(id) as last from events", &|need| match need {
            Need::Describe { .. } => columns(&[("count()", "UInt64"), ("last", "UInt64")]),
            Need::Query { sql, .. } if sql.contains("system.functions") => serde_json::from_str(AGGREGATES).unwrap(),
            Need::Query { .. } => json!({ "rows": [] }),
        });
        assert!(analysis.diagnostics[0].message.contains("Give it one with `as`"), "{:?}", analysis.diagnostics);
        // Alone, it is the statement's value.
        let (analysis, _) = run("select count() from events", &|need| match need {
            Need::Describe { .. } => columns(&[("count()", "UInt64")]),
            Need::Query { sql, .. } if sql.contains("system.functions") => serde_json::from_str(AGGREGATES).unwrap(),
            Need::Query { .. } => json!({ "rows": [] }),
        });
        assert!(analysis.diagnostics.is_empty() && analysis.value);
        assert_eq!(analysis.cardinality, Cardinality::One);
    }
}
