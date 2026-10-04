# Bug Tracker

## Open Bugs

### RESOLVED — A/B compare and the System-wide EQ switch

**Verified working 2026-10-04** (user-confirmed: both the System-wide EQ switch
and A/B, with System-wide EQ on).

**Cause of the earlier "A/B does nothing" reports: the EQ was out of the audio
path.** `routed: false` was confirmed via `GetState` while those reports were
being filed. The A/B switch bypasses the EQ *inside* `mini_eq_sink`, so with
systemwide routing off, playback streams go straight to the sound card and no EQ
control — including A/B — can affect anything. Same behaviour as upstream.

Fixed by making the switch **insensitive while routing is off**, with a tooltip
explaining why, and re-enabling it with an explanatory tooltip when routing goes
on. Seeded from the routing switch's real state at startup, since `state-set`
never fires for an initial state. That removes the "looks live but does nothing"
trap that made this so hard to diagnose.

**A wrong turn is recorded here on purpose.** An intermediate change tried to
"fix" the props pod by emitting `SPA_POD_PROP_FLAG_HINT_DICT` and a leading
`Int: n_items`, by analogy with upstream's `GLib.Variant("a{sd}", …)`. That was
wrong and it made the EQ completely inert. Reverted.

Ground truth from PipeWire source (`spa/include/spa/param/props.h`,
`spa/plugins/filter-graph/filter-graph.c`):

- `SPA_PROP_START_Other = 0x80000` and `SPA_PROP_params` is the first entry, so
  the property key is **0x80001**.
- Payload is `Struct((String : key, Pod : value)*)` — **no item count, no dict
  flag**.
- `parse_params()` `break`s on the first field it cannot read as a string, which
  is why the bogus leading `Int` dropped every control.
- `find_port()` splits `node:control` on `:` and resolves the node by name across
  the whole graph, so `band_l_0:b0` naming is correct.

The original encoding was therefore already right. It is now pinned by
`props_pod_is_a_bare_struct_of_string_value_pairs` and
`props_pod_carries_every_control_name` — the first asserts the payload *starts
with a string*, so the broken shape cannot return silently. `pipewire_backend.rs`
previously had **no tests at all**.

Also changed, and kept because they are correct regardless:

- The 400 ms fader-drag debounce no longer applies to A/B; a bypass toggle
  pushes on the next tick.
- `A/B compare: bypassed=… (eq_enabled=…)` logs at INFO on every toggle, and each
  backend push logs `eq_enabled`. The switch previously gave no feedback at all.

### Measurement traps hit while investigating this

Recorded because they will bite again on this machine:

- The app's analyzer **auto-normalises** — it reports an identical value for a
  +18 dB and a −18 dB curve. Identical readings prove nothing about level. Two
  of my conclusions during this session were wrong because of it.
- `pw-record` captured digital silence (−80 dBFS) even for a tone played
  straight to the sound card.
- `pw-top` exposes no dB columns in this build.
- `filter_type: 0` is **Off** (Bell is `1`), and the preset preamp key is
  `preamp_db`, not `preamp`. Both mistakes silently yield flat test fixtures.

Only a human ear is reliable here. When in doubt, read PipeWire's source.

### ~~**No single-instance guard.**~~ **FIXED 2026-10-04**

`instance.rs` defined an `InstanceGuard` that was an in-process
`Arc<Mutex<bool>>` and was **never instantiated**, so it could not prevent
anything: two instances could run at once and both would try to own
`mini_eq_sink`.

Now a real exclusive `flock` on `$XDG_RUNTIME_DIR/mini-eq-rr.lock`, acquired in
`main()` after the one-shot subcommands (which must keep working while the app is
open). A second launch prints the holder's PID and exits 1. The kernel releases
the lock when the holder dies, so a crash never leaves the app unlaunchable.

**No orphaned-filter-chain reaping was added, and that is deliberate.** Upstream
`instance.py` also walks `/proc` killing orphaned `pipewire -c ...` children,
because the *Python* app spawns the filter chain as a child process. This port
does not — `PipeWireBackend` uses `pw_context_load_module`, so the module lives
in the daemon and is owned by our client connection. Verified empirically: after
`kill -9` of a running instance, `mini_eq_sink` and `mini_eq_sink_output` are
both gone within a few seconds. A `/proc` scanner that kills processes would be a
liability, not a safety net.

