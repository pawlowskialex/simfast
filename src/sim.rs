//! Pins, goldens and runs.
//!
//! Why this is fast: a fresh simulator spends ~45-65s on first-boot setup; one
//! that has booted before comes up in 2-4s. simfast boots one *golden* per pin
//! set once, in a private device set Xcode never lists, and starts every
//! simulator as an APFS clone of it.
//!
//! Why it's hermetic: the golden is keyed on the exact device type, runtime
//! build and CoreSimulator version, and every command first checks the Mac
//! against the pins in simfast.toml and refuses to run on a mismatch. With slim
//! on, the exact disabled set is pinned by hash and read back after each boot.

use crate::clone;
use crate::config::Config;
use crate::simctl::{self, DeviceType, Runtime};
use crate::slim;
use crate::util::{short_hash, state_dir, FileLock};
use anyhow::{bail, Context, Result};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub const PREFIX: &str = "simfast: ";

/// Which config a simulator was started from. Its id is in the simulator's
/// name, so `list` and `stop` act on one config's simulators only:
/// - a config with `name` is identified by that name, the same on every Mac;
/// - a file by its path (two configs in one directory stay separate);
/// - stdin or flags by the config's content, so the same pins piped again
///   (e.g. `simfast stop - < ui.toml`) find the same simulators.
pub struct Project {
    pub id: String,
    pub name: String,
}

impl Project {
    pub fn of(loaded: &crate::config::Loaded) -> Project {
        use crate::config::Source;
        if let Some(n) = &loaded.cfg.name {
            return Project { id: short_hash([&format!("name:{n}")]), name: n.clone() };
        }
        match &loaded.source {
            Source::File(path) => {
                let path = path.canonicalize().unwrap_or_else(|_| path.clone());
                let dir = path.parent().and_then(|d| d.file_name()).map_or("project".into(), |n| n.to_string_lossy().into_owned());
                let name = match path.file_name().and_then(|f| f.to_str()) {
                    Some(crate::config::FILE) | None => dir,
                    Some(_) => format!("{dir}/{}", path.file_stem().unwrap_or_default().to_string_lossy()),
                };
                Project { id: short_hash([&path.to_string_lossy().into_owned()]), name }
            }
            Source::Stdin | Source::Flags => {
                let what = if matches!(loaded.source, Source::Stdin) { "piped config" } else { "flags" };
                let id = short_hash([&format!("content:{}", loaded.text)]);
                Project { name: format!("{what} {id}"), id }
            }
        }
    }
    /// "simfast: iPhone Air (iOS 26.5 23F77) · MyApp #1a2b3c4d"
    pub fn device_name(&self, p: &Pinned) -> String {
        format!("{PREFIX}{} · {} #{}", p.label(), self.name, self.id)
    }
}

/// The trailing "#id" of a simfast device name: a project id for running
/// simulators, a pin key for spares.
pub fn tag(name: &str) -> Option<&str> {
    name.rsplit_once(" #").map(|(_, k)| k)
}
const BOOT_TIMEOUT: Duration = Duration::from_secs(600);

/// Goldens live in a sibling of CoreSimulator/Devices with an equally long
/// name, so clone.rs can rewrite paths byte for byte. Xcode doesn't list it.
pub fn golden_set() -> PathBuf {
    crate::util::home().join("Library/Developer/CoreSimulator/Simfast")
}

/// Where versions before 0.2 kept goldens; `clean` removes it.
fn legacy_golden_set() -> PathBuf {
    state_dir().join("goldens")
}

/// One pinned value compared against this Mac.
pub struct Check {
    pub what: &'static str,
    pub pinned: String,
    pub actual: String,
}

impl Check {
    pub fn ok(&self) -> bool {
        self.pinned == self.actual
    }
}

/// The pins resolved against this Mac.
pub struct Pinned {
    pub device: DeviceType,
    pub runtime: Runtime,
    pub coresimulator: String,
    pub disabled: BTreeSet<String>,
}

impl Pinned {
    pub fn label(&self) -> String {
        format!("{} ({} {})", self.device.name, self.runtime.name, self.runtime.build)
    }
    /// Identifies everything pinned, including the disabled set.
    pub fn key(&self) -> String {
        short_hash([&self.golden_name(), &short_hash(self.disabled.iter())])
    }
    fn golden_name(&self) -> String {
        format!("golden {} {} cs{}", self.device.identifier, self.runtime.build, self.coresimulator)
    }
}

