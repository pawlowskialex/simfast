use anyhow::{bail, Context, Result};
use std::fs::{self, File};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/tmp".into()))
}

/// Where goldens live. Overridable for tests.
pub fn state_dir() -> PathBuf {
    std::env::var_os("SIMFAST_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join("Library/Developer/simfast"))
}

/// An exclusive flock, released on drop.
pub struct FileLock(File);

impl FileLock {
    pub fn acquire(path: &Path) -> Result<FileLock> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let f = File::create(path).with_context(|| format!("open lock {}", path.display()))?;
        if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
            bail!("flock {}: {}", path.display(), std::io::Error::last_os_error());
        }
        Ok(FileLock(f))
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        unsafe { libc::flock(self.0.as_raw_fd(), libc::LOCK_UN) };
    }
}

/// Wait for a child, killing it after `timeout`.
pub fn wait_timeout(mut child: Child, timeout: Duration, what: &str) -> Result<()> {
    let start = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            if status.success() {
                return Ok(());
            }
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read;
                let _ = e.read_to_string(&mut err);
            }
            bail!("{what} failed: {}", err.trim());
        }
        if start.elapsed() > timeout {
            let _ = child.kill();
            bail!("{what} timed out after {}s", timeout.as_secs());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub fn output(cmd: &str, args: &[&str]) -> Result<String> {
    let out = Command::new(cmd)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("run {cmd}"))?;
    if !out.status.success() {
        bail!("{cmd} {} failed: {}", args.join(" "), String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// "this Mac is heavily loaded: load 80 on 10 cores", when it is.
pub fn load_warning() -> Option<String> {
    let mut load = [0f64; 3];
    let cores = std::thread::available_parallelism().map_or(1, |n| n.get()) as f64;
    if unsafe { libc::getloadavg(load.as_mut_ptr(), 3) } < 1 || load[0] < 2.0 * cores {
        return None;
    }
    Some(format!("this Mac is heavily loaded: load {:.0} on {cores:.0} cores", load[0]))
}

/// load_warning() as a parenthetical suffix, or "".
pub fn load_note() -> String {
    load_warning().map(|w| format!(" ({w})")).unwrap_or_default()
}

/// Elapsed-time feedback on stderr for slow steps: a live line on a terminal,
/// a line every 30s otherwise. Stops when dropped.
pub struct Progress(Option<std::sync::mpsc::Sender<()>>, Option<std::thread::JoinHandle<()>>);

impl Progress {
    pub fn start(what: &'static str) -> Progress {
        let tty = unsafe { libc::isatty(2) } == 1;
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let start = Instant::now();
        let handle = std::thread::spawn(move || {
            let tick = if tty { Duration::from_secs(1) } else { Duration::from_secs(30) };
            while rx.recv_timeout(tick).is_err_and(|e| e == std::sync::mpsc::RecvTimeoutError::Timeout) {
                let s = start.elapsed().as_secs();
                if tty {
                    eprint!("\r\x1b[2K  {what}: {}:{:02}", s / 60, s % 60);
                } else {
                    eprintln!("  {what}: {s}s");
                }
            }
            if tty {
                eprint!("\r\x1b[2K");
            }
        });
        Progress(Some(tx), Some(handle))
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        drop(self.0.take());
        if let Some(h) = self.1.take() {
            let _ = h.join();
        }
    }
}

/// Levenshtein distance, for "did you mean" suggestions.
pub fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut prev = row[0];
        row[0] = i + 1;
        for j in 0..b.len() {
            let cur = row[j + 1];
            row[j + 1] = (prev + (ca != b[j]) as usize).min(row[j] + 1).min(cur + 1);
            prev = cur;
        }
    }
    row[b.len()]
}

/// Stable short hash (FNV-1a), used to pin the exact set of slimmed services.
pub fn short_hash<'a>(items: impl IntoIterator<Item = &'a String>) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for item in items {
        for b in item.bytes().chain(std::iter::once(0)) {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    format!("{:08x}", h as u32)
}

static RECORDER_PID: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

extern "C" fn forward_stop(_: libc::c_int) {
    let pid = RECORDER_PID.load(std::sync::atomic::Ordering::SeqCst);
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGINT) };
    }
}

/// Make Ctrl-C (and SIGTERM, e.g. `kill` from a script) stop `pid` with
/// SIGINT instead of us, so it can finalize its output before we exit.
pub fn forward_stop_signals_to(pid: i32) {
    RECORDER_PID.store(pid, std::sync::atomic::Ordering::SeqCst);
    unsafe {
        libc::signal(libc::SIGINT, forward_stop as usize);
        libc::signal(libc::SIGTERM, forward_stop as usize);
    }
}

/// Send SIGINT to `pid` (ask it to finish up).
pub fn interrupt(pid: i32) {
    unsafe { libc::kill(pid, libc::SIGINT) };
}
