#![deny(rustdoc::broken_intra_doc_links, rustdoc::bare_urls, rust_2018_idioms)]
#![warn(
    clippy::clone_on_ref_ptr,
    clippy::use_self,
    clippy::str_to_string,
    clippy::string_to_string
)]
#![allow(dead_code, clippy::too_many_arguments)]
mod chunk;
mod column;
mod metrics;
mod row_group;
mod schema;
pub mod table;
mod value;

// Identifiers that are exported as part of the public API.
pub use self::schema::*;
pub use chunk::{Chunk as RBChunk, ChunkBuilder as RBChunkBuilder, Error};
pub use metrics::Metrics as ChunkMetrics;
pub use row_group::{BinaryExpr, Predicate};
pub use table::ReadFilterResults;

/// THIS MODULE SHOULD ONLY BE IMPORTED FOR BENCHMARKS.
///
/// This module lets us expose internal parts of the crate so that we can use
/// libraries like criterion for benchmarking.
///
/// It should not be imported into any non-testing or benchmarking crates.
pub mod benchmarks {
    pub use crate::column::{
        cmp::Operator,
        encoding::scalar::transcoders::*,
        encoding::scalar::{Fixed, FixedNull, ScalarEncoding},
        encoding::string,
        Column, RowIDs,
    };
    pub use crate::row_group::{ColumnType, RowGroup};
    use crate::{ChunkMetrics, RBChunk};

    // Allow external benchmarks to use this crate-only test method
    pub fn new_from_row_group(row_group: RowGroup) -> RBChunk {
        RBChunk::new_from_row_group(row_group, ChunkMetrics::new_unregistered())
    }
}