/// Compare every pin with this Mac. Returns all checks, and the resolved pins
/// only if every one matched.
pub fn check(cfg: &Config) -> Result<(Vec<Check>, Option<Pinned>)> {
    let cat = simctl::catalog()?;
    let mut checks = vec![];

    let device = cat.devicetypes.iter().find(|t| t.identifier == cfg.device || t.name == cfg.device).cloned();
    checks.push(Check {
        what: "device",
        pinned: cfg.device.clone(),
        actual: device.as_ref().map_or("not installed".into(), |_| cfg.device.clone()),
    });

    let runtime = cat.runtimes.iter().find(|r| r.build == cfg.runtime && r.is_available).cloned();
    let installed: Vec<String> = cat.runtimes.iter().filter(|r| r.is_available).map(|r| format!("{} ({})", r.build, r.name)).collect();
    checks.push(Check {
        what: "runtime",
        pinned: cfg.runtime.clone(),
        actual: match &runtime {
            Some(r) => r.build.clone(),
            None => format!("not installed (have {})", if installed.is_empty() { "none".into() } else { installed.join(", ") }),
        },
    });
    if let (Some(d), Some(r)) = (&device, &runtime) {
        if !r.supports(&d.identifier) {
            checks.push(Check { what: "runtime supports device", pinned: "yes".into(), actual: "no".into() });
        }
    }

    let coresimulator = simctl::coresimulator_version()?;
    checks.push(Check { what: "coresimulator", pinned: cfg.coresimulator.clone(), actual: coresimulator.clone() });

    let mut disabled = BTreeSet::new();
    if cfg.slim {
        if let Some(r) = &runtime {
            disabled = slim::disabled_set(&cfg.keep, r)?;
            // A different simfast (catalog) or keep list changes the set: re-pin explicitly.
            let pinned = if cfg.services.is_empty() { "(not set)".into() } else { cfg.services.clone() };
            checks.push(Check { what: "services", pinned, actual: short_hash(disabled.iter()) });
        }
    }

    let pinned = match (checks.iter().all(Check::ok), device, runtime) {
        (true, Some(device), Some(runtime)) => Some(Pinned { device, runtime, coresimulator, disabled }),
        _ => None,
    };
    Ok((checks, pinned))
}

/// Resolve the pins or fail with every mismatch.
pub fn pinned(cfg: &Config) -> Result<Pinned> {
    let (checks, pinned) = check(cfg)?;
    match pinned {
        Some(p) => Ok(p),
        None => {
            let bad: Vec<String> = checks
                .iter()
                .filter(|c| !c.ok())
                .map(|c| format!("  {}: pinned {}, this Mac: {}", c.what, c.pinned, c.actual))
                .collect();
            bail!("this Mac doesn't match simfast.toml:\n{}\n(re-pin with `simfast init` if the change is intended)", bad.join("\n"))
        }
    }
}

/// The golden for these pins, booted once to finish first-boot setup. Built on
/// first use (~1 min).
pub fn golden(p: &Pinned, rebuild: bool) -> Result<String> {
    let set = golden_set();
    std::fs::create_dir_all(&set)?;
    let name = p.golden_name();
    let _lock = FileLock::acquire(&state_dir().join(format!("golden-{}.lock", short_hash([&name]))))?;
    let existing: Vec<_> = simctl::devices(Some(&set))?.into_iter().filter(|d| d.name == name).collect();
    // No stamp means the build never finished.
    if let (Some(d), false) = (existing.iter().find(|d| stamp(&d.udid).exists()), rebuild) {
        return Ok(d.udid.clone());
    }
    for d in existing {
        simctl::delete(Some(&set), &d.udid)?;
    }

    eprintln!("simfast: building the template for {} (one-time first boot, usually 1-2 min)", p.label());
    let start = Instant::now();
    let _progress = crate::util::Progress::start("first boot of the template");
    let udid = simctl::run(Some(&set), &["create", &name, &p.device.identifier, &p.runtime.identifier])?;
    let built = (|| -> Result<()> {
        simctl::boot(Some(&set), &udid)?;
        simctl::wait_booted(Some(&set), &udid, BOOT_TIMEOUT)?;
        // Let post-boot work (indexing, cache builds) finish so clones inherit it.
        std::thread::sleep(Duration::from_secs(10));
        simctl::shutdown(Some(&set), &udid);
        settle_migration_result(&udid)?;
        // Written after shutdown returns: nothing may touch the template after this.
        std::fs::write(stamp(&udid), "")?;
        Ok(())
    })();
    drop(_progress);
    if let Err(e) = built {
        let _ = simctl::delete(Some(&set), &udid);
        return Err(e.context("building the template"));
    }
    eprintln!("simfast: template ready in {:.0}s", start.elapsed().as_secs_f64());
    Ok(udid)
}

