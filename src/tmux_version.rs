//! The oldest supported tmux, and how a tmux version string compares.
//!
//! One rule serves every check: `master` and OpenBSD's base tmux
//! (`openbsd-*`) are newer than any release; otherwise one leading `next-`
//! is ignored and the leading `MAJOR.MINOR` compares, so `3.3a`, `3.3-rc`
//! and `next-3.3` all read as 3.3. A version this rule cannot read is
//! treated as newest: the check guides users, it does not protect anything.

use std::process::{Command, Stdio};
use std::time::Duration;

/// The oldest tmux release Termnav supports, as (major, minor).
///
/// The README, the man page, docs/reference.md and examples/tmux.conf state
/// it; this module's tests keep them in step.
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

/// Ask the tmux server that `command` (a `tmux` command already aimed at its
/// socket) reaches for its version, and check it.
///
/// A server that does not answer is not refused here: the caller's own tmux
/// commands report that failure in their own words.
///
/// # Errors
///
/// Returns the message to show when the server is older than
/// [`TMUX_MINIMUM`].
pub(crate) fn check_server(mut command: Command) -> Result<(), String> {
    command
        .args(["display-message", "-p", "#{version}"])
        .stdin(Stdio::null());
    match crate::process::output_timeout(&mut command, Duration::from_secs(2)) {
        Ok(output) if output.status.success() => {
            tmux_supported(&String::from_utf8_lossy(&output.stdout))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::{TMUX_MINIMUM, TmuxVersion, check_server, tmux_supported};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The canonical vectors, shared verbatim by every repository that checks
    /// the tmux floor: each version and how it reads, independent of any
    /// floor. Whether one is supported is computed from [`TMUX_MINIMUM`].
    const VECTORS: &[(&str, &str)] = &[
        ("1.8", "1.8"),
        ("2.9a", "2.9"),
        ("next-2.9", "2.9"),
        ("3.0", "3.0"),
        ("3.1c", "3.1"),
        ("3.2", "3.2"),
        ("3.2a", "3.2"),
        ("3.2-rc3", "3.2"),
        ("next-3.2", "3.2"),
        ("next-3.2-rc", "3.2"),
        ("3.3", "3.3"),
        ("3.3a", "3.3"),
        ("3.3-rc", "3.3"),
        ("next-3.3", "3.3"),
        ("3.4", "3.4"),
        ("3.10", "3.10"),
        ("next-3.10", "3.10"),
        ("master", "newest"),
        ("openbsd-7.6", "newest"),
        ("openbsd-7.6-current", "newest"),
        ("3.x", "unknown"),
        ("3.", "unknown"),
        ("", "unknown"),
    ];

    /// Rows around the floor itself, so a floor bump is tested at its edge.
    fn floor_rows() -> Vec<(String, String)> {
        let (major, minor) = TMUX_MINIMUM;
        assert!(
            minor > 0,
            "the floor-edge rows cannot express a floor of {major}.0"
        );
        let below = format!("{major}.{}", minor - 1);
        let floor = format!("{major}.{minor}");
        let above = format!("{major}.{}", minor + 1);
        [
            (below.clone(), below.clone()),
            (format!("{below}a"), below.clone()),
            (format!("{below}-rc"), below.clone()),
            (format!("next-{below}"), below),
            (floor.clone(), floor),
            (above.clone(), above),
        ]
        .into_iter()
        .collect()
    }

    fn reads(version: TmuxVersion) -> String {
        match version {
            TmuxVersion::Release(major, minor) => format!("{major}.{minor}"),
            TmuxVersion::Newest => "newest".to_owned(),
            TmuxVersion::Unknown => "unknown".to_owned(),
        }
    }

    /// Whether a version that reads as `read` meets the floor.
    fn supported(read: &str) -> bool {
        let Some((major, minor)) = read.split_once('.') else {
            return true;
        };
        (major.parse::<u32>().unwrap(), minor.parse::<u32>().unwrap()) >= TMUX_MINIMUM
    }

    #[test]
    fn every_canonical_vector_reads_and_compares_as_listed() {
        let rows = VECTORS
            .iter()
            .map(|&(version, read)| (version.to_owned(), read.to_owned()))
            .chain(floor_rows());
        for (version, read) in rows {
            assert_eq!(reads(TmuxVersion::parse(&version)), read, "{version:?}");
            let want = supported(&read);
            assert_eq!(tmux_supported(&version).is_ok(), want, "{version:?}");
            // tmux -V prefixes the same version.
            let reported = format!("tmux {version}");
            assert_eq!(tmux_supported(&reported).is_ok(), want, "{reported:?}");
        }
    }

    #[test]
    fn a_refusal_names_the_floor_and_the_server() {
        let (major, minor) = TMUX_MINIMUM;
        assert_eq!(
            tmux_supported("2.9a\n").unwrap_err(),
            format!("needs tmux {major}.{minor} or newer (server is 2.9a)")
        );
    }

    #[test]
    fn feature_checks_count_unreleased_builds_as_newest() {
        assert!(TmuxVersion::parse("master").at_least((9, 9)));
        assert!(TmuxVersion::parse("openbsd-7.6").at_least((3, 6)));
        assert!(!TmuxVersion::parse("3.5a").at_least((3, 6)));
        assert!(TmuxVersion::parse("next-3.6").at_least((3, 6)));
    }

    /// A stand-in for `tmux` that prints `answer` and exits `status`.
    fn server(answer: &str, status: i32) -> Command {
        let mut command = Command::new("sh");
        command.args([
            "-c",
            &format!("printf '%s\\n' '{answer}'; exit {status}"),
            "tmux",
        ]);
        command
    }

    #[test]
    fn the_server_check_refuses_only_an_old_server_that_answers() {
        assert!(check_server(server("2.9a", 0)).is_err());
        assert!(check_server(server("3.6b", 0)).is_ok());
        assert!(check_server(server("next-3.9", 0)).is_ok());
        // No answer: the caller's own commands report that.
        assert!(check_server(server("2.9a", 1)).is_ok());
    }

    fn root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
    }

    #[test]
    fn the_documents_state_the_floor() {
        let (major, minor) = TMUX_MINIMUM;
        let statement = format!("tmux {major}.{minor} or newer");
        for path in ["README.md", "man/man1/termnav.1", "docs/reference.md"] {
            let text = std::fs::read_to_string(root().join(path)).unwrap();
            assert!(text.contains(&statement), "{path} must say {statement:?}");
        }
        let example = std::fs::read_to_string(root().join("examples/tmux.conf")).unwrap();
        assert!(
            example.contains(&format!("\n# tmux minimum: {major}.{minor}\n")),
            "examples/tmux.conf must carry the floor marker"
        );
    }

    /// Text that may name an older tmux, by file and by the words it uses.
    const LINT_ALLOWED: &[(&str, &str)] = &[];

    /// Each line without its leading space and comment or roff markers, all
    /// joined, so a mention wrapped across lines reads as one.
    fn joined(text: &str) -> String {
        text.lines()
            .map(|line| {
                let line = line.trim_start();
                let line = line.trim_start_matches(['/', '#', '*', '!', ';', '-', '>']);
                let line = match line.strip_prefix('.') {
                    Some(roff) => roff.trim_start_matches(|c: char| c.is_ascii_alphabetic()),
                    None => line,
                };
                line.trim()
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Every "tmux <version>" mention, case-insensitive, with an optional
    /// `-`, `>=`, `before` or `v` in between, that names a release below the
    /// floor.
    fn old_mentions(text: &str) -> Vec<String> {
        let lower = text.to_ascii_lowercase();
        let mut found = Vec::new();
        let mut start = 0;
        while let Some(offset) = lower[start..].find("tmux") {
            let at = start + offset;
            start = at + 4;
            let mut rest = &lower[at + 4..];
            rest = rest.strip_prefix([' ', '-']).unwrap_or(rest);
            for word in [">= ", ">=", "before ", "v"] {
                if let Some(after) = rest.strip_prefix(word) {
                    rest = after;
                    break;
                }
            }
            if let TmuxVersion::Release(major, minor) = TmuxVersion::parse(rest)
                && rest.starts_with(|c: char| c.is_ascii_digit())
                && (major, minor) < TMUX_MINIMUM
            {
                let end = (at + 40).min(text.len());
                let end = (end..=text.len())
                    .find(|&i| text.is_char_boundary(i))
                    .unwrap();
                found.push(text[at..end].to_owned());
            }
        }
        found
    }

    fn walk(directory: &Path, files: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, files);
            } else {
                files.push(path);
            }
        }
    }

    #[test]
    fn nothing_names_a_tmux_older_than_the_floor() {
        // Samples built from the floor, so they stay true after a bump.
        let (major, minor) = TMUX_MINIMUM;
        assert!(minor > 0, "the samples cannot express a floor of {major}.0");
        let below = format!("{major}.{}", minor - 1);
        let older = format!("{}.0", major - 1);
        assert_eq!(
            old_mentions(&format!("needs tmux\n/// {below}a here")),
            Vec::<String>::new()
        );
        assert_eq!(
            old_mentions(&joined(&format!("needs tmux\n/// {below}a here"))).len(),
            1
        );
        assert_eq!(
            old_mentions(&format!("tmux before {older} and tmux >= {below}")).len(),
            2
        );
        assert_eq!(
            old_mentions(&format!(
                "tmux {major}.{minor}a, tmux {major}.{}, tmux v{major}.{}",
                minor + 7,
                minor + 1
            ))
            .len(),
            0
        );
        let mut files = vec![root().join("README.md")];
        for directory in [
            "docs", "man", "examples", "src", "test", "tests", "lib", "share",
        ] {
            walk(&root().join(directory), &mut files);
        }
        let mut offenders = Vec::new();
        for path in files {
            let Ok(text) = std::fs::read_to_string(&path) else {
                continue;
            };
            let relative = path
                .strip_prefix(root())
                .unwrap()
                .to_string_lossy()
                .into_owned();
            // This file's own tests spell out what they reject.
            if relative == "src/tmux_version.rs" {
                continue;
            }
            for mention in old_mentions(&joined(&text)) {
                let allowed = LINT_ALLOWED
                    .iter()
                    .any(|&(file, words)| file == relative && mention.contains(words));
                if !allowed {
                    offenders.push(format!("{relative}: {mention}"));
                }
            }
        }
        assert!(offenders.is_empty(), "{offenders:#?}");
    }
}
