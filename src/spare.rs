//! Frozen spares: simulators booted ahead of time, then frozen in RAM.
//!
//! CoreSimulator can't save or restore a simulator's memory. But a simulator
//! isn't a VM: it's a tree of ordinary host processes under its launchd_sim.
//! SIGSTOP on everything below launchd_sim freezes it (0% CPU, memory kept,
//! CoreSimulator still sees it as Booted) and SIGCONT resumes it in
//! milliseconds. So `run` thaws a spare instead of booting, and a background
//! process freezes a replacement.
//!
//! launchd_sim itself stays running (it idles at 0% CPU): if it were frozen,
//! an outside `simctl shutdown` (say, a CI script's `shutdown all`) would hang
//! forever. Running, it handles the shutdown and SIGKILLs its frozen children.
//!
//! Hermeticity: spares carry their pin key in their name and are only handed
//! out for identical pins. Each is booted from the pinned golden, slim-verified,
//! and frozen only once it has gone quiet, so every spare is frozen at the
//! same point. The simulator uses the host clock, so time jumps on thaw.

use crate::sim::{self, Pinned};
use crate::simctl;
use crate::sim::tree;
use crate::util::{state_dir, FileLock};
use anyhow::Result;
use std::collections::HashSet;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const WARMING: &str = "simfast-spare-warming: ";
pub const FROZEN: &str = "simfast-spare: ";

fn name(prefix: &str, p: &Pinned) -> String {
    format!("{prefix}{} #{}", p.label(), p.key())
}

fn key_of(name: &str) -> Option<&str> {
    sim::tag(name)
}

fn status_file(key: &str) -> std::path::PathBuf {
    state_dir().join(format!("spare-{key}.error"))
}

/// Why no spare was ready for these pins, for `run` to explain its slow path.
pub fn why_none(p: &Pinned) -> String {
    if let Ok(err) = std::fs::read_to_string(status_file(&p.key())) {
        return format!("the last attempt to prepare one failed: {} (log: {})", err.trim(), state_dir().join("spares.log").display());
    }
    if busy_file(&p.key()).exists() { "one is still being prepared".into() } else { "preparing one now".into() }
}

fn busy_file(key: &str) -> std::path::PathBuf {
    state_dir().join(format!("spare-{key}.busy"))
}

/// Record the outcome of a background replenish for `why_none`.
pub fn record(p: &Pinned, result: &Result<()>) {
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(status_file(&p.key()));
        }
        Err(e) => {
            let _ = std::fs::write(status_file(&p.key()), format!("{e:#}"));
        }
    }
}

/// Take a frozen spare for these pins, thaw it and return its UDID.
/// What `claim` found.
pub struct Claim {
    pub udid: Option<String>,
    /// Spares that had been shut down from outside simfast and were deleted.
    pub discarded: usize,
}

pub fn claim(p: &Pinned, project: &sim::Project) -> Result<Claim> {
    let _lock = FileLock::acquire(&state_dir().join("spares.lock"))?;
    let mut discarded = 0;
    for d in simctl::devices(None)? {
        if !d.name.starts_with(FROZEN) || key_of(&d.name) != Some(&p.key()) {
            continue;
        }
        // A spare that didn't survive (Simulator.app quit, `simctl shutdown`,
        // a reboot) is deleted; `run` says so.
        if d.state != "Booted" || tree(&d.udid).is_empty() {
            sim::stop(&d.udid);
            discarded += 1;
            continue;
        }
        simctl::rename(&d.udid, &project.device_name(p))?;
        thaw(&d.udid);
        return Ok(Claim { udid: Some(d.udid), discarded });
    }
    Ok(Claim { udid: None, discarded })
}

