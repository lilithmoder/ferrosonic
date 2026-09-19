# macOS Port — Handover

This document records the state of the macOS (Intel and Apple Silicon) port and
contains a paste-ready prompt for an OpenCode session running on a Mac.

## Summary

- There is no upstream prebuilt macOS binary and `install.sh` is Linux-only.
  For this fork, the `macos-release` GitHub Actions workflow builds
  downloadable `x86_64-apple-darwin` (Intel) and `aarch64-apple-darwin`
  (Apple Silicon) binaries on every push to `main`/`macos-port` (see
  "Fast-build workflow" below), so a native build from source is only needed
  when fixing macOS-only code.
- The codebase is close to portable already. Build-time native dependencies are
  effectively nil: TLS is `rustls`, D-Bus is pure-Rust `zbus`, cover-art `chafa`
  is `dlopen`ed at runtime, and credentials use the platform keychain API.
- `x86_64-apple-darwin` (Intel) and `aarch64-apple-darwin` (Apple Silicon) are
  both Rust Tier-1 targets.
- **Status: CI-verified, partially runtime-verified (macOS 15 Sequoia, Intel).**
  The branch builds cleanly for `x86_64-apple-darwin` in CI (including a
  lib/bins clippy pass that type-checks the `#[cfg(target_os = "macos")]`
  modules, which the Linux build cannot see). Playback, the daemon, and the
  Keychain have been confirmed working on hardware. **Notifications do not
  appear** on Sequoia via `osascript` and need a functional fix (see decision
  points). MPRIS over Homebrew D-Bus remains untested.

## What changed in this branch

| File | Change |
|---|---|
| `src/daemon/notify.rs` | Added a `#[cfg(target_os = "macos")]` `Notifier` that shells out to `osascript` (`display notification`). Widened the no-op stub gate to platforms other than Linux/macOS. The module-level doc now covers both backends. |
| `src/audio/pipewire.rs` | `PipeWireController` now records whether the construction probe could execute `pw-metadata`. When it cannot (missing binary, e.g. macOS), `set_rate` / `clear_forced_rate` are silent no-ops instead of warning on every track. Runner-injection tests are unaffected. |
| `README.md` | Marked PipeWire/WirePlumber/D-Bus as Linux-specific in the dependency table, corrected the stale "OpenSSL/D-Bus dev headers" build note, and added a macOS section (Homebrew deps, config path, feature caveats, CI artifacts). |
| `.github/workflows/macos-release.yml` | Push/dispatch-triggered artifact builds (`release-fast` profile) for `x86_64-apple-darwin` and `aarch64-apple-darwin` with caching and a lib+bins clippy pass per target, plus a `cargo test --lib` job on Apple Silicon. |
| `.github/workflows/linux-release.yml` | New: Linux counterpart building `x86_64-unknown-linux-gnu` on `main` pushes. |
| `.github/workflows/test.yml` | Full test gate now triggers on `main`/`macos-port` pushes; the earlier report-only `build_macos` matrix was folded into `macos-release.yml` (it duplicated the build with a heavier `--all-targets` clippy on the same runners). |
| `Cargo.toml` | Added `[profile.release-fast]` (thin LTO, 16 codegen units). The canonical `[profile.release]` is unchanged. |

## Feature behaviour on macOS

| Area | Behaviour |
|---|---|
| Core playback (mpv) | Works. mpv uses CoreAudio. Gapless/prefetch flags are platform-neutral. |
| Daemon + queue persistence | Works. IPC is a Unix domain socket; with no `XDG_RUNTIME_DIR` it falls back to `/tmp/ferrosonic-<uid>/ferrosonicd.sock`. |
| Keychain | Works. keyring v4's default `v1` feature auto-selects macOS Keychain Services. |
| Cover art | Works. `probe_chafa` already tries `libchafa.dylib` and both Homebrew prefixes (`/opt/homebrew/lib` for ARM, `/usr/local/lib` for Intel). Terminal image protocols (iTerm2/kitty/sixel) are handled by `ratatui-image`. |
| Sample-rate switching | No PipeWire, but `MacosAudioMode` passes mpv's CoreAudio options instead: `"physical-format"` makes the device follow each track's rate (closest analog to `clock.force-rate`), `"exclusive"` adds hog mode. Default `"off"` (shared). The `RateSwitchDelayMs` settle pause is skipped on macOS, so playback never stalls waiting for a re-clock that cannot happen. The decoded quality readout still works. |
| Notifications | Prefers Homebrew `terminal-notifier` when installed (modern `UserNotifications`, banner replacement, cover art); falls back to text-only `osascript`, which **does not appear on macOS 15 Sequoia** (runtime-confirmed). Failures log at `warn`. |
| MPRIS | Compiles and runs against a D-Bus session bus, but macOS has no native MPRIS consumer, so it does **not** provide media-key / Control Center integration by itself. |
| systemd unit / installer | N/A. |

