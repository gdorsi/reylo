//! What a `sqlparser` tree of a ClickHouse statement shows that the server's describe does not:
//! how many rows it returns, how its select list names its columns, the tables it reads and
//! writes, and the values an `INSERT … VALUES` writes. A statement the tree does not read is typed
//! from the describe alone, and nothing bounds its rows.
//!
//! ClickHouse has no unique key: a primary key orders a table and identifies no row, so no filter
//! bounds a query to one row. A query returns at most one row where it says `LIMIT 1`, and exactly
//! one where it has no FROM, or aggregates with no GROUP BY, which gives one row over no rows too.
//! That last holds under ClickHouse's default settings: `empty_result_for_aggregation_by_empty_set`
//! makes such a query return none.

use crate::catalog::Aggregates;
use crate::params;
use reylo_engine_core::analysis::ResultTarget;
use reylo_engine_core::fresh::Tables;
use reylo_engine_core::types::Cardinality;
use sqlparser::ast::{Expr, Fetch, Function, GroupByExpr, LimitClause, ObjectName, ObjectNamePart, OrderByKind, Query, Select, SelectItem, SetExpr, Statement, TableObject, Value, Visit, Visitor};
use sqlparser::dialect::ClickHouseDialect;
use sqlparser::parser::Parser;
use std::collections::HashSet;
use std::ops::ControlFlow;

/// The one statement of `text` as `sqlparser`'s ClickHouse dialect reads it, if it reads it.
pub fn parse(text: &str) -> Option<Statement> {
    let mut statements = Parser::parse_sql(&ClickHouseDialect {}, text).ok()?;
    if statements.len() == 1 {
        statements.pop()
    } else {
        None
    }
}

/// A name's parts, each unquoted.
pub fn parts(name: &ObjectName) -> Vec<String> {
    name.0
        .iter()
        .map(|p| match p {
            ObjectNamePart::Identifier(i) => i.value.clone(),
            other => other.to_string(),
        })
        .collect()
}

/// The functions `e` calls at its own level, not over a window and not inside a subquery, whose
/// rows are its own.
fn calls(e: &Expr, found: &mut Vec<String>) {
    struct Calls<'a> {
        depth: usize,
        found: &'a mut Vec<String>,
    }
    impl Visitor for Calls<'_> {
        type Break = ();
        fn pre_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
            self.depth += 1;
            ControlFlow::Continue(())
        }
        fn post_visit_query(&mut self, _: &Query) -> ControlFlow<()> {
            self.depth -= 1;
            ControlFlow::Continue(())
        }
        fn pre_visit_expr(&mut self, e: &Expr) -> ControlFlow<()> {
            if let (0, Expr::Function(Function { name, over: None, .. })) = (self.depth, e) {
                if let [name] = parts(name).as_slice() {
                    self.found.push(name.clone());
                }
            }
            ControlFlow::Continue(())
        }
    }
    let _ = e.visit(&mut Calls { depth: 0, found });
}

/// Whether `text` holds `word` as a word, without case.
fn has_word(text: &str, word: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    let b = lower.as_bytes();
    let is_name = |c: u8| c.is_ascii_alphanumeric() || c == b'_';
    lower.match_indices(word).any(|(at, _)| (at == 0 || !is_name(b[at - 1])) && !b.get(at + word.len()).is_some_and(|c| is_name(*c)))
}

fn is_one(e: &Expr) -> bool {
    matches!(e, Expr::Value(v) if matches!(&v.value, Value::Number(n, _) if n.to_string() == "1"))
}

/// What a query's own LIMIT says of its rows: `Some(true)` for a `LIMIT 1`, `Some(false)` for any
/// other limit or offset, `None` for none.
fn limited(q: &Query) -> Option<bool> {
    let by_fetch = |f: &Fetch| !f.with_ties && !f.percent && f.quantity.as_ref().is_some_and(is_one);
    match (&q.limit_clause, &q.fetch) {
        (None, None) => None,
        // `LIMIT 1 BY key` keeps one row of each key.
        (Some(LimitClause::LimitOffset { limit, limit_by, .. }), fetch) => Some(limit_by.is_empty() && (limit.as_ref().is_some_and(is_one) || fetch.as_ref().is_some_and(by_fetch))),
        (Some(LimitClause::OffsetCommaLimit { limit, .. }), _) => Some(is_one(limit)),
        (None, Some(fetch)) => Some(by_fetch(fetch)),
    }
}