Verified live: second instance refused with the holder PID; restart after
`kill -9` succeeds; restart after `SIGTERM` succeeds; `--check-deps` still works
while running.

### ~~**Output dropdown lists only hardcoded labels.**~~ **FIXED 2026-10-04**
(selection is honest about being inert — see below)

The Output dropdown in the Headroom panel was
`StringList::new(&["System Output", "Virtual Sink"])`: two hardcoded labels
naming no real device, with **no handler** and nothing reading the selection.
It was a purely decorative widget.

Underneath, two more layers were broken:

- `RoutingEngine::detect_routes()` filtered on `node.name` containing
  `"audio.sink" || "output" || "analog"`. That misses devices whose names
  contain none of those (USB, bluetooth, network sinks) **and** matches inputs —
  `alsa_input.pci-....analog-stereo` is a microphone.
- `PipeWireBackend::detect_output_routes()` had **zero callers**.

Now: `RoutingEngine::list_output_sinks()` selects `media.class == "Audio/Sink"`
exactly, which is what PipeWire defines and what upstream does
(`is_audio_sink` -> `media.class == AUDIO_SINK`), excluding the EQ's own virtual
sink. The dropdown is index 0 = "Follow default output" (matching upstream's
`[follow_default_label, *sinks]`) followed by each real sink by description,
refreshed on the existing 500 ms device watcher, and the model is only touched
when the list actually changes so it does not fight the user's selection.

Verified on this machine: `pw-dump` reports 6 `Audio/Sink` nodes, of which
`mini_eq_sink` is ours; the app now enumerates the other **5** —
HDMI, onboard analog, Logitech USB, PUPGSIS USB and Multi-Output.

### Selecting a device now really re-targets the EQ — but it costs a rebuild

The dropdown used to accept a selection and do nothing. Investigating why turned
up three separate causes:

1. **The wrong key.** Upstream writes `target.object` with the sink's
   **`object.serial`** (and `target.node` with its id). Writing a device *name*
   — which is what `pw-metadata` takes and what the app's own
   `create_filter_chain` puts in `playback.props.target.object` — is silently
   ignored.
2. **It would not have worked anyway.** The output client is created with
   `node.passive = true`, so PipeWire deliberately does not re-target it after
   creation. Verified: with the correct serial and both keys written, the
   metadata write reported success and `target.object` did not move.
3. **The live node proxy was captured only once**, guarded by
   `filter_node.borrow().is_none()`. After any module reload it would still hold
   a dead node, so every subsequent live band push would have gone nowhere. This
   was a latent bug in the pre-existing `update_band_coefficients` path, not
   something this change introduced.

Because of (2), the destination is fixed at module load, so changing device
means **rebuilding the filter chain**. `retarget_output()` therefore:

- validates the sink exists, so a bad name never triggers a disruptive rebuild;
- reloads the module with the new `output_sink` in its args;
- **waits (bounded, 2 s) for the recreated `mini_eq_sink` to appear** —
  `pw_context_load_module` returns before the registry carries the new node, so
  an immediate re-route failed with "Creation failed";
- re-routes all playback streams, because the recreated sink has a **new
  `object.serial`** and every stream still points at the old one;
- clears the cached node proxy on unload so the listener re-captures (fix (3)).

Verified live: the filter output's `target.object` moved
`alsa_output.pci-0000_04_00.6.analog-stereo` -> `alsa_output.pci-0000_04_00.1.hdmi-stereo-extra1`
and streams were re-routed.

**Cost:** the sink node is destroyed and recreated, so there is a brief audio
gap on every device switch, and this has **not** been validated by ear. Needs a
human listening test on a real switch.

### FIXED 2026-10-04 — the monitor died on every output switch

Reported by the user: *"the monitor is working only on the initial default
output source: the moment I switch to another output the monitor stops
working."*

