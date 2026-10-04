# Gap Closure Plan — 2026-10-04

Source: source-level comparison of `mini-eq RR` (`main` @ `dd85b52`) against
upstream `bhack/mini-eq` @ `upstream/main`, plus verification of the claims in
`AGENTS.md`, `docs/BUGS.md` and `docs/PLAN/implementation-todo.md`.

Supersedes the stale checkboxes in `docs/PLAN/implementation-todo.md` (several
are already implemented: CLI via clap in `main.rs`, CI workflow, window-state
monitor geometry, `SELECTABLE_FILTER_TYPES`, `format_frequency`,
`total_response_db_at_frequencies`, `estimate_response_peak_db`,
`FILTER_TYPE_INDEX_BY_VALUE`, preset revert/reapply/file-monitoring).

## Verified baseline

| Check | Result |
|---|---|
| `cargo test --lib` | 93 passed, 0 failed |
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| Worktree | clean, `main` @ `dd85b52` |
| Size | 14,767 lines / 29 modules |
| Upstream | 41 Python modules, 41 test files |

## Documentation corrections (do these alongside the code work)

`AGENTS.md` states numbers that no longer hold:

| Location | Says | Actual |
|---|---|---|
| L243 | 29 unit tests | 93 |
| L247 | 36 tests | 93 |
| L246-248 | ~7,200 lines / 32 modules | 14,767 / 29 |
| L281 | "No CI workflow" | `.github/workflows/ci.yml` exists |
| L145 | `EQ_PREAMP_MIN_DB: -24.0` | -36.0 (widened 2026-09-28) |
| L236 | Phase 1 backend is "stubs" | verified live via `pw-dump`, see 2026-09-28-handover |
| L241 | Phase 6 presets lack revert/reapply/file monitoring | `window_presets.rs` has `revert_to_baseline`, `reset_to_neutral`, `set_curve_revert_baseline`, `start_file_monitoring` |
| L85-121 | module list | names 6 nonexistent files, omits `window_band_editor.rs`, `window_band_fader.rs`, `style.rs` |

`docs/BUGS.md`: duplicated block at L25-37; Known Issues app-ID entry
contradicts itself (flags the ID as wrong, then proposes the same ID).

---

## Gap 1 — D-Bus remote control is a non-functional shell (P0) — **FIXED**

**Status: done and verified live.** Full write-up in
`docs/2026-10-04-updates.md`.

### What was found

The D-Bus surface itself was complete — `GetState`, `ListPresets`,
`SetEqEnabled`, `SetRoutingEnabled`, `SetPreset`, `PresentWindow`,
`PresentWindowWithStartupId`, `Quit`, plus `StateChanged` /
`AnalyzerLevelsChanged` / `PresetsChanged` — and dispatch was implemented.
Four independent defects stacked up behind it:

1. **The service was unreachable.** `acquire_bus_name()` existed but was never
   called. Registering the object on a connection does not claim the
   well-known name, so every remote call got `ServiceUnknown`.
2. **The handler was disconnected from the window.** `AppState` in `main.rs`
   was the only `MiniEqAppHandler` impl and `window.rs` referenced it zero
   times. `SetEqEnabled`/`SetRoutingEnabled` flipped a `Mutex<bool>` nothing
   read; `SetPreset`, `PresentWindow` and `Quit` were empty bodies;
   `AnalyzerLevels` always returned `[]`. `GetState` reported
   `eq_enabled=true` unconditionally.
3. **Argument extraction was wrong.** `on_method_call` used
   `params.get::<bool>()`, but `params` is the whole argument *tuple* — a
   one-arg method arrives as `(b)`. Every setter answered
   `InvalidArguments`. Latent until (1) made the service reachable.
4. **Both array builders were type-wrong.**
   `array_from_iter::<Variant>(...)` inferred the wrong element type and
   tripped a `glib` `is_type` assertion, panicking the process inside a
   `cannot unwind` context.

The GNOME Shell extension depends on this path, so it was blocked.

### What was done

- `AppState` moved into the library as `src/remote_control.rs`, because
  `window.rs` cannot reference the binary crate.