/// Whether an ORDER BY fills the gaps between its rows with rows of its own.
fn fills(q: &Query) -> bool {
    q.order_by.as_ref().is_some_and(|o| matches!(&o.kind, OrderByKind::Expressions(keys) if keys.iter().any(|k| k.with_fill.is_some())))
}

/// The functions a SELECT's list calls, where its shape lets it return one row for sure: no
/// GROUP BY, HAVING or QUALIFY to make or drop rows. A list that calls an aggregate is then one
/// row, and so is one that calls none where the SELECT has no FROM and no filter.
fn one_row_shape(s: &Select) -> Option<(Vec<String>, bool)> {
    let grouped = !matches!(&s.group_by, GroupByExpr::Expressions(keys, modifiers) if keys.is_empty() && modifiers.is_empty());
    if grouped || s.having.is_some() || s.qualify.is_some() || s.top.is_some() || !s.lateral_views.is_empty() {
        return None;
    }
    let mut found = Vec::new();
    for item in &s.projection {
        match item {
            SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } | SelectItem::ExprWithAliases { expr: e, .. } => calls(e, &mut found),
            // A `*` over an aggregate's own FROM is refused by ClickHouse; with no FROM it is none.
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => return None,
        }
    }
    let bare = s.from.is_empty() && s.selection.is_none() && s.prewhere.is_none();
    Some((found, bare))
}

/// The functions a statement's select list calls, where whether one of them is an aggregate
/// decides how many rows it returns: what [`cardinality`] needs the server's aggregates for.
pub fn asks_aggregates(statement: &Statement) -> bool {
    fn body(set: &SetExpr) -> bool {
        match set {
            SetExpr::Select(s) => one_row_shape(s).is_some_and(|(calls, bare)| !bare && !calls.is_empty()),
            SetExpr::Query(q) => body(&q.body),
            _ => false,
        }
    }
    matches!(statement, Statement::Query(q) if body(&q.body))
}

/// How many rows a query returns, and why not one where it is not proven so. `text` is the
/// statement the tree was read from.
pub fn cardinality(q: &Query, aggregates: &Aggregates, text: &str) -> (Cardinality, Option<String>) {
    // A totals row follows the result's own, and `arrayJoin` makes a row of each element.
    if has_word(text, "totals") {
        return (Cardinality::Many, Some("WITH TOTALS adds a row of totals to the result".into()));
    }
    let limit = limited(q);
    if limit == Some(true) {
        // A LIMIT written after a UNION is its last SELECT's on ClickHouse, not the whole result's.
        return match q.body.as_ref() {
            SetExpr::SetOperation { .. } => (Cardinality::Many, Some("a LIMIT after a UNION bounds its last SELECT alone: write the UNION in a subquery to bound the whole result".into())),
            _ => (Cardinality::AtMostOne, None),
        };
    }
    let unbounded = |why: &str| (Cardinality::Many, Some(why.to_string()));
    match q.body.as_ref() {
        SetExpr::Query(inner) if limit.is_none() && !fills(q) => cardinality(inner, aggregates, text),
        SetExpr::Select(s) => {
            let Some((calls, bare)) = one_row_shape(s) else { return unbounded("nothing bounds its rows to one: a ClickHouse table has no unique key, so only LIMIT 1 or an aggregate with no GROUP BY does") };
            if has_word(text, "arrayjoin") || fills(q) {
                return unbounded("arrayJoin and WITH FILL make rows of their own");
            }
            let aggregated = calls.iter().any(|name| aggregates.has(name));
            match (aggregated || bare, limit) {
                (true, None) => (Cardinality::One, None),
                // A limit of zero rows, or an offset past the one row.
                (true, Some(_)) => (Cardinality::AtMostOne, None),
                (false, _) => unbounded("nothing bounds its rows to one: a ClickHouse table has no unique key, so only LIMIT 1 or an aggregate with no GROUP BY does"),
            }
        }
        _ => unbounded("nothing bounds its rows to one"),
    }
}

