//! Library side of the `multi` binary: pieces that integration tests and
//! later work packages use directly.

pub mod auth;
pub mod models;
pub mod run;
pub mod segment;
pub mod service;
pub mod supervisor;
pub mod tls;
pub mod web;
