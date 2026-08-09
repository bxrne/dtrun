//! Load and validate OCI `config.json` documents.

use serde::{Deserialize, Deserializer, Serialize};
use std::collections::HashMap;
use std::path::Path;

/// Errors produced while loading or validating an OCI config.
#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(serde_json::Error),
    Validate(String),
}

impl std::fmt::Display for ConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "failed to read config: {e}"),
            ConfigError::Parse(e) => write!(f, "failed to parse config.json: {e}"),
            ConfigError::Validate(e) => write!(f, "invalid config.json: {e}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            ConfigError::Io(e) => Some(e),
            ConfigError::Parse(e) => Some(e),
            ConfigError::Validate(_) => None,
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(value: std::io::Error) -> Self {
        ConfigError::Io(value)
    }
}

impl From<serde_json::Error> for ConfigError {
    fn from(value: serde_json::Error) -> Self {
        ConfigError::Parse(value)
    }
}

/// Linux namespace entry.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinuxNamespace {
    pub r#type: String,
    pub path: Option<String>,
}

/// Network device mapping under `linux.netDevices`.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinuxNetDevice {
    /// Name of the device inside the container (must be a string when present).
    pub name: Option<String>,
}

/// RDMA resource limits for a single device.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LinuxRdma {
    pub hca_handles: Option<u32>,
    pub hca_objects: Option<u32>,
}

/// Hugepage limit entry. `pageSize` must match the OCI pattern `^[1-9][0-9]*[KMG]i?B$`.
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct LinuxHugepageLimit {
    pub limit: u64,
    #[serde(deserialize_with = "deserialize_hugepage_size")]
    pub page_size: String,
}

fn deserialize_hugepage_size<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    if is_valid_hugepage_size(&s) {
        Ok(s)
    } else {
        Err(serde::de::Error::custom(format!(
            "invalid hugepage pageSize '{s}': must match ^[1-9][0-9]*[KMG]i?B$"
        )))
    }
}

/// OCI hugepage `pageSize` grammar: one-or-more digits (no leading zero), unit K/M/G, optional `i`, then `B`.
fn is_valid_hugepage_size(s: &str) -> bool {
    let bytes = s.as_bytes();
    if bytes.len() < 2 {
        return false;
    }

    // Find where the numeric prefix ends.
    let mut i = 0;
    if !bytes[0].is_ascii_digit() || bytes[0] == b'0' {
        // Must start with [1-9]
        return false;
    }
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    if i == 0 {
        return false;
    }

    // Unit: [KMG] i? B
    let rest = &s[i..];
    matches!(rest, "KB" | "MB" | "GB" | "KiB" | "MiB" | "GiB")
}

/// Linux resource constraints. Fields beyond those strictly typed are accepted via flatten.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinuxResources {
    pub rdma: Option<HashMap<String, LinuxRdma>>,
    pub hugepage_limits: Option<Vec<LinuxHugepageLimit>>,
    /// Remaining resource fields (cpu, memory, devices, …) kept as raw JSON.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Platform-specific Linux configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct LinuxConfig {
    pub namespaces: Option<Vec<LinuxNamespace>>,
    pub resources: Option<LinuxResources>,
    pub net_devices: Option<HashMap<String, LinuxNetDevice>>,
    /// Remaining linux fields kept as raw JSON so full runtime-spec examples still parse.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Windows platform configuration (loosely typed for now).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct WindowsConfig {
    pub ignore_flushes_during_boot: Option<bool>,
    pub hyperv: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// FreeBSD jail network/hostname isolation mode (`new` or `inherit`).
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FreeBSDNamespaceMode {
    New,
    Inherit,
}

/// FreeBSD jail settings.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct FreeBSDJail {
    pub host: Option<FreeBSDNamespaceMode>,
    pub vnet: Option<FreeBSDNamespaceMode>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// FreeBSD platform configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct FreeBSDConfig {
    pub jail: Option<FreeBSDJail>,
    pub devices: Option<Vec<serde_json::Value>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// z/OS namespace entry.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ZosNamespace {
    pub r#type: String,
    pub path: Option<String>,
}

/// z/OS platform configuration.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ZosConfig {
    pub namespaces: Option<Vec<ZosNamespace>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// Root filesystem configuration.
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RootFs {
    pub path: String,
    pub readonly: Option<bool>,
}

/// Process user credentials.
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct ProcessUser {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub additional_gids: Option<Vec<u32>>,
}

/// The container process configuration (`process`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Process {
    pub args: Option<Vec<String>>,
    pub env: Option<Vec<String>>,
    pub cwd: Option<String>,
    pub user: Option<ProcessUser>,
    pub terminal: Option<bool>,
    /// Remaining process fields kept as raw JSON so full runtime-spec examples still parse.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// A mount point requested by the config (`mounts`).
#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Mount {
    pub destination: String,
    #[serde(rename = "type")]
    pub fstype: Option<String>,
    pub source: Option<String>,
    pub options: Option<Vec<String>>,
}

/// The main OCI runtime configuration structure (`config.json`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OciConfig {
    pub oci_version: String,
    pub root: RootFs,
    pub process: Option<Process>,
    pub hostname: Option<String>,
    pub mounts: Option<Vec<Mount>>,
    pub linux: Option<LinuxConfig>,
    pub windows: Option<WindowsConfig>,
    pub freebsd: Option<FreeBSDConfig>,
    pub zos: Option<ZosConfig>,
    /// Remaining top-level fields (mounts, hooks, annotations, …).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}
impl std::str::FromStr for OciConfig {
    /// Parse and validate an OCI config from a JSON string.
    fn from_str(json: &str) -> Result<Self, ConfigError> {
        let config: OciConfig = serde_json::from_str(json)?;
        config.validate()?;
        Ok(config)
    }

    type Err = ConfigError;
}

impl OciConfig {
    /// Read, parse, and validate `config.json` from a filesystem path.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        let config: OciConfig = serde_json::from_str(&content)?;
        config.validate()?;
        Ok(config)
    }

    /// Extra semantic checks beyond what serde types already enforce.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_version()?;
        self.validate_root()?;
        self.validate_process()?;
        self.validate_hostname()?;
        self.validate_mounts()?;
        Ok(())
    }

    fn validate_version(&self) -> Result<(), ConfigError> {
        if self.oci_version.is_empty() {
            return Err(ConfigError::Validate("ociVersion must not be empty".into()));
        }
        let (major, minor, patch) = parse_semver(&self.oci_version).ok_or_else(|| {
            ConfigError::Validate(format!(
                "ociVersion '{}' is not valid semver (expected MAJOR.MINOR.PATCH[-pre][+build])",
                self.oci_version
            ))
        })?;
        if !compatible_version(major, minor, patch) {
            return Err(ConfigError::Validate(format!(
                "ociVersion '{}' is not supported by dtrun",
                self.oci_version
            )));
        }
        Ok(())
    }

    fn validate_root(&self) -> Result<(), ConfigError> {
        if self.root.path.is_empty() {
            return Err(ConfigError::Validate("root.path must not be empty".into()));
        }
        Ok(())
    }

    fn validate_process(&self) -> Result<(), ConfigError> {
        let Some(process) = &self.process else {
            return Ok(());
        };
        if let Some(args) = &process.args {
            if args.is_empty() {
                return Err(ConfigError::Validate(
                    "process.args must not be empty".into(),
                ));
            }
            if args.iter().any(|a| a.is_empty()) {
                return Err(ConfigError::Validate(
                    "process.args entries must not be empty".into(),
                ));
            }
        }
        if let Some(cwd) = &process.cwd
            && !cwd.starts_with('/')
        {
            return Err(ConfigError::Validate(format!(
                "process.cwd '{}' must be an absolute path",
                cwd
            )));
        }
        if let Some(env) = &process.env {
            for kv in env {
                if kv.split_once('=').is_none() {
                    return Err(ConfigError::Validate(format!(
                        "process.env entry '{kv}' must be KEY=VALUE"
                    )));
                }
            }
        }
        Ok(())
    }

    fn validate_hostname(&self) -> Result<(), ConfigError> {
        let Some(hostname) = &self.hostname else {
            return Ok(());
        };
        if hostname.is_empty() || hostname.len() > 64 {
            return Err(ConfigError::Validate(format!(
                "hostname '{hostname}' must be 1..=64 characters"
            )));
        }
        if hostname
            .bytes()
            .any(|b| !(b.is_ascii_alphanumeric() || b == b'-' || b == b'.'))
        {
            return Err(ConfigError::Validate(format!(
                "hostname '{hostname}' contains invalid characters"
            )));
        }
        Ok(())
    }

    fn validate_mounts(&self) -> Result<(), ConfigError> {
        let Some(mounts) = &self.mounts else {
            return Ok(());
        };
        let mut seen = std::collections::HashSet::new();
        for m in mounts {
            if m.destination.is_empty() {
                return Err(ConfigError::Validate(
                    "mount.destination must not be empty".into(),
                ));
            }
            if !m.destination.starts_with('/') {
                return Err(ConfigError::Validate(format!(
                    "mount.destination '{}' must be an absolute path",
                    m.destination
                )));
            }
            if !seen.insert(&m.destination) {
                return Err(ConfigError::Validate(format!(
                    "duplicate mount.destination '{}'",
                    m.destination
                )));
            }
        }
        Ok(())
    }
}

