//! The dialects the engine reads: the name a request carries, and the placeholder rules the lexer
//! applies, which are settled before any database's parser runs. Each dialect is read by its own
//! database's engine: Postgres with libpg_query, SQLite through SQLite's own resolved tree, MySQL
//! from the server's prepare metadata and a `sqlparser` tree, DuckDB from its prepared
//! statement's types and a `sqlparser` tree, and ClickHouse from the server's `DESCRIBE` of the
//! statement and a `sqlparser` tree.

use crate::lex::Target;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Dialect {
    Postgres,
    Sqlite,
    Mysql,
    Duckdb,
    Clickhouse,
}

impl Dialect {
    pub fn name(self) -> &'static str {
        match self {
            Dialect::Postgres => "postgres",
            Dialect::Sqlite => "sqlite",
            Dialect::Mysql => "mysql",
            Dialect::Duckdb => "duckdb",
            Dialect::Clickhouse => "clickhouse",
        }
    }

    /// The placeholder form an analysis hands the statement back in, the one the driver binds.
    /// ClickHouse binds `{pN:Type}`, which carries the parameter's type: its engine writes it over
    /// the `$n` this names, and a statement it could not type, which never runs, keeps the `$n`.
    pub fn binds(self) -> Target {
        match self {
            Dialect::Postgres | Dialect::Duckdb | Dialect::Clickhouse => Target::Dollar,
            Dialect::Sqlite | Dialect::Mysql => Target::Question,
        }
    }
}
