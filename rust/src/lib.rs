//! `review-kb` Rust twin — library root.
//!
//! Modules are added incrementally per the phased plan. Phase 1 ships the
//! foundation primitives; later phases add checklist parsing, the repository,
//! service orchestration, and the CLI.

pub mod checklist;
pub mod cli;
pub mod config;
pub mod description;
pub mod errors;
pub mod json_util;
pub mod models;
pub mod path_util;
pub mod repository;
pub mod service;
pub mod suggestions;
pub mod time_util;
