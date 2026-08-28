//! `termnav ssh` command adapter.

use std::ffi::OsString;
use std::io;

/// Run the connection-owned SSH supervisor.
pub fn run(arguments: &[OsString]) -> io::Result<i32> {
    crate::ssh::run(arguments)
}
