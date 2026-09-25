//! Deterministic, source-only dependency resolution for RBE packages.
//!
//! `resolve` keeps the original shared/global dependency semantics. `resolve_scoped`
//! gives each explicitly requested root its own private transitive graph so roots
//! may select conflicting dependency versions without exposing those dependencies
//! as project-level imports.

#![forbid(unsafe_code)]

mod core;
mod scoped;

pub use core::*;
pub use scoped::*;
