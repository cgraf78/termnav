//! Build-version reporting shared by the CLI and release smoke tests.

/// Concrete Git commit embedded at build time.
pub const COMMIT: &str = env!("TERMNAV_BUILD_COMMIT");

/// Timestamp/hash release identity embedded at build time.
pub const VERSION: &str = env!("TERMNAV_BUILD_VERSION");

/// Render the user-facing version line.
#[must_use]
pub fn line() -> String {
    format!("termnav {VERSION}")
}