## Fast-build workflow (recommended)

Do not run full `cargo build --release` or `--all-targets` checks on the Mac:
the release profile uses whole-program LTO (`lto = true`, `codegen-units = 1`)
and `--all-targets` compiles ~151 integration-test binaries. That combination
is what made the first port attempt unbearably slow. Two faster paths:

### A. Download a prebuilt artifact (no local build)

1. Push to `main` or `macos-port` on the fork
   (`https://github.com/lilithmoder/ferrosonic`) — every push builds a fresh
   artifact automatically.
2. Open the finished run under the Actions tab → **macos-release**, download
   `ferrosonic-macos-aarch64` (Apple Silicon) or `ferrosonic-macos-x86_64`
   (Intel), then:
   ```bash
   shasum -a 256 -c ferrosonic-macos-aarch64.sha256
   chmod +x ferrosonic-macos-aarch64
   ./ferrosonic-macos-aarch64 --standalone
   ```
   The first CI run compiles all dependencies cold; later runs reuse the cache
   and are much quicker. The Mac itself never compiles anything.

### B. Local fast iteration on the Mac

For fixing the macOS-only compile errors (the `#[cfg(target_os = "macos")]`
code is not type-checked by the Linux build):

1. Toolchain and runtime:
   ```bash
   xcode-select --install
   brew install llvm mpv cava chafa dbus   # dbus only for MPRIS
   ```
2. Faster linker (`~/.cargo/config.toml`, user-level, not committed; Homebrew
   prefix is `/usr/local` on Intel):
   ```toml
   [target.x86_64-apple-darwin]
   linker = "clang"
   rustflags = ["-C", "link-arg=-fuse-ld=/usr/local/opt/llvm/bin/ld64.lld"]
   ```
3. Compile-error loop (metadata only, no codegen or link):
   `cargo check --bin ferrosonic`
4. Run/debug build (dev profile, no LTO, incremental):
   `cargo build --bin ferrosonic && ./target/debug/ferrosonic --standalone`
5. Optimized-but-quick local build:
   `cargo build --profile release-fast --bin ferrosonic`
6. Never run `cargo clippy --all-targets` or `cargo nextest` on the Mac; run
   those on the Linux host or let CI do it.

Smoke test with whichever binary you have, then report back: exact commands,
observed output, code fixes (file:line), and any remaining failures.

## Mac-side checklist (full verification, once the binary runs)

1. Toolchain and runtime (only needed for path B):
   ```bash
   xcode-select --install
   brew install mpv cava chafa dbus   # dbus only for MPRIS
   ```
2. Smoke test: see the prompt below.
3. Report back: exact commands, observed output, code fixes (file:line), and any
   remaining failures.

## Decision points to settle on the Mac

