mod clone;
mod config;
mod sim;
mod simctl;
mod slim;
mod spare;
mod util;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use config::{Config, Flags, Loaded, Source};
use sim::Project;
use std::process::Command;
use std::time::Instant;

/// Hermetic, slimmed iOS simulators that boot in seconds.
///
/// simfast.toml pins the device model, iOS runtime build, CoreSimulator version
/// and the exact set of disabled services. Every command checks this Mac
/// against it first and refuses to run if anything differs.
#[derive(Parser)]
#[command(version, max_term_width = 100)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Pin simfast.toml to this Mac (re-pins an existing file, keeping its choices)
    Init {
        /// Device type, e.g. "iPhone 17 Pro" (default: current pin, else iPhone 17 Pro)
        device: Option<String>,
        /// Runtime version or build, e.g. 26.5 or 23F77 (default: newest installed)
        #[arg(short, long)]
        runtime: Option<String>,
        /// Services to leave on: categories, features or labels (replaces the current list)
        #[arg(long, value_delimiter = ',')]
        keep: Option<Vec<String>>,
        /// Don't disable any services
        #[arg(long)]
        no_slim: bool,
        /// Name that groups this config's simulators in list/stop, on every Mac
        #[arg(long)]
        name: Option<String>,
        /// Write this file instead of simfast.toml ("-": print the config to stdout)
        #[arg(short = 'c', long = "config", value_name = "FILE|-")]
        out: Option<String>,
    },
    /// Check this Mac against the pins
    Check {
        #[command(flatten)]
        src: Flags,
    },
    /// Build the template and freeze spares now (otherwise the first run does), or trim spares
    Build {
        /// Rebuild the template from scratch
        #[arg(long)]
        rebuild: bool,
        #[command(flatten)]
        src: Flags,
    },
    /// Start a clean, booted simulator and print its UDID once apps can launch
    Run {
        /// Config file to use ("-" reads stdin); same as -c
        #[arg(value_name = "CONFIG")]
        file: Option<String>,
        /// Show it in Simulator.app
        #[arg(short, long)]
        open: bool,
        #[command(flatten)]
        src: Flags,
    },
    /// List this config's simulators and spares
    List {
        /// Every config's
        #[arg(long)]
        all: bool,
        #[command(flatten)]
        src: Flags,
    },
    /// Shut down and delete simulators (default: this config's)
    Stop {
        udids: Vec<String>,
        /// Every simfast simulator on this Mac
        #[arg(long, conflicts_with = "udids")]
        all: bool,
        #[command(flatten)]
        src: Flags,
    },
    /// Remove leftovers: this config's shut-down simulators and dead spares
    Clean {
        /// Also delete templates and spares of other pins (other projects on this Mac
        /// rebuild theirs on their next run, 1-2 min)
        #[arg(long)]
        other_pins: bool,
        /// Delete every template, spare and shut-down simulator simfast made
        #[arg(long, conflicts_with = "other_pins")]
        all: bool,
        #[command(flatten)]
        src: Flags,
    },
    /// Record a simulator's screen to a video file until Ctrl-C (or --for)
    Record {
        /// Output file, e.g. run.mp4
        output: std::path::PathBuf,
        /// Simulator to record (default: this config's running simulator, if there is exactly one)
        #[arg(long)]
        udid: Option<String>,
        /// Stop after this many seconds
        #[arg(long = "for", value_name = "SECONDS")]
        seconds: Option<f64>,
        /// h264 plays everywhere; hevc is smaller
        #[arg(long, default_value = "h264", value_parser = ["h264", "hevc"])]
        codec: String,
        #[command(flatten)]
        src: Flags,
    },
    /// List the service categories and features `keep` accepts
    Services,
    #[command(hide = true, name = "_replenish")]
    Replenish {
        #[command(flatten)]
        src: Flags,
    },
}