/// How the statement writes its result columns: a query's select list, the first arm's of a set
/// operation, whose names are the result's. `None` for any other statement.
pub fn targets(statement: &Statement) -> Option<Vec<ResultTarget>> {
    fn body(set: &SetExpr) -> Option<Vec<ResultTarget>> {
        match set {
            SetExpr::Select(s) => s.projection.iter().map(item).collect(),
            SetExpr::Query(q) => body(&q.body),
            SetExpr::SetOperation { left, .. } => body(left),
            _ => None,
        }
    }
    fn item(item: &SelectItem) -> Option<ResultTarget> {
        Some(match item {
            SelectItem::UnnamedExpr(e) => match e {
                // A parameter is a value, which the statement gives no name.
                Expr::Identifier(i) if params::marked(&i.value).is_some() => ResultTarget::Unnamed(None),
                Expr::Identifier(_) | Expr::CompoundIdentifier(_) => ResultTarget::Named,
                // `COLUMNS('^a')` and `untuple(t)` stand for as many columns as they match or hold.
                Expr::Function(f) if matches!(parts(&f.name).as_slice(), [name] if name.eq_ignore_ascii_case("columns") || name == "untuple") => ResultTarget::Star,
                _ => ResultTarget::Unnamed(None),
            },
            SelectItem::ExprWithAlias { .. } | SelectItem::ExprWithAliases { .. } => ResultTarget::Named,
            SelectItem::Wildcard(_) | SelectItem::QualifiedWildcard(..) => ResultTarget::Star,
        })
    }
    match statement {
        Statement::Query(q) => body(&q.body),
        _ => None,
    }
}