1. **Native media keys.** MPRIS is not sufficient on macOS, and since macOS
   15.4 the private MediaRemote framework rejects unentitled processes, so the
   only sanctioned route is `MPNowPlayingInfoCenter` + `MPRemoteCommandCenter`.
   The open question is whether those work from a bare CLI process (no
   `NSApplication`/app bundle). `souvlaki` and `playwire` document that macOS
   needs a run loop, so run this spike on the Mac before choosing an
   implementation:

   ```swift
   // /tmp/nowplaying-probe.swift — run: swift /tmp/nowplaying-probe.swift
   import MediaPlayer
   import Foundation

   let info = MPNowPlayingInfoCenter.default()
   info.nowPlayingInfo = [
       MPMediaItemPropertyTitle: "Ferrosonic probe",
       MPMediaItemPropertyArtist: "Test Artist",
       MPMediaItemPropertyPlaybackDuration: 300.0,
       MPNowPlayingInfoPropertyElapsedPlaybackTime: 0.0,
       MPNowPlayingInfoPropertyPlaybackRate: 1.0,
   ]
   info.playbackState = .playing

   let cc = MPRemoteCommandCenter.shared()
   cc.playCommand.addTarget { _ in print("play"); return .success }
   cc.pauseCommand.addTarget { _ in print("pause"); return .success }
   cc.togglePlayPauseCommand.addTarget { _ in print("toggle"); return .success }
   cc.nextTrackCommand.addTarget { _ in print("next"); return .success }
   cc.previousTrackCommand.addTarget { _ in print("prev"); return .success }
   cc.changePlaybackPositionCommand.addTarget { event in
       if let e = event as? MPChangePlaybackPositionCommandEvent {
           print("seek \(e.positionTime)")
       }
       return .success
   }

   print("now playing set; press media keys / check Control Center")
   RunLoop.main.run()
   ```

   Check whether the track appears in Control Center and whether the media
   keys print events. Repeat with the process detached (`nohup swift ... &`) to
   mimic the daemon. If events arrive without a bundle, implement a
   `#[cfg(target_os = "macos")]` Now Playing backend fed by the daemon's
   `MprisPropertySnapshot`/`build_metadata_for` seams and the `DaemonRequest`
   command path; if not, a signed helper app bundle is required. Either way
   this is a new subsystem — scope it separately.
2. **Notifications.** Runtime testing on macOS 15 Sequoia showed no banner
   appears from `osascript` `display notification` at all. The notifier now
   prefers Homebrew's `terminal-notifier` when it is on PATH
   (`brew install terminal-notifier`; v3 is maintained, rebuilt on
   `UserNotifications`, and has Sequoia/Tahoe bottles), using `-group` to
   replace the previous banner and `-contentImage` for cover art. Without it,
   the `osascript` fallback remains, and failures now log at `warn` level so
   they are visible in `ferrosonicd.log` instead of only with `-v`.
   To diagnose a still-missing banner on the Mac:
   - `osascript -e 'display notification "test" with title "ferrosonic"'` from
     Terminal; check System Settings -> Notifications for Script Editor (grant
     if listed) and for terminal-notifier.
   - `log stream --predicate 'process == "osascript" or process == "terminal-notifier"'`
     while a track changes, and check the daemon log for the new `warn` line.
   If both paths are blocked by TCC, the remaining option is a signed helper
   app bundle (`UNUserNotificationCenter`); scope that separately.
3. **Gate the macOS build.** The old report-only `build_macos` matrix in
   `test.yml` was folded into the `macos-release` workflow, which now builds
   the macOS binary and runs a lib+bins clippy pass on every push to
   `main`/`macos-port`. To make that block instead of just report, have the
   clippy step use `-D warnings` once the port is verified on hardware.
4. **Surface `MacosAudioMode` in Settings.** It is config-file only for now.
   Once `physical-format` / `exclusive` are verified against real DACs and
   Bluetooth outputs, add a Settings row (the page's row list is currently a
   fixed array, so this needs a small conditional-row change) and decide
   whether the mode can be switched live rather than at the next mpv start.

## Transfer

All work — the personal features, the macOS port, and the fast-build tooling —
lives on the fork at `https://github.com/lilithmoder/ferrosonic`:

- `main` (default branch): the primary development line. Future features land
  here, so they are never left out of the macOS build.
- `macos-port`: branch for Mac-side porting and verification work. Merge
  `main` into it (or fast-forward it) to pick up new features; the
  `macos-release` workflow builds artifacts on pushes to either branch.
- `master`: untouched mirror of the upstream project, used for pulling
  upstream updates.

On the Mac:

```bash
git clone https://github.com/lilithmoder/ferrosonic.git   # lands on main
cd ferrosonic
```

(The changes were previously moved as `ferrosonic-macos-port.bundle`; the fork
replaces that.)

---

## Paste-ready prompt for the Mac session