- The `Send`-bounded D-Bus vtable is bridged to the main-thread widgets with a
  `RemoteCommand` queue drained at the top of the 33 ms tick — draining
  *before* `bands` is computed, so a preset loaded over D-Bus reaches the
  filter chain in the same tick.
- The window now mirrors state back into `AppState` (route switch, preset
  selection, bypass, per-tick analyzer/visibility), so `GetState` and the
  signals are live rather than a shadow copy.
- `register()` now claims the bus name; `MiniEqDBusControl` retains the owner
  id so the name is not dropped.
- `build_state`'s hardcoded `running: false` replaced with a real
  `running()` (upstream: `controller is not None`).

### Also fixed along the way

- **The A/B compare switch was a dead widget** — built and added to the graph
  header with no handler, while `update_state_live_or_reload` hardcoded
  `eq_enabled = true`. Now wired, and the state is folded into the push
  signature so toggling it triggers a push.
- `AnalyzerLevelsChanged` was first implemented value-gated, which starved it
  (silence yields an identical spectrum every frame). Now time-throttled at
  100 ms, matching upstream `CONTROL_ANALYZER_EMIT_INTERVAL_SECONDS`.

### Verified live

`GetState`, `ListPresets`, `SetEqEnabled`, `SetRoutingEnabled`, `SetPreset`
and `PresentWindow` all take effect; `StateChanged`, `PresetsChanged` and
`AnalyzerLevelsChanged` (~10/s) all emit. 99 unit tests pass; fmt and clippy
clean.

## Gap 2 — AutoEq has no UI entry point (P0, contradicts README)

`autoeq.rs` (705 lines) is complete and tested: `search_autoeq_entries`,
`load_autoeq_entries`, `download_autoeq_preset`,
`format_autoeq_parametric_eq`, `parse_autoeq_app_entries`, cache handling.
`test_search_autoeq_entries` and `test_search_autoeq_entries_empty_query` pass.

`window_autoeq.rs` has three functions — `new`, `show`,
`draw_autoeq_preview` — and is **never constructed**. Grep for `AutoEq` across
`window.rs`, `window_layout.rs`, `window_presets.rs`, `window_utility.rs`
returns nothing. Upstream `window_autoeq.py` has 37 methods (debounced search,
keyboard-navigable results, preview load, grid/axis/palette drawing, import
flow, busy states).

README:23 advertises "Search and import headphone correction presets from
AutoEq" — not reachable today.

### Steps

1. Add an AutoEq button to the presets panel that constructs the dialog.
2. Implement search entry + debounce (240 ms) + results list.
3. Wire preview drawing to `search_autoeq_entries` output.
4. Wire import to `format_autoeq_parametric_eq` → apply bands to the engine.

## Gap 3 — No single-instance guard (P0)

`instance.rs:6` `InstanceGuard` is an in-process `Arc<Mutex<bool>>` and is
never instantiated — grep finds only its own definition. It cannot prevent a
second instance.

Upstream `instance.py` (292 lines) does a validated absolute-path lock file
(ownership + permission checks), a `/proc` scan for active instances, and
**stale filter-chain reaping** — terminating orphaned `filter_chain` processes
whose parent is gone.

For a PipeWire app this matters: a killed instance leaves an orphaned
`filter-chain` node holding `mini_eq_sink`, and the next launch stacks a
second one.

### Steps

1. Replace `InstanceGuard` with a real lock file under `$XDG_RUNTIME_DIR`.
2. On acquire failure, reap stale filter-chain processes before exiting.
3. Remove the stale `AGENTS.md` claim that single-instance handling exists.

## Gap 4 — Preset lifecycle (P1, partly recorded)

Upstream `window_presets.py`: 67 methods. Rust: 23 functions.

Present: rename, name prompt, revert/reapply baseline, reset-to-neutral, file
monitoring.

Missing: save / save-as, delete, import/export payload, fallback presets,
unsaved-change confirmation, and **output-preset linking**
(`output_preset_link_*`, `on_use_preset_for_output_clicked`,
`apply_output_preset_for_current_output`).

