//! Backend implementations of [`crate::storage::KnowledgeStore`].
//!
//! Each backend lives in its own submodule. The OverGraph backend
//! continues to live in `crate::storage::db` for now — its `Db` type
//! implements the trait directly. The Neo4j backend is the second
//! supported destination.

pub mod neo4j;
