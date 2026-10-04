# Graph parity: what the Rust port is missing vs upstream

Audit date: 2026-10-04. Upstream read from the `upstream/main` git ref
(`src/mini_eq/window_graph.py`, `analyzer_widget.py`, `window_layout.py`);
Rust read from the working tree at `709c52e`.

Upstream's graph is a 3-layer `Gtk.Overlay` (`window_layout.py:370-422`):
bottom = framed/labelled grid, middle = `AnalyzerPlotWidget` spectrum, top =
`graph_response_area` with **five** distinct curve elements plus a dot per
active band — and it is directly editable.

The Rust port has the same three-layer overlay (`window_graph.rs:114-117`) but
the layers are near-empty, and **the graph has no event controllers at all**
(`grep add_controller` finds controllers only in `band_fader.rs` and
`window.rs:719`). The graph is display-only.

> `AGENTS.md` previously claimed "Phase 2 Complete — 3-layer graph overlay with
> click+drag editing". That was false: the overlay existed, the gestures did not.
> Corrected there on 2026-10-04.

## 1. Curves and markers (upstream `draw_graph_response_overlay`, :1000-1107)

| Upstream | Behaviour | Rust | Effort |
|---|---|---|---|
| :1034-1047 | Gradient area fill between curve and the 0 dB baseline (amber, alpha .24→.02; .10→.01 when bypassed) | absent — `draw_response` only strokes | small |
| :1049-1055 + `selected_response_points` :799 | **Second curve**: the selected band's own contribution only | absent (`total_response_db(&[band], 0.0, …)` already exists in `core.rs`) | small |
| :1057-1063 | 6 px low-alpha glow under the main curve | absent | trivial |
| :1065-1074 | Main curve 2.6 px; **grey + half alpha when `eq_enabled` is false** so bypass is visible | always `(0,0.8,0)`; `EqGraphState` has no `eq_enabled` | small |
| :1025-1029 | Selected-band vertical focus line, blue, alpha .18/.24 | white alpha .5, full widget height (no plot margins) | small |
| :1076-1107 | **One dot per active band**: r 5.8 selected / 4.2 effective / 3.6 muted-by-solo, state colours, 12 px selected halo | dot for the **selected band only**, always r=6 white (`window_graph.rs:296-313`); `bands_have_solo` / `band_is_effective` exist in `core.rs` but the graph ignores them | small |
| :1082-1092 | Dot Y = `total_response_db(...)` **at the band frequency**, so the dot sits on the curve | dot Y = raw `band.gain_db` (`window_graph.rs:302`) → the dot floats off the curve whenever preamp ≠ 0 or bands overlap. **Visible bug.** | small |
| — | Upstream has no preamp/target line either: preamp only offsets the curve | n/a | — |

Rust-only artifact: `ctx.move_to(0.0, center_y)` before the polyline
(`window_graph.rs:277`) leaves a vertical spike at the left edge; upstream calls
`cr.new_path()` before stroking.

## 2. Interaction — entirely absent

| Upstream | Behaviour | Rust |
|---|---|---|
| `on_graph_pressed` :338 | Click **anywhere** selects the nearest band (log-frequency distance, prefers active bands); auto-grows `visible_band_count` | absent — selection only via fader clicks (`window.rs:354-388`) |
| `on_graph_drag_begin` :355 | Hit-tests every active dot within `GRAPH_POINT_HIT_RADIUS_PX = 32` (:48), stores `drag_band_index`, selects the band | absent |
| `on_graph_drag_update` :416-506 | X → frequency; Y → gain with **all other bands' contribution subtracted** so the dragged dot stays under the mouse (:467-489); ×2 for shelves; live per-frame DSP update (16 ms `schedule_band_engine_update`), fader/focus/editor refresh, redraw | absent — a fader drag is the only way to move a band |
| :446-450 | **Shift + vertical drag = Q only** | absent |
| `graph_drag_threshold_passed` :409 | 2 px threshold (`GRAPH_DRAG_START_THRESHOLD_PX` :47) separates click from drag | absent |
| `on_graph_drag_end` :508 | clears drag state | absent |
| :511 | Preamp edit redraws only the response layer | Rust redraws all 3 layers every 33 ms tick |
| — | Single-band drag only upstream too (`drag_band_index` is a scalar); no graph keyboard control upstream either | parity / parity |

