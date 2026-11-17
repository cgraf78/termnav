//! The oldest supported tmux, and how a tmux version string compares.
//!
//! One rule serves every check: `master` and OpenBSD's base tmux
//! (`openbsd-*`) are newer than any release; otherwise one leading `next-`
//! is ignored and the leading `MAJOR.MINOR` compares, so `3.3a`, `3.3-rc`
//! and `next-3.3` all read as 3.3. A version this rule cannot read is
//! treated as newest: the check guides users, it does not protect anything.

/// The oldest tmux release Termnav supports, as (major, minor).
pub(crate) const TMUX_MINIMUM: (u32, u32) = (3, 3);

/// What a tmux version string says about its release.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TmuxVersion {
    /// A release, or a build that names the release it precedes.
    Release(u32, u32),
    /// A build newer than any release: `master`, or OpenBSD's base tmux.
    Newest,
    /// A version this rule cannot read; it compares as newest.
    Unknown,
}

impl TmuxVersion {
    /// Read `version` as `#{version}` or `tmux -V` reports it.
    pub(crate) fn parse(version: &str) -> Self {
        let version = version.trim();
        let version = version.strip_prefix("tmux ").unwrap_or(version);
        if version == "master" || version.starts_with("openbsd-") {
            return Self::Newest;
        }
        let version = version.strip_prefix("next-").unwrap_or(version);
        let number = |text: &str| -> Option<(u32, usize)> {
            let digits = text.bytes().take_while(u8::is_ascii_digit).count();
            Some((text[..digits].parse().ok()?, digits))
        };
        let Some((major, digits)) = number(version) else {
            return Self::Unknown;
        };
        let Some(rest) = version[digits..].strip_prefix('.') else {
            return Self::Unknown;
        };
        match number(rest) {
            Some((minor, _)) => Self::Release(major, minor),
            None => Self::Unknown,
        }
    }

    /// Whether this is at least (major, minor); unreleased and unknown
    /// builds are.
    pub(crate) fn at_least(self, minimum: (u32, u32)) -> bool {
        match self {
            Self::Release(major, minor) => (major, minor) >= minimum,
            Self::Newest | Self::Unknown => true,
        }
    }
}

/// Check that the tmux server reporting `version` is supported.
///
/// # Errors
///
/// Returns the message to show when it is older than [`TMUX_MINIMUM`].
pub(crate) fn tmux_supported(version: &str) -> Result<(), String> {
    if TmuxVersion::parse(version).at_least(TMUX_MINIMUM) {
        return Ok(());
    }
    let (major, minor) = TMUX_MINIMUM;
    Err(format!(
        "needs tmux {major}.{minor} or newer (server is {})",
        version.trim()
    ))
}

#[cfg(test)]
mod tests {
    use super::{TMUX_MINIMUM, TmuxVersion, tmux_supported};

    /// The canonical vectors, shared verbatim by every repository that
    /// checks the tmux floor: version, supported, and how it reads.
    const VECTORS: &[(&str, bool, &str)] = &[
        ("1.8", false, "1.8"),
        ("2.9a", false, "2.9"),
        ("3.0", false, "3.0"),
        ("3.1c", false, "3.1"),
        ("3.2", false, "3.2"),
        ("3.2a", false, "3.2"),
        ("3.2-rc3", false, "3.2"),
        ("next-3.2", false, "3.2"),
        ("3.3", true, "3.3"),
        ("3.3a", true, "3.3"),
        ("3.3-rc", true, "3.3"),
        ("next-3.3", true, "3.3"),
        ("3.4", true, "3.4"),
        ("3.10", true, "3.10"),
        ("next-3.10", true, "3.10"),
        ("master", true, "newest"),
        ("openbsd-7.6", true, "newest"),
        ("openbsd-7.6-current", true, "newest"),
        ("", true, "unknown"),
    ];

    fn reads(version: TmuxVersion) -> String {
        match version {
            TmuxVersion::Release(major, minor) => format!("{major}.{minor}"),
            TmuxVersion::Newest => "newest".to_owned(),
            TmuxVersion::Unknown => "unknown".to_owned(),
        }
    }

    #[test]
    fn every_canonical_vector_reads_and_compares_as_listed() {
        // The table's boundary is the floor itself.
        assert_eq!(TMUX_MINIMUM, (3, 3));
        for &(version, supported, read) in VECTORS {
            assert_eq!(reads(TmuxVersion::parse(version)), read, "{version:?}");
            assert_eq!(tmux_supported(version).is_ok(), supported, "{version:?}");
            // tmux -V prefixes the same version.
            let reported = format!("tmux {version}");
            assert_eq!(tmux_supported(&reported).is_ok(), supported, "{reported:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_floor_and_the_server() {
        assert_eq!(
            tmux_supported("3.2a\n").unwrap_err(),
            "needs tmux 3.3 or newer (server is 3.2a)"
        );
    }

    #[test]
    fn feature_checks_count_unreleased_builds_as_newest() {
        assert!(TmuxVersion::parse("master").at_least((9, 9)));
        assert!(TmuxVersion::parse("openbsd-7.6").at_least((3, 6)));
        assert!(!TmuxVersion::parse("3.5a").at_least((3, 6)));
        assert!(TmuxVersion::parse("next-3.6").at_least((3, 6)));
    }
}
