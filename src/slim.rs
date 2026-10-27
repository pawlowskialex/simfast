//! Service slimming, ported from simslim (https://github.com/MobAI-App/simslim,
//! MIT, Copyright (c) 2026 Interlap).
//!
//! A booted iOS simulator runs ~180 launchd daemons (Siri, Spotlight, photo
//! analysis, widgets, iCloud...). Disabling the ones apps rarely need roughly
//! halves a simulator's memory and removes most background noise. launchd_sim
//! keeps its disable overrides on the host, in
//! /private/var/tmp/com.apple.CoreSimulator.SimDevice.<UDID>/disabled.plist, and
//! reads the file at boot. `simctl clone` does not copy it, so simfast writes it
//! for every clone before its first boot: slimming costs one file write.

use crate::simctl::Runtime;
use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

#[derive(Debug, Deserialize)]
pub struct Category {
    pub id: String,
    pub downside: String,
    pub approx_mb: u32,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Feature {
    pub id: String,
    pub name: String,
    pub labels: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct Catalog {
    pub source: String,
    pub categories: Vec<Category>,
    pub features: Vec<Feature>,
}

pub fn catalog() -> &'static Catalog {
    static C: OnceLock<Catalog> = OnceLock::new();
    C.get_or_init(|| serde_json::from_str(include_str!("catalog.json")).expect("embedded catalog"))
}

/// The exact labels to disable: every catalog daemon, minus what `keep` names.
/// `keep` entries are category ids, feature ids, or individual launchd labels.
pub fn disabled_set(keep: &[String], runtime: &Runtime) -> Result<BTreeSet<String>> {
    if !supports_persistent(runtime) {
        bail!("{} can't keep launchd overrides across boots (needs iOS/tvOS 18.5, watchOS 11.5, visionOS 2.5+); set slim = false", runtime.name);
    }
    let cat = catalog();
    let mut set: BTreeSet<String> = cat.categories.iter().flat_map(|c| c.labels.iter().cloned()).collect();
    for k in keep {
        // Categories overlap, so keeping one re-enables its labels everywhere.
        let labels: Vec<&String> = if let Some(c) = cat.categories.iter().find(|c| &c.id == k) {
            c.labels.iter().collect()
        } else if let Some(f) = cat.features.iter().find(|f| &f.id == k) {
            f.labels.iter().collect()
        } else if k.starts_with("com.apple.") {
            vec![k]
        } else {
            bail!("unknown keep entry {k:?}: use a category, a feature or a launchd label (see `simfast services`)");
        };
        for l in labels {
            set.remove(l);
        }
    }
    // Runtime quirks found by simslim: these re-enable themselves or break boot.
    match runtime.platform() {
        "watchOS" => {
            set.remove("com.apple.homed");
        }
        "visionOS" => {
            set.remove("com.apple.mobileassetd");
        }
        _ => {}
    }
    Ok(set)
}

/// Persistent overrides need the iOS 18.5 generation of runtimes; older ones
/// accept the store and silently come back stock.
fn supports_persistent(rt: &Runtime) -> bool {
    let min = match rt.platform() {
        "iOS" | "tvOS" => (18, 5),
        "watchOS" => (11, 5),
        "visionOS" => (2, 5),
        _ => return false,
    };
    let v = rt.version_parts();
    (v.first().copied().unwrap_or(0), v.get(1).copied().unwrap_or(0)) >= min
}

// ------------------------------------------------------------- disabled store

pub fn store_dir(udid: &str) -> PathBuf {
    PathBuf::from(format!("/private/var/tmp/com.apple.CoreSimulator.SimDevice.{udid}"))
}

fn store_path(udid: &str) -> PathBuf {
    store_dir(udid).join("disabled.plist")
}

/// label -> disabled. A missing file means nothing disabled.
fn read_store(udid: &str) -> BTreeMap<String, bool> {
    let Ok(text) = fs::read_to_string(store_path(udid)) else {
        return BTreeMap::new();
    };
    // The store is a flat <key>label</key><true/>|<false/> dict.
    let mut out = BTreeMap::new();
    let mut rest = text.as_str();
    while let Some(i) = rest.find("<key>") {
        rest = &rest[i + 5..];
        let Some(j) = rest.find("</key>") else { break };
        let label = unescape(&rest[..j]);
        rest = rest[j + 6..].trim_start();
        if rest.starts_with("<true/>") {
            out.insert(label, true);
        } else if rest.starts_with("<false/>") {
            out.insert(label, false);
        }
    }
    out
}

fn write_store(udid: &str, entries: &BTreeMap<String, bool>) -> Result<()> {
    let dir = store_dir(udid);
    fs::create_dir_all(&dir)?;
    let mut s = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<plist version=\"1.0\">\n<dict>\n",
    );
    for (label, off) in entries {
        s += &format!("\t<key>{}</key>\n\t<{}/>\n", escape(label), if *off { "true" } else { "false" });
    }
    s += "</dict>\n</plist>\n";
    // Atomic, so launchd_sim never reads a half-written store.
    let tmp = dir.join("disabled.plist.simfast-tmp");
    fs::write(&tmp, s)?;
    fs::rename(&tmp, store_path(udid))?;
    Ok(())
}

/// Give a fresh clone the golden's store (the runtime's own entries) plus the
/// pinned disables. Must run before the clone's first boot.
pub fn apply_to_clone(golden: &str, clone: &str, disabled: &BTreeSet<String>) -> Result<()> {
    let mut entries = read_store(golden);
    for l in disabled {
        entries.insert(l.clone(), true);
    }
    if entries.is_empty() {
        return Ok(());
    }
    write_store(clone, &entries)
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&amp;", "&")
}
