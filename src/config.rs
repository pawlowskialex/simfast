//! The pinned simulator. It can come from a file (simfast.toml found by walking
//! up from the current directory, like .bazelversion, or any path given), from
//! stdin, or from command-line flags. Every source goes through the same pin
//! checks, so a config checked into a monorepo can be sent to any Mac as is:
//!
//!   ssh pool-mac simfast run - < sims/ui_tests.toml

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};

pub const FILE: &str = "simfast.toml";

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Groups this config's simulators for `list` and `stop`, on every Mac.
    /// Default: the file's path, or the config's content for stdin and flags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Device type identifier (or exact name), e.g. com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro
    pub device: String,
    /// Simulator runtime build, e.g. 23F77
    pub runtime: String,
    /// CoreSimulator framework version, e.g. 1051.55
    pub coresimulator: String,
    #[serde(default = "yes")]
    pub slim: bool,
    /// Categories, features or launchd labels to leave running.
    #[serde(default)]
    pub keep: Vec<String>,
    /// Hash of the exact set of services slim disables.
    #[serde(default)]
    pub services: String,
    /// Booted-then-frozen simulators kept ready for `run` (~1 GB RAM each, no CPU).
    #[serde(default = "one")]
    pub spares: usize,
}

fn yes() -> bool {
    true
}

fn one() -> usize {
    1
}

/// Where a config came from.
#[derive(Debug, Clone)]
pub enum Source {
    File(PathBuf),
    Stdin,
    Flags,
}

/// A config plus its origin. `text` is a normalized copy, used to hand the
/// exact same pins to the background spare preparer.
pub struct Loaded {
    pub cfg: Config,
    pub source: Source,
    pub text: String,
}

/// Pins given as command-line flags instead of a file.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Flags {
    /// Config file to use instead of simfast.toml ("-" reads stdin)
    #[arg(short = 'c', long = "config", value_name = "FILE|-", env = "SIMFAST_CONFIG")]
    pub config: Option<String>,
    /// Pin flags (no file): device type identifier or exact name
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub device: Option<String>,
    /// Runtime build, e.g. 23F77
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub runtime: Option<String>,
    /// CoreSimulator version, e.g. 1051.55
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub coresimulator: Option<String>,
    /// Hash of the disabled services (`simfast check` shows it)
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub services: Option<String>,
    /// Services to leave on
    #[arg(long, value_delimiter = ',', help_heading = "Pins as flags (instead of a file)")]
    pub keep: Vec<String>,
    /// Don't disable any services
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub no_slim: bool,
    /// Frozen spares to keep ready (default 1)
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub spares: Option<usize>,
    /// Name grouping these simulators for list/stop
    #[arg(long, help_heading = "Pins as flags (instead of a file)")]
    pub name: Option<String>,
}

impl Flags {
    fn any_pin(&self) -> bool {
        self.device.is_some()
            || self.runtime.is_some()
            || self.coresimulator.is_some()
            || self.services.is_some()
            || !self.keep.is_empty()
            || self.no_slim
            || self.spares.is_some()
            || self.name.is_some()
    }

    /// Load the config: flags, an explicit file or stdin, else simfast.toml
    /// found by walking up. `None` only when nothing was given and none is found.
    pub fn load_optional(&self) -> Result<Option<Loaded>> {
        if self.any_pin() {
            if self.config.is_some() {
                bail!("give either a config (-c/--config, or a file argument) or pin flags, not both");
            }
            let mut missing = vec![];
            for (flag, v) in [("--device", &self.device), ("--runtime", &self.runtime), ("--coresimulator", &self.coresimulator)] {
                if v.is_none() {
                    missing.push(flag);
                }
            }
            if !missing.is_empty() {
                bail!("pins as flags need {} too (`simfast check` on a pinned Mac shows the values)", missing.join(", "));
            }
            let cfg = Config {
                name: self.name.clone(),
                device: self.device.clone().unwrap(),
                runtime: self.runtime.clone().unwrap(),
                coresimulator: self.coresimulator.clone().unwrap(),
                slim: !self.no_slim,
                keep: self.keep.clone(),
                services: self.services.clone().unwrap_or_default(),
                spares: self.spares.unwrap_or(1),
            };
            return Ok(Some(Loaded { text: toml::to_string(&cfg)?, cfg, source: Source::Flags }));
        }
        match self.config.as_deref() {
            Some("-") => {
                let mut text = String::new();
                std::io::stdin().read_to_string(&mut text).context("read config from stdin")?;
                let cfg: Config = toml::from_str(&text).context("parse config from stdin")?;
                Ok(Some(Loaded { text: toml::to_string(&cfg)?, cfg, source: Source::Stdin }))
            }
            Some(path) => read_file(Path::new(path)).map(Some),
            None => find().map(|p| read_file(&p)).transpose(),
        }
    }

    pub fn load(&self) -> Result<Loaded> {
        self.load_optional()?.ok_or_else(|| {
            anyhow::anyhow!("no {FILE} in this directory or any parent; run `simfast init` to pin one, or pass a config (-c FILE, -c - for stdin) or pin flags")
        })
    }
}

/// simfast.toml in the current directory or the nearest parent.
pub fn find() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        let p = dir.join(FILE);
        if p.is_file() {
            return Some(p);
        }
        if !dir.pop() {
            return None;
        }
    }
}

fn read_file(path: &Path) -> Result<Loaded> {
    let text = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let cfg: Config = toml::from_str(&text).with_context(|| format!("parse {}", path.display()))?;
    Ok(Loaded { text: toml::to_string(&cfg)?, cfg, source: Source::File(path.to_path_buf()) })
}

impl Config {
    /// The commented file `init` writes.
    pub fn render(&self, comment: &str) -> String {
        let q = |s: &str| format!("{s:?}");
        let keep: Vec<String> = self.keep.iter().map(|k| q(k)).collect();
        let name = self.name.as_ref().map(|n| format!("name = {}  # groups these simulators in list/stop, on every Mac\n", q(n))).unwrap_or_default();
        format!(
            "# Pinned simulator. `simfast init` writes it from the current Mac; every\n\
             # simfast command refuses to run if the Mac differs.\n\
             \n\
             {name}\
             device = {}  # {comment}\n\
             runtime = {}\n\
             coresimulator = {}\n\
             \n\
             # Disable ~170 background daemons (Siri, Spotlight, widgets, iCloud...).\n\
             # keep: categories, features or launchd labels to leave on; see `simfast services`.\n\
             slim = {}\n\
             keep = [{}]\n\
             services = {}  # hash of the exact disabled set\n\
             \n\
             # Simulators booted ahead of time and frozen in RAM (~1 GB each, no CPU);\n\
             # `run` thaws one in well under a second. 0 = always boot.\n\
             spares = {}\n",
            q(&self.device),
            q(&self.runtime),
            q(&self.coresimulator),
            self.slim,
            keep.join(", "),
            q(&self.services),
            self.spares,
        )
    }
}