**Cause.** The monitor taps the **monitor ports of a physical sink**
(`alsa_output...:monitor_FL/FR -> mini-eq-analyzer:input_FL/FR`, see
`pump_monitor_link`). The filter-chain rebuild performed by `retarget_output`
does not touch those ports — they belong to the ALSA node, not to the EQ — so
after a switch the capture stream was still listening to the sink the EQ had
just left, where no audio arrives. Spectrum and peak meter froze.

The dropdown handler only called `retarget_output()`; the monitor was moved to a
new sink **only** by the 500 ms default-sink watcher, which fires when the
*system default* changes. Choosing a device in the dropdown does not change the
system default, so the monitor never followed.

**Fix** (all in `window.rs`):

- `engine_sink` became `Rc<RefCell<String>>` — the one place that records which
  sink the chain actually plays out to — and the dropdown updates it on a
  successful switch, together with `AppState::output_sink` so D-Bus `GetState`
  agrees.
- After a successful `retarget_output()`, the monitor is retargeted too
  (`retarget_monitor`) when it is enabled.
- `resolve_monitor_target()` is now the single rule for where the monitor taps:
  **the sink the filter chain targets**, with the system default only as a
  fallback for when the engine never started. The analyzer panel's enable
  toggle and the startup restore both used `default_output_sink()` directly and
  had the same latent bug.
- The default-sink watcher no longer fires while an explicit device is selected,
  which would otherwise pull the monitor off the engine's own sink and freeze it
  again.

Index 0 ("Default Output") also used to be a pure no-op — it logged "following
the system default" without touching anything, so selecting it left the chain
on the previously chosen device. It now resolves the current default and runs
the same switch path.

### P1 — user-visible, small fix

- ~~**AutoEq is unreachable: the dialog is never constructed.**~~ **FIXED
  2026-10-04.** Four separate defects, not one — see below.

- ~~**AutoEq is unreachable: the dialog is never constructed.**~~
  *(original entry, kept for context)*
  `autoeq.rs` (705 lines) is complete and tested — `search_autoeq_entries`,
  `load_autoeq_entries`, `download_autoeq_preset`,
  `format_autoeq_parametric_eq`, cache handling — but nothing calls it.
  `window_autoeq.rs` has three functions (`new`, `show`,
  `draw_autoeq_preview`) and `grep` for `AutoEq` across `window.rs`,
  `window_layout.rs`, `window_presets.rs` and `window_utility.rs` returns
  nothing, so the dialog can never open. Upstream `window_autoeq.py` has 37
  methods. README:23 advertises the feature; it does not work.
- ~~**The app installs no icon.**~~ **FIXED — using the supplied artwork.** `desktop_integration.rs` sets
  `APP_ICON_SEARCH_PATH = "assets/icons"`, but `src/assets` does not exist, so
  `copy_app_icons` hits its `if !source_dir.exists() { return; }` and silently
  installs nothing while the generated `.desktop` file still declares
  `Icon=io.github.mrproject72.mini_eq_rr`.
- ~~**Analyzer renders blank: it is never fed data.**~~ **FIXED** — the 33 ms
  tick now calls `utility.analyzer.update(&levels)`.
- ~~**Cannot name a new preset or rename one.**~~ **FIXED.** Add now
  prompts for a name (suggesting the old `preset_N` as a default) and a
  Rename button was added to the toolbar. Rename uses `fs::rename` on the
  preset file so the stored content is preserved exactly rather than
  re-serialised from the in-memory bands.

### P2 — previously recorded

- ~~**The entire app menu is dead — no GActions are registered anywhere.**~~
  **FIXED.** `app.preferences`, `app.about` and `app.quit` are now
  registered as `gio::SimpleAction`s on the window. Preferences opens the
  existing (previously unreachable) `PreferencesDialog`, About shows an
  `AdwAboutDialog` wired to `CARGO_PKG_VERSION` and the new repo, Quit
  closes the window.
  (Note: they need the `win.` prefix, not `app.` — the actions live on the
  `ApplicationWindow`, not the `GApplication`. Without the prefix GTK found no
  action and rendered every item insensitive.)
- **Appearance is applied but never persisted.** `window.rs` calls
  `AppearanceSettings::load()` + `apply_appearance_preference`, but there
  are **0** call sites for `AppearanceSettings::save` / `save_appearance`,
  and `appearance.rs::sync_appearance_css_class` has no callers. Upstream
  persists the `appearance` key.
