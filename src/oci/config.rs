// Load config.json and parse it

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinuxConfig {
    pub namespaces: Vec<LinuxNamespace>,
    pub resources: Option<serde_json::Value>, // Replace with strict types as needed
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct LinuxNamespace {
    pub r#type: String,
    pub path: Option<String>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct WindowsConfig {
    #[serde(rename = "ignoreFlushesDuringBoot")]
    pub ignore_flushes_during_boot: bool,
    pub hyperv: Option<serde_json::Value>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct FreeBSDConfig {
    pub jail: serde_json::Value,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(untagged)] // This allows for flexible deserialization based on the presence of keys
pub enum PlatformSpec {
    // Serde matches these keys by their field names inside the sub-structs
    Linux { linux: LinuxConfig },
    Windows { windows: WindowsConfig },
    FreeBSD { freebsd: FreeBSDConfig },

    // Fallback for other platforms or bare containers
    None,
}

// The main OCI configuration structure
#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct OciConfig {
    #[serde(rename = "ociVersion")]
    pub oci_version: String,

    pub root: RootFs,
    pub process: serde_json::Value, // Generic JSON placeholder for brevity
    pub hostname: Option<String>,

    // This flattens the enum keys directly into the root object
    #[serde(flatten)]
    pub platform: PlatformSpec,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct RootFs {
    pub path: String,
    pub readonly: Option<bool>,
}
