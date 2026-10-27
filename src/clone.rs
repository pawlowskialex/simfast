//! Copying a golden so the copy owes it nothing.
//!
//! `simctl clone` takes ~4.5s and its copies are still tied to their source:
//! symlinks and system databases inside the data directory hold absolute paths
//! into the source device, and the running copy opens and writes through them.
//! (Seen here: copies writing Metal caches and poster snapshots into the golden.)
//!
//! simfast instead creates an empty device, swaps in an APFS clone of the
//! golden's data directory with one clonefile() call (~0.3s), and sanitizes it
//! with simslim's recipe (https://github.com/MobAI-App/simslim, MIT): rebase
//! symlinks and plists, and delete the stores the system regenerates from
//! current paths. Goldens live in CoreSimulator/Simfast, a sibling of
//! CoreSimulator/Devices with the same path length, so a path inside the golden
//! can be rewritten byte for byte, which keeps binary plists valid. After boot,
//! `audit` fails the device if any of its processes has a golden file open.

use anyhow::{bail, Context, Result};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// System stores that persist absolute simulator paths and are regenerated
/// by their daemons. Relative to the data directory.
const REGENERATED: &[&str] = &[
    "Library/AppleMediaServices/Engagement/internal/database",
    "Library/Spotlight",
    "Library/chronod/replicator",
    "Library/com.apple.nsurlsessiond",
    "private/var/MobileAsset/AssetsV2/locks",
    "private/var/MobileAsset/AssetsV2/persisted",
    "private/var/db/assetsubscriptiond/history",
    "var/db/diagnostics/Persist",
    "var/db/diagnostics/Special",
    "Library/Caches",
    "tmp",
];

fn logs_dir() -> PathBuf {
    crate::util::home().join("Library/Logs/CoreSimulator")
}

/// Replace `device`'s data directory with a sanitized clone of `golden`'s.
/// Both are device directories (…/<UDID>), with paths of equal length.
pub fn clone_data(golden: &Path, golden_udid: &str, device: &Path, udid: &str) -> Result<()> {
    if golden.as_os_str().len() != device.as_os_str().len() || golden_udid.len() != udid.len() {
        bail!("template and device paths must have equal length to rebase in place");
    }
    let data = device.join("data");
    let staged = device.join("data.simfast-new");
    let aside = device.join("data.simfast-old");
    // CoreSimulator keeps populating a new device's data for a few ms after
    // `simctl create` returns. A late write would recreate `data` under us, or
    // land inside the golden's copy, so wait for it to stop first.
    wait_settled(&data)?;
    let src = CString::new(golden.join("data").as_os_str().as_bytes())?;
    let dst = CString::new(staged.as_os_str().as_bytes())?;
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } != 0 {
        bail!("clonefile template data: {}", std::io::Error::last_os_error());
    }
    let swapped = std::time::SystemTime::now();
    fs::rename(&data, &aside).context("move the new device's empty data aside")?;
    fs::rename(&staged, &data).context("move the template's copy into place")?;
    let _ = fs::remove_dir_all(&aside);
    // The copy keeps the golden's mtimes, so anything newer was written by
    // someone else after the swap: refuse rather than hand out a mixed device.
    std::thread::sleep(std::time::Duration::from_millis(50));
    let mut late = None;
    walk(&data, &mut |path, meta| {
        if late.is_none() && meta.modified().is_ok_and(|t| t >= swapped) {
            late = Some(path.to_path_buf());
        }
        Ok(())
    })?;
    if let Some(p) = late {
        bail!("{} was written into the new device during setup", p.display());
    }
    sanitize(golden, golden_udid, device, udid)
}

/// Wait until nothing under `dir` has changed for 150ms (max 5s).
fn wait_settled(dir: &Path) -> Result<()> {
    let snapshot = || {
        let (mut n, mut newest) = (0u64, std::time::UNIX_EPOCH);
        let _ = walk(dir, &mut |_, m| {
            n += 1;
            newest = newest.max(m.modified().unwrap_or(std::time::UNIX_EPOCH));
            Ok(())
        });
        (dir.exists(), n, newest)
    };
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    let (mut last, mut stable) = (snapshot(), 0);
    while std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(50));
        let now = snapshot();
        stable = if now == last && now.0 { stable + 1 } else { 0 };
        if stable >= 3 {
            return Ok(());
        }
        last = now;
    }
    bail!("CoreSimulator kept writing the new device's data for 5s")
}