- **Window position cannot be restored — NOT FIXABLE in GTK4.**
  `AppearanceSettings` carries `window_x`/`window_y`, but GTK4 removed
  `gtk_window_move` and `gtk_window_get_position` entirely: there are
  **zero** occurrences of "position" in GtkWindow's 1901-line generated
  API. The compositor owns window placement, so there is no portable
  getter to capture from and no setter to restore with. Window *size* is
  persisted and restored (`window_width`/`window_height`); position is
  vestigial. Close this as won't-fix rather than leaving it open forever.
  The `window_x`/`window_y` fields could be dropped to avoid the impression
  that support is imminent.

- ~~Monitor-enabled state not persisted.~~ **FIXED** — `load_monitor_enabled()`
  is now called at startup and `save_monitor_enabled()` on every successful
  toggle. Verified: "Monitor restored on <sink>" on launch.
  ~~Monitor settings popover sliders unwired~~ **FIXED** — smoothing,
  display-gain and freeze are all wired to the backend now.** The smoothing / display-gain sliders and `settings.rs::load_monitor_enabled`/`save_monitor_enabled` exist but nothing calls them. (Also: freeze switch missing window-side.)
- **Default-sink change: the MONITOR now follows, the filter-chain output
  still does not (partial fix).**
  - `routing::refresh_default_audio_sink_name()` was added because
    `default_audio_sink_name()` only pumps the loop when its cache is
    empty, so it could never observe a later change.
  - The 500 ms tick now detects the default output changing and moves the
    monitor with `retarget_monitor()`. This is the safe half: the monitor
    is an independent capture stream, so stop+start cannot interrupt the
    EQ audio path.
  - **Still open:** the filter-chain's own output re-link. Doing it blind
    (remove link + create link) risks silence or a feedback loop and needs
    live validation against a real sink switch, so it was deliberately not
    attempted. Until then, EQ'd audio keeps going to the sink the engine
    was started with.
- **No `node.dont-move` / foreign-target guards on the EQ sink.** WirePlumber or other tools can re-link around it.
- ~~Dead code in `analyzer.rs`.~~ **Partially cleaned — and two of the
  three reported items were NOT dead.** Verified before removing:
  `spectrum_db_values_to_levels` is used by `display_levels()` and
  `smooth_power_values` is used by the smoothing path. Only
  `interleaved_f32le_bytes_to_channel_payloads` was genuinely
  unreferenced, and that one was removed.
- ~~`EqGraphState` carries three dead fields.~~ **FIXED.** `frequency`,
  `q` and `filter_type` were written every tick and read by nothing (the
  curve and the selected-band marker both derive from `bands` +
  `selected_band`). Removed the fields, the writes, the `update()`
  parameters, and the `sel_freq`/`sel_q`/`sel_type` computation in
  `window.rs` that existed only to feed them.
- **`EqGraphState` carries three dead fields.** `frequency`, `q`, and `filter_type` are written by `EqGraph::update` on every tick but never read; the response curve and the selected-band marker both derive their values from `bands`/`selected_band` instead. Either drop the fields or use them for the marker overlay.
- **Appearance is applied but never persisted.** `window.rs` calls `AppearanceSettings::load()` and `apply_appearance_preference`, but nothing calls `AppearanceSettings::save()`, and `settings.rs::{load_appearance, save_appearance}` and `appearance.rs::sync_appearance_css_class` have no callers. Upstream persists the `appearance` key via `settings.py`.
- `window_autoeq.rs` and `window_preferences.rs` are placeholder dialogs, not functional.
- `window_state.rs` does not restore window position.
- Preset lifecycle incomplete: no revert/reapply, import/export/delete file monitoring, or output-preset auto-load on device switch.
- No Flatpak manifest, no GNOME Shell extension. (CI **does** exist at
  `.github/workflows/ci.yml` — an earlier note claiming otherwise was stale.)

## Fixed Bugs

