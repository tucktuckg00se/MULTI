//! Shared building blocks for MULTI: caption text types, configuration, and
//! the framing used between the main process and its worker processes.

pub mod clean;
pub mod config;
pub mod degrade;
pub mod filter;
pub mod ipc;
pub mod models;
pub mod quality;
pub mod segment;
pub mod types;
pub mod worker;

pub use config::Config;
pub use types::{Clause, Translation, Word};
