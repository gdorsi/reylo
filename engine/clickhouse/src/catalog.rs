//! What the analysis reads of ClickHouse's catalog: the columns of the table an INSERT writes,
//! whose types are the types of the parameters its VALUES list writes whole, and the names of the
//! aggregate functions and their combinators, by which a query with no GROUP BY is known to
//! return one row.

use crate::host::{as_str, Host, Lazy};
use reylo_engine_core::api::Scope;

/// One row per column of the table `{table}` of `{database}`, in the table's order: its name, its
/// type, how its default is made (`MATERIALIZED`, `ALIAS` and `EPHEMERAL` columns take no value of
/// an INSERT that lists no columns) and its comment.
const COLUMNS_QUERY: &str = "select name, type, default_kind, comment from system.columns where database = {database} and table = {table} order by position";

/// Every aggregate function, with whether its name is read without case, and every combinator
/// that makes another of one (`-If`, `-Array`, `-State`).
const AGGREGATES_QUERY: &str = "select name, toString(case_insensitive), 'function' from system.functions where is_aggregate \
    union all select name, '0', 'combinator' from system.aggregate_function_combinators";

/// A string as a ClickHouse literal: a backslash and a quote escaped.
pub fn literal(text: &str) -> String {
    format!("'{}'", text.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// A table's column as the catalog has it.
#[derive(Debug, Clone, PartialEq)]
pub struct TableColumn {
    pub name: String,
    /// The type as ClickHouse writes it.
    pub ty: String,
    /// Whether an INSERT that lists no columns writes it: not a `MATERIALIZED`, an `ALIAS` or an
    /// `EPHEMERAL` one.
    pub written: bool,
    pub comment: Option<String>,
}

/// The columns of the table `parts` names (`events`, or `analytics.events`), in its order; empty
/// where the catalog has no such table.
pub fn columns(host: &Host, parts: &[String]) -> Lazy<Vec<TableColumn>> {
    let (database, table) = match parts {
        [table] => ("currentDatabase()".to_string(), table),
        [database, table] => (literal(database), table),
        _ => return Some(Vec::new()),
    };
    let query = COLUMNS_QUERY.replace("{database}", &database).replace("{table}", &literal(table));
    let Ok(rows) = host.query(&query, Vec::new(), Scope::Statement)? else { return Some(Vec::new()) };
    Some(
        rows.iter()
            .filter_map(|r| {
                let kind = as_str(r.get(2)?).unwrap_or_default();
                Some(TableColumn {
                    name: as_str(r.first()?)?.to_string(),
                    ty: as_str(r.get(1)?)?.to_string(),
                    written: !matches!(kind, "MATERIALIZED" | "ALIAS" | "EPHEMERAL"),
                    comment: r.get(3).and_then(as_str).filter(|c| !c.is_empty()).map(String::from),
                })
            })
            .collect(),
    )
}

/// The server's aggregate functions and combinators.
#[derive(Debug, Clone, Default)]
pub struct Aggregates {
    /// The names read as written.
    exact: Vec<String>,
    /// The names read without case, lowercased.
    folded: Vec<String>,
    combinators: Vec<String>,
}

impl Aggregates {
    /// Whether `name` is an aggregate function: one of the server's, or one with combinators
    /// after it (`sumIf`, `uniqExactArrayIf`).
    pub fn has(&self, name: &str) -> bool {
        if self.exact.iter().any(|n| n == name) || self.folded.contains(&name.to_ascii_lowercase()) {
            return true;
        }
        self.combinators.iter().any(|c| name.strip_suffix(c.as_str()).is_some_and(|rest| !rest.is_empty() && self.has(rest)))
    }

    #[cfg(test)]
    pub fn of(exact: &[&str], folded: &[&str], combinators: &[&str]) -> Aggregates {
        let own = |names: &[&str]| names.iter().map(|n| n.to_string()).collect();
        Aggregates { exact: own(exact), folded: own(folded), combinators: own(combinators) }
    }
}

/// The server's aggregate functions; none where it does not say, which proves no statement an
/// aggregate.
pub fn aggregates(host: &Host) -> Lazy<Aggregates> {
    let Ok(rows) = host.query(AGGREGATES_QUERY, Vec::new(), Scope::Session)? else { return Some(Aggregates::default()) };
    let mut found = Aggregates::default();
    for r in &rows {
        let (Some(name), Some(folds), Some(what)) = (r.first().and_then(as_str), r.get(1).and_then(as_str), r.get(2).and_then(as_str)) else { continue };
        match (what, folds) {
            ("combinator", _) => found.combinators.push(name.to_string()),
            (_, "1") => found.folded.push(name.to_ascii_lowercase()),
            _ => found.exact.push(name.to_string()),
        }
    }
    Some(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_aggregate_is_known_through_its_combinators() {
        let aggregates = Aggregates::of(&["uniqExact", "groupArray"], &["count", "sum"], &["If", "Array", "State", "OrNull"]);
        assert!(aggregates.has("count") && aggregates.has("COUNT") && aggregates.has("uniqExact"));
        assert!(aggregates.has("sumIf") && aggregates.has("uniqExactArrayIf") && aggregates.has("sumOrNull"));
        assert!(!aggregates.has("uniqexact") && !aggregates.has("If") && !aggregates.has("length") && !aggregates.has("toStartOfDayIf"));
        assert_eq!(literal("it's a \\ path"), "'it\\'s a \\\\ path'");
    }
}