Output-preset linking is what makes a preset follow a device, and it is the
dependency for Gap 5.

## Gap 5 — Filter-chain output does not follow a default-sink change (P1)

Monitor re-targets (`retarget_monitor`); the EQ chain's own output link does
not. Needs two physical outputs to validate — a wrong link produces silence or
a feedback loop. Blocked behind Gap 4's output-preset linking.

## Gap 6 — Missing modules (P2)

| Upstream | Rust | Action |
|---|---|---|
| `deps.py` (15 KB) | 3 hardcoded paths, `main.rs:179` | port version checks + `wpctl` probe |
| `diagnostics.py` | absent | port `--startup-trace` JSONL |
| `screenshot.py` | absent | port widget→PNG |
| `release_notes.py` | absent | port in-app release notes |

`pipewire_routes.py` / `pipewire_stream_router.py` were consolidated into
`routing.rs` — acceptable, no action.

## Gap 7 — Packaging and assets (P1)

- No Flatpak manifest (upstream `io.github.bhack.mini-eq.yaml`).
- No GNOME Shell extension (blocked behind Gap 1).
- **No app icons.** `desktop_integration.rs:14` points `APP_ICON_SEARCH_PATH`
  at `assets/icons`, but `src/assets` does not exist. `copy_app_icons` hits its
  `if !source_dir.exists() { return; }` and silently installs nothing while the
  generated `.desktop` file still declares `Icon=`.
- `data/gnome-shell/extensions/io.github.bhack.mini-eq/` exists, is empty, and
  carries upstream's `bhack` app ID. Remove or repurpose.
- No GSettings schema. Deliberate divergence — `settings.rs` uses a JSON file
  instead. Document it rather than porting it.

## Gap 8 — Test coverage asymmetry (P1)

All 93 Rust tests are pure unit tests. `pipewire_backend.rs` (834 lines) and
`routing.rs` (758 lines) — the two highest-risk modules — have zero automated
coverage and are verified on one machine only.

Upstream ships live-runtime tests for exactly this
(`test_check_headless_pipewire_runtime.py`, `test_check_live_ui_runtime.py`,
`test_check_autoeq_live.py`). `docs/TODO.md` already proposes a containerised
PipeWire smoke test; that stays the right answer.

## Gap 9 — Documented, still open (P2)

- Appearance preference never persisted: `settings.rs:156 save_appearance` and
  `appearance.rs:49 sync_appearance_css_class` both have zero call sites.
- No config migration from `~/.config/mini-eq` to `~/.config/mini-eq-rr`.
- `target/release/mini-eq` is from Sep 22, predating the Sep 28 work.

---

## Execution order

1. **Gap 1** — wire the D-Bus handler to the window. Unblocks remote control
   and the Shell extension. ✅ **done 2026-10-04, verified live**
2. **Gap 2** — construct the AutoEq dialog; backend already done.
3. **Gap 3** — real instance lock + stale filter-chain reaping.
4. **Doc corrections** — fix `AGENTS.md` numbers and phase status, de-duplicate
   `BUGS.md`, record Gaps 1-3 as bugs now that they are written down.
5. **Gap 7 icons** — add `src/assets/icons` or fix the `.desktop` reference.
6. **Gap 4** — preset save/delete/import/export, then output-preset linking.
7. **Gap 5** — filter-chain re-target (needs two-output hardware).
8. **Gap 8** — containerised PipeWire smoke test.
9. **Gap 6** — `deps.py` port, then diagnostics/screenshot/release-notes.

## Verification gate for every step

```bash
export PKG_CONFIG_PATH="$HOME/code/mini-eq/deps/usr/lib/x86_64-linux-gnu/pkgconfig:$PKG_CONFIG_PATH"
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --lib
```

Live audio checks need `pkill -x mini-eq` to stop (exact match — without `-x`
it can match the invoking shell). Confirm `ps -o lstart -C mini-eq` against the
binary mtime before retesting; stale-binary reports have already wasted a
session once.