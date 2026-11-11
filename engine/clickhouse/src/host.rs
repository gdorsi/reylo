//! The engine's view of a ClickHouse server ([`reylo_engine_core::host`]), whose describe is the
//! columns `DESCRIBE` reports for a statement ([`ClickhouseDescribe`]).

use crate::api::ClickhouseDescribe;
pub use reylo_engine_core::host::{as_str, Lazy};

pub type Host<'a> = reylo_engine_core::host::Host<'a, ClickhouseDescribe>;