/// Bring this pin set's frozen spares up to `n`. Blocks while they boot.
pub fn replenish(p: &Pinned, n: usize) -> Result<()> {
    // Marks "a spare is on its way" for `why_none` from the moment we start.
    struct Busy(std::path::PathBuf);
    impl Drop for Busy {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let _ = std::fs::create_dir_all(state_dir());
    let _busy = (n > 0).then(|| std::fs::write(busy_file(&p.key()), "").map(|_| Busy(busy_file(&p.key())))).transpose()?;
    let _lock = FileLock::acquire(&state_dir().join(format!("replenish-{}.lock", p.key())))?;
    let mine: Vec<simctl::Device> =
        simctl::devices(None)?.into_iter().filter(|d| key_of(&d.name) == Some(&p.key())).collect();
    // With the lock held nobody else is warming these pins: leftovers are from a crash.
    for d in mine.iter().filter(|d| d.name.starts_with(WARMING)) {
        sim::stop(&d.udid);
    }
    let frozen: Vec<&simctl::Device> = mine.iter().filter(|d| d.name.starts_with(FROZEN)).collect();
    // Lowered `spares` (e.g. to 0): give the memory back.
    for d in frozen.iter().skip(n) {
        thaw(&d.udid);
        sim::stop(&d.udid);
        eprintln!("simfast: deleted extra spare {}", d.udid);
    }
    for _ in frozen.len()..n {
        let start = Instant::now();
        let udid = sim::prepare(p, &name(WARMING, p))?;
        let quiet = settle(&udid);
        freeze(&udid);
        simctl::rename(&udid, &name(FROZEN, p))?;
        eprintln!(
            "simfast: froze spare {udid} after {:.0}s{}",
            start.elapsed().as_secs_f64(),
            if quiet { "" } else { " (never went quiet; froze at the timeout)" }
        );
    }
    Ok(())
}

/// Run `simfast _replenish` detached, so `run` returns immediately.
pub fn replenish_in_background(p: &Pinned, loaded: &crate::config::Loaded) {
    let Ok(exe) = std::env::current_exe() else { return };
    let dir = state_dir();
    // The preparer gets the exact pins, whether they came from a file, stdin or flags.
    let config = dir.join("configs").join(format!("{}.toml", p.key()));
    if std::fs::create_dir_all(config.parent().unwrap()).and_then(|_| std::fs::write(&config, &loaded.text)).is_err() {
        return;
    }
    let Ok(log) = std::fs::OpenOptions::new().create(true).append(true).open(dir.join("spares.log")) else {
        return;
    };
    let mut cmd = Command::new(exe);
    cmd.arg("_replenish").env("SIMFAST_CONFIG", &config).stdin(Stdio::null());
    if let Ok(out) = log.try_clone() {
        cmd.stdout(out);
    }
    cmd.stderr(log);
    unsafe {
        use std::os::unix::process::CommandExt;
        cmd.pre_exec(|| {
            libc::setsid();
            Ok(())
        });
    }
    let _ = cmd.spawn();
}

/// Spares of any pins (`key` None) or of every pin set except `key`.
/// Delete spares that were shut down from outside (always garbage), and, if
/// `live` says so, running ones too: `Some(key)` = every pin set but `key`,
/// `None` = all. Spares still being prepared are left alone: a preparer may be
/// working on them and cleans up its own leftovers. Returns how many went.
pub fn delete_spares(live: Option<Option<&str>>) -> Result<usize> {
    let mut n = 0;
    for d in simctl::devices(None)? {
        if !d.name.starts_with(FROZEN) {
            continue;
        }
        let why = if d.state != "Booted" {
            "shut down"
        } else {
            match live {
                Some(None) => "all",
                Some(Some(keep)) if key_of(&d.name) != Some(keep) => "other pins",
                _ => continue,
            }
        };
        eprintln!("simfast: deleted spare {} ({why})", d.udid);
        thaw(&d.udid); // otherwise launchd_sim waits before killing its frozen children
        sim::stop(&d.udid);
        n += 1;
    }
    Ok(n)
}

// ---------------------------------------------------------------- processes

fn signal(pids: &[i32], sig: i32) {
    for &pid in pids {
        unsafe { libc::kill(pid, sig) };
    }
}

/// SIGSTOP everything below launchd_sim, repeating until no unfrozen process
/// remains (a process could spawn a child between listing and stopping).
fn freeze(udid: &str) {
    // tree() lists launchd_sim first; it is never stopped (see module docs).
    let mut stopped: HashSet<i32> = tree(udid).into_iter().take(1).collect();
    for _ in 0..10 {
        let new: Vec<i32> = tree(udid).into_iter().filter(|p| !stopped.contains(p)).collect();
        if new.is_empty() {
            return;
        }
        signal(&new, libc::SIGSTOP);
        stopped.extend(new);
    }
}

fn thaw(udid: &str) {
    signal(&tree(udid), libc::SIGCONT);
}

/// Wait until the device's processes go quiet after boot (first-boot-of-clone
/// work: caches, indexing), so every spare is frozen at the same point.
/// Quiet = under 0.25 CPU-seconds per second for three 2s windows; gives up
/// after 90s. Returns whether it went quiet.
fn settle(udid: &str) -> bool {
    let deadline = Instant::now() + Duration::from_secs(90);
    let mut last = cpu_seconds(udid);
    let mut quiet = 0;
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_secs(2));
        let now = cpu_seconds(udid);
        quiet = if now - last < 0.5 { quiet + 1 } else { 0 };
        if quiet >= 3 {
            return true;
        }
        last = now;
    }
    false
}

/// Total CPU time used so far by the device's process tree.
fn cpu_seconds(udid: &str) -> f64 {
    let pids: Vec<String> = tree(udid).iter().map(|p| p.to_string()).collect();
    if pids.is_empty() {
        return 0.0;
    }
    // Not `output`: ps exits 1 if any pid has since exited, but still reports the rest.
    let Ok(out) = Command::new("ps").args(["-o", "time=", "-p", &pids.join(",")]).output() else { return 0.0 };
    let out = String::from_utf8_lossy(&out.stdout);
    // [[dd-]hh:]mm:ss.ss
    out.lines()
        .map(|t| {
            let t = t.trim();
            let (days, rest) = t.split_once('-').map_or((0.0, t), |(d, r)| (d.parse().unwrap_or(0.0), r));
            rest.split(':').fold(0.0, |acc, part| acc * 60.0 + part.parse::<f64>().unwrap_or(0.0)) + days * 86400.0
        })
        .sum()
}
