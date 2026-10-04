# AGENTS.md — mini-eq (Rust Rewrite)

## Required reading (in this order)

0. **This document** — static starting point, reference for onboarding not a journal. Always update `docs/{date}-updates.md` and `docs/{date}-handover.md`.
1. **`README.md`** — project overview, features, install instructions
2. **`docs/BUGS.md`** — outstanding and fixed bugs (check before implementing)
3. **`docs/PLAN/`** — implementation plans and architecture decisions
4. **`docs/{date}-updates.md`** — latest implementation changes
5. **`docs/{date}-handover.md`** — dev session handover for context

Important considerations:

### This is a Rust rewrite of the Python mini-eq project
- Original: https://github.com/bhack/mini-eq (Python/GTK/PipeWire)
- Goal: Same functionality, significantly lower CPU usage, no GIL
- Target: PipeWire system-wide parametric EQ using Rust + GTK4 + Libadwaita
- Use the `pipewire` crate (v0.10.1) for PipeWire integration
- Use `gtk4` crate (v0.11.4) + `libadwaita` crate (v0.9.2) for UI
- No Python, no GIL, no garbage collection

### Always update the status for next session under docs/{date}-updates.md

### Never leave artifacts when you complete a task
- Clean your mess: /tmp/ files, temporary scripts, anything else
- Keep track of what you create and where

### finish the task that you started and then continue with the backlog
- If asked to look at another issue, evaluate the priority and add it to docs/BUGS.md as backlog or docs/TODO.md as task to do

---

## Project Overview

