//! The decision core. Pure functions: no I/O, no clock, no keys.

pub mod engine;
pub mod resolver;

pub use engine::{BudgetWindow, Policy, PolicyDecision, RejectReason};
pub use resolver::{Allowlist, ResolveError, Resolved};