/// The tables of the session's database the statement reads and writes, for the freshness facts:
/// each named with no database before it, and no WITH query's name.
pub fn tables(statement: &Statement) -> Tables {
    let mut ctes: HashSet<String> = HashSet::new();
    let _ = sqlparser::ast::visit_statements(statement, |s| {
        if let Statement::Query(q) = s {
            for cte in q.with.iter().flat_map(|w| &w.cte_tables) {
                ctes.insert(cte.alias.name.value.clone());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    let _ = sqlparser::ast::visit_expressions(statement, |e| {
        if let Expr::Subquery(q) | Expr::Exists { subquery: q, .. } | Expr::InSubquery { subquery: q, .. } = e {
            for cte in q.with.iter().flat_map(|w| &w.cte_tables) {
                ctes.insert(cte.alias.name.value.clone());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    let write: Vec<String> = match statement {
        Statement::Insert(i) => match &i.table {
            TableObject::TableName(name) => match parts(name).as_slice() {
                [table] => vec![table.clone()],
                _ => Vec::new(),
            },
            _ => Vec::new(),
        },
        _ => Vec::new(),
    };
    let mut read: Vec<String> = Vec::new();
    let _ = sqlparser::ast::visit_relations(statement, |name| {
        if let [table] = parts(name).as_slice() {
            if !ctes.contains(table) && !write.contains(table) && !read.contains(table) {
                read.push(table.clone());
            }
        }
        ControlFlow::<()>::Continue(())
    });
    Tables { read, write, ..Default::default() }
}

/// What an INSERT writes, as the tree reads it.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The table's name, with its database where the statement writes one.
    pub table: Vec<String>,
    /// The columns the statement lists; empty where it lists none.
    pub columns: Vec<String>,
    /// Each row of a `VALUES` list: per value, the number of the parameter that is the whole value.
    pub values: Option<Vec<Vec<Option<usize>>>>,
    /// Whether a query feeds it (`INSERT … SELECT`).
    pub selects: bool,
}

/// The INSERT a statement is, where it names a table.
pub fn insert(statement: &Statement) -> Option<Insert> {
    let Statement::Insert(i) = statement else { return None };
    let TableObject::TableName(name) = &i.table else { return None };
    let columns = i.columns.iter().filter_map(|c| parts(c).pop()).collect();
    let whole = |e: &Expr| match e {
        Expr::Identifier(ident) => params::marked(&ident.value),
        _ => None,
    };
    let (values, selects) = match i.source.as_deref().map(|q| q.body.as_ref()) {
        Some(SetExpr::Values(values)) => (Some(values.rows.iter().map(|row| row.content.iter().map(whole).collect()).collect()), false),
        Some(_) => (None, true),
        None => (None, false),
    };
    Some(Insert { table: parts(name), columns, values, selects })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aggregates() -> Aggregates {
        Aggregates::of(&["uniqExact", "groupArray"], &["count", "sum", "max"], &["If", "Array"])
    }

    fn rows(sql: &str) -> Cardinality {
        let Some(Statement::Query(q)) = parse(sql) else { panic!("{sql}") };
        cardinality(&q, &aggregates(), sql).0
    }

    #[test]
    fn a_limit_or_an_aggregate_bounds_the_rows() {
        assert_eq!(rows("select id from events where id = 1"), Cardinality::Many);
        assert_eq!(rows("select id from events order by id limit 1"), Cardinality::AtMostOne);
        assert_eq!(rows("select id from events limit 5, 1"), Cardinality::AtMostOne);
        assert_eq!(rows("select id from events limit 1 offset 3"), Cardinality::AtMostOne);
        assert_eq!(rows("select id from events limit 2"), Cardinality::Many);
        assert_eq!(rows("select id from events limit 1 by name"), Cardinality::Many);
        assert_eq!(rows("select id from events limit reylo_parameter_1"), Cardinality::Many);
        assert_eq!(rows("select count() from events"), Cardinality::One);
        assert_eq!(rows("select COUNT(*) as n, max(id) + 1 as next from events where name = 'a'"), Cardinality::One);
        assert_eq!(rows("select sumIf(id, id > 1) as s, uniqExactArray(tags) as u from events"), Cardinality::One);
        assert_eq!(rows("select (select count() from events) as n"), Cardinality::One);
        assert_eq!(rows("select count() from events limit 0"), Cardinality::AtMostOne);
        assert_eq!(rows("select name, count() from events group by name"), Cardinality::Many);
        assert_eq!(rows("select count() from events having count() > 1"), Cardinality::Many);
        assert_eq!(rows("select count() over () from events"), Cardinality::Many);
        assert_eq!(rows("select length(name) from events"), Cardinality::Many);
        assert_eq!(rows("select 1, now()"), Cardinality::One);
        assert_eq!(rows("select 1 where 0"), Cardinality::Many);
        assert_eq!(rows("select arrayJoin([1, 2, 3])"), Cardinality::Many);
        assert_eq!(rows("select max(arrayJoin([1, 2, 3]))"), Cardinality::Many);
        assert_eq!(rows("(select count() from events)"), Cardinality::One);
        assert_eq!(rows("select 1 union all select 2"), Cardinality::Many);
        assert_eq!(rows("select 1 union all select 2 limit 1"), Cardinality::Many);
        assert_eq!(rows("select name, count() from events group by name with totals limit 1"), Cardinality::Many);
    }

    #[test]
    fn a_select_list_names_its_columns_or_does_not() {
        let of = |sql: &str| targets(&parse(sql).unwrap()).unwrap();
        assert_eq!(of("select id, e.name, count() as n, id + 1, *, e.* from events e"), [ResultTarget::Named, ResultTarget::Named, ResultTarget::Named, ResultTarget::Unnamed(None), ResultTarget::Star, ResultTarget::Star]);
        assert_eq!(of("select reylo_parameter_1, COLUMNS('^a'), untuple(t) from events"), [ResultTarget::Unnamed(None), ResultTarget::Star, ResultTarget::Star]);
        assert_eq!(of("select 1 as a union all select 2"), [ResultTarget::Named]);
        assert!(targets(&parse("insert into events (id) values (1)").unwrap()).is_none());
    }

    #[test]
    fn the_tables_of_the_session_s_database_are_the_facts() {
        let found = tables(&parse("with recent as (select * from events) select * from recent join users on users.id = recent.id join analytics.pages p on 1 = 1").unwrap());
        assert_eq!((found.read, found.write), (vec!["events".to_string(), "users".to_string()], Vec::<String>::new()));
        let found = tables(&parse("insert into events (id) select id from staging").unwrap());
        assert_eq!((found.read, found.write), (vec!["staging".to_string()], vec!["events".to_string()]));
    }

    #[test]
    fn an_insert_is_read_for_its_table_columns_and_values() {
        let read = |sql: &str| insert(&parse(sql).unwrap()).unwrap();
        assert_eq!(read("insert into events (id, name) values (reylo_parameter_1, lower(reylo_parameter_2)), (2, reylo_parameter_3)"), Insert { table: vec!["events".into()], columns: vec!["id".into(), "name".into()], values: Some(vec![vec![Some(1), None], vec![None, Some(3)]]), selects: false });
        assert_eq!(read("insert into analytics.events select * from staging"), Insert { table: vec!["analytics".into(), "events".into()], columns: vec![], values: None, selects: true });
    }
}