Mini EQ is a compact system-wide parametric equalizer for PipeWire desktops. This is a Rust rewrite of the original Python project (https://github.com/bhack/mini-eq) to eliminate the 30% CPU usage caused by Python's GIL and real-time audio processing overhead.

### Original Python project (reference)
- ~30 Python modules, ~2000+ lines
- GTK4/Libadwaita UI via PyGObject
- pipewire-gobject for PipeWire routing
- PipeWire filter-chain with biquad DSP filters
- NumPy FFT analyzer, libebur128 loudness metering
- AutoEq preset support, APO preset import
- GNOME Shell extension, D-Bus control
- Background mode with auto-route

### Rust rewrite target
- Same features, lower CPU, no GIL
- Direct PipeWire filter-chain API via `pipewire` crate
- GTK4 + Libadwaita via `gtk4` + `libadwaita` crates
- Compiled native binary, no interpreter overhead
- Real-time DSP at native speed

## Tech Stack

| Layer | Technology | Notes |
|-------|------------|-------|
| Language | Rust 1.98.1 | Compiled native binary |
| UI | GTK4 + Libadwaita | `gtk4` v0.11.4, `libadwaita` v0.9.2 |
| Audio | PipeWire | `pipewire` v0.10.1, `pipewire-sys` v0.10.1 |
| DSP | PipeWire filter-chain | Builtin biquad filters |
| FFT Analyzer | `rustfft` or `numpy` bindings | Spectrum visualization |
| Loudness | `libebur128` FFI | LUFS metering |
| Build | Cargo | Standard Rust build system |
| Packaging | Flatpak | Same distribution model as original |
| License | GPL-3.0-or-later | Same as original |

## Environment

| Item | Value |
|------|-------|
| Rust | 1.98.1 (cargo 1.98.1) |
| GTK4 | System packages required |
| PipeWire | System PipeWire with filter-chain module |
| Original project | /var/lib/flatpak/app/io.github.bhack.mini-eq/ |
| Original config | ~/.config/mini-eq/ |
| Original presets | ~/.config/mini-eq/ |
| AutoEq profile index | ~/.config/mini-eq/autoeq/entries.json |

## Architecture

### Core modules (actual `src/` as of 2026-10-04)

The original list below named files that were never created and omitted ones
that exist. This is the real tree.

```
src/
├── main.rs                    # Entry point, CLI (clap), backend bootstrap
├── lib.rs                     # Library crate root (module list)
├── core.rs                    # EQ constants, biquad coefficients, band config
├── pipewire_backend.rs        # PipeWire connection, filter-chain management
├── filter_chain.rs            # PipeWire filter-chain DSP configuration
├── routing.rs                 # Routing logic (absorbs upstream pipewire_routes
│                              #  + pipewire_stream_router)
├── band_fader.rs              # Individual EQ band control widget
├── analyzer.rs                # FFT spectrum analysis
├── ebur128.rs                 # LUFS loudness metering (FFI)
├── assets/                    # App icons (16-512px + symbolic) and AppStream metainfo
├── autoeq.rs                  # AutoEq search + APO preset parsing
├── settings.rs                # User settings persistence (JSON, not GSettings)
├── dbus_control.rs            # D-Bus interface XML, dispatch, signal emission
├── remote_control.rs          # Shared AppState + D-Bus→window command queue
├── background.rs              # Background mode / login items
├── instance.rs                # Single-instance flock guard
├── appearance.rs              # GTK theme/appearance
├── desktop_integration.rs     # Desktop file, autostart, icons
├── routing + window_*         # See below
├── window.rs                  # Main GTK window (1460 lines — the big one)
├── window_analyzer.rs         # Analyzer panel
├── window_autoeq.rs           # AutoEq import dialog (reachable since 2026-10-04)
├── window_band_editor.rs      # Selected-band editor
├── window_band_fader.rs       # Fader construction helpers
├── window_graph.rs            # Frequency graph
├── window_headroom.rs         # Headroom / Auto-Safe display
├── window_layout.rs           # Window layout management
├── window_preferences.rs      # Preferences dialog
├── window_presets.rs          # Preset management panel
├── window_state.rs            # Window size persistence
├── window_utility.rs          # Sidebar utility pane
├── window_utils.rs            # Window helper functions (5 lines)
└── style.rs                   # GTK CSS loading (CSS lives in style.rs, not style.css)
```

**Not ported from upstream:** `cli.py` (folded into `main.rs` via clap),
`deps.py` (3 hardcoded paths in `main.rs`), `diagnostics.py`,
`release_notes.py`, `screenshot.py`, `glib_utils.py`, `gtk_utils.py`. No
`src/assets/` and no GSettings schema — icons and settings are handled
differently.

### PipeWire filter-chain architecture
- Creates virtual sink: `mini_eq_sink` → the system default output node
  (`<default-output-node>`). **The target node is discovered at runtime**
  via `routing.rs::find_node_target` / `target_sink: Option<String>` and
  the PipeWire `default` metadata — it is never hardcoded to a specific
  sound card. Any `alsa_output.pci-*` name seen in logs or traces comes
  from the host, not from this codebase.
- Uses `libpipewire-module-filter-chain` for DSP
- Biquad filters calculate coefficients at DSP clock rate
- EQ processing not pinned to 48 kHz
- Native PipeWire biquad filters via SPA plugin

### Key constants (from original)
- `MAX_BANDS`: 32
- `DEFAULT_ACTIVE_BANDS`: 10
- `SAMPLE_RATE`: 48000.0
- `EQ_FREQUENCY_MIN_HZ`: 20.0
- `EQ_FREQUENCY_MAX_HZ`: 20000.0
- `EQ_GAIN_MIN_DB`: -20.0
- `EQ_GAIN_MAX_DB`: 20.0
- `EQ_Q_MIN`: 0.18248
- `EQ_Q_MAX`: 6.0
- `EQ_PREAMP_MIN_DB`: -24.0
- `EQ_PREAMP_MAX_DB`: 6.0
- `FILTER_TYPES`: Off, Bell, Hi-pass, Hi-shelf, Lo-pass, Lo-shelf, Notch, Resonance, Allpass, Bandpass, Ladder-pass, Ladder-rej

## Launch & Development

### Create new project
```bash
cd ~/code/mini-eq
cargo init --name mini-eq
```

### Build
```bash
cargo build --release
```

### Run
```bash
cargo run --release
```

### Check dependencies
```bash
# Verify PipeWire, GTK4, Libadwaita are available
# See docs/PLAN/ for dependency checklist
```

### Test
```bash
cargo test
```

## Common Commands

```bash
# Build release
cargo build --release

# Run with debug output
cargo run -- --verbose

# Run background mode
cargo run -- --background --auto-route

# Import APO preset
cargo run -- --import-apo path/to/ParametricEQ.txt

# Check dependencies
cargo run -- --check-deps

# Headless mode (no GUI)
cargo run -- --headless --background --duration 30
```

## Key Business Logic

**Critical rules:**
- PipeWire filter-chain must use native biquad filters (SPA plugin)
- Coefficients calculated at DSP clock rate, not pinned to 48 kHz
- Virtual sink name: `mini_eq_sink` with description `Mini-EQ-Sink`
- Output client name: `Mini EQ Output`
- Background mode keeps EQ active after window closes
- Auto-route routes all output streams to virtual sink
- Presets stored under `~/.config/mini-eq/`
- AutoEq profile index cached under `~/.config/mini-eq/autoeq/entries.json`

**Important:** The Rust rewrite must produce identical audio processing results to the Python original. Biquad filter coefficients must match exactly.

## Gotchas

- PipeWire filter-chain module must be loaded: `libpipewire-module-filter-chain`
- SPA builtin filter graph plugin required: `libspa-filter-graph-plugin-builtin.so`
- `pipewire-gobject` Python binding is NOT available in Rust — use `pipewire` crate directly
- The original uses `pipewire-gobject` for app-facing PipeWire routing — the Rust `pipewire` crate provides equivalent functionality
- NumPy FFT analyzer will need `rustfft` or similar crate
- libebur128 will need FFI binding or Rust crate
- GTK4/Libadwaita must be installed on the system
- Flatpak packaging will need to be recreated for Rust

## External References

- Original project: https://github.com/bhack/mini-eq
- Flathub: https://flathub.org/apps/io.github.bhack.mini-eq
- AutoEq: https://autoeq.app/
- PipeWire: https://pipewire.org/
- Libadwaita: https://gitlab.gnome.org/GNOME/libadwaita

## Status Tracking

- **Phase 0**: ✅ Complete — Source-linked divergence assessment against upstream `bhack/mini-eq` main branch; full 8-phase Rust conversion plan documented at `docs/PLAN/divergence-assessment-and-rust-plan.md`.
- **Phase 1**: ⏳ In Progress — Backend engine parity: `pipewire_backend.rs` and `routing.rs` compile but are unverified on live PipeWire; virtual sink/filter-chain/output node creation and stream routing are stubs.
- **Phase 2**: ✅ Complete — Graph and fader interactions: fader rendering/interaction converged with upstream Python; 3-layer graph overlay with click+drag editing; responsive fader heights.
- **Phase 3**: ✅ Complete — Adaptive shell: `AdwOverlaySplitView` + `AdwClamp(max=1480)` + `AdwToastOverlay`, breakpoints at 1320sp/1080sp, F9 toggle, toolbar with output dropdown + route switch + inspector toggle.
- **Phase 4**: ✅ Complete — Utility/sidebar: preset section + system section with headroom 3-segment meter + monitor strip.
- **Phase 5**: ✅ Complete — Analyzer and headroom: FFT spectrum, LUFS metering, smoothing/display-gain/freeze controls, 3-segment headroom meter with Set Safe button.
- **Phase 6**: ⏳ Partial — Presets: rename, name prompt, revert/reapply baseline, reset-to-neutral and file monitoring are implemented; **no save/save-as, delete, import/export, fallback presets, or output-preset linking**. AutoEq: **reachable** — dialog opened from the Presets panel, index fetched on open, import saves + loads a preset (2026-10-04). Preferences: functional dialog wired to settings persistence.
- **Phase 7**: ✅ Complete — DSP/core math: biquad coefficients and constants match upstream for 9 selectable filter types; `total_response_db`, `format_frequency`, index maps, graph response helpers all implemented. Window state: monitor geometry fallback implemented.
- **Phase 8**: ⏳ Pending — Testing (29 unit tests pass), Flatpak packaging, performance validation, GNOME Shell extension.

### Current state (2026-10-04)
- `cargo check --release` compiles with 0 warnings, 0 errors.
- `cargo test --lib` passes: 115 tests, 0 failures.
- `cargo fmt --check` and `cargo clippy --all-targets -- -D warnings` clean.
- ~15,300 lines of Rust across 31 modules.
- Upstream reference fetched: `https://github.com/bhack/mini-eq.git`
  (41 Python modules, 41 test files).

**The binary is `mini-eq-rr`, not `mini-eq`.** `target/debug/mini-eq` is a
stale pre-rename artifact — do not run it and do not trust a live check that
used it. Use `./target/debug/mini-eq-rr`. This trap has already invalidated one
verification round.

**Live D-Bus verification** (the control interface is now wired and working):

```bash
D=io.github.mrproject72.mini_eq_rr
O=/io/github/mrproject72/mini_eq_rr/Control
M=io.github.mrproject72.MiniEqRR.Control
gdbus call --session --dest $D --object-path $O --method $M.GetState
gdbus call --session --dest $D --object-path $O --method $M.SetEqEnabled false
gdbus monitor --session --dest $D     # StateChanged / AnalyzerLevelsChanged
```

### Verified gaps against upstream

Full analysis and prioritised next steps:
**`docs/PLAN/gap-closure-2026-10-04.md`**. Highest-value open items:

- ~~**No single-instance guard.**~~ **FIXED 2026-10-04** — `flock` on
  `$XDG_RUNTIME_DIR/mini-eq-rr.lock`; reaping not needed (daemon drops the module
  on client disconnect).
- **Preset lifecycle** — no save/save-as, delete, import/export, fallback
  presets, or output-preset linking.
- **Filter-chain output does not follow a default-sink change** (the monitor
  does). Needs two-output hardware to validate.
- No Flatpak manifest, no GNOME Shell extension. (CI **does** exist at
  `.github/workflows/ci.yml`.)
- **Zero automated coverage** of `pipewire_backend.rs` (834 lines) and
  `routing.rs` (758 lines) — only `pipewire_backend.rs` pod encoding is
  covered; both still lack behavioural tests.

### Build instructions

```bash
# Install dev dependencies (if not already installed)
cd /tmp/kilo && apt download libgtk-4-dev libadwaita-1-dev libgraphene-1.0-dev libpipewire-0.3-dev libspa-0.2-dev libdbus-1-dev
for deb in *.deb; do dpkg-deb -x "$deb" ~/code/mini-eq/deps/; done

# Only env var needed: PKG_CONFIG_PATH. The linker search path for deps/*.so
# is added automatically by build.rs (conditional on deps/ existing, so CI is
# unaffected).
export PKG_CONFIG_PATH="$HOME/code/mini-eq/deps/usr/lib/x86_64-linux-gnu/pkgconfig:$PKG_CONFIG_PATH"

# Build / test / run
cargo build --release
cargo test --lib
cargo run --release -- --background --auto-route
```

### Upstream reference

```bash
git remote add upstream https://github.com/bhack/mini-eq.git 2>/dev/null || true
git fetch upstream --depth 1
git ls-tree --name-only -r upstream/main | head -80
```

### Current blockers

- ~~**P0 — the EQ cannot be edited at all.**~~ **RESOLVED 2026-10-04** — see `docs/BUGS.md`.
  Cause was the EQ being out of the audio path with systemwide routing off; the
  A/B switch is now insensitive in that state so it cannot look live while inert.
- ~~**AutoEq is unreachable.**~~ **FIXED 2026-10-04** — dialog is now reachable from the
  Presets panel, HTTP runs on a worker thread (it previously aborted the
  process), and imports save + load a preset. See `docs/BUGS.md`.
- ~~**No single-instance guard.**~~ **FIXED 2026-10-04** — real `flock` on
  `$XDG_RUNTIME_DIR/mini-eq-rr.lock`; a second launch is refused and reports the
  holder PID. Stale filter-chain reaping is not needed: the module lives in the
  daemon and is dropped on client disconnect (verified with `kill -9`).
- Preset lifecycle incomplete: no save/save-as, import/export/delete, fallback
  presets, or output-preset linking.
- Filter-chain output does not follow a default-sink change (the monitor does),
  though the Output dropdown can now be pointed at a chosen device — it
  rebuilds the filter chain, so there is a brief audio gap per switch and it
  still needs a human listening test.
- ~~No app icons.~~ **FIXED 2026-10-04** — icons ship at 16–512px plus symbolic
  variants, derived from `assets/parametric_eq_mixer_icon.png` with
  premultiplied-alpha downscaling.
- No Flatpak manifest, no GNOME Shell extension. (CI **does** exist at
  `.github/workflows/ci.yml`.)
- `pipewire_backend.rs` (834 lines) and `routing.rs` (758 lines) have **zero**
  automated test coverage — verified on one machine only.

## CI Notes

- GitHub Actions workflow installs `libdbus-1-dev` (required by `libdbus-sys v0.2.7`)
- CI runs on `ubuntu-24.04` with `stable` Rust toolchain
- Jobs: `cargo check --release`, `cargo test --lib`, `cargo clippy -- -D warnings`, `cargo fmt --check`, `cargo build --release`
- CI is currently **passing** (commit `002db80`)
- UI directive: use native Rust UI (GTK4/Libadwaita crates), no web/interpreted layer — maximum performance

> **Full project status:** see `docs/{date}-updates.md` and `docs/{date}-handover.md`.
> This document is a static starting point, reference for onboarding not a journal or dynamic status update.
