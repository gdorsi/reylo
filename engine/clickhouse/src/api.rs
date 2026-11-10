//! The ClickHouse side of the request protocol ([`reylo_engine_core::api`]): what a describe is
//! answered with.

use crate::host::Host;
use reylo_engine_core::database::Database;
use reylo_engine_core::dialect::Dialect;
use reylo_engine_core::host::Lazy;
use reylo_engine_core::types::StatementKind;
use serde::Deserialize;

pub use reylo_engine_core::api::{Analysis, Column, DbError, Diagnostic, Need, Param, Response, Scope};

/// What the host learned for one need: a describe is a [`ClickhouseDescribe`].
pub type Answer = reylo_engine_core::api::Answer<ClickhouseDescribe>;
pub type Answers = reylo_engine_core::api::Answers<ClickhouseDescribe>;

/// What `DESCRIBE (<statement>)` reports before the statement runs: each result column's name and
/// its type as ClickHouse writes it.
#[derive(Debug, Clone, Deserialize)]
pub struct ClickhouseDescribe {
    pub columns: Vec<ClickhouseColumn>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ClickhouseColumn {
    pub name: String,
    /// The type as ClickHouse writes it: `UInt64`, `Nullable(String)`, `DateTime64(3, 'UTC')`.
    #[serde(rename = "type")]
    pub ty: String,
}

/// ClickHouse, for [`reylo_engine_core::api::call`].
pub struct Clickhouse;

impl Database for Clickhouse {
    const DIALECT: Dialect = Dialect::Clickhouse;
    const FINGERPRINT: &'static str = crate::FINGERPRINT;
    type Describe = ClickhouseDescribe;

    fn analyze(sql: &str, host: &Host) -> Lazy<Analysis> {
        host.on_engine_stack(|host| crate::clickhouse::analyze(sql, host))
    }

    /// A query writes nothing on ClickHouse: only an INSERT, a mutation or DDL does.
    fn read_only(analysis: &Analysis, _host: &Host) -> Lazy<bool> {
        Some(analysis.kind == Some(StatementKind::Select) && analysis.diagnostics.is_empty())
    }
}

#[cfg(test)]
mod protocol {
    use super::*;
    use reylo_engine_core::api::call;
    use serde_json::Value;

    fn ask(request: &str) -> Value {
        serde_json::from_str(&call::<Clickhouse>(request)).unwrap()
    }

    #[test]
    fn the_artifact_names_its_database_and_refuses_another_s_statement() {
        let ping = ask(r#"{"op":"ping"}"#);
        assert_eq!((ping["status"].as_str(), ping["database"].as_str(), ping["fingerprint"].as_str()), (Some("ok"), Some("clickhouse"), Some(crate::FINGERPRINT)));
        // Lexing and the project file are core's, for any dialect.
        assert_eq!(ask(r#"{"op":"lex","sql":"select {id:UInt64}, :name","dialect":"clickhouse"}"#)["lexed"]["params"], serde_json::json!(["id", "name"]));
        assert_eq!(ask(r#"{"op":"config","text":"url: clickhouse://localhost:8123/app"}"#)["status"], "config");
        let refused = ask(r#"{"op":"analyze","sql":"select 1","dialect":"sqlite","answers":{}}"#);
        assert_eq!((refused["status"].as_str(), refused["message"].as_str()), (Some("error"), Some("this engine reads clickhouse statements, not sqlite ones")));
    }
}