fn migration_plist(golden: &str) -> PathBuf {
    golden_set().join(golden).join("data/Library/Preferences/com.apple.migration.plist")
}

/// Every copy inherits the template's first-boot data-migration result, and
/// CoreSimulator reads it at boot: if it says "failed", `bootstatus` returns as
/// soon as the boot starts instead of waiting for SpringBoard, so `run` would
/// hand out a device ~15s before it can launch apps. Some device types (iPhone
/// Air on iOS 26.5) record "failed" every time although every migration plugin
/// succeeded. Correct the summary only in that case; a plugin that really
/// failed is a broken template.
fn settle_migration_result(golden: &str) -> Result<()> {
    let plist = migration_plist(golden);
    let path = plist.to_str().unwrap_or("");
    let json = crate::util::output("plutil", &["-convert", "json", "-r", "-o", "-", path])
        .context("read the template's data-migration result")?;
    let v: serde_json::Value = serde_json::from_str(&json)?;
    if v["DMLastMigrationResults"]["success"].as_bool() == Some(true) {
        return Ok(());
    }
    let plugins = v["DMMigrationPluginResults"].as_object().cloned().unwrap_or_default();
    let failed: Vec<&String> = plugins
        .iter()
        .filter(|(_, r)| r["kDMMigrationPluginResultsPhaseDescription"].as_str() != Some("kDMMigrationPhaseDescriptionDidFinishWithSuccess"))
        .map(|(k, _)| k)
        .collect();
    if plugins.is_empty() || !failed.is_empty() {
        bail!("the template's first-boot data migration failed (plugins: {})", if failed.is_empty() { "none ran".into() } else { failed.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ") });
    }
    crate::util::output("plutil", &["-replace", "DMLastMigrationResults.success", "-bool", "true", path])?;
    eprintln!("simfast: note: first boot recorded data migration as failed although all {} plugins succeeded; corrected", plugins.len());
    Ok(())
}

/// The finished golden for these pins, if built.
pub fn existing_golden(p: &Pinned) -> Result<Option<String>> {
    let set = golden_set();
    if !set.exists() {
        return Ok(None);
    }
    let name = p.golden_name();
    Ok(simctl::devices(Some(&set))?.into_iter().find(|d| d.name == name && stamp(&d.udid).exists()).map(|d| d.udid))
}

fn stamp(golden: &str) -> PathBuf {
    golden_set().join(golden).join("simfast-built")
}

/// Fail if any file in the golden changed after it was built: every clone
/// would start from that drifted state.
pub fn check_untouched(golden: &str) -> Result<()> {
    let built = std::fs::metadata(stamp(golden))?.modified()?;
    let mut changed = vec![];
    let mut stack = vec![golden_set().join(golden).join("data")];
    while let Some(dir) = stack.pop() {
        for e in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(m) = e.path().symlink_metadata() else { continue };
            if m.is_dir() {
                stack.push(e.path());
            } else if m.modified().is_ok_and(|t| t > built) {
                changed.push(e.path());
            }
        }
    }
    if let Some(first) = changed.first() {
        bail!(
            "the template changed after it was built ({} files, e.g. {}); rebuild it with `simfast build --rebuild`",
            changed.len(),
            first.display()
        );
    }
    Ok(())
}

/// Start a clean, booted simulator from the golden. Returns its UDID.
pub fn run(p: &Pinned, project: &Project) -> Result<String> {
    prepare(p, &project.device_name(p))
}

/// Copy the golden as `name`, slim it, boot it and verify it is slimmed and
/// independent of the golden.
pub fn prepare(p: &Pinned, name: &str) -> Result<String> {
    let golden = golden(p, false)?;
    check_untouched(&golden)?;
    // An empty device of exactly the pinned type and runtime, then the golden's data.
    let udid = simctl::run(None, &["create", name, &p.device.identifier, &p.runtime.identifier])?;
    let started = (|| -> Result<()> {
        clone::clone_data(&golden_set().join(&golden), &golden, &simctl::default_set().join(&udid), &udid)?;
        slim::apply_to_clone(&golden, &udid, &p.disabled)?;
        simctl::boot(None, &udid)?;
        simctl::wait_booted(None, &udid, BOOT_TIMEOUT)?;
        verify(p, &udid)?;
        clone::audit(&tree(&udid), &golden_set().join(&golden))
    })();
    if let Err(e) = started {
        stop(&udid);
        return Err(e);
    }
    Ok(udid)
}

/// Read back what launchd actually honoured; never hand out a drifted device.
pub fn verify(p: &Pinned, udid: &str) -> Result<()> {
    if p.disabled.is_empty() {
        return Ok(());
    }
    let live: BTreeSet<String> = simctl::live_disabled(udid)?.into_iter().collect();
    let missing: Vec<&String> = p.disabled.difference(&live).collect();
    if !missing.is_empty() {
        bail!("{} of {} pinned services are still enabled after boot (e.g. {})", missing.len(), p.disabled.len(), missing[0]);
    }
    Ok(())
}

pub fn stop(udid: &str) {
    if let Err(e) = simctl::delete(None, udid) {
        eprintln!("simfast: delete {udid}: {e:#}");
    }
    // launchd_sim's per-device host dir (slim store, launchd.log) outlives the device.
    let _ = std::fs::remove_dir_all(slim::store_dir(udid));
}

/// PIDs of the device's launchd_sim (first) and all its descendants.
pub fn tree(udid: &str) -> Vec<i32> {
    let pattern = format!("{udid}/data/var/run/launchd_bootstrap");
    let Ok(root) = crate::util::output("pgrep", &["-f", &pattern]) else { return vec![] };
    let Some(root) = root.split_whitespace().next().and_then(|r| r.parse::<i32>().ok()) else { return vec![] };
    let Ok(ps) = crate::util::output("ps", &["-axo", "pid=,ppid="]) else { return vec![root] };
    let mut children: std::collections::HashMap<i32, Vec<i32>> = Default::default();
    for line in ps.lines() {
        let mut f = line.split_whitespace().filter_map(|x| x.parse::<i32>().ok());
        if let (Some(pid), Some(ppid)) = (f.next(), f.next()) {
            children.entry(ppid).or_default().push(pid);
        }
    }
    let mut out = vec![];
    let mut stack = vec![root];
    while let Some(pid) = stack.pop() {
        out.push(pid);
        stack.extend(children.get(&pid).into_iter().flatten());
    }
    out
}

/// Simulators started by simfast, in the default device set.
pub fn running() -> Result<Vec<simctl::Device>> {
    Ok(simctl::devices(None)?.into_iter().filter(|d| d.name.starts_with(PREFIX)).collect())
}

/// Delete goldens. With `keep`, the golden of those pins survives.
pub fn clean_goldens(keep: Option<&Pinned>) -> Result<usize> {
    let mut n = 0;
    let legacy = legacy_golden_set();
    if legacy.exists() {
        for g in simctl::devices(Some(&legacy)).unwrap_or_default() {
            let _ = simctl::delete(Some(&legacy), &g.udid);
            let _ = std::fs::remove_dir_all(slim::store_dir(&g.udid));
        }
        std::fs::remove_dir_all(&legacy)?;
        eprintln!("simfast: deleted pre-0.2 templates in {}", legacy.display());
        n += 1;
    }
    let set = golden_set();
    if !set.exists() {
        return Ok(n);
    }
    for g in simctl::devices(Some(&set))? {
        if keep.is_some_and(|p| p.golden_name() == g.name) {
            continue;
        }
        simctl::delete(Some(&set), &g.udid)?;
        let _ = std::fs::remove_dir_all(slim::store_dir(&g.udid));
        eprintln!("simfast: deleted template {}", g.name.trim_start_matches("golden "));
        n += 1;
    }
    Ok(n)
}