- **A/B compare felt broken because it waited on the fader-drag debounce.**
  The bypass state was correctly folded into the push signature, but the whole
  push was still gated on `last_push.elapsed() >= 400ms` — a debounce that
  exists to stop *fader drags* thrashing the DSP. A single deliberate A/B
  toggle therefore took up to 400ms, and toggling back inside that window
  cancelled it entirely, so the switch could produce no audible change at all.
  The debounce now applies only to band/preamp edits; a bypass change pushes on
  the next tick immediately (measured: applied within 200ms).

  The DSP side was verified correct, not assumed: `bq_raw_control_values` with
  `eq_enabled=false` collapses every band node to unity gain (b0 == a0, all
  other coefficients 0) and differs from the active set. That matches upstream
  `filter_chain.py` formula-for-formula. Pinned by
  `test_eq_bypass_pushes_flat_response_not_the_active_curve`.

  **Worth knowing when testing this:** bypass is only audible if the EQ curve is
  actually doing something. On a flat curve (all bands 0 dB or `Off`) the
  bypassed and active responses are identical, so there is correctly nothing to
  hear. Boost a band first.
- **Analyzer controls were unlabelled.** Two `Scale` widgets and a `Switch` were
  appended straight into one horizontal `Box` with only tooltips, so nothing on
  screen identified them. Now three labelled rows — "Smoothing" (with a live
  `NN%` readout), "Display Gain" (with a `+N dB` readout) and "Freeze" — using a
  `boxed-list` `ListBox`, mirroring upstream's `Adw.ActionRow` layout in
  `window_utility.py`. Tooltips now explain what each does rather than merely
  naming it.
- **The D-Bus control service was completely unreachable, and everything behind
  it was a stub.** Four independent defects stacked up, so no remote client
  could do anything at all:
  1. `acquire_bus_name()` existed but **was never called**. Registering the
     object on the connection does not claim the well-known name, so
     `gdbus call --dest io.github.mrproject72.mini_eq_rr` returned
     `ServiceUnknown`. `register()` now owns the name itself.
  2. `AppState` (`main.rs`) was the only `MiniEqAppHandler` impl and was
     disconnected from the window — `window.rs` had zero references to it.
     `SetEqEnabled` / `SetRoutingEnabled` flipped a `Mutex<bool>` nothing read;
     `SetPreset`, `PresentWindow` and `Quit` were empty; `AnalyzerLevels`
     always returned `[]`. `GetState` reported `eq_enabled=true`
     unconditionally.
  3. `on_method_call` read arguments with `params.get::<bool>()`, but `params`
     is the whole argument **tuple** — a one-arg method arrives as `(b)`.
     Every setter therefore answered `InvalidArguments`. Latent until (1) made
     the service reachable.
  4. `array_from_iter::<Variant>(...)` inferred the wrong element type and
     tripped a `glib` `is_type` assertion at runtime. `ListPresets` and
     `AnalyzerLevelsChanged` both panicked. Fixed with
     `array_from_iter_with_type`.

  Fixed by moving `AppState` into the library as `src/remote_control.rs` and
  bridging the `Send` D-Bus vtable to the main-thread widgets with a command
  queue (`RemoteCommand`) drained at the top of the 33 ms tick — draining
  before `bands` is computed, so a preset loaded over D-Bus is pushed to the
  filter chain in the same tick. The window now mirrors state back into
  `AppState` (route switch, preset selection, bypass), so `GetState` and all
  three signals are live. Verified on a running instance: `GetState`,
  `ListPresets`, `SetEqEnabled`, `SetRoutingEnabled`, `SetPreset`,
  `PresentWindow` all take effect; `StateChanged`, `PresetsChanged` and
  `AnalyzerLevelsChanged` (~10/s) all emit. See
  `docs/PLAN/gap-closure-2026-10-04.md` Gap 1.

- **A/B compare (bypass) switch was a dead widget.** `utility.bypass_switch`
  was built and added to the graph header but had **no handler**, and
  `update_state_live_or_reload` hardcoded `eq_enabled = true`, so bands were
  always pushed wet. It now has a `state-set` handler, the tick folds the
  switch state into the push signature, and `eq_enabled` is passed through to
  `apply_live_controls`. `MiniEqAppHandler` gained `running()` (upstream:
  `controller is not None`) — `build_state` previously hardcoded
  `running: false`.

