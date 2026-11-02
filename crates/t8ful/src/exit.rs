//! Process exit codes, documented for scripts and the headless contract.

/// The turn completed.
pub const SUCCESS: i32 = 0;

/// A runtime failure: provider error, unusable configuration file, I/O error.
pub const FAILURE: i32 = 1;

/// Invalid usage or configuration.
pub const USAGE: i32 = 2;

/// The run needed an interactive answer that headless execution cannot provide.
pub const INPUT_REQUIRED: i32 = 3;

/// The run was interrupted by the user.
pub const INTERRUPTED: i32 = 130;
