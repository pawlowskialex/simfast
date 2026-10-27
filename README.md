# simfast

Clean iOS simulators in under a second, pinned to an exact device, iOS build and
CoreSimulator version.

```sh
simfast init "iPhone 17 Pro"     # once per project: writes simfast.toml (commit it)
UDID=$(simfast run)              # a clean, booted simulator, ready to launch apps
xcodebuild test -scheme App -destination "id=$UDID"
simfast stop                     # stop this project's simulators
```

Each `run` gives you a fresh device that starts from exactly the same state as the last one.
Nothing is left over from previous runs, and a Mac that doesn't match the pins is refused.

## How much faster

These were measured back to back on one Mac (iPhone Air, iOS 26.5). "Ready" means an app can
be launched.

| Getting a clean simulator | Time to ready |
|---|---|
| `simctl create` + boot | 36–50s |
| `simctl erase` + boot | 37–69s |
| **`simfast run`, cold** | **~19s** |
| **`simfast run`, from a frozen spare** (the default) | **~0.9s** (0.4s for simfast, plus the app launch) |

Each simulator also uses about half the memory (~0.75 GB instead of ~1.6 GB), because simfast
turns off background services that apps rarely need. See [Slimming](#slimming).

The first `run` for a new pin set builds a template, which takes 1–2 minutes once.

## Install

Requires macOS with Xcode and the iOS simulator runtime you want to use. To build it, you
need Rust.

```sh
cargo install --path .           # from this directory; installs ~/.cargo/bin/simfast
```

## Everyday use

**Set up a project** (once). Run this in the project root:

```sh
simfast init                     # iPhone 17 Pro, newest installed iOS
simfast init "iPhone Air"        # or pick a device
simfast init "iPhone Air" -r 26.5 --keep push,universal-links
```

`init` writes `simfast.toml` in the current directory (or wherever `-c` says), re-pinning it if
it exists, and prints what it pinned, which features slimming turns off, and what to do next. Commit
`simfast.toml`, so everyone and CI use the same simulator. simfast finds the file from any
subdirectory, the same way git does.

**Get a simulator:**

```sh
UDID=$(simfast run)              # stdout is only the UDID, so it's safe in scripts
simfast run --open               # same, and show it in Simulator.app
```

Use the UDID with anything that takes one:

```sh
xcodebuild test -scheme App -destination "id=$UDID"
xcrun simctl install "$UDID" path/to/App.app
xcrun simctl launch "$UDID" com.example.App
```

**Record the screen:**

```sh
simfast record run.mp4           # this config's running simulator; Ctrl-C to stop
simfast record run.mp4 --for 30  # or stop after 30s
simfast record run.mp4 --udid "$UDID" &   # in scripts: stop it with `kill $!`
```

The file is always finalized, whether the recording ends with Ctrl-C, `kill` or `--for`. Use
`--codec hevc` for smaller files (the default, h264, plays everywhere).

**Look and clean up:**

```sh
simfast list                     # this project's simulators and spares
simfast list --all               # every project's, with owners
simfast stop                     # stop this project's simulators
simfast stop "$UDID"             # or specific ones
simfast stop --all               # every simfast simulator on this Mac
```

`stop` deletes the simulator. Every `run` gives you a new one.

## In CI

```sh
simfast check                    # fail fast if this runner doesn't match the pins
UDID=$(simfast run)
trap 'simfast stop "$UDID"' EXIT
xcodebuild test -scheme App -destination "id=$UDID"
```

- Run `simfast build` when you provision the runner. It builds the template and freezes the
  spares, so the first job doesn't pay the 1–2 minute template build.
- On short-lived runners that do one job and disappear, set `spares = 0`. A spare only pays
  off if something runs again later on the same machine.
- `simfast check` exits non-zero and names each mismatch, for example a runner with a
  different Xcode.

## Several configs, pipes and flags

A project can have any number of configs, and a config can come from anywhere:

```sh
simfast run                                  # simfast.toml, found by walking up
simfast run sims/ui_tests.toml               # a specific file (or -c FILE on any command)
simfast run - < sims/ui_tests.toml           # stdin
simfast run --device "iPhone Air" --runtime 23F77 --coresimulator 1051.55 --services 805a9234
```

Every source goes through the same pin checks, so nothing runs half-pinned. A file and pin
flags can't be combined. To write a new config, use `simfast init -c FILE`; `-c -` prints it
to stdout instead.

```sh
simfast init "iPhone Air" -c sims/ui_tests.toml --name ui-tests --keep push
```

**Grouping.** `list` and `stop` act on one config's simulators. A config with
`name = "ui-tests"` is grouped by that name, the same on every Mac, whether it arrives as a
file or through a pipe. Without a name, a file is grouped by its path, and stdin or flags are
grouped by the config's content. So piping the same config to `stop` finds the simulators
it started:

```sh
simfast list -c sims/ui_tests.toml
simfast stop -c - < sims/ui_tests.toml
```

Spares depend only on the pins, so configs with identical pins share them.

**Monorepo to a pool of Macs.** Check configs in next to the tests, then send them to
whichever Mac runs the job. The pins travel with the config, and a Mac that doesn't match is
refused with the reason:

```sh
# sims/ui_tests.toml is checked into the repo
UDID=$(ssh mac-07 simfast run - < sims/ui_tests.toml)
ssh mac-07 xcodebuild test-without-building -xctestrun ... -destination "id=$UDID"
ssh mac-07 simfast stop -c - < sims/ui_tests.toml
```

## simfast.toml

`simfast init` writes this file. Edit `keep` and `spares` by hand if you like, but re-run
`simfast init` after changing `keep`, because the services hash has to be updated.

```toml
name = "ui-tests"            # optional: groups these simulators in list/stop, on every Mac
device = "com.apple.CoreSimulator.SimDeviceType.iPhone-17-Pro"  # iPhone 17 Pro (iPhone18,1), iOS 26.5 23F77
runtime = "23F77"            # iOS runtime build, not just "26.5"
coresimulator = "1051.55"    # CoreSimulator version on the Mac

slim = true                  # turn off ~170 background services
keep = ["push"]              # ...except these (see `simfast services`)
services = "805a9234"        # hash of the exact set turned off; written by init

spares = 1                   # frozen simulators kept ready for `run` (~1 GB RAM each)
```

| Key | Meaning |
|---|---|
| `name` | Optional. Groups this config's simulators for `list` and `stop`, on every Mac. |
| `device` | Exact device type: an identifier, or an exact name such as `"iPhone Air"`. |
| `runtime` | Exact iOS runtime **build**. A runtime can update in place under the same "iOS 26.5" name, so the build is what's pinned. |
| `coresimulator` | The simulator framework version. There is one per Mac, shared by every installed Xcode, and installing a newer Xcode replaces it. |
| `slim`, `keep`, `services` | Which background services are off. See [Slimming](#slimming). |
| `spares` | How many frozen, ready-to-go simulators to keep for this pin set. `0` turns this off. |

Every command checks the Mac against `device`, `runtime`, `coresimulator` and `services`, and
refuses to run if any differ. Nothing falls back to "newest". To move to a new Xcode or
runtime on purpose, run `simfast init` (it keeps your device and `keep` choices and shows what
changed) and commit the file.

## Slimming

With `slim = true`, a simulator boots with about 170 background services turned off. That
roughly halves its memory and removes most background noise. Services your app actually
uses are switched off too, unless you keep them:

| If your app uses… | keep |
|---|---|
| push notifications | `push` |
| in-app purchase / StoreKit | `storekit` |
| universal links / associated domains | `universal-links` |
| Contacts, Calendar, Photos pickers | `contacts`, `calendar`, `photos` |
| HealthKit, HomeKit | `health`, `homekit` |
| Spotlight | `spotlight` |

```sh
simfast init --keep push,storekit    # then commit simfast.toml
simfast services                     # every category and feature you can keep
```

`keep` also accepts whole categories (`siri`, `icloud`, …) and individual launchd labels
(`com.apple.locationd`). Slimming needs iOS 18.5 or later. Use `slim = false`, or
`simfast init --no-slim`, for a stock simulator.

## Spares

With `spares = 1` (the default), simfast keeps one simulator booted and *frozen* in the
background. A frozen spare uses no CPU, only memory. `run` thaws it in 0.4s, then starts
preparing the next one in the background (30–50s).

- If no spare is ready, `run` boots one (~19s) and says why: either one is still being
  prepared, or the last attempt failed. A failure message includes the log path.
- A spare is only handed out for identical pins, so it is just as clean as a cold boot. One
  thing differs: the simulator's clock jumps forward when it is thawed, so timers that came
  due fire right away.
- A Mac reboot discards the spares, and simfast notices and boots instead.
- To free the memory, set `spares = 0` and run `simfast build`.

## Commands

| Command | |
|---|---|
| `simfast init [DEVICE] [-r VERSION\|BUILD] [--keep a,b] [--no-slim] [--name N] [-c FILE\|-]` | Pin a config to this Mac (default `simfast.toml`; `-c -` prints it), or re-pin an existing one. |
| `simfast check` | Compare this Mac with the pins and check the template. |
| `simfast run [CONFIG\|-] [--open]` | Print the UDID of a clean simulator once apps can launch. |
| `simfast list [--all]` | Show this config's (or every config's) simulators and spares. |
| `simfast stop [UDID…] [--all]` | Stop and delete this config's simulators, or the given ones, or all. |
| `simfast build [--rebuild]` | Build the template and spares now, or trim spares to `spares`. |
| `simfast clean [--other-pins\|--all]` | Remove leftovers: this config's shut-down simulators and dead spares. `--other-pins` also deletes templates and spares for other pins (other projects on this Mac rebuild theirs on their next run). `--all` deletes every template, spare and shut-down simulator. Running simulators are always left alone. |
| `simfast record FILE [--for S] [--udid U] [--codec h264\|hevc]` | Record a simulator's screen until Ctrl-C, `kill`, or `--for` seconds. |
| `simfast services` | List what `keep` accepts. |

`check`, `build`, `run`, `list`, `stop` and `clean` all take a config the same way: by
default `simfast.toml` (found by walking up), or `-c FILE`, `-c -` (stdin), `$SIMFAST_CONFIG`,
or pin flags (`--device`, `--runtime`, `--coresimulator`, `--services`, `--keep`, `--no-slim`,
`--spares`, `--name`).

## Troubleshooting

**`this Mac doesn't match simfast.toml: runtime: pinned 23F81a, this Mac: not installed (have 23F77 …)`**
The Mac has a different Xcode, runtime or CoreSimulator than the project pins. Either install
what's pinned, or run `simfast init` to re-pin to this Mac and commit the change. The other
pins fail the same way, for example `coresimulator: pinned 1051.55, this Mac: 1051.60` after
an Xcode update.

**`services: pinned …, this Mac: …`**
`keep` was edited by hand, or a newer simfast changed its list of services. Run `simfast init`
to update the hash, and commit the change.

**`the template changed after it was built (…); rebuild it with simfast build --rebuild`**
Something modified the template. The usual cause is running a plain `xcrun simctl clone` on
it: copies made that way keep writing into their source. Rebuild it with
`simfast build --rebuild` (1–2 min).

**Push, StoreKit or universal links don't work in the simulator.**
Slimming turned them off. Run `simfast init --keep push` (or `storekit`, `universal-links`).

**`run` takes ~19s instead of under a second.**
No spare was ready, and `run` printed the reason. Common causes: this is the first run after
`init` or a reboot, two runs happened back to back, or `spares = 0`. If it says the last
attempt failed, see `~/Library/Developer/simfast/spares.log`.

**Simulators (and spares) are suddenly shut down.**
Quitting Simulator.app shuts down every booted simulator on the Mac, including frozen spares
it wasn't showing. `simctl shutdown all` and a reboot do the same. simfast copes: `list`
shows a dead spare as `spare (shut down)`, the next `run` says what happened and boots a
replacement, and `simfast clean` (or `stop`) removes the shut-down simulators.

**`note: this Mac is heavily loaded …`**
The Mac's load average is more than twice its core count. Boots will be slower than the
numbers above.

**`no device type matches "…"; did you mean …?`**
Use one of the suggested names, or any name from `xcrun simctl list devicetypes`.

## Uninstall

```sh
simfast stop --all && simfast clean --all     # simulators, spares and templates
rm -rf ~/Library/Developer/simfast            # logs and lock files
cargo uninstall simfast
```

## How it works

- **Templates.** For each pin set, simfast boots one stock simulator once, so that first-boot
  setup is done, then shuts it down. Templates live in
  `~/Library/Developer/CoreSimulator/Simfast`, where Xcode doesn't list them. They are
  stamped when built and refused if any of their files change later.
- **Independent copies.** `run` creates an empty device of the pinned type and runtime,
  swaps in an APFS clone of the template's data (a single `clonefile()` call, 0.3s), then
  sanitizes it. Symlinks and plists that point into the template are rewritten, and system
  databases that store absolute paths are deleted so the simulator rebuilds them. This
  matters because a plain `simctl clone` keeps those paths, and its copies write into the
  template. After boot, simfast checks with `lsof` that no process has a template file open.
- **Ready means ready.** `run` waits until apps can launch, not just until `bootstatus`
  says the device has booted. On iOS 26.5 the iPhone Air's first boot always records its
  data migration as "failed", even though all 35 migration steps succeed, and that made
  `bootstatus` return ~15s early. When every step succeeded, the template build corrects
  the record.
- **Slimming** writes launchd's list of disabled services for the copy before its first boot,
  so it costs no extra reboot. simfast then reads the list back after boot to confirm it
  took effect.
- **Spares** are simulators whose processes are stopped with SIGSTOP, every process except
  `launchd_sim` itself, so outside commands such as `simctl shutdown all` still work.

## License

MIT. The service catalog (`src/catalog.json`), the slimming technique and the
clone-sanitizing recipe come from [simslim](https://github.com/MobAI-App/simslim) (MIT,
Copyright (c) 2026 Interlap). See `NOTICE`.