- **`AnalyzerLevelsChanged` was value-gated, which starved it.** The first
  implementation emitted only when the level vector differed from the last
  publish. Silence and steady tones produce an identical spectrum every frame,
  so the signal never fired at all. Now time-throttled at 100 ms, matching
  upstream `CONTROL_ANALYZER_EMIT_INTERVAL_SECONDS`.

- **`target/debug/mini-eq` is a stale artifact that shadows the real binary.**
  The crate builds `mini-eq-rr`; `target/debug/mini-eq` is left over from before
  the rename (mtime 2026-09-28). `docs/2026-09-28-handover.md` instructs
  running `./target/debug/mini-eq`, so a live check can silently test 6-day-old
  code. Delete it and correct the handover/`dev-restart.sh`.

- **Headroom/Auto-Safe peak estimate was graph-clamped (under-compensated stacked boosts).** `estimate_response_peak_db` sampled through the display-clamped `total_response_db` (±36 dB), so 4× HiShelf @ +20 dB (true peak ~+84 dB) reported only +36 dB and Auto-Safe pinned at the −24 dB floor could never clear the warning. Split into `total_response_db_unclamped` + clamped wrapper; the estimate now uses the unclamped path (upstream `clamp_output=False`). Locked in by `test_estimate_response_peak_db_is_unclamped`. See `2026-09-28-updates.md`.
- **System EQ toggle unwired / GUI had no backend path / output dropdown placeholder.** Resolved in the 2026-09-27 sessions: the window owns `PipeWireBackend`, fader/preamp edits push live `SPA_PARAM_Props` (`bq_raw`), the output dropdown is populated from detected routes, and System EQ routes/unroutes app streams via default metadata. See `2026-09-27-handover.md`.
- **EQ detached on filter-type change.** The native-biquad strategy reloaded the module on type change, recreating the virtual sink with a new node id and orphaning routed streams. Switched to the upstream `bq_raw` strategy (type encoded in coefficients, fixed topology, live pushes only). See `2026-09-27-updates.md`.
- **Mute was conflated with the filter-type "Off" state and lost on save/load.** `EqBand` modelled a single `enabled` flag that upstream treats as two independent things (`filter_type != Off` for "active", `mute` for "muted"). Because `eq_band_to_dict` wrote `"mute": !enabled`, an `Off` band and a muted Bell band serialised identically, and a muted band came back unmuted. `EqBand` now has a real `mute` field matching upstream, `band_is_effective` honours it, and `test_muted_band_roundtrips_through_preset` locks the behaviour in. Legacy presets written by this port that used `enabled` still load (inverted).
- **Band editor was a non-functional stub.** `window_layout.rs::build_band_editor()` built Mute/Solo toggles and Type/Freq/Q/Gain controls with no signal handlers and no link to the selected band. Replaced by `src/window_band_editor.rs`, a view over the selected fader that mirrors upstream `update_selected_band_editor` / `on_selected_band_*`: mute, solo, filter type, frequency, Q, and gain all write through to the fader and re-render it, with an `updating_ui`-style guard so programmatic repopulation does not echo back as user edits.
- **Editor handlers would have panicked on first edit.** The change handlers used `if let Some(index) = *index.borrow()`, which keeps the `Ref` alive for the whole body on edition 2024; the callback then re-entered the same cell via `refresh()` and hit `RefCell already borrowed`. Switched to `let ... else`, which drops the temporary before the callback runs.
- **Preset apply/reset left the editor stale.** Both callbacks now refresh the editor, and reset recomputes `solo_active` instead of carrying the pre-reset value forward.
- **`cargo clippy -- -D warnings` (the CI gate) did not pass.** Three `needless_return` warnings in `band_fader.rs` and an `approx_constant` error (`3.14` in a `window_headroom.rs` test) broke it. Fixed; the full CI command set is green again.

