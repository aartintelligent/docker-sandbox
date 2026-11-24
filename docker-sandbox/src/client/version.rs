//! Daemon API versioning.
//!
//! The `sandboxd` API is private and only versioned through the `api_version`
//! field of `GET /daemon/health`. Each release of this crate is validated
//! against a range of API versions, [`SUPPORTED_API`], widened only after an
//! audit of the new `sbx` release; [`VersionPolicy`] says what
//! [`Client::connect`](crate::Client::connect) does outside of it.

use std::cmp::Ordering;
use std::fmt;
use std::str::FromStr;

use crate::error::{Error, Result};

/// A `major.minor.patch` API version.
///
/// ```
/// use docker_sandbox::ApiVersion;
///
/// let v: ApiVersion = "0.52.0".parse()?;
/// assert_eq!(v, ApiVersion::new(0, 52, 0));
/// assert!(v < ApiVersion::new(0, 53, 0));
/// # Ok::<(), docker_sandbox::InvalidApiVersion>(())
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ApiVersion {
    /// Major component.
    pub major: u32,
    /// Minor component.
    pub minor: u32,
    /// Patch component.
    pub patch: u32,
}

impl ApiVersion {
    /// Builds a version from its components.
    #[must_use]
    pub const fn new(major: u32, minor: u32, patch: u32) -> Self {
        Self {
            major,
            minor,
            patch,
        }
    }
}

impl Ord for ApiVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch).cmp(&(other.major, other.minor, other.patch))
    }
}

impl PartialOrd for ApiVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for ApiVersion {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// Error returned when parsing an invalid [`ApiVersion`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("invalid API version {0:?}: expected major.minor.patch")]
pub struct InvalidApiVersion(String);

impl FromStr for ApiVersion {
    type Err = InvalidApiVersion;

    /// Parses `major.minor.patch`, tolerating a leading `v` and a pre-release
    /// or build suffix (`v0.52.0-rc1` parses as `0.52.0`).
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let core = s.trim().trim_start_matches('v');
        let core = core.split(['-', '+']).next().unwrap_or_default();
        let mut parts = core.split('.').map(str::parse::<u32>);
        match (parts.next(), parts.next(), parts.next(), parts.next()) {
            (Some(Ok(major)), Some(Ok(minor)), Some(Ok(patch)), None) => {
                Ok(Self::new(major, minor, patch))
            }
            _ => Err(InvalidApiVersion(s.to_owned())),
        }
    }
}

/// A half-open range of API versions: `min <= v < max_exclusive`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VersionRange {
    /// Lowest supported version.
    pub min: ApiVersion,
    /// First unsupported version above the range.
    pub max_exclusive: ApiVersion,
}

impl VersionRange {
    /// Whether `version` falls within the range.
    #[must_use]
    pub fn contains(&self, version: ApiVersion) -> bool {
        self.min <= version && version < self.max_exclusive
    }
}

impl fmt::Display for VersionRange {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, ">={}, <{}", self.min, self.max_exclusive)
    }
}

/// API versions this release has been validated against.
pub const SUPPORTED_API: VersionRange = VersionRange {
    min: ApiVersion::new(0, 52, 0),
    max_exclusive: ApiVersion::new(0, 53, 0),
};

/// What to do when the daemon reports a version outside [`SUPPORTED_API`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[non_exhaustive]
pub enum VersionPolicy {
    /// Refuse to connect.
    #[default]
    Strict,
    /// Connect anyway and log a warning.
    Warn,
    /// Do not check the version at all.
    Ignore,
}

/// Applies `policy` to the API version `reported` by the daemon.
pub(crate) fn check(reported: &str, policy: VersionPolicy) -> Result<()> {
    if policy == VersionPolicy::Ignore {
        return Ok(());
    }
    let supported = reported
        .parse::<ApiVersion>()
        .is_ok_and(|v| SUPPORTED_API.contains(v));
    if supported {
        return Ok(());
    }
    if policy == VersionPolicy::Strict {
        return Err(Error::UnsupportedVersion {
            found: reported.to_owned(),
            supported: SUPPORTED_API.to_string(),
        });
    }
    tracing::warn!(api_version = reported, supported = %SUPPORTED_API, "unsupported sandboxd API version");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_parse_leniently() {
        assert_eq!(
            "v0.52.0-rc1".parse::<ApiVersion>(),
            Ok(ApiVersion::new(0, 52, 0))
        );
        assert!("0.52".parse::<ApiVersion>().is_err());
        assert!("0.52.0.1".parse::<ApiVersion>().is_err());
        assert_eq!(SUPPORTED_API.to_string(), ">=0.52.0, <0.53.0");
    }

    #[test]
    fn the_policy_decides_outside_the_supported_range() {
        assert!(check("0.52.0", VersionPolicy::Strict).is_ok());
        assert!(check("0.52.7", VersionPolicy::Strict).is_ok());
        assert!(matches!(
            check("0.53.0", VersionPolicy::Strict),
            Err(Error::UnsupportedVersion { .. })
        ));
        assert!(matches!(
            check("garbage", VersionPolicy::Strict),
            Err(Error::UnsupportedVersion { .. })
        ));
        assert!(check("0.53.0", VersionPolicy::Warn).is_ok());
        assert!(check("garbage", VersionPolicy::Ignore).is_ok());
    }
}