## 3. Focus / quick-fader strip

| Upstream | Behaviour | Rust |
|---|---|---|
| `update_focus_summary` :212 | `focus_label` under the graph, e.g. `Band 3 • 1.0k • +6.0 dB`, + `band_count_label` chip with the filter type (`window_layout.py:424-436`) | absent — the band editor is a separate row below the faders |
| `update_quick_fader_strip` :123 / `update_band_fader` :130 | fader strip title `N Bands`; per-band CSS `eq-band-box-selected` / `eq-band-box-muted`, opacity 0.98 vs 0.62 from `band_is_effective(band, solo_active)` | no dimming by solo effectiveness, no `N Bands` title; `solo_active` is only computed inside `recompute_solo_active` |
| `band_fader.py:549` / `:577` | drag begin calls `grab_focus()` **and selects the band**; hover highlight + `ns-resize` cursor | drag/scroll/arrow-keys do not select; `EqBandFader.hovered` is read (`band_fader.rs:587`) but no `EventControllerMotion` is ever added, so the hover state is dead |

## 4. Axes, grid, layout

| Upstream | Behaviour | Rust |
|---|---|---|
| :925-940 | 9 dB grid lines (−24…24 step 6), 0 dB thicker (1.6 px), each with a left-hand label | 5 lines (−20…20), no labels, uniform 0.5 px (`window_graph.rs:220-225`) |
| :942-951 | **Second, analyzer dBFS scale** (−60/−40/−20/0) with right-hand labels | absent |
| :953-971 | 11 frequency grid lines with bottom labels (`50`, `1k`), `20 Hz` / `20 kHz` edge labels, `"Monitor"` caption when the monitor is on | 10 lines, **no labels at all** (`window_graph.rs:227-234`) |
| :42-45, :843 | Plot margins 58/62/26/34; two cached `ImageSurface`s invalidated by revision counters (:92) so a drag repaints one surface | `db_to_y` ignores margins (`window_graph.rs:16-20`); all three areas redrawn every 33 ms, response recomputed per draw |
| :879-923 | Gradient plot background, rounded rect, 1 px border, **light/dark palettes** | hardcoded `0.08` grey painted over the whole area, no border, no rounding → dark graph in a light theme (`window_graph.rs:208-213`) |
| `window_layout.py:628-660` | Graph width 900/760 per breakpoint, height **interpolated continuously** 196…280 from window height; analyzer widget inset by the plot margins | `GraphMode::Roomy` exists but is **never used**; no width request; analyzer overlay has no margins, so the bars do not line up with the curve/grid |
| :381-383 | `AccessibleRole.IMG` + label "Curve" on the graph, PRESENTATION on overlays | no accessible roles/labels |

## 5. Analyzer spectrum inside the graph

| Upstream | Behaviour | Rust |
|---|---|---|
| `analyzer_widget.py:251-256` | **Bars AND a 1.3 px stepped line** through the bar tops | bars only (`window_graph.rs:243-261`) |
| :75-114 | Bars positioned by **log-spaced `ANALYZER_BAND_FREQUENCIES` edges**, inner gap `min(1.5, w*0.35)` | `bar_count = len.min(64)` with **uniform** `x = i*bar_width` → the 25 Hz bar is drawn where 1 kHz belongs. Helpers `analyzer_bin_center_frequencies` (`analyzer.rs:828`) and `analyzer_band_edges` (:849) exist and are unused |
| :25-36 | Dark/light palette (cyan) | hardcoded green `(0,0.8,0,0.3)` |
| :47-53 | Alpha 0.15/0.06 and 0.32/0.14 by monitor on/off | one fixed alpha; monitor-off makes levels empty so the bars vanish instantly — no dimmed state and **no decay tail** (upstream `window_analyzer.py:374-403` decays until quiet) |
| `window_analyzer.py:295` | Height via `analyzer_level_to_display_norm` (x42 shaping + display gain) | `level * height`, a linear map (`window_graph.rs:255`). Display gain *is* applied upstream of the graph in `display_levels()`; the x42 curve is not |
| :232 | Early-out unless some level > 0.01 | only checks `levels.is_empty()` |

