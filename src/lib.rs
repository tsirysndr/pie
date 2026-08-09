//! pie — build official language runtimes from source with PIE enabled.
//!
//! The binary is a thin shell over these modules; they are public so the
//! recipes shipped in this repository can be linted by the integration tests.

pub mod build;
pub mod elf;
pub mod recipe;
pub mod resolve;
pub mod runner;
pub mod template;
pub mod ui;