fn sanitize(golden: &Path, golden_udid: &str, device: &Path, udid: &str) -> Result<()> {
    let data = device.join("data");
    // Generated from installed apps and current paths; rebuilt at boot.
    // Rewriting these in place instead (same-length paths) was measured: no faster.
    remove_matching(&data.join("var/db/lsd"), |n| n.contains("com.apple.LaunchServices-") && n.contains(".csstore"))?;
    remove_matching(&data.join("Library/FrontBoard"), |n| n.starts_with("applicationState.db"))?;
    let mut dirs: Vec<PathBuf> = REGENERATED.iter().map(|d| data.join(d)).collect();
    if let Ok(groups) = fs::read_dir(data.join("Containers/Shared/AppGroup")) {
        dirs.extend(groups.flatten().map(|g| g.path().join("replicatord")));
    }
    for d in dirs {
        clear_dir(&d)?;
    }

    let (old_dev, new_dev) = (golden.as_os_str().as_bytes(), device.as_os_str().as_bytes());
    let (old_logs, new_logs) = (logs_dir().join(golden_udid), logs_dir().join(udid));
    walk(&data, &mut |path, meta| {
        if meta.file_type().is_symlink() {
            let target = fs::read_link(path)?;
            let rebased = if let Ok(rest) = target.strip_prefix(golden) {
                Some(device.join(rest))
            } else if let Ok(rest) = target.strip_prefix(&old_logs) {
                Some(new_logs.join(rest))
            } else {
                None
            };
            if let Some(t) = rebased {
                fs::remove_file(path)?;
                std::os::unix::fs::symlink(t, path)?;
            }
        } else if meta.is_file() && path.extension().is_some_and(|e| e.eq_ignore_ascii_case("plist")) {
            let bytes = fs::read(path)?;
            if contains(&bytes, golden_udid.as_bytes()) {
                // Same-length replacement keeps binary plists' offsets intact.
                let bytes = replace(&replace(&bytes, old_dev, new_dev), golden_udid.as_bytes(), udid.as_bytes());
                let tmp = path.with_extension("plist.simfast-tmp");
                fs::write(&tmp, bytes)?;
                fs::set_permissions(&tmp, meta.permissions())?;
                fs::rename(&tmp, path)?;
            }
        }
        Ok(())
    })?;

    // Belt and braces: nothing may still resolve into the golden.
    walk(&data, &mut |path, meta| {
        if meta.file_type().is_symlink() && fs::read_link(path)?.starts_with(golden) {
            bail!("{} still points into the template", path.display());
        }
        Ok(())
    })
}

/// Fail if any of `pids` has a file under `golden` open.
pub fn audit(pids: &[i32], golden: &Path) -> Result<()> {
    if pids.is_empty() {
        bail!("simulator has no processes to audit");
    }
    let list: Vec<String> = pids.iter().map(|p| p.to_string()).collect();
    // lsof exits 1 if any pid has exited, but still lists the rest.
    let out = std::process::Command::new("lsof")
        .args(["-n", "-P", "-Fn", "-p", &list.join(",")])
        .output()
        .context("run lsof")?;
    let prefix = golden.to_string_lossy();
    let open: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.strip_prefix('n'))
        .filter(|n| n.starts_with(prefix.as_ref()))
        .map(String::from)
        .collect();
    if let Some(first) = open.first() {
        bail!("simulator has {} file(s) of the template open, e.g. {first}", open.len());
    }
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn walk(dir: &Path, f: &mut dyn FnMut(&Path, &fs::Metadata) -> Result<()>) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        let path = e.path();
        let meta = fs::symlink_metadata(&path)?;
        f(&path, &meta)?;
        if meta.is_dir() {
            walk(&path, f)?;
        }
    }
    Ok(())
}

fn clear_dir(dir: &Path) -> Result<()> {
    let Ok(meta) = fs::symlink_metadata(dir) else { return Ok(()) };
    if !meta.is_dir() {
        return Ok(());
    }
    for e in fs::read_dir(dir)?.flatten() {
        let p = e.path();
        if fs::symlink_metadata(&p)?.is_dir() {
            fs::remove_dir_all(&p)
        } else {
            fs::remove_file(&p)
        }
        .with_context(|| format!("clear {}", p.display()))?;
    }
    Ok(())
}

fn remove_matching(dir: &Path, pred: impl Fn(&str) -> bool) -> Result<()> {
    let Ok(entries) = fs::read_dir(dir) else { return Ok(()) };
    for e in entries.flatten() {
        if e.file_name().to_str().is_some_and(&pred) && fs::symlink_metadata(e.path())?.is_file() {
            fs::remove_file(e.path())?;
        }
    }
    Ok(())
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

fn replace(hay: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(hay.len());
    let mut i = 0;
    while i < hay.len() {
        if hay[i..].starts_with(from) {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}