## 6. Dead code found on the way

- `GraphMode::Roomy` unreachable (`window_graph.rs:26`).
- `EqBandFader.hovered` never written (`band_fader.rs:58`).
- `analyzer_loudness_meter_area` `DrawingArea` in the monitor strip with **no draw func** (`window_utility.rs`), so the LUFS meter is an empty box.
- `window_analyzer.rs` duplicated the monitor settings and the spectrum (removed 2026-10-04, see `docs/BUGS.md`).
- `GraphMode::Roomy` was unreachable; removed. Upstream instead interpolates the
  graph height continuously between 196 and 280 from the window height
  (`window_layout.py:646-660`) — that is step 8 territory, not a dead constant.
- `EqBandFader.hovered` was read by the draw function with no motion controller
  ever setting it, so the hover highlight could not appear. Now wired
  (`EventControllerMotion` enter/leave plus the `ns-resize` cursor), matching
  upstream `band_fader.py:577-584`.
- The LUFS meter `DrawingArea` in the monitor strip had no draw func and rendered
  as an empty box. Removed for now; upstream draws a real bar
  (`window_analyzer.py:123 on_loudness_meter_draw`), so it is a genuine gap, not
  a deletion. Only the live LUFS number and the status line are kept.

## Progress

**Done 2026-10-04 (steps 1 and 2, one commit).** Needs a visual check by the
user; there is no screenshot tool on this Wayland session.

- Plot margins `58/62/26/34` applied to all three layers, and the mappers
  rewritten to take them: `frequency_to_x` / `x_to_frequency` / `db_to_y` /
  `y_to_db`, matching upstream signatures. `x_to_frequency` and `y_to_db` are
  the inverses the drag editing needs and are unit-tested as round trips.
- Background is now upstream's: rounded, vertically graded plot area with a
  border; 9 dB grid lines (-24..24 step 6) with 0 dB at 1.6 px and every line
  labelled; the analyzer's own dBFS scale (-60/-40/-20/0) with right-hand
  labels, drawn only while the monitor is running; 11 labelled frequency lines;
  "20 Hz" / "20 kHz" edge labels and a "Monitor" caption.
- `GraphPalette` carries both the dark and light palettes (upstream's tables),
  so the graph is no longer a dark slab in a light theme.
- **A dot per active band** with upstream's radii (5.8 selected / 4.2
  effective / 3.6 solo-muted), state colours and the 12 px selected halo, plus
  the selected-band focus line now confined to the plot rect.
- **Dot Y bug fixed**: it is `total_response_db(...)` at the band's frequency,
  so a dot sits on the curve instead of floating off it whenever the preamp is
  non-zero or bands overlap. Pinned by
  `test_band_dot_y_is_the_total_response_not_the_raw_gain`.
- The curve no longer starts with `move_to(0, centre_y)`, which drew a vertical
  spike up the left edge.
- The analyzer overlay honours the plot rect, so the spectrum lines up with the
  frequency axis and the curve instead of sitting a label width away, and uses
  the upstream bar colour/alpha with the upstream gap rule.

## Ordered next steps (user-visible value first)

1. ~~**Plot margins + axis labels**~~ — DONE.
2. ~~**A dot per active band** with state colours + `total_response_db` dot
   Y~~ — DONE.
3. **Graph drag editing** (medium): GestureClick + GestureDrag, 32 px dot
   hit-test, 2 px threshold, X→freq / Y→gain with other-band subtraction,
   Shift→Q, live DSP push through the existing `gain_request` path so the
   Gaussian coupling still applies.
4. **Click-to-select anywhere** (small).
5. **Analyzer bars + line, log-spaced, palette/alpha by appearance and monitor
   state** (small — helpers already exist).
6. **A/B-bypass curve styling + gradient fill + glow + selected-band
   contribution curve** (small).
7. **Focus label / band-count chip / solo dimming** (small).
8. Fader drag/scroll/arrow selects the band; hover + resize cursor (small).
9. LUFS meter draw func; remove the left-edge polyline spike (trivial).