```text
You are working in the Ferrosonic repo (a Rust terminal Subsonic client). The
project was developed for Linux (PipeWire sample-rate switching, freedesktop
D-Bus notifications/MPRIS). The macOS port is committed on the `macos-port`
branch (and merged into `main`) of the fork at
https://github.com/lilithmoder/ferrosonic; CI already builds it for
x86_64-apple-darwin and aarch64-apple-darwin on every push. Your job is to
validate runtime behaviour on this Mac, fix defects, and report.

Constraints:
- Platform-specific code stays behind #[cfg(target_os = "macos")].
- Do not add dependencies without justification; prefer shelling out to system
  tools for small jobs.
- No unwrap()/expect() in production code. Run `cargo fmt` when done.
- Do not commit or push unless explicitly asked.

Setup:
  git clone https://github.com/lilithmoder/ferrosonic.git && cd ferrosonic
  xcode-select --install
  brew install mpv cava chafa terminal-notifier dbus
  # dbus only if you test MPRIS; terminal-notifier for reliable notifications

Get a binary (choose ONE; do NOT run `cargo build --release` or
`--all-targets` checks locally - the release profile uses whole-program LTO
and --all-targets compiles ~151 test binaries, which is what made the first
port attempt unbearably slow):
1. Preferred: download `ferrosonic-macos-aarch64` (Apple Silicon) or
   `ferrosonic-macos-x86_64` (Intel) from the latest green macos-release run
   at https://github.com/lilithmoder/ferrosonic/actions, then:
     shasum -a 256 -c ferrosonic-macos-aarch64.sha256
     chmod +x ferrosonic-macos-aarch64
2. Only when editing macOS-only code, build locally the fast way:
     cargo check --bin ferrosonic                       # compile-error loop
     cargo build --bin ferrosonic && ./target/debug/ferrosonic --standalone
     cargo build --profile release-fast --bin ferrosonic   # quick optimized
   Optional faster linker: brew install llvm, and in ~/.cargo/config.toml:
     [target.x86_64-apple-darwin]
     linker = "clang"
     rustflags = ["-C", "link-arg=-fuse-ld=/usr/local/opt/llvm/bin/ld64.lld"]

Smoke test (record exact commands and observed results):
- Standalone playback: run the binary with `--standalone` against a test
  Navidrome config (use `./ferrosonic-macos-aarch64 --standalone` for the CI
  artifact, or `./target/debug/ferrosonic --standalone` for a local build).
  Verify play/pause, seek, next/prev, gapless, queue edits,
  ReplayGain, cover art (iTerm2/kitty/sixel), cava, resize, and mouse input.
- Daemon: run normally (daemon mode on). Confirm the daemon auto-spawns
  detached, the TUI reconnects, and the queue + position persist across a
  daemon restart (check queue.json). Confirm --standalone still works.
- Keychain: set the password on the Server page (F5). Confirm it lands in the
  macOS Keychain (`security find-generic-password -s ferrosonic`) and is not
  written as plaintext in the config.
- Notifications: with Notifications=true, play tracks and confirm a banner
  appears (install terminal-notifier first; the notifier prefers it). If not,
  check ferrosonicd.log for the new warn line, then run the diagnostics in the
  "Decision points" section above and report.
- MPRIS: `brew services start dbus` (or `dbus-launch --sh-syntax`), then check:
    dbus-send --session --print-reply --dest=org.freedesktop.DBus \
      /org/freedesktop/DBus org.freedesktop.DBus.ListNames | grep mpris
  Note: macOS has no native MPRIS consumer, so media keys will NOT work via
  MPRIS. Report this and give a recommendation on native Now Playing.

Notes:
- Config/logs/queue live under ~/Library/Application Support/ferrosonic/.
  Override with FERROSONIC_CONFIG_DIR.
- PipeWire is absent; rate switching is skipped silently. Test
  `MacosAudioMode = "physical-format"` and `"exclusive"` in config.toml
  (restart the daemon between changes) and report what the DAC reports.
- The daemon IPC socket falls back to /tmp/ferrosonic-<uid>/ferrosonicd.sock.

Final report: commands run, results, code fixes (file:line), skipped checks, and
remaining risks. Do not claim success just because it compiles.
```