- **Band selection was multi-select and never reached the graph.** Each fader set `selected = true` on click/keypress with no coordination, so multiple bands could render as selected and `EqGraph::set_selected_band` was never called. Selection is now owned by `window.rs`, which clears sibling faders and mirrors the index into the graph.
- **Preset preamp was ignored on load, reset, and change detection.** The apply callback discarded `_preamp`, the reset callback left the old preamp, and the state-signature callback hard-coded `0.0` so preamp-only edits never marked the preset dirty. All three now use the headroom panel's preamp value.
- **Inspector toggle button was unwired.** The `ToggleButton` in the header now drives `AdwOverlaySplitView::set_collapsed`.
- Fader drag direction was inverted: dragging down increased gain instead of decreasing. The drag formula also had an incorrect scale factor that limited a full drag to ~0.44 dB instead of the full ±20 dB range.
- Fader interaction did not match upstream Python: missing drag threshold, modifier-aware fine/coarse steps, snap-to-0.1 dB rounding, and full keyboard bindings.
- Fader rendering did not match upstream Python: missing track gradient, zero-line fill, tick marks, knob shadow/highlight, state badges, frequency/Q labels, and gain pill.

## Known Issues

- **App ID decision — settled, but a config migration is still missing.**
  `core::APP_ID`, `analyzer::ANALYZER_APPLICATION_ID`, the D-Bus service name
  and the autostart file all use `io.github.mrproject72.mini_eq_rr`. This was
  verified to be consistent across `core.rs`, `dbus_control.rs` and
  `desktop_integration.rs`, and it no longer collides with an upstream install
  on the same machine. Config lives in `~/.config/mini-eq-rr`, so an existing
  `~/.config/mini-eq` is **not** picked up — add a one-shot migration if that
  matters.
- **Auto-Safe lowers the volume noticeably — EXPECTED BEHAVIOUR, not a bug.**
  With Auto-Safe engaged the whole output is attenuated by
  **`1 dB + your maximum EQ boost`**, because
  `auto_safe_preamp_db = (target − curve_peak).clamp(EQ_PREAMP_MIN_DB, 0)`
  with `target = −1 dBFS`.

  | max EQ boost | preamp set | output drop | perceived loudness |
  |---|---|---|---|
  | +3 dB | −4.0 dB | 4 dB | ~76% |
  | +6 dB | −7.0 dB | 7 dB | ~62% |
  | +10 dB | −11.0 dB | 11 dB | ~47% |

  This is correct and unavoidable here: there is **no sample-accurate
  limiter in the PipeWire filter chain**, so the only way to guarantee no
  clipping is feed-forward — assume material hits full scale at the most
  boosted frequency and reserve headroom for it in advance. A `+6 dB`
  boost at 100 Hz therefore attenuates the entire spectrum, even though
  only 100 Hz is at risk; that is the cost of a single global preamp.

  **Do not replace this with a live/monitor-driven preamp.** Tried and
  reverted (see `docs/2026-09-28-updates.md`): the UI tick is 33 ms and
  program transients are 1–10 ms, so a feedback loop clips before it can
  even observe the transient. Monitor-on + Auto-Safe clipped; monitor-off
  (feed-forward) worked.

  Practical mitigation: keep the maximum boost modest — the drop tracks it
  1:1. Reclaiming the loudness properly requires a real limiter in the
  filter chain, not a faster UI loop.
