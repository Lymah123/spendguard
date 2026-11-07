//! Command-line interface.

pub mod approval;
pub mod commands;

pub use approval::PromptApprover;
pub use commands::{execute, keygen, Cli, Command};
