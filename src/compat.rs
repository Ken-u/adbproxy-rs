//! Hub ↔ proxy version compatibility.

use std::fmt;

/// Oldest `adb-proxy` release this hub is willing to talk to.
///
/// Bump when the hub depends on a proxy protocol/behavior change
/// (for example `proxy:version`, auth changes, device filtering).
pub const MIN_PROXY_VERSION: &str = "0.4.4";

/// Smart-socket service name answered by `adb-proxy` after auth.
pub const PROXY_VERSION_SERVICE: &str = "proxy:version";

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct SemVer {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl SemVer {
    pub fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        // Allow optional leading `v` and ignore pre-release/build metadata.
        let s = s.strip_prefix('v').unwrap_or(s);
        let core = s.split_once('-').map(|(a, _)| a).unwrap_or(s);
        let core = core.split_once('+').map(|(a, _)| a).unwrap_or(core);
        let mut parts = core.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        let patch = parts.next().unwrap_or("0").parse().ok()?;
        if parts.next().is_some() {
            return None;
        }
        Some(Self {
            major,
            minor,
            patch,
        })
    }
}

impl fmt::Display for SemVer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ProxyCompat {
    Ok { version: String },
    TooOld { version: String },
    /// `proxy:version` unsupported / unreachable (treat as too old).
    Unknown { reason: String },
}

impl ProxyCompat {
    pub fn is_ok(&self) -> bool {
        matches!(self, Self::Ok { .. })
    }

    pub fn user_message(&self, backend: &str, addr: impl fmt::Display) -> String {
        match self {
            Self::Ok { version } => {
                format!("backend '{backend}' ({addr}) adb-proxy {version} OK")
            }
            Self::TooOld { version } => {
                format!(
                    "backend '{backend}' ({addr}) runs adb-proxy {version}, but this hub requires \
                     adb-proxy >= {MIN_PROXY_VERSION}. Please upgrade adb-proxy on that host \
                     (and keep the same pair code)."
                )
            }
            Self::Unknown { reason } => {
                format!(
                    "backend '{backend}' ({addr}) did not report a version ({reason}). \
                     This hub requires adb-proxy >= {MIN_PROXY_VERSION}. Please upgrade \
                     adb-proxy on that host (older builds have no proxy:version)."
                )
            }
        }
    }
}

pub fn evaluate_proxy_version(reported: &str) -> ProxyCompat {
    let Some(got) = SemVer::parse(reported) else {
        return ProxyCompat::Unknown {
            reason: format!("invalid version string '{reported}'"),
        };
    };
    let Some(min) = SemVer::parse(MIN_PROXY_VERSION) else {
        return ProxyCompat::Ok {
            version: reported.trim().to_string(),
        };
    };
    if got >= min {
        ProxyCompat::Ok {
            version: reported.trim().to_string(),
        }
    } else {
        ProxyCompat::TooOld {
            version: reported.trim().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_semver() {
        assert_eq!(
            SemVer::parse("0.4.4").unwrap(),
            SemVer {
                major: 0,
                minor: 4,
                patch: 4
            }
        );
        assert_eq!(SemVer::parse("v1.2").unwrap().patch, 0);
        assert!(SemVer::parse("0.4.3-beta").unwrap() < SemVer::parse("0.4.4").unwrap());
    }

    #[test]
    fn evaluate_ok_and_too_old() {
        assert!(evaluate_proxy_version("0.4.4").is_ok());
        assert!(evaluate_proxy_version("1.0.0").is_ok());
        assert!(matches!(
            evaluate_proxy_version("0.4.3"),
            ProxyCompat::TooOld { .. }
        ));
        assert!(matches!(
            evaluate_proxy_version("nope"),
            ProxyCompat::Unknown { .. }
        ));
    }
}
