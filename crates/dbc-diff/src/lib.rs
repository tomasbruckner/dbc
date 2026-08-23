//! G7: read-only schema/data diff engine. No GPUI, no driver crates, no
//! write path anywhere in this crate — see the module docs on each
//! submodule for what each half does.

pub mod data_diff;
pub mod schema_diff;
pub mod text_diff;
