//! Reylo's analysis engine for ClickHouse, over the core every database shares
//! ([`reylo_engine_core`]): the types the server's `DESCRIBE` of a statement reports
//! ([`clickhouse`]), each read from ClickHouse's own name for it ([`types`]), which says whether
//! the value may be NULL, with how many rows the statement returns read off a `sqlparser` tree of
//! it ([`rows`]) and the columns an insert writes off the catalog ([`catalog`]).
//!
//! ClickHouse prepares no statement and types no parameter from where it stands: a parameter
//! states its type, as ClickHouse's own `{name:Type}` does, and the engine describes the statement
//! with a value of that type in each parameter's place. The engine itself never opens a
//! connection: its host answers the describe and the catalog queries over ClickHouse's HTTP
//! interface (engine/native/src/clickhouse.rs).

pub mod api;
pub mod catalog;
pub mod clickhouse;
pub mod host;
pub mod params;
pub mod rows;
pub mod types;

pub use api::Clickhouse;

/// A fingerprint of the sources this engine was built from: core's and this crate's
/// ([`reylo_engine_core::fingerprint`]).
pub const FINGERPRINT: &str = reylo_engine_core::fingerprint!("REYLO_ENGINE_CLICKHOUSE_HASH");