fn main() {
    if let Err(e) = run(Cli::parse()) {
        eprintln!("simfast: {e:#}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> Result<()> {
    simctl::ensure_xcode()?;
    match cli.cmd {
        Cmd::Init { device, runtime, keep, no_slim, name, out } => init(device, runtime, keep, no_slim, name, out)?,
        Cmd::Check { src } => {
            let loaded = src.load()?;
            let (checks, pinned) = sim::check(&loaded.cfg)?;
            println!("{} ({})", describe(&loaded), Project::of(&loaded).name);
            for c in &checks {
                let mark = if c.ok() { "ok  " } else { "FAIL" };
                println!("  {mark} {:<14} {}{}", c.what, c.pinned, if c.ok() { String::new() } else { format!("  (this Mac: {})", c.actual) });
            }
            let Some(p) = pinned else { bail!("this Mac doesn't match the pins") };
            println!("  {} services disabled", p.disabled.len());
            match sim::existing_golden(&p)? {
                None => println!("  template not built yet (`simfast build`, or the first `simfast run`)"),
                Some(g) => match sim::check_untouched(&g) {
                    Ok(()) => println!("  ok   template       unchanged since it was built"),
                    Err(e) => bail!("{e:#}"),
                },
            }
            if let Some(w) = util::load_warning() {
                println!("  note: {w}; expect slow boots");
            }
        }
        Cmd::Build { rebuild, src } => {
            let cfg = src.load()?.cfg;
            let p = sim::pinned(&cfg)?;
            sim::golden(&p, rebuild)?;
            spare::replenish(&p, cfg.spares)?;
            let spares = if cfg.spares == 1 { "1 spare".to_string() } else { format!("{} spares", cfg.spares) };
            eprintln!("simfast: template and {spares} for {} are ready", p.label());
        }
        Cmd::Replenish { src } => {
            let cfg = src.load()?.cfg;
            let p = sim::pinned(&cfg)?;
            let r = spare::replenish(&p, cfg.spares);
            spare::record(&p, &r);
            r?;
        }
        Cmd::Run { file, open, mut src } => {
            let start = Instant::now();
            if file.is_some() {
                src.config = file; // the positional argument wins over $SIMFAST_CONFIG
            }
            let loaded = src.load()?;
            let cfg = loaded.cfg.clone();
            let project = Project::of(&loaded);
            let p = sim::pinned(&cfg)?;
            // Spares were slim-verified before freezing, and nothing can change
            // the store while they're frozen; claim checks they're still alive.
            let claim = spare::claim(&p, &project)?;
            let (udid, how) = match claim.udid {
                Some(u) => (u, "thawed a spare"),
                None => {
                    if claim.discarded > 0 {
                        eprintln!(
                            "simfast: the spare had been shut down outside simfast (Simulator.app quit, `simctl shutdown`, or a reboot); deleted it, booting one"
                        );
                    } else if cfg.spares > 0 && sim::existing_golden(&p)?.is_some() {
                        // Before the first template exists there is nothing to explain.
                        eprintln!("simfast: no spare ready ({}); booting one", spare::why_none(&p));
                    }
                    (sim::run(&p, &project)?, "booted")
                }
            };
            if cfg.spares > 0 {
                spare::replenish_in_background(&p, &loaded);
            }
            if open {
                let _ = Command::new("open").args(["-a", "Simulator", "--args", "-CurrentDeviceUDID", &udid]).status();
            }
            let load = if how == "booted" { util::load_note() } else { String::new() };
            eprintln!("simfast: {} {how}, ready in {:.1}s{load}", p.label(), start.elapsed().as_secs_f64());
            println!("{udid}");
        }
        Cmd::List { all, src } => {
            let here = if all { None } else { here(&src)? };
            if let Some((project, p)) = &here {
                eprintln!("simfast: {} ({}); --all for every config", project.name, p.label());
            }
            for d in simctl::devices(None)? {
                let tag = sim::tag(&d.name).unwrap_or("");
                let (what, owner, mine) = if d.name.starts_with(sim::PREFIX) {
                    let name = d.name.rsplit_once(" · ").map_or("?", |(_, r)| r.rsplit_once(" #").map_or(r, |(n, _)| n));
                    (d.state.to_lowercase(), name.to_string(), here.as_ref().map_or(true, |(pr, _)| pr.id == tag))
                } else if d.name.starts_with(spare::FROZEN) || d.name.starts_with(spare::WARMING) {
                    // A spare is only usable while booted; one shut down from outside is dead.
                    let state = match (d.name.starts_with(spare::FROZEN), d.state.as_str()) {
                        (true, "Booted") => "spare",
                        (true, _) => "spare (shut down)",
                        (false, _) => "preparing",
                    };
                    (state.to_string(), "(shared)".to_string(), here.as_ref().map_or(true, |(_, p)| p.key() == tag))
                } else {
                    continue;
                };
                if mine {
                    let label = d.name.split_once(": ").map_or("", |(_, r)| r).split(" · ").next().unwrap_or("").split(" #").next().unwrap_or("");
                    if all {
                        println!("{}  {:<17}  {:<34}  {}", d.udid, what, label, owner);
                    } else {
                        println!("{}  {:<17}  {}", d.udid, what, label);
                    }
                }
            }
        }
        Cmd::Stop { udids, all, src } => {
            let ours = sim::running()?;
            let targets: Vec<String> = if all {
                ours.iter().map(|d| d.udid.clone()).collect()
            } else if !udids.is_empty() {
                for u in &udids {
                    if !ours.iter().any(|d| &d.udid == u) {
                        bail!("{u} wasn't started by simfast; not touching it");
                    }
                }
                udids
            } else {
                let Some((project, _)) = here(&src)? else {
                    bail!("no config (no simfast.toml here or above, no -c, no pin flags): give UDIDs, a config, or --all for every simfast simulator");
                };
                let mine: Vec<String> = ours.iter().filter(|d| sim::tag(&d.name) == Some(&project.id)).map(|d| d.udid.clone()).collect();
                if mine.is_empty() {
                    eprintln!("simfast: {} has no running simulators", project.name);
                }
                mine
            };
            for u in targets {
                sim::stop(&u);
                eprintln!("simfast: stopped {u}");
            }
        }
        Cmd::Clean { all, other_pins, src } => {
            // Shut-down simfast simulators are never used again: Simulator.app quit,
            // `simctl shutdown all` or a reboot leaves them behind.
            let project = if all { None } else { here(&src)?.map(|(project, _)| project) };
            let mut n = 0;
            for d in sim::running()? {
                if d.state != "Booted" && (all || project.as_ref().is_some_and(|pr| sim::tag(&d.name) == Some(&pr.id))) {
                    sim::stop(&d.udid);
                    eprintln!("simfast: deleted shut-down simulator {}", d.udid);
                    n += 1;
                }
            }
            if all {
                n += spare::delete_spares(Some(None))?;
                n += sim::clean_goldens(None)?;
            } else if other_pins {
                let p = sim::pinned(&src.load()?.cfg)?;
                n += spare::delete_spares(Some(Some(&p.key())))?;
                n += sim::clean_goldens(Some(&p))?;
            } else {
                n += spare::delete_spares(None)?;
            }
            if n == 0 {
                eprintln!("simfast: nothing to clean (running simulators are left alone: `simfast stop` stops this config's)");
            }
        }
        Cmd::Record { output, udid, seconds, codec, src } => record(output, udid, seconds, &codec, &src)?,
        Cmd::Services => services(),
    }
    Ok(())
}

/// The given (or found) config and its pins, if there is one and this Mac matches it.
fn here(src: &Flags) -> Result<Option<(Project, sim::Pinned)>> {
    let Some(loaded) = src.load_optional()? else { return Ok(None) };
    Ok(Some((Project::of(&loaded), sim::pinned(&loaded.cfg)?)))
}

/// `simctl io recordVideo` with a lifetime that's easy to script: it stops on
/// Ctrl-C, SIGTERM (`kill <pid>`) or after `--for`, and always finalizes the file.
fn record(output: std::path::PathBuf, udid: Option<String>, seconds: Option<f64>, codec: &str, src: &Flags) -> Result<()> {
    let udid = match udid {
        Some(u) => u,
        None => {
            let Some((project, _)) = here(src)? else {
                bail!("no config here: give --udid, or a config (-c FILE)");
            };
            let mine: Vec<_> = sim::running()?
                .into_iter()
                .filter(|d| d.state == "Booted" && sim::tag(&d.name) == Some(&project.id))
                .collect();
            match mine.as_slice() {
                [d] => d.udid.clone(),
                [] => bail!("{} has no running simulator; start one with `simfast run`", project.name),
                many => bail!(
                    "{} has {} running simulators; pick one with --udid ({})",
                    project.name,
                    many.len(),
                    many.iter().map(|d| d.udid.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    };
    let mut child = Command::new(simctl::simctl_path())
        .args(["io", &udid, "recordVideo", &format!("--codec={codec}"), "--force"])
        .arg(&output)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()?;
    let pid = child.id() as i32;
    util::forward_stop_signals_to(pid);

    // simctl says "Recording started" once the first frame is in.
    let stderr = child.stderr.take().unwrap();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        use std::io::BufRead;
        let mut errors = vec![];
        for line in std::io::BufReader::new(stderr).lines().map_while(Result::ok) {
            if line.contains("Recording started") {
                let _ = started_tx.send(());
            } else if !line.trim().is_empty() {
                errors.push(line);
            }
        }
        errors
    });
    if started_rx.recv_timeout(std::time::Duration::from_secs(20)).is_ok() {
        let stop = seconds.map_or("Ctrl-C to stop".to_string(), |s| format!("stopping after {s}s"));
        eprintln!("simfast: recording {udid} to {} ({stop})", output.display());
        if let Some(s) = seconds {
            let deadline = Instant::now() + std::time::Duration::from_secs_f64(s);
            while Instant::now() < deadline && child.try_wait()?.is_none() {
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
            util::interrupt(pid);
        }
    }
    let status = child.wait()?;
    let errors = reader.join().unwrap_or_default();
    let size = std::fs::metadata(&output).map(|m| m.len()).unwrap_or(0);
    if size == 0 {
        bail!("recording failed{}", if errors.is_empty() { format!(" ({status})") } else { format!(": {}", errors.join("; ")) });
    }
    eprintln!("simfast: saved {} ({:.1} MB)", output.display(), size as f64 / 1e6);
    Ok(())
}

fn describe(loaded: &Loaded) -> String {
    match &loaded.source {
        Source::File(p) => p.display().to_string(),
        Source::Stdin => "config from stdin".into(),
        Source::Flags => "config from flags".into(),
    }
}

fn init(
    device: Option<String>,
    runtime: Option<String>,
    keep: Option<Vec<String>>,
    no_slim: bool,
    name: Option<String>,
    out: Option<String>,
) -> Result<()> {
    // Where to write: -c FILE, -c - (stdout), else ./simfast.toml. Never a parent
    // directory's: in a monorepo that would silently re-pin another project.
    let to_stdout = out.as_deref() == Some("-");
    let path = match &out {
        Some(p) if p != "-" => std::path::PathBuf::from(p),
        _ => std::path::PathBuf::from(config::FILE),
    };
    if out.is_none() && !path.is_file() {
        if let Some(parent) = config::find() {
            eprintln!("simfast: note: creating a new {} here; {} (above) is left as is", config::FILE, parent.display());
        }
    }
    // Re-pinning keeps the choices of the file being replaced.
    let old: Option<Config> = if to_stdout || !path.is_file() {
        None
    } else {
        Some(Flags { config: Some(path.display().to_string()), ..Default::default() }.load()?.cfg)
    };
    let cat = simctl::catalog()?;

    let defaulted = device.is_none() && old.is_none();
    let query = device.or_else(|| old.as_ref().map(|c| c.device.clone())).unwrap_or_else(|| "iPhone 17 Pro".into());
    let norm = |s: &str| s.to_lowercase().replace('-', " ");
    let q = norm(&query);
    let mut hits: Vec<_> =
        cat.devicetypes.iter().filter(|t| norm(&t.name) == q || t.identifier.eq_ignore_ascii_case(&query)).collect();
    if hits.is_empty() {
        hits = cat.devicetypes.iter().filter(|t| norm(&t.name).contains(&q)).collect();
    }
    if hits.is_empty() {
        let mut near: Vec<(usize, &str)> = cat.devicetypes.iter().map(|t| (util::distance(&q, &norm(&t.name)), t.name.as_str())).collect();
        near.sort();
        let near: Vec<&str> = near.iter().take(3).map(|(_, n)| *n).collect();
        bail!("no device type matches {query:?}; did you mean {}? (all: `xcrun simctl list devicetypes`)", near.join(", "));
    }
    if hits.len() > 1 {
        let names: Vec<&str> = hits.iter().map(|t| t.name.as_str()).collect();
        bail!("{query:?} matches several device types: {}", names.join(", "));
    }
    let dtype = hits[0];

    let mut rts: Vec<&simctl::Runtime> = cat.runtimes.iter().filter(|r| r.is_available && r.supports(&dtype.identifier)).collect();
    if let Some(rq) = &runtime {
        rts.retain(|r| &r.build == rq || &r.version == rq);
    }
    let Some(rt) = rts.into_iter().max_by_key(|r| r.version_parts()) else {
        bail!("no installed runtime for {}{}", dtype.name, runtime.map(|r| format!(" matching {r:?}")).unwrap_or_default());
    };

    let slim = !no_slim && (keep.is_some() || old.as_ref().map_or(true, |c| c.slim));
    let keep = keep.or_else(|| old.as_ref().map(|c| c.keep.clone())).unwrap_or_default();
    let disabled = if slim { slim::disabled_set(&keep, rt)? } else { Default::default() };
    let cfg = Config {
        device: dtype.identifier.clone(),
        runtime: rt.build.clone(),
        coresimulator: simctl::coresimulator_version()?,
        slim,
        keep,
        services: if slim { util::short_hash(disabled.iter()) } else { String::new() },
        spares: old.as_ref().map_or(1, |c| c.spares),
        name: name.or_else(|| old.as_ref().and_then(|c| c.name.clone())),
    };
    let comment = format!("{} ({}), {} {}", dtype.name, dtype.model_identifier, rt.name, rt.build);
    let text = cfg.render(&comment);
    if to_stdout {
        print!("{text}");
    } else {
        std::fs::write(&path, text).map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
    }

    // What changed (re-pin) or what was chosen (first pin). On stderr: stdout may be the config.
    let verb = if old.is_some() { "Re-pinned" } else { "Pinned" };
    let target = if to_stdout { "a config (on stdout)".to_string() } else { path.display().to_string() };
    eprintln!("{verb} {target} to this Mac:");
    let row = |what: &str, new: String, old: Option<String>| match old {
        Some(o) if o != new => eprintln!("  {what:<14} {o} -> {new}"),
        _ => eprintln!("  {what:<14} {new}"),
    };
    let default_hint = if defaulted { "   (default; e.g. `simfast init \"iPhone Air\"` for another)" } else { "" };
    row("device", format!("{} ({}){default_hint}", dtype.name, dtype.model_identifier), old.as_ref().filter(|c| c.device != cfg.device).map(|c| c.device.clone()));
    row("iOS runtime", format!("{} ({})", rt.build, rt.name), old.as_ref().map(|c| c.runtime.clone()));
    row("CoreSimulator", cfg.coresimulator.clone(), old.as_ref().map(|c| c.coresimulator.clone()));

    if slim {
        // Name the user-visible features that are off, so nobody debugs a missing push for an hour.
        let off: Vec<&str> = slim::catalog()
            .features
            .iter()
            .filter(|f| f.labels.iter().any(|l| disabled.contains(l)))
            .map(|f| f.id.as_str())
            .collect();
        let shown: Vec<&str> = off.iter().take(8).copied().collect();
        let more = if off.len() > shown.len() { format!(", +{} more", off.len() - shown.len()) } else { String::new() };
        eprintln!("  slim           {} background services off, including {}{more}", disabled.len(), shown.join(", "));
        eprintln!("                 keep any with `simfast init --keep {}`; see `simfast services`", shown.first().unwrap_or(&"push"));
    } else {
        eprintln!("  slim           off: all background services run");
    }
    if cfg.spares > 0 {
        eprintln!("  spares         {} frozen simulator(s) kept ready (~1 GB RAM each) so `run` takes under a second", cfg.spares);
    }
    if to_stdout {
        eprintln!("\nSend it anywhere, e.g. `simfast init ... -c - > sims/ui.toml`, then `ssh <mac> simfast run - < sims/ui.toml`.");
    } else if old.is_none() {
        let run = if path.file_name().and_then(|f| f.to_str()) == Some(config::FILE) { "simfast run".to_string() } else { format!("simfast run {}", path.display()) };
        eprintln!("\nNext: `{run}` (the first run builds a template, 1-2 min), and commit {}.", path.display());
    }
    Ok(())
}

fn services() {
    let c = slim::catalog();
    println!("Categories (slim disables all of them; `keep` one to leave it on):");
    for cat in &c.categories {
        println!("  {:<13} ~{:>4} MB  {:>3} daemons  {}", cat.id, cat.approx_mb, cat.labels.len(), cat.downside);
    }
    println!("\nFeatures (`keep` one to leave on just the daemons it needs):");
    for f in &c.features {
        println!("  {:<16} {}", f.id, f.name);
    }
    println!("\nIndividual launchd labels (com.apple.*) work in `keep` too.");
    println!("Service catalog from {}", c.source);
}
