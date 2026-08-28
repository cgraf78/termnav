//! Authoritative Rust vocabulary for the frozen relay v2 wire protocol.

pub(crate) const VERSION: u8 = 2;

pub(crate) mod operation {
    pub(crate) const NAVIGATE: &str = "navigate";
    pub(crate) const PREPARE_PATH: &str = "prepare-path";
    pub(crate) const ABORT_PATH: &str = "abort-path";
    pub(crate) const COMMIT_PATH: &str = "commit-path";
    pub(crate) const FOCUS: &str = "focus";
}

pub(crate) mod result {
    pub(crate) const ARMED: &str = "armed";
    pub(crate) const CLAIMED: &str = "claimed";
    pub(crate) const DECLINED: &str = "declined";
    pub(crate) const EMITTED: &str = "emitted";
    pub(crate) const ERROR: &str = "error";
    pub(crate) const RELEASED: &str = "released";
}
