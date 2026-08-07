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

/// Linux resource constraints. Fields beyond those we type strictly are accepted via flatten.
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

/// The main OCI runtime configuration structure (`config.json`).
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct OciConfig {
    pub oci_version: String,
    pub root: RootFs,
    pub process: Option<serde_json::Value>,
    pub hostname: Option<String>,
    pub linux: Option<LinuxConfig>,
    pub windows: Option<WindowsConfig>,
    pub freebsd: Option<FreeBSDConfig>,
    pub zos: Option<ZosConfig>,
    /// Remaining top-level fields (mounts, hooks, annotations, …).
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

impl OciConfig {
    /// Parse and validate an OCI config from a JSON string.
    pub fn from_str(json: &str) -> Result<Self, ConfigError> {
        let config: OciConfig = serde_json::from_str(json)?;
        config.validate()?;
        Ok(config)
    }

    /// Read, parse, and validate `config.json` from a filesystem path.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self, ConfigError> {
        let content = std::fs::read_to_string(path)?;
        Self::from_str(&content)
    }

    /// Extra semantic checks beyond what serde types already enforce.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.oci_version.is_empty() {
            return Err(ConfigError::Validate("ociVersion must not be empty".into()));
        }
        if self.root.path.is_empty() {
            return Err(ConfigError::Validate("root.path must not be empty".into()));
        }
        Ok(())
    }
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
