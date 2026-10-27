//! Thin wrapper over `xcrun simctl`, plus the host facts simfast pins.

use crate::util::{output, wait_timeout};
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

const CORESIMULATOR_PLIST: &str =
    "/Library/Developer/PrivateFrameworks/CoreSimulator.framework/Versions/A/Resources/Info.plist";

pub fn default_set() -> PathBuf {
    crate::util::home().join("Library/Developer/CoreSimulator/Devices")
}

/// simctl's path, resolved once: going through `xcrun` costs ~100ms a call.
pub fn simctl_path() -> &'static str {
    static P: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    P.get_or_init(|| output("xcrun", &["--find", "simctl"]).map(|s| s.trim().to_string()).unwrap_or_else(|_| "simctl".into()))
}

fn args<'a>(set: Option<&'a Path>, rest: &[&'a str]) -> Vec<&'a str> {
    let mut v = vec![];
    if let Some(s) = set {
        v.push("--set");
        v.push(s.to_str().unwrap());
    }
    v.extend_from_slice(rest);
    v
}

pub fn run(set: Option<&Path>, rest: &[&str]) -> Result<String> {
    Ok(output(simctl_path(), &args(set, rest))?.trim().to_string())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Device {
    pub udid: String,
    pub name: String,
    pub state: String,
}

pub fn devices(set: Option<&Path>) -> Result<Vec<Device>> {
    #[derive(Deserialize)]
    struct List {
        devices: HashMap<String, Vec<Device>>,
    }
    let list: List = serde_json::from_str(&run(set, &["list", "-j", "devices"])?)?;
    Ok(list.devices.into_values().flatten().collect())
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DeviceType {
    pub identifier: String,
    pub name: String,
    #[serde(default)]
    pub model_identifier: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Runtime {
    pub identifier: String,
    pub name: String,
    pub version: String,
    #[serde(rename = "buildversion", default)]
    pub build: String,
    pub is_available: bool,
    #[serde(default)]
    pub supported_device_types: Vec<DeviceType>,
}

impl Runtime {
    /// "iOS", "tvOS", "watchOS", "visionOS"
    pub fn platform(&self) -> &str {
        self.name.split_whitespace().next().unwrap_or("")
    }
    pub fn version_parts(&self) -> Vec<u32> {
        self.version.split('.').filter_map(|p| p.parse().ok()).collect()
    }
    pub fn supports(&self, type_id: &str) -> bool {
        self.supported_device_types.iter().any(|t| t.identifier == type_id)
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Catalog {
    pub devicetypes: Vec<DeviceType>,
    pub runtimes: Vec<Runtime>,
}

pub fn catalog() -> Result<Catalog> {
    Ok(serde_json::from_str(&run(None, &["list", "-j", "devicetypes", "runtimes"])?)?)
}

/// The system-wide CoreSimulator framework every Xcode on this Mac shares.
pub fn coresimulator_version() -> Result<String> {
    Ok(output("plutil", &["-extract", "CFBundleVersion", "raw", CORESIMULATOR_PLIST])
        .context("read CoreSimulator version")?
        .trim()
        .to_string())
}

pub fn boot(set: Option<&Path>, udid: &str) -> Result<()> {
    run(set, &["boot", udid]).map(|_| ())
}

/// Block until the device has finished booting.
pub fn wait_booted(set: Option<&Path>, udid: &str, timeout: Duration) -> Result<()> {
    let child = Command::new(simctl_path())
        .args(args(set, &["bootstatus", udid, "-b"]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn simctl bootstatus")?;
    wait_timeout(child, timeout, &format!("boot of {udid}"))
}

pub fn shutdown(set: Option<&Path>, udid: &str) {
    let _ = run(set, &["shutdown", udid]);
}

pub fn delete(set: Option<&Path>, udid: &str) -> Result<()> {
    shutdown(set, udid);
    run(set, &["delete", udid]).map(|_| ())
}

pub fn rename(udid: &str, name: &str) -> Result<()> {
    run(None, &["rename", udid, name]).map(|_| ())
}

/// Labels launchd reports as disabled in the booted device's system domain.
pub fn live_disabled(udid: &str) -> Result<Vec<String>> {
    let out = run(None, &["spawn", udid, "launchctl", "print-disabled", "system"])?;
    Ok(out
        .lines()
        .filter(|l| l.trim_end().ends_with("=> disabled") || l.trim_end().ends_with("=> true"))
        .filter_map(|l| l.split('"').nth(1).map(str::to_string))
        .collect())
}

pub fn ensure_xcode() -> Result<()> {
    if output("xcrun", &["--find", "simctl"]).is_err() {
        bail!("xcrun simctl not found: simfast needs macOS with Xcode installed");
    }
    Ok(())
}
