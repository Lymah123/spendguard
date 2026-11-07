//! SpendGuard: the AI proposes, deterministic Rust code decides.

pub mod audit;
pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod parser;
pub mod policy;
pub mod service;
pub mod solana;
pub mod wallet;

pub use error::{Error, Result};
