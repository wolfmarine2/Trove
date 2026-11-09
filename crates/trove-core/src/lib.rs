//! Core domain types shared by every Trove crate.
//!
//! This crate deliberately has no knowledge of HTTP, storage backends or
//! database drivers beyond error conversion; it defines the vocabulary the
//! rest of the system speaks.

pub mod config;
pub mod error;
pub mod model;
pub mod util;

pub use error::{Error, Result};