/// Versions of the OCI runtime-spec that dtrun understands.
/// Compatible with the 1.x line; accepts the legacy 0.5.0 draft too.
fn compatible_version(major: u64, minor: u64, _patch: u64) -> bool {
    matches!((major, minor), (1, _) | (0, 5))
}

/// Minimal semver parser: `MAJOR.MINOR.PATCH` with optional `-prerelease` and
/// `+build` suffixes, exactly as the OCI spec requires for `ociVersion`.
fn parse_semver(s: &str) -> Option<(u64, u64, u64)> {
    let core = s.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some((major, minor, patch))
}

#[cfg(test)]
mod hugepage_tests {
    use super::is_valid_hugepage_size;

    #[test]
    fn accepts_spec_page_sizes() {
        for s in ["2MB", "64KB", "1GiB", "2MiB", "1GB"] {
            assert!(is_valid_hugepage_size(s), "expected valid: {s}");
        }
    }

    #[test]
    fn rejects_invalid_page_sizes() {
        for s in ["64kB", "2mb", "0MB", "MB", "2", "2B", ""] {
            assert!(!is_valid_hugepage_size(s), "expected invalid: {s}");
        }
    }
}

#[cfg(test)]
mod validate_tests {
    use super::*;
    use std::collections::HashMap;

    fn base_config(oci_version: &str) -> OciConfig {
        OciConfig {
            oci_version: oci_version.to_owned(),
            root: RootFs {
                path: "rootfs".to_owned(),
                readonly: None,
            },
            process: Some(Process {
                args: Some(vec!["sh".to_owned()]),
                env: None,
                cwd: Some("/".to_owned()),
                user: None,
                terminal: None,
                extra: HashMap::new(),
            }),
            hostname: None,
            mounts: None,
            linux: None,
            windows: None,
            freebsd: None,
            zos: None,
            extra: HashMap::new(),
        }
    }

    #[test]
    fn accepts_supported_versions() {
        for v in ["1.0.0", "1.3.0", "1.2.3-dev", "1.0.0+build", "0.5.0-dev"] {
            assert!(base_config(v).validate().is_ok(), "expected ok: {v}");
        }
    }

    #[test]
    fn rejects_unsupported_versions() {
        for v in ["", "2.0.0", "0.1.0", "banana", "1", "1.2", "1.2.3.4"] {
            assert!(base_config(v).validate().is_err(), "expected err: {v}");
        }
    }

    #[test]
    fn rejects_empty_root_path() {
        let mut c = base_config("1.0.0");
        c.root.path = String::new();
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_empty_args() {
        let mut c = base_config("1.0.0");
        c.process.as_mut().unwrap().args = Some(vec![]);
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_relative_cwd() {
        let mut c = base_config("1.0.0");
        c.process.as_mut().unwrap().cwd = Some("usr".to_owned());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_malformed_env() {
        let mut c = base_config("1.0.0");
        c.process.as_mut().unwrap().env = Some(vec!["PATH".to_owned()]);
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_bad_hostname() {
        let mut c = base_config("1.0.0");
        c.hostname = Some("bad_host/name!".to_owned());
        assert!(c.validate().is_err());
        c.hostname = Some("".to_owned());
        assert!(c.validate().is_err());
    }

    #[test]
    fn rejects_duplicate_mounts() {
        let mut c = base_config("1.0.0");
        c.mounts = Some(vec![
            Mount {
                destination: "/proc".to_owned(),
                fstype: Some("proc".to_owned()),
                source: Some("proc".to_owned()),
                options: None,
            },
            Mount {
                destination: "/proc".to_owned(),
                fstype: Some("proc".to_owned()),
                source: Some("proc".to_owned()),
                options: None,
            },
        ]);
        assert!(c.validate().is_err());
    }
}