- **Fader bottom slightly clipped at some window sizes (minor).** The fader's
  bottom edge (Q value "1.50" + the box's bottom border) can still be a few
  px short of fully visible at certain window heights. The band_scrolled's
  `min_content_height` was raised to match the fader height per breakpoint
  (164 compact / 208 wide, see `window.rs` breakpoints), which improved it
  a lot, but a small residual clip remains at some sizes. The faders are
  fully usable; only the last border row is tight. Root cause is the tight
  vertical budget (graph + faders + editor + header vs MIN_WINDOW_HEIGHT).
  Possible follow-ups: shave the graph height, reduce fader CONTENT_H, or
  let the fader area scroll.
- GTK4/Libadwaita dev packages not permanently installed (using `deps/` directory).
- PipeWire filter-chain module availability not verified for Rust `pipewire` crate.
- No CI/CD pipeline running yet.

## Bug Report Template

When reporting bugs, please include:
1. mini-eq version or commit hash
2. Steps to reproduce
3. Expected vs actual behavior
4. Relevant log output (`--verbose` flag)
5. System information (PipeWire version, GTK version, OS)

See [CONTRIBUTING.md](../CONTRIBUTING.md) for details.

### FIXED 2026-10-04 — duplicated analyzer controls, and Freeze with the wrong scope

User report: the Analyzer panel showed Smoothing and Display Gain sliders and
then a settings icon "with the same options".

- The Monitor strip's gear popover (`build_monitor_panel`) built a second
  Smoothing scale, Display Gain scale and Freeze switch as **locals that were
  never returned**, so nothing read them: the controls looked live and were
  inert. The Rust-only Analyzer sidebar page held the only wired copy.
- Upstream (`window_utility.py:255-355`) has exactly one spectrum — overlaid on
  the frequency graph — and exactly one settings UI, that gear popover. The
  sidebar page with its own spectrum and its own sliders had no upstream
  counterpart.

Fixed by deleting `window_analyzer.rs` (the page keeps the monitor strip) and
wiring the popover's controls to the backend.

Scope, which the duplication made ambiguous:

- Smoothing → `analyzer.response_speed`, and Display Gain → applied inside
  `display_levels()`. Both therefore always affected the **shared** level
  stream, i.e. the graph spectrum and the D-Bus levels, not just the panel.
- Freeze → the sidebar panel only; the graph overlay ignored it and kept
  scrolling. Upstream gates the level stream itself
  (`window_analyzer.py:320` and `:353`), so a freeze holds the spectrum and the
  LUFS readout there. Now matched.

One deliberate difference from upstream: upstream's preview tick *decays* the
held frame to nothing while frozen (`window_analyzer.py:374-403`), we hold it
still. Holding still is what a control labelled Freeze is expected to do.

### FIXED 2026-10-04 — the Analyzer sidebar page removed, settings moved to the main window

Follow-up to the duplicate-controls fix above. The user asked to catch up with
upstream instead of keeping a second surface for the same three settings.

- The Analyzer page existed only to hold the (already deleted) duplicate spectrum
  and the wired copy of Smoothing / Display Gain / Freeze. Removed: page
  constant, stack child, header toggle button and `window_analyzer.rs`.
- The gear button (the upstream location for those settings,
  `window_utility.py:255-355`) is now in the **main window's output control
  row**, in a fixed-width cell immediately **before the Smooth dropdown**.
- The sidebar keeps two pages, Preset and Output. Output carries the device
  settings plus the monitor's LUFS readout, which is the only piece of the old
  strip that was actually showing live data.

Left undone and now recorded as gaps (see `docs/PLAN/graph-parity-2026-10-04.md`):

- The LUFS **meter bar** is gone rather than faked: the DrawingArea never had a
  draw func, so it was an empty box. Upstream draws a real one
  (`window_analyzer.py:123`).
- `EqBandFader.hovered` had no controller setting it. Rather than delete the field
  and the hover branch in the draw function, it is now wired
  (`EventControllerMotion` + `ns-resize` cursor), which is upstream behaviour.
- `GraphMode::Roomy` removed as unreachable. Upstream instead interpolates the
  graph height from the window height (`window_layout.py:646-660`).

### FIXED 2026-10-04 — the Smoothing slider needed a monitor restart

User report: *"in the setting gears we have smooth level bar but it doesn't have
a realtime effect on the EQ: only when I restart the monitor (off and then on
again) it applies the new setting."*

`response_speed` lived on `OutputSpectrumAnalyzer`, and the capture callback
captured it **by value** when the stream was created
(`analyzer.rs`, `let response_speed = self.response_speed;` feeding the
`move` closure on `.process`). So `set_response_speed()` mutated a field nobody
read any more, and the only thing that could pick up a new value was a fresh
`start_capture` — exactly what switching the monitor off and on does.

It now lives in the `Arc<MonitorShared>` the callback already holds, with
`MonitorShared::set_response_speed` / `response_speed()` as the two ends of that
seam. Pinned by
`the_smoothing_value_the_ui_writes_is_the_one_the_dsp_reads`, which writes
through the UI-side accessor and reads it back for the smoothing alpha.

Display gain never had this bug, which is why only one of the two sliders looked
broken: it is applied in `display_levels()` on the way to the UI, not inside the
realtime callback.
