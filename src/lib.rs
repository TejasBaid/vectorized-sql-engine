//! # sqlengine
//!
//! A vectorized, multi-threaded SQL query engine written from scratch:
//!
//! ```text
//! SQL text ─► lexer/parser ─► AST ─► binder ─► logical plan ─► optimizer
//!          ─► physical planner ─► vectorized, partitioned execution ─► batches
//! ```
//!
//! The entry point is [`Session`].

pub mod catalog;
pub mod column;
pub mod compute;
pub mod datasource;
pub mod error;
pub mod format;
pub mod logical;
pub mod optimizer;
pub mod physical;
pub mod session;
pub mod sql;
pub mod tpch;
pub mod types;
pub mod util;

pub use error::{Error, ErrorKind, Result};
pub use session::{QueryResult, Session, SessionConfig};
pub use sql::ast;
