//! Self-update support: ask crates.io for the latest release and, when it is
//! newer than the running binary, reinstall through `cargo install`.
//!
//! Network access and the `cargo` toolchain are both optional dependencies of
//! this feature, so every failure is reported as a message rather than a panic.

use std::{
    cmp::Ordering,
    process::{Command, Stdio},
};

/// Metadata endpoint for the `dsopt` crate on crates.io.
pub const CRATE_API: &str = "https://crates.io/api/v1/crates/dsopt";

/// What `dsopt --update` did.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UpdateOutcome {
    /// The running version is already the newest published release.
    AlreadyLatest { current: String, latest: String },
    /// A newer release was found and installed.
    Updated { from: String, to: String },
}

/// Parse a `major.minor.patch` version, ignoring any pre-release suffix.
pub fn parse_version(text: &str) -> Option<(u64, u64, u64)> {
    let core = text.trim().trim_start_matches('v');
    let core = core.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Compare two semantic versions, treating unparseable input as equal.
pub fn compare_versions(left: &str, right: &str) -> Ordering {
    match (parse_version(left), parse_version(right)) {
        (Some(left), Some(right)) => left.cmp(&right),
        _ => Ordering::Equal,
    }
}

/// Whether `candidate` is strictly newer than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    compare_versions(candidate, current) == Ordering::Greater
}

/// The running binary's version, as recorded by Cargo.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Look up the newest published version of `dsopt` on crates.io.
///
/// crates.io answers 403 to requests without a `User-Agent`, so one is always
/// sent; curl's own default is not accepted.
pub fn latest_version() -> Result<String, String> {
    let user_agent = format!("dsopt/{}", current_version());
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--max-time",
            "20",
            "--user-agent",
            &user_agent,
            CRATE_API,
        ])
        .output()
        .map_err(|error| format!("could not run curl: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "crates.io lookup failed ({}); check your network connection",
            output.status
        ));
    }
    let payload: serde_json::Value = serde_json::from_slice(&output.stdout)
        .map_err(|error| format!("unexpected crates.io response: {error}"))?;
    payload["crate"]["max_stable_version"]
        .as_str()
        .or_else(|| payload["crate"]["max_version"].as_str())
        .map(str::to_owned)
        .ok_or_else(|| "crates.io response did not include a version".to_owned())
}

/// Reinstall `dsopt` from crates.io, upgrading it in place.
pub fn install_latest() -> Result<(), String> {
    let status = Command::new("cargo")
        .args(["install", "dsopt", "--force"])
        .stdin(Stdio::null())
        .status()
        .map_err(|error| {
            format!("could not run cargo: {error}. Install Rust from https://rustup.rs")
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("cargo install exited with {status}"))
    }
}

/// Entry point for `dsopt --update`: check, then install when needed.
pub fn run(force: bool) -> Result<UpdateOutcome, String> {
    let current = current_version().to_owned();
    let latest = latest_version()?;
    if !force && !is_newer(&latest, &current) {
        return Ok(UpdateOutcome::AlreadyLatest { current, latest });
    }
    install_latest()?;
    Ok(UpdateOutcome::Updated {
        from: current,
        to: latest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_release_and_pre_release_versions() {
        assert_eq!(parse_version("0.1.0"), Some((0, 1, 0)));
        assert_eq!(parse_version("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_version("1.2.3-rc.1"), Some((1, 2, 3)));
        assert_eq!(parse_version("2"), Some((2, 0, 0)));
        assert_eq!(parse_version("nonsense"), None);
    }

    #[test]
    fn detects_newer_versions() {
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.0.9", "0.1.0"));
    }

    #[test]
    fn compares_unparseable_versions_as_equal() {
        assert_eq!(compare_versions("garbage", "0.1.0"), Ordering::Equal);
    }
}
