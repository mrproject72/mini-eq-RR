//! Main application window for mini-eq.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;

use glib::ControlFlow;

use crate::appearance::{AppearancePreference, apply_appearance_preference};
use crate::pipewire_backend::PipeWireBackend;
use crate::style;
use crate::window_band_editor::{BandEditor, BandEditorCallbacks};
use crate::window_layout;
use crate::window_state;
use crate::window_utility::UtilityPane;
use crate::window_utils;

/// Snapshot the live fader state as `EqBand`s (the shape the DSP/peak math uses).
///
/// When `smooth` is set the Smooth override is applied on the way out:
/// every non-`Off` band reports as the internal `Sin` bell. `spread_bands`
/// is the spread in bands (Gaussian sigma); the bell Q is derived from it
/// so the bump always bridges whatever bands participate. The faders' own
/// type/Q are never mutated, so switching Smooth off restores them exactly.
fn fader_band_snapshot(
    registry: &Rc<RefCell<Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>>>,
    smooth: bool,
    spread_bands: f64,
) -> Vec<crate::core::EqBand> {
    let bands: Vec<crate::core::EqBand> = registry
        .borrow()
        .iter()
        .map(|f| {
            let fader = f.borrow();
            crate::core::EqBand {
                index: fader.index,
                frequency: fader.frequency,
                gain_db: fader.gain_db,
                q: fader.q_value,
                filter_type: fader.filter_type,
                mute: fader.muted,
                solo: fader.soloed,
                coefficients: crate::core::BiquadCoefficients::identity(),
            }
        })
        .collect();
    if smooth {
        let spacing = crate::core::band_spacing_oct(&bands);
        let q = crate::core::smooth_bell_q(spread_bands, spacing);
        return bands
            .iter()
            .map(|b| crate::core::smooth_effective_band(b, q))
            .collect();
    }
    bands
}

/// Apply `edit` to the fader at `index`, redrawing it when it exists.
fn edit_fader(
    registry: &Rc<RefCell<Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>>>,
    index: usize,
    edit: impl FnOnce(&mut crate::band_fader::EqBandFader),
) {
    let Some(fader) = registry.borrow().get(index).cloned() else {
        return;
    };
    edit(&mut fader.borrow_mut());
    fader.borrow().drawing_area.queue_draw();
}

/// Mirror `solo_active` onto every fader: upstream computes it once from the
/// whole band list (`bands_have_solo`) and hands it to each `set_band_state`.
fn recompute_solo_active(faders: &[Rc<RefCell<crate::band_fader::EqBandFader>>]) {
    let solo_active = faders.iter().any(|fader| fader.borrow().soloed);
    for fader in faders.iter() {
        let mut fader = fader.borrow_mut();
        if fader.solo_active != solo_active {
            fader.solo_active = solo_active;
            fader.drawing_area.queue_draw();
        }
    }
}

/// Load the preset linked to an output sink, if the sink actually changed.
///
/// Mirrors upstream's `output_preset_target_transition`: the previous target's
/// identity is remembered, and only a real change loads a preset. Without that
/// guard every unrelated output event would re-apply a preset and throw away
/// whatever the user had been editing.
///
/// The fallback preset covers sinks with no link of their own, so a device
/// nobody configured still gets a sensible curve.
///
/// Called from the Output dropdown only. The 500 ms default-sink watcher does
/// not take part: the filter chain deliberately does not follow the system
/// default (its output re-link needs live validation), so the sink the EQ is
/// feeding has not changed and there is nothing to switch. If the chain ever
/// starts following the default, this call belongs there too.
fn apply_output_preset_for_sink(
    sink_name: &str,
    last_identity: &RefCell<Option<String>>,
    presets: &Rc<RefCell<crate::window_presets::PresetPanel>>,
) {
    let identity = crate::core::output_preset_key_for_sink(sink_name);
    if identity.is_empty() {
        return;
    }
    if matches!(&*last_identity.borrow(), Some(seen) if seen == &identity) {
        return;
    }
    *last_identity.borrow_mut() = Some(identity.clone());
    match crate::core::output_preset_for_sink(&identity) {
        Some(preset) => match presets.borrow_mut().load_library_preset(&preset) {
            Ok(()) => log::info!("Output {identity}: loaded its preset '{preset}'"),
            Err(e) => log::warn!("Output {identity}: preset '{preset}' failed to load: {e}"),
        },
        None => log::debug!("Output {identity}: no preset linked, leaving the curve alone"),
    }
}

/// The sink the output monitor must tap: the one the filter chain actually
/// plays out to.
///
/// The monitor reads a physical sink's monitor ports, so tapping anything but
/// the engine's own destination means listening to a sink no audio is
/// reaching — the spectrum and the peak meter simply freeze. The system
/// default is only a fallback for the case where the engine never started.
fn resolve_monitor_target(engine_sink: &Rc<RefCell<String>>, be: &mut PipeWireBackend) -> String {
    let current = engine_sink.borrow().clone();
    if !current.is_empty() {
        return current;
    }
    be.default_output_sink().unwrap_or_default()
}

/// Main application window.
pub struct MiniEqWindow {
    pub window: adw::ApplicationWindow,
    pub toolbar_view: adw::ToolbarView,
    pub utility: UtilityPane,
    pub split_view: Rc<RefCell<adw::OverlaySplitView>>,
    pub band_scrolled: gtk4::ScrolledWindow,
    pub band_faders: Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>,
}

impl MiniEqWindow {
    pub fn new(
        app: &adw::Application,
        backend: Rc<RefCell<Option<PipeWireBackend>>>,
        engine_sink: String,
        app_state: Arc<crate::remote_control::AppState>,
    ) -> Self {
        // The sink the filter chain actually plays out to. Shared mutable
        // state because the Output dropdown rebuilds the chain onto a
        // different device, and everything downstream has to follow it: the
        // live DSP pushes, the output monitor's tap and D-Bus `GetState`.
        let engine_sink: Rc<RefCell<String>> = Rc::new(RefCell::new(engine_sink));
        let window = adw::ApplicationWindow::new(app);
        let (default_width, default_height) = window_state::initial_window_default_size();
        window.set_default_size(default_width, default_height);
        // Enforce a minimum so components never get cut, while staying small
        // enough not to dominate a low-res screen.
        window.set_size_request(
            window_state::MIN_WINDOW_WIDTH,
            window_state::MIN_WINDOW_HEIGHT,
        );
        window.set_title(Some("mini-eq RR"));

        // Load CSS styling
        style::load_style();

        // Apply appearance
        let settings = crate::appearance::AppearanceSettings::load();
        apply_appearance_preference(AppearancePreference::parse(&settings.preference));

        // Build utility pane
        let utility = UtilityPane::new();

        // A/B compare belongs with the other output toggles at the top of
        // the graph, not on the crowded output control row.
        {
            let ab_item = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
            ab_item.set_tooltip_text(Some("Bypass the EQ to compare with/without"));
            let ab_label = gtk4::Label::new(Some("A/B"));
            ab_label.set_valign(gtk4::Align::Center);
            ab_label.set_css_classes(&["monitor-toggle-label"]);
            ab_item.append(&ab_label);
            ab_item.append(&utility.bypass_switch);
            utility.graph.borrow().add_top_control(&ab_item);
        }

        // Build header bar
        let header_bar = adw::HeaderBar::new();
        let window_title = adw::WindowTitle::new("mini-eq RR", "Rust Rewrite");
        header_bar.set_title_widget(Some(&window_title));

        // The output-device selector now lives in the Headroom panel's
        // "Output Controls" section (see window_utility.rs), not the header.

        // Main menu button
        let menu_button = gtk4::MenuButton::new();
        menu_button.set_icon_name("open-menu-symbolic");
        let menu_model = create_menu_model();
        menu_button.set_menu_model(Some(&menu_model));
        header_bar.pack_end(&menu_button);

        // --- App actions -------------------------------------------
        // create_menu_model() references app.preferences / app.about /
        // app.quit, but no GAction was ever registered anywhere in the
        // codebase, so every hamburger-menu item was a silent no-op.
        {
            let prefs_action = gio::SimpleAction::new("preferences", None);
            let win = window.clone();
            prefs_action.connect_activate(move |_, _| {
                crate::window_preferences::PreferencesDialog::new(&win).show();
            });
            window.add_action(&prefs_action);
        }
        {
            let about_action = gio::SimpleAction::new("about", None);
            let win = window.clone();
            about_action.connect_activate(move |_, _| {
                let about = adw::AboutDialog::new();
                about.set_application_name("mini-eq RR");
                about.set_version(env!("CARGO_PKG_VERSION"));
                about.set_comments(
                    "Rust rewrite of mini-eq: system-wide parametric EQ for PipeWire.",
                );
                about.set_website("https://github.com/mrproject72/mini-eq-RR");
                about.set_developers(&["mrproject72"]);
                about.set_license_type(gtk4::License::Gpl30Only);
                about.present(Some(&win));
            });
            window.add_action(&about_action);
        }
        {
            let quit_action = gio::SimpleAction::new("quit", None);
            let win = window.clone();
            quit_action.connect_activate(move |_, _| {
                win.close();
            });
            window.add_action(&quit_action);
        }

        // System-wide EQ toggle with an explicit ON/OFF readout. The bare
        // switch left the routing state ambiguous at a glance, and the
        // tooltip is only reachable with the pointer.
        let route_switch = gtk4::Switch::new();
        route_switch.set_tooltip_text(Some("System-wide EQ"));
        let route_state_label = gtk4::Label::new(Some("Off"));
        route_state_label.set_css_classes(&["metric-title"]);
        route_state_label.set_valign(gtk4::Align::Center);
        route_state_label.set_width_chars(3);
        route_state_label.set_xalign(1.0);
        {
            let lbl = route_state_label.clone();
            // notify::active rather than state-set so programmatic changes
            // (restore-on-startup, menu actions) update the readout too.
            route_switch.connect_notify_local(Some("active"), move |sw, _| {
                let on = sw.is_active();
                lbl.set_label(if on { "On" } else { "Off" });
            });
        }
        let route_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
        route_box.set_valign(gtk4::Align::Center);
        route_box.append(&route_state_label);
        route_box.append(&route_switch);
        header_bar.pack_end(&route_box);

        // Panel switch buttons: each swaps the sidebar to a different panel
        // (Preset / Signal Analyzer / Headroom) and opens it. Mutually
        // exclusive; clicking the active one closes the sidebar. Replaces the
        // old single inspector toggle. Wired to the stack after the layout is
        // built (they need the split_view + utility pane handles).
        let panel_switch_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        panel_switch_box.set_css_classes(&["linked", "panel-switch-group"]);
        let preset_btn = gtk4::ToggleButton::new();
        preset_btn.set_icon_name("view-list-symbolic");
        preset_btn.set_tooltip_text(Some("Presets (F9)"));
        let headroom_btn = gtk4::ToggleButton::new();
        headroom_btn.set_icon_name("audio-volume-high-symbolic");
        headroom_btn.set_tooltip_text(Some("Output / Device"));
        panel_switch_box.append(&preset_btn);
        panel_switch_box.append(&headroom_btn);
        header_bar.pack_start(&panel_switch_box);

        // Build main layout with utility pane.
        //
        // Selection is coordinated here rather than inside a fader: the clicked
        // fader reports the index and the owner clears its siblings and mirrors
        // the choice into the response graph. The registry is filled in after
        // the layout is built, so the closure captures an empty cell for now.
        let fader_registry: Rc<RefCell<Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>>> =
            Rc::new(RefCell::new(Vec::new()));
        // Filled once the editor exists; the callbacks below are created first
        // so they can be handed to the editor's constructor.
        let editor_cell: Rc<RefCell<Option<Rc<BandEditor>>>> = Rc::new(RefCell::new(None));
        let refresh_editor: Rc<dyn Fn()> = {
            let editor_cell = editor_cell.clone();
            let registry = fader_registry.clone();
            Rc::new(move || {
                let Some(editor) = editor_cell.borrow().clone() else {
                    return;
                };
                let selected = registry
                    .borrow()
                    .iter()
                    .find(|fader| fader.borrow().selected)
                    .cloned();
                match selected {
                    Some(fader) => editor.refresh(Some(&fader.borrow())),
                    None => editor.refresh(None),
                }
            })
        };
        // Owner-authoritative gain request shared by EVERY gain source: the
        // fader drag / scroll / keyboard / zero gestures AND the band editor
        // spin. It returns the gain the band may actually use after peak
        // safety, so callers apply the returned value instead of what they
        // asked for.
        //
        // Why this matters: the preamp floor (-24 dB) is the only headroom
        // lever. Two maxed Hi-Shelves stack their plateaus and measured
        // ~+59 dB raw, so the preamp would need ~-60 dB to stay safe. The
        // cap keeps the raw peak inside what the preamp can always absorb.
        let gain_request: Rc<dyn Fn(usize, f64) -> f64> = {
            let registry = fader_registry.clone();
            let refresh = refresh_editor.clone();
            let smooth = utility.headroom.borrow().smooth.clone();
            let spread_bands = utility.headroom.borrow().smooth_spread_bands.clone();
            Rc::new(move |index, gain_db| {
                let bands = fader_band_snapshot(&registry, smooth.get(), spread_bands.get());
                let clamped = crate::core::clamp_gain_for_peak(
                    &bands,
                    index,
                    gain_db,
                    crate::core::SAMPLE_RATE,
                    crate::core::MAX_SAFE_RAW_PEAK_DB,
                );
                edit_fader(&registry, index, |fader| {
                    fader.gain_db = clamped;
                });

                // Smooth mode: the dragged band's EFFECTIVE delta (post-cap)
                // drags the neighbours through a Gaussian kernel, so the
                // summed response is one broad, smooth curve instead of an
                // isolated hump. The slider's width sets BOTH the bell width
                // and how many bands participate (sigma = width / spacing);
                // they must move together or the bells cannot bridge the
                // participating region and ripple appears on the bump.
                // Neighbours are clamped one at a time against a fresh
                // snapshot so the peak cap always sees the latest state.
                if smooth.get() && index < bands.len() {
                    let delta = clamped - bands[index].gain_db;
                    if delta != 0.0 {
                        // The slider value IS the Gaussian sigma (in bands).
                        let sigma = spread_bands.get();
                        for (other, weighted) in
                            crate::core::smooth_kernel_weights(index, delta, sigma, bands.len())
                        {
                            let cur =
                                fader_band_snapshot(&registry, smooth.get(), spread_bands.get());
                            let Some(cur_band) = cur.get(other) else {
                                continue;
                            };
                            let want = cur_band.gain_db + weighted;
                            let c = crate::core::clamp_gain_for_peak(
                                &cur,
                                other,
                                want,
                                crate::core::SAMPLE_RATE,
                                crate::core::MAX_SAFE_RAW_PEAK_DB,
                            );
                            edit_fader(&registry, other, |fader| {
                                fader.gain_db = c;
                            });
                        }
                    }
                }
                refresh();
                clamped
            })
        };

        let selection_callback = {
            let registry = fader_registry.clone();
            let graph = utility.graph.clone();
            let refresh = refresh_editor.clone();
            Rc::new(move |index: usize| {
                // Toggle: clicking the already-selected fader deselects it so
                // the editor disappears again. Clicking any other fader moves
                // the selection to it.
                let already_selected = registry
                    .borrow()
                    .iter()
                    .any(|f| f.borrow().index == index && f.borrow().selected);
                if already_selected {
                    for fader in registry.borrow().iter() {
                        let mut f = fader.borrow_mut();
                        if f.selected {
                            f.selected = false;
                            f.drawing_area.queue_draw();
                        }
                    }
                    graph.borrow_mut().set_selected_band(None);
                } else {
                    for fader in registry.borrow().iter() {
                        let mut f = fader.borrow_mut();
                        let should_select = f.index == index;
                        if f.selected != should_select {
                            f.selected = should_select;
                            f.drawing_area.queue_draw();
                        }
                    }
                    graph.borrow_mut().set_selected_band(Some(index));
                }
                refresh();
            }) as Rc<dyn Fn(usize)>
        };

        let band_editor = Rc::new(BandEditor::new(BandEditorCallbacks {
            frequency_changed: {
                let registry = fader_registry.clone();
                let refresh = refresh_editor.clone();
                Box::new(move |index, frequency| {
                    edit_fader(&registry, index, |fader| {
                        let clamped = frequency.clamp(
                            crate::core::EQ_FREQUENCY_MIN_HZ,
                            crate::core::EQ_FREQUENCY_MAX_HZ,
                        );
                        fader.frequency = clamped;
                        fader.frequency_label =
                            crate::window_band_fader::format_frequency_label(clamped);
                    });
                    refresh();
                })
            },
            q_changed: {
                let registry = fader_registry.clone();
                let refresh = refresh_editor.clone();
                Box::new(move |index, q| {
                    edit_fader(&registry, index, |fader| {
                        let clamped = q.clamp(crate::core::EQ_Q_MIN, crate::core::EQ_Q_MAX);
                        fader.q_value = clamped;
                        fader.q_label = crate::window_band_fader::format_q_label(clamped);
                    });
                    refresh();
                })
            },
            gain_changed: {
                let gain_request = gain_request.clone();
                Box::new(move |index, gain_db| {
                    // Route through the shared request so the editor spin and
                    // the faders obey the same peak cap. `refresh()` inside
                    // pulls the clamped value back into the spin.
                    gain_request(index, gain_db);
                })
            },
            filter_type_changed: {
                let registry = fader_registry.clone();
                let refresh = refresh_editor.clone();
                Box::new(move |index, filter_type| {
                    edit_fader(&registry, index, |fader| {
                        fader.filter_type = filter_type;
                        fader.filter_type_label =
                            crate::band_fader::filter_type_short_label(filter_type).into();
                        // Upstream `update_band_fader` derives `active` from the
                        // filter type, so selecting `Off` dims the fader.
                        fader.active = filter_type != crate::core::FilterType::Off;
                    });
                    refresh();
                })
            },
            mute_changed: {
                let registry = fader_registry.clone();
                let refresh = refresh_editor.clone();
                Box::new(move |index, muted| {
                    edit_fader(&registry, index, |fader| fader.muted = muted);
                    refresh();
                })
            },
            solo_changed: {
                let registry = fader_registry.clone();
                let refresh = refresh_editor.clone();
                Box::new(move |index, soloed| {
                    edit_fader(&registry, index, |fader| fader.soloed = soloed);
                    recompute_solo_active(&registry.borrow());
                    refresh();
                })
            },
        }));
        *editor_cell.borrow_mut() = Some(band_editor.clone());

        let (split_view, band_scrolled, band_faders) = window_layout::build_main_layout(
            &utility,
            &band_editor,
            crate::core::DEFAULT_ACTIVE_BANDS,
            gain_request.clone(),
            selection_callback,
        );
        *fader_registry.borrow_mut() = band_faders.clone();

        // Drop the output row's captions when the row gets narrow. Driven from
        // the tick because there is no usable size-change signal: GTK4 does not
        // emit `notify::width` on a FlowBox, and libadwaita applies only one
        // breakpoint per window, which the fader-height breakpoints already
        // claim. Two attempts hung off those and neither ever fired, which is
        // why the cells were sized down to their compact widths to compensate
        // for captions that were never going to be hidden -- and why Set Safe
        // ended up alone on a second row at the minimum window width.

        // Smooth override wiring. The switch already drives the shared
        // `smooth` cell (read by the DSP snapshot + gain path); this handler
        // reflects it onto the UI: every fader shows "Sin" and the type/Q
        // controls lock, because the override pins them. The bands' own
        // type/Q are never mutated, so switching off restores them exactly.
        {
            let registry = fader_registry.clone();
            let editor = band_editor.clone();
            let width_ctl = utility.headroom.borrow().smooth_width_scale.clone();
            let smooth_menu = utility.headroom.borrow().smooth_menu.clone();
            utility
                .headroom
                .borrow()
                .smooth_switch
                .connect_state_set(move |_sw, on| {
                    for fader in registry.borrow().iter() {
                        fader.borrow_mut().smooth_override = on;
                        fader.borrow().drawing_area.queue_draw();
                    }
                    editor.set_type_and_q_enabled(!on);
                    // The width control only means anything while Smooth is on.
                    width_ctl.set_sensitive(on);
                    // Light up the dropdown so the active mode is visible
                    // without opening it.
                    if on {
                        smooth_menu.add_css_class("smooth-on");
                    } else {
                        smooth_menu.remove_css_class("smooth-on");
                    }
                    glib::Propagation::Proceed
                });
        }

        // A/B compare switch. This switch had NO handler at all: the widget was
        // built and added to the graph header but nothing read it, and
        // `update_state_live_or_reload` hardcoded `eq_enabled = true`, so the
        // bands were always pushed wet. The tick now folds the switch state
        // into the push signature and passes it through as `eq_enabled`.
        // `state-set` (not `notify::active`) so a programmatic `set_active`
        // from the D-Bus drain does not re-enter — that path sets the flag
        // itself.
        {
            let state_for_bypass = app_state.clone();
            utility
                .bypass_switch
                .connect_state_set(move |_switch, bypassed| {
                    let eq_enabled = !bypassed;
                    // INFO, not DEBUG: "the A/B switch does nothing" was
                    // reported repeatedly and the switch gave no feedback at
                    // all. One line per toggle makes the state observable from
                    // a log however the change was triggered (click or D-Bus).
                    log::info!("A/B compare: bypassed={bypassed} (eq_enabled={eq_enabled})");
                    if *state_for_bypass.eq_enabled.lock().unwrap() != eq_enabled {
                        *state_for_bypass.eq_enabled.lock().unwrap() = eq_enabled;
                        state_for_bypass.emit_state_changed();
                    }
                    glib::Propagation::Proceed
                });
        }
        // `state-set` does not fire for the switch's initial state, so seed the
        // A/B sensitivity from the routing switch's actual state here. Routing
        // starts off unless something else enabled it, which is the default —
        // and in that state the A/B switch must already look inert.
        {
            let routed = route_switch.is_active();
            utility.bypass_switch.set_sensitive(routed);
            utility.bypass_switch.set_tooltip_text(Some(if routed {
                "Compare with/without the EQ. Works because app audio is routed through the EQ."
            } else {
                "Turn on the systemwide EQ switch first — audio has to be routed \
                 through the EQ for this to have any effect."
            }));
        }
        // Reflect a restored Smooth state on the dropdown immediately.
        if utility.headroom.borrow().smooth.get() {
            utility
                .headroom
                .borrow()
                .smooth_menu
                .add_css_class("smooth-on");
        }
        refresh_editor();
        let split_view = Rc::new(RefCell::new(split_view));

        // Build toolbar view
        let toolbar_view = adw::ToolbarView::new();
        toolbar_view.add_top_bar(&header_bar);

        // ToastOverlay inside ToolbarView, Clamp inside ToastOverlay (matches upstream)
        let toast_overlay = adw::ToastOverlay::new();
        let clamp = adw::Clamp::new();
        clamp.set_orientation(gtk4::Orientation::Horizontal);
        clamp.set_maximum_size(1480);
        clamp.set_tightening_threshold(1320);
        clamp.set_hexpand(true);
        clamp.set_vexpand(true);
        clamp.set_child(Some(&*split_view.borrow()));
        toast_overlay.set_child(Some(&clamp));
        toolbar_view.set_content(Some(&toast_overlay));

        // Disable split-view swipe gestures so fader drags don't hide the sidebar
        split_view.borrow_mut().set_enable_show_gesture(false);
        split_view.borrow_mut().set_enable_hide_gesture(false);

        window.set_content(Some(&toolbar_view));

        // Panel switch buttons: mutually exclusive. Each shows its sidebar
        // page and opens the sidebar (as an OVERLAY over the full-width main
        // content, since the split view is collapsed=TRUE). Clicking the
        // already-active button closes the sidebar. The stack handle is cloned
        // because `utility` is moved into Self at the end.
        {
            let stack = utility.container.clone();
            let split_view_for_panel = split_view.clone();
            let guard = Rc::new(std::cell::Cell::new(false));
            let all_buttons: Vec<gtk4::ToggleButton> =
                vec![preset_btn.clone(), headroom_btn.clone()];
            let pages = [
                crate::window_utility::PAGE_PRESET,
                crate::window_utility::PAGE_OUTPUT,
            ];
            for (btn, page) in all_buttons.iter().zip(pages.iter()) {
                let stack = stack.clone();
                let split_view_for_panel = split_view_for_panel.clone();
                let guard = guard.clone();
                let all_buttons = all_buttons.clone();
                let page = *page;
                btn.connect_toggled(move |b| {
                    if guard.get() {
                        return;
                    }
                    guard.set(true);
                    if b.is_active() {
                        for other in all_buttons.iter() {
                            if other != b {
                                other.set_active(false);
                            }
                        }
                        stack.set_visible_child_name(page);
                        split_view_for_panel.borrow_mut().set_show_sidebar(true);
                    } else {
                        // The active button was toggled off -> close the sidebar.
                        split_view_for_panel.borrow_mut().set_show_sidebar(false);
                    }
                    guard.set(false);
                });
            }
            // Default the sidebar to the Preset page, but keep it CLOSED on
            // startup: the user opens it explicitly. (Activating the button
            // here would auto-open the sidebar via the toggled handler.)
            stack.set_visible_child_name(crate::window_utility::PAGE_PRESET);
        }

        // Breakpoints: 1320sp collapses sidebar/pins END, 1080sp compacts toolbar/faders
        let narrow_bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            1320.0,
            adw::LengthUnit::Sp,
        ));
        let compact_bp = adw::Breakpoint::new(adw::BreakpointCondition::new_length(
            adw::BreakpointConditionLengthType::MaxWidth,
            1080.0,
            adw::LengthUnit::Sp,
        ));

        let band_scrolled_for_narrow = band_scrolled.clone();
        narrow_bp.connect_apply(move |_| {
            // NOTE: do NOT set_collapsed(true) or swap sidebar position here.
            // The utility panel stays pinned to the End (right) at all sizes;
            // swapping it made the panel jump sides on resize. Keep only the
            // height compaction. Must stay >= the fader height or the fader's
            // bottom (Q label + border) gets clipped. In this width range the
            // faders are 182 (initial) or 208 (after a compact->wide cycle),
            // so use 208 to cover the tallest case.
            band_scrolled_for_narrow.set_min_content_height(208);
        });
        let band_scrolled_for_narrow = band_scrolled.clone();
        narrow_bp.connect_unapply(move |_| {
            band_scrolled_for_narrow.set_min_content_height(208);
        });

        let band_faders_for_compact = band_faders.clone();
        let graph_for_compact = utility.graph.clone();
        let band_scrolled_for_compact = band_scrolled.clone();
        compact_bp.connect_apply(move |_| {
            for fader in band_faders_for_compact.iter() {
                fader.borrow().set_height(164);
            }
            graph_for_compact
                .borrow_mut()
                .set_mode(crate::window_graph::GraphMode::Compact);
            band_scrolled_for_compact.set_min_content_height(164);
        });
        let band_faders_for_compact = band_faders.clone();
        let graph_for_compact = utility.graph.clone();
        let band_scrolled_for_compact = band_scrolled.clone();
        compact_bp.connect_unapply(move |_| {
            for fader in band_faders_for_compact.iter() {
                fader.borrow().set_height(208);
            }
            graph_for_compact
                .borrow_mut()
                .set_mode(crate::window_graph::GraphMode::Default);
            band_scrolled_for_compact.set_min_content_height(208);
        });

        window.add_breakpoint(narrow_bp);
        window.add_breakpoint(compact_bp);

        // Bind window state
        window_state::bind_window_state(&window);

        // Center window
        window_utils::center_window();

        // F9 toggles the side panel. When opening, ensure a panel button is
        // active (Preset if none); when closing, deactivate all. The button
        // handlers also drive `show-sidebar`, so we set it directly here too
        // to keep the two paths consistent.
        let split_for_f9 = split_view.clone();
        let f9_buttons = [preset_btn.clone(), headroom_btn.clone()];
        let key_controller = gtk4::EventControllerKey::new();
        key_controller.connect_key_pressed(move |_, key, _, _| {
            if key == gtk4::gdk::Key::F9 {
                let open = !split_for_f9.borrow().shows_sidebar();
                split_for_f9.borrow_mut().set_show_sidebar(open);
                if open {
                    if !f9_buttons.iter().any(|b| b.is_active()) {
                        f9_buttons[0].set_active(true);
                    }
                } else {
                    for b in f9_buttons.iter() {
                        b.set_active(false);
                    }
                }
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
        window.add_controller(key_controller);

        // Start real-time update loop for graph + headroom + backend push
        {
            let graph = utility.graph.clone();
            let headroom = utility.headroom.clone();
            let band_faders = band_faders.clone();
            let backend = backend.clone();
            let engine_sink = engine_sink.clone();
            let monitor_loudness_value = utility.monitor.loudness_value.clone();
            let monitor_summary = utility.monitor.summary.clone();
            // Shared Freeze state. The spectrum is the graph overlay and the
            // loudness readout lives in the monitor strip, so freezing has to
            // gate both -- which is exactly what upstream's `analyzer_frozen`
            // does to `on_analyzer_levels` / `on_analyzer_loudness`.
            let app_for_exit = app.clone();
            let last_state_signature: Rc<RefCell<String>> = Rc::new(RefCell::new(String::new()));
            let presets_for_chip = utility.presets.clone();
            let monitor_frozen = utility.monitor.frozen.clone();
            let monitor_display_gain = utility.monitor.display_gain_scale.clone();
            // Debounce state: only reload the filter-chain when the effective
            // state actually changed and at most every 400 ms, so fader drags
            // do not thrash the module.
            let last_pushed_sig = Rc::new(RefCell::new(String::new()));
            // Tracked separately from the payload signature so an A/B toggle can
            // skip the 400 ms fader-drag debounce.
            let last_pushed_eq_enabled = Rc::new(RefCell::new(true));
            let last_push = Rc::new(RefCell::new(
                std::time::Instant::now() - std::time::Duration::from_millis(500),
            ));
            // Handles the remote-control drain needs. `window` is used for
            // PresentWindow/Quit; `route_switch` so a D-Bus SetRoutingEnabled
            // moves the real widget (and the widget's own handler pushes the
            // change back, keeping the two in sync).
            let window_handle = window.clone();
            let route_switch_handle = route_switch.clone();
            let bypass_switch_handle = utility.bypass_switch.clone();
            let presets_handle = utility.presets.clone();
            let app_state_handle = app_state.clone();

            // Wire the Monitor Settings controls (the only place the app
            // exposes them, as upstream) to the backend.
            {
                let backend_ctl = backend.clone();
                let smoothing = utility.monitor.smoothing_scale.clone();
                smoothing.connect_value_changed(move |sc| {
                    if let Some(be) = backend_ctl.borrow_mut().as_mut() {
                        be.set_analyzer_smoothing(sc.value() / 100.0);
                    }
                });
            }
            {
                let backend_ctl = backend.clone();
                let gain = utility.monitor.display_gain_scale.clone();
                gain.connect_value_changed(move |sc| {
                    if let Some(be) = backend_ctl.borrow_mut().as_mut() {
                        be.set_analyzer_display_gain(sc.value());
                    }
                });
            }
            {
                let frozen = monitor_frozen.clone();
                let summary = monitor_summary.clone();
                utility
                    .monitor
                    .freeze_switch
                    .connect_state_set(move |_sw, on| {
                        frozen.set(on);
                        if on {
                            summary.set_text("Frozen");
                        }
                        glib::Propagation::Proceed
                    });
            }

            // Freeze plumbing for the tick: the flag itself plus the last frame
            // to hold while it is set.
            let monitor_frozen_tick = monitor_frozen.clone();
            let monitor_display_gain_tick = monitor_display_gain.clone();
            let held_levels: Rc<RefCell<Vec<f64>>> = Rc::new(RefCell::new(Vec::new()));

            glib::timeout_add_local(std::time::Duration::from_millis(33), move || {
                // --- Remote control (D-Bus) -----------------------------
                // The handlers cannot touch GTK objects directly (the vtable
                // closure is Send, GTK is main-thread-only), so they queue
                // commands here. Draining at the very top of the tick means a
                // preset loaded over D-Bus is applied BEFORE `bands` is
                // computed below, so the backend push at the end of this same
                // tick already carries the new coefficients.
                for cmd in app_state_handle.drain_pending() {
                    apply_remote_command(
                        &cmd,
                        &app_state_handle,
                        &backend,
                        &engine_sink,
                        &route_switch_handle,
                        &bypass_switch_handle,
                        &presets_handle,
                        &window_handle,
                    );
                }

                // Mirror the loaded preset into the shared state so GetState
                // agrees with the panel. Done here rather than inside the
                // panel's apply callback because that callback runs while
                // `presets` is already mutably borrowed by load_library_preset
                // — re-borrowing it there panics. One sync point also covers
                // both the UI row selection and the D-Bus command above.
                {
                    let loaded = presets_handle.borrow().current_preset_name();
                    if *app_state_handle.preset_name.lock().unwrap() != loaded {
                        *app_state_handle.preset_name.lock().unwrap() = loaded;
                        app_state_handle.emit_state_changed();
                    }
                }

                // Smooth override: the graph, the peak estimate and the
                // backend push must all see the SAME effective bands, or the
                // displayed curve would disagree with the audio.
                let smooth_on = headroom.borrow().smooth.get();
                let spread = headroom.borrow().smooth_spread_bands.get();
                let bands: Vec<crate::core::EqBand> = band_faders
                    .iter()
                    .map(|f| {
                        let fader = f.borrow();
                        crate::core::EqBand {
                            index: fader.index,
                            frequency: fader.frequency,
                            gain_db: fader.gain_db,
                            q: fader.q_value,
                            filter_type: fader.filter_type,
                            mute: fader.muted,
                            solo: fader.soloed,
                            coefficients: crate::core::BiquadCoefficients::identity(),
                        }
                    })
                    .collect();
                let bands = if smooth_on {
                    let spacing = crate::core::band_spacing_oct(&bands);
                    let q = crate::core::smooth_bell_q(spread, spacing);
                    bands
                        .iter()
                        .map(|b| crate::core::smooth_effective_band(b, q))
                        .collect()
                } else {
                    bands
                };
                // Auto-Safe: continuously clamp the preamp so the curve peak
                // stays under the target. Runs before reading `preamp_db` so
                // the graph, the meter and the backend push all see the
                // adjusted value. Sliding the EQ up auto-lowers the preamp;
                // sliding down lets it rise back toward 0.
                // Read the live monitor peak ONCE per tick: the analyzer's
                // take_window_peak() consumes the value, so a second read in
                // the same tick would come back empty.
                let live_peak = backend
                    .borrow()
                    .as_ref()
                    .and_then(|be| be.monitor_peak_dbfs());

                // The live spectrum, read ONCE per tick and shared by the graph
                // overlay and D-Bus. Cheap when the monitor is off:
                // monitor_levels() returns empty, so nothing is drawn.
                let fresh_levels = backend
                    .borrow()
                    .as_ref()
                    .map(|be| be.monitor_levels())
                    .unwrap_or_default();
                // Freeze holds the last frame instead of accepting new ones, so
                // the graph spectrum stands still. Upstream gates the same way
                // (`analyzer_frozen` in `on_analyzer_levels_idle`).
                let frozen = monitor_frozen_tick.get();
                // A caught SIGTERM/SIGINT becomes a clean quit here, because
                // this is the only place that can leave GTK properly -- and the
                // clean quit is what runs the routing restore. Without it a
                // `kill` takes the virtual sink down with the app still holding
                // the streams, and every player goes silent.
                if crate::exit_guard::take_terminate_request() {
                    log::warn!("termination signal received: shutting down cleanly");
                    app_for_exit.quit();
                    return ControlFlow::Break;
                }

                let levels = if frozen {
                    held_levels.borrow().clone()
                } else {
                    *held_levels.borrow_mut() = fresh_levels.clone();
                    fresh_levels
                };

                // Publish what the D-Bus interface reports, reusing the levels
                // already read above rather than asking the backend twice.
                // `visible` is derived rather than tracked so it cannot drift.
                app_state_handle.publish(
                    levels.clone(),
                    monitor_display_gain_tick.value(),
                    window_handle.is_visible(),
                    backend.borrow().is_some(),
                    backend
                        .borrow()
                        .as_ref()
                        .map(|be| be.monitor_enabled())
                        .unwrap_or(false),
                );
                // Rate-limited inside AppState (upstream parity: 100 ms). Only
                // while the monitor is running, matching upstream, which stops
                // the preview source when the analyzer is off and the spectrum
                // has decayed away.
                if !levels.is_empty() {
                    app_state_handle.maybe_emit_analyzer_levels_changed();
                }
                if headroom.borrow().auto_safe_enabled() {
                    let raw_peak = crate::core::estimate_response_peak_db(
                        &bands,
                        0.0,
                        crate::core::SAMPLE_RATE,
                    );
                    // Feed-forward ONLY, deliberately. A live-driven
                    // preamp was tried and rejected: this tick runs at
                    // 33 ms and there is no hard limiter in the PipeWire
                    // filter chain, so program transients (1-10 ms) pass
                    // and clip before any feedback loop can react. The
                    // curve peak is known in advance, which is the only
                    // thing here that can prevent clipping.
                    //
                    // The live peak is still used -- for the numeric label
                    // and the LED -- just never to drive the preamp.
                    let desired = crate::window_headroom::auto_safe_preamp_db(
                        raw_peak,
                        crate::window_headroom::AUTO_SAFE_TARGET_DBFS,
                    );
                    if (desired - headroom.borrow().preamp_value()).abs() > 0.01 {
                        headroom.borrow_mut().set_preamp_value(desired);
                    }
                }
                let preamp_db = headroom.borrow().preamp_value();

                // Keep the preset panel's state chip honest while editing.
                //
                // It was only refreshed when a preset was loaded or selected, so
                // it kept reading "Saved" while the curve moved away from the
                // preset -- and anything gated on that chip (the Update action)
                // would never wake up. Recomputed only when the signature
                // actually changes, so the cost is one payload hash per edit.
                {
                    let signature = crate::core::preset_payload_state_signature(
                        &crate::core::preset_payload(&bands, preamp_db),
                    );
                    if *last_state_signature.borrow() != signature {
                        *last_state_signature.borrow_mut() = signature;
                        presets_for_chip.borrow_mut().update_state_chip();
                    }
                }

                {
                    let mut g = graph.borrow_mut();
                    // Feed the live spectrum from the output monitor (empty when
                    // the monitor is off, so the overlay draws nothing).
                    //
                    // The graph also needs to know the A/B state (so a bypass
                    // greys the curve, as upstream does) and the display gain
                    // (so the analyzer's dBFS labels sit on the bars' scale).
                    g.update(
                        preamp_db,
                        &bands,
                        &levels,
                        !bypass_switch_handle.is_active(),
                        monitor_display_gain_tick.value(),
                        !levels.is_empty(),
                    );
                }
                // Live loudness readout in the monitor strip (short-term LUFS).
                // Frozen too: upstream only advances the loudness snapshot when
                // `analyzer_frozen` is clear.
                if let Some(be) = backend.borrow().as_ref() {
                    if be.monitor_enabled() && !frozen {
                        if let Some(loud) = be.monitor_loudness() {
                            let lufs = loud.shortterm_lufs;
                            if lufs.is_finite() {
                                monitor_loudness_value.set_text(&format!("{lufs:.1} LUFS"));
                                monitor_summary.set_text(&format!("On \u{00b7} {lufs:.1} LUFS"));
                            }
                        }
                    }
                }
                headroom.borrow_mut().update_curve_peak(&bands, preamp_db);

                // The peak readout prefers the LIVE monitor peak (actual output
                // level). When the monitor is off it falls back to the estimated
                // curve peak but only shows it as a boost, not a level, so a
                // flat/neutral EQ never looks like clipping. The curve peak is a
                // property of the EQ settings, not a measurement.
                //
                // Whether the clipping alert applies is NOT decided here: Fix
                // decides that itself (it knows whether Auto-Safe already owns
                // the preamp) and paints its own red class, and the blink follows
                // that class. An earlier version recomputed the condition from
                // the level here, which disagreed with the button's own state.
                headroom.borrow_mut().apply_live_peak(live_peak);

                // Push UI state to the PipeWire engine.
                //
                // Two different cadences, deliberately:
                //  - Band/preamp edits go through the 400 ms debounce. Those
                //    arrive in bursts while dragging and each push is a full
                //    control set; debouncing stops the drag thrashing the DSP.
                //  - The A/B bypass bypasses that debounce entirely. It is a
                //    single deliberate toggle and the whole point of A/B is
                //    instant comparison, so making the user wait up to 400 ms
                //    (and it can be missed entirely if they toggle back inside
                //    the window) makes the switch feel broken.
                let eq_enabled = !bypass_switch_handle.is_active();
                let payload_sig = crate::core::preset_payload_state_signature(
                    &crate::core::preset_payload(&bands, preamp_db),
                );
                let payload_changed = payload_sig != *last_pushed_sig.borrow();
                let bypass_changed = eq_enabled != *last_pushed_eq_enabled.borrow();
                let debounced_payload = payload_changed
                    && last_push.borrow().elapsed() >= std::time::Duration::from_millis(400);
                if debounced_payload || bypass_changed {
                    // Startup grace: the live node proxy is captured
                    // asynchronously after the module load. Pushing before it
                    // exists used to fall through to a full module
                    // unload+reload, cutting the audio a SECOND time just
                    // after startup. Leave the signature unpushed so the next
                    // tick retries once the proxy has arrived.
                    let live_ready = backend
                        .borrow()
                        .as_ref()
                        .map(|be| be.has_live_node())
                        .unwrap_or(false);
                    if live_ready {
                        if let Some(be) = backend.borrow_mut().as_mut() {
                            // Re-read the engine sink every tick: the Output
                            // dropdown can move it while this timer runs.
                            let sink_now = engine_sink.borrow().clone();
                            if !sink_now.is_empty() {
                                let _ = be.set_preamp(preamp_db);
                                *be.get_bands_mut() = bands.clone();
                                match be.update_state_live_or_reload(&sink_now, eq_enabled) {
                                    Ok(()) => {
                                        log::debug!(
                                            "Backend state applied (eq_enabled={eq_enabled})"
                                        );
                                    }
                                    Err(e) => log::warn!("Failed to apply backend state: {}", e),
                                }
                            }
                            // Mark pushed either way so a failing state is not
                            // retried every tick; further edits change the sig.
                            *last_pushed_sig.borrow_mut() = payload_sig;
                            *last_pushed_eq_enabled.borrow_mut() = eq_enabled;
                            *last_push.borrow_mut() = std::time::Instant::now();
                        }
                    }
                }
                ControlFlow::Continue
            });
        }

        // Blink the Headroom header icon while the EQ curve peak is over the
        // -1 dBFS target. GTK4 CSS has no @keyframes, so we toggle a class
        // on a 500 ms timer; a CSS color transition smooths it into a pulse.
        // Blink the clipping alert on the **Set Safe** button while the EQ
        // curve peak is over the -1 dBFS target. The header icon no longer
        // flashes: the alert belongs on the control that fixes it. GTK4 CSS
        // has no @keyframes, so we toggle a class on a 500 ms timer; a CSS
        // color transition smooths it into a pulse.
        // --- Output device dropdown.
        //
        // Populated from PipeWire rather than hardcoded. Index 0 is
        // "follow the system default", which is what the app already did;
        // the real sinks follow it.
        let output_names: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
        let output_dropdown = utility.output_dropdown.clone();
        let output_names_for_refresh = output_names.clone();
        let dropdown_for_refresh = output_dropdown.clone();
        let backend_for_outputs = backend.clone();
        // Identity of the output whose preset was last applied. Shared by the
        // dropdown and the default-sink watcher so neither of them re-applies a
        // preset for the sink the other already handled.
        let output_preset_identity: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        // True while index 0 ("Default Output") is selected. The default-sink
        // watcher below must not fight an explicitly chosen device.
        let output_follows_default = Rc::new(std::cell::Cell::new(true));

        let refresh_output_sinks = std::rc::Rc::new(move || {
            let sinks = backend_for_outputs
                .borrow()
                .as_ref()
                .map(|be| be.list_output_sinks())
                .unwrap_or_default();

            let mut names: Vec<String> = Vec::with_capacity(sinks.len() + 1);
            names.push(crate::window_utility::FOLLOW_DEFAULT_LABEL.to_string());
            let mut labels: Vec<String> =
                vec![crate::window_utility::FOLLOW_DEFAULT_LABEL.to_string()];
            for sink in &sinks {
                let label = crate::routing::display_label(&sink.description, &sink.name);
                labels.push(label);
                names.push(sink.name.clone());
            }

            // Only touch the model when it actually changed, otherwise the
            // dropdown would fight the user's selection on every poll.
            {
                let current = output_names_for_refresh.borrow();
                if *current == names {
                    return;
                }
            }

            let selected = dropdown_for_refresh.selected() as usize;
            let model = match dropdown_for_refresh.model() {
                Some(m) => m.downcast_ref::<gtk4::StringList>().cloned(),
                None => None,
            };
            if let Some(list) = model {
                let refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                list.splice(0, list.n_items(), &refs);
            }
            *output_names_for_refresh.borrow_mut() = names;

            // Preserve the selection by device name where possible.
            let want = selected.min(output_names_for_refresh.borrow().len().saturating_sub(1));
            dropdown_for_refresh.set_selected(want as u32);
            dropdown_for_refresh.set_sensitive(output_names_for_refresh.borrow().len() > 1);
        });

        {
            let names_for_select = output_names.clone();
            let backend_for_select = backend.clone();
            let engine_sink_for_select = engine_sink.clone();
            let follow_default_for_select = output_follows_default.clone();
            let output_preset_identity = output_preset_identity.clone();
            let presets_for_output = utility.presets.clone();
            let state_for_select = app_state.clone();
            let summary_for_select = utility.monitor.summary.clone();
            output_dropdown.connect_notify_local(Some("selected"), move |dd, _| {
                let idx = dd.selected() as usize;
                let names = names_for_select.borrow();
                // Index 0 means "follow the system default". That still has to
                // be resolved to a concrete sink: the filter chain's
                // destination is fixed when the module loads, so going back to
                // the default has to rebuild the chain onto whatever the
                // default currently is — the same work as picking a device.
                let chosen = if idx == 0 {
                    follow_default_for_select.set(true);
                    backend_for_select
                        .borrow_mut()
                        .as_mut()
                        .and_then(|be| be.default_output_sink())
                        .unwrap_or_default()
                } else {
                    follow_default_for_select.set(false);
                    match names.get(idx) {
                        Some(name) => name.clone(),
                        None => return,
                    }
                };
                if chosen.is_empty() {
                    log::warn!("Output device: selection resolved to no sink");
                    return;
                }
                if Some(chosen.as_str()) == Some(engine_sink_for_select.borrow().as_str()) {
                    log::debug!("Output device: already on {chosen}");
                    return;
                }
                // Rebuilding the filter chain is disruptive: the sink node is
                // destroyed and recreated, so there is a brief audio gap and
                // every routed stream has to be re-pointed at the new node. Do
                // it off the UI's critical path.
                // Inline on the GTK thread: PipeWire is main-thread-only, and
                // the existing System-EQ switch handler already does its
                // routing work the same way.
                let ok = backend_for_select
                    .borrow_mut()
                    .as_mut()
                    .map(|b| b.retarget_output(&chosen))
                    .unwrap_or(false);
                log::info!(
                    "Output switch to {chosen}: {}",
                    if ok { "ok" } else { "FAILED" }
                );
                if !ok {
                    return;
                }
                *engine_sink_for_select.borrow_mut() = chosen.clone();
                apply_output_preset_for_sink(&chosen, &output_preset_identity, &presets_for_output);
                if state_for_select.output_sink.lock().unwrap().as_deref() != Some(chosen.as_str())
                {
                    *state_for_select.output_sink.lock().unwrap() = Some(chosen.clone());
                    state_for_select.emit_state_changed();
                }
                // The monitor taps the monitor ports of a PHYSICAL sink, and
                // those ports are not touched by the chain rebuild — so after
                // a switch it is still listening to the sink the EQ just left,
                // where nothing plays any more. That is what froze the
                // spectrum and the peak meter the moment another output was
                // chosen. Follow the new output.
                if let Some(be) = backend_for_select.borrow_mut().as_mut() {
                    if be.monitor_enabled() {
                        match be.retarget_monitor(&chosen) {
                            Ok(()) => summary_for_select.set_text("On \u{00b7} Live (retargeted)"),
                            Err(e) => log::warn!("Monitor retarget to {chosen} failed: {e}"),
                        }
                    }
                }
            });
        }

        // Populate once at startup.
        refresh_output_sinks();
        let refresh_output_sinks = refresh_output_sinks.clone();

        {
            let set_safe = utility.headroom.borrow().set_safe_button.clone();
            let blink_on = Rc::new(std::cell::Cell::new(false));
            let last_default_sink: Rc<std::cell::Cell<String>> =
                Rc::new(std::cell::Cell::new(String::new()));
            let backend_sink_watch = backend.clone();
            let summary_sink_watch = utility.monitor.summary.clone();
            let refresh_outputs_watch = refresh_output_sinks.clone();
            let follow_default_watch = output_follows_default.clone();
            let engine_sink_watch = engine_sink.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                // Detect the system default output changing and follow it
                // with the MONITOR. The monitor is a separate capture
                // stream, so stop+start cannot interrupt the EQ audio path.
                //
                // The filter-chain's own output re-link is deliberately
                // NOT attempted here: doing it blind (remove link + create
                // link) risks silence or a feedback loop and needs live
                // validation against a real sink switch. See docs/TODO.md.
                // Re-read the output device list on the same cadence. The
                // refresh closure is a no-op unless the list actually changed,
                // so polling is cheap and also catches hotplug.
                refresh_outputs_watch();

                if let Some(be) = backend_sink_watch.borrow_mut().as_mut() {
                    // Only while the user has NOT picked a device. After an
                    // explicit choice this watcher would pull the monitor off
                    // the sink the EQ is actually playing to, freezing it
                    // again — the same failure the dropdown had.
                    let follow = follow_default_watch.get();
                    let on_engine_sink = follow
                        || be.default_output_sink().as_deref()
                            == Some(engine_sink_watch.borrow().as_str());
                    if let Some(now) = be.refresh_default_audio_sink_name() {
                        let prev = last_default_sink.replace(now.clone());
                        if !prev.is_empty() && prev != now && be.monitor_enabled() && on_engine_sink
                        {
                            log::info!("Default output changed {prev} -> {now}, following monitor");
                            match be.retarget_monitor(&now) {
                                Ok(()) => {
                                    summary_sink_watch.set_text("On \u{00b7} Live (retargeted)");
                                }
                                Err(e) => log::warn!("Monitor retarget failed: {e}"),
                            }
                        }
                    }
                }
                // Blink exactly while the Fix button is RED. The trigger is the
                // button's own red class, not the separate `headroom_warning`
                // level condition: those two are computed from different things
                // (live peak over the target vs the curve peak needing a
                // manual fix) and used to disagree, so the button could sit red
                // and still, or blink while grey. Red now means red-and-blinking
                // with no second opinion.
                let fix_is_red = set_safe.has_css_class(crate::window_headroom::CLIP_FIX_NEEDED);
                if fix_is_red {
                    blink_on.set(!blink_on.get());
                    if blink_on.get() {
                        set_safe.add_css_class("headroom-warning");
                    } else {
                        set_safe.remove_css_class("headroom-warning");
                    }
                } else if blink_on.get() || set_safe.has_css_class("headroom-warning") {
                    blink_on.set(false);
                    set_safe.remove_css_class("headroom-warning");
                }
                ControlFlow::Continue
            });
        }

        // events, sync roundtrips and module callbacks are dispatched without
        // running a second OS thread.
        {
            let backend = backend.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(10), move || {
                if let Some(be) = backend.borrow_mut().as_mut() {
                    be.pump();
                    // Advance the pending monitor port-linking (non-blocking,
                    // bounded work per tick).
                    be.pump_monitor_link();
                }
                ControlFlow::Continue
            });
        }

        // System-wide EQ switch: route all app playback streams into the
        // virtual EQ sink (on) / log that unrouting is not yet implemented
        // (off).
        {
            let backend_for_switch = backend.clone();
            let engine_sink_for_switch = engine_sink.clone();
            let state_for_switch = app_state.clone();
            let bypass_for_route = utility.bypass_switch.clone();
            route_switch.connect_state_set(move |_switch, on| {
                if let Some(be) = backend_for_switch.borrow_mut().as_mut() {
                    if on {
                        if let Err(e) = be.auto_route_to_sink(crate::core::VIRTUAL_SINK_BASE) {
                            log::warn!("System EQ: auto-route failed: {}", e);
                        }
                    } else {
                        // Recorded targets are restored verbatim; the EQ's own
                        // output sink is only the fallback for streams this
                        // process never routed.
                        let chain_output = engine_sink_for_switch.borrow().clone();
                        if let Err(e) = be.unroute_all(Some(&chain_output)) {
                            log::warn!("System EQ off: unroute failed: {}", e);
                        }
                    }
                }
                // The A/B switch bypasses the EQ *inside* mini_eq_sink, so it
                // can only be audible while app audio is actually routed
                // through that sink. With systemwide routing off, playback
                // streams go straight to the real output and every EQ control —
                // including this one — is out of the signal path. Leaving the
                // switch live and sensitive in that state made it look broken.
                bypass_for_route.set_sensitive(on);
                bypass_for_route.set_tooltip_text(Some(if on {
                    "Compare with/without the EQ. Works because app audio is routed through the EQ."
                } else {
                    "Turn on the systemwide EQ switch first — audio has to be routed \
                     through the EQ for this to have any effect."
                }));
                // Keep GetState / StateChanged honest when the change came
                // from the UI rather than from D-Bus.
                if *state_for_switch.routed.lock().unwrap() != on {
                    *state_for_switch.routed.lock().unwrap() = on;
                    state_for_switch.emit_state_changed();
                }
                glib::Propagation::Proceed
            });
        }

        // --- Tear down routing BEFORE the backend goes away.
        // Without this, closing the window unloads the filter-chain module
        // and destroys mini_eq_sink while every playback stream still has
        // target.node pointing at it. The audio then has nowhere to go and
        // the user hears silence until each app is restarted. This is the
        // "audio stops when I close the app" bug.
        {
            let backend_close = backend.clone();
            let engine_sink_close = engine_sink.clone();
            window.connect_close_request(move |_win| {
                log::info!("shutdown: close-request received, unrouting before teardown");
                if let Some(be) = backend_close.borrow_mut().as_mut() {
                    // Hand the streams back to the sink the EQ was feeding while
                    // the metadata object is still alive, then drop the monitor.
                    // Naming the destination rather than only clearing the
                    // target: clearing leaves it to session-manager policy, and a
                    // stream that is idle at shutdown can come back with nothing
                    // to play through once the virtual sink is gone.
                    let chain_output = engine_sink_close.borrow().clone();
                    if let Err(e) = be.restore_routing_on_exit(Some(&chain_output)) {
                        log::warn!("shutdown: routing restore failed: {}", e);
                    }
                    if be.monitor_enabled() {
                        be.stop_monitor();
                    }
                }
                glib::Propagation::Proceed
            });
        }

        // Monitor switch: start/stop the output spectrum capture. Mirrors
        // upstream `set_analyzer_enabled` -> `ensure_output_analyzer`, where
        // the analyzer captures the controller's output sink (the sink the
        // EQ outputs to, i.e. what you hear) via a monitor tap. Our capture
        // is a separate stream, so (unlike upstream) enabling it does not
        // require restarting the filter-chain engine.
        {
            let backend_for_monitor = backend.clone();
            let monitor_target = engine_sink.clone();
            let summary = utility.monitor.summary.clone();
            utility
                .graph
                .borrow()
                .monitor_switch
                .connect_state_set(move |_switch, on| {
                    if let Some(be) = backend_for_monitor.borrow_mut().as_mut() {
                        if on {
                            let target = resolve_monitor_target(&monitor_target, be);
                            if target.is_empty() {
                                log::warn!("Monitor: no output sink to capture");
                                summary.set_text("Off · no sink");
                                return glib::Propagation::Stop;
                            }
                            match be.start_monitor(&target) {
                                Ok(()) => {
                                    log::info!("Monitor enabled on {target}");
                                    summary.set_text("On · Live");
                                    let _ = crate::settings::save_monitor_enabled(true);
                                }
                                Err(e) => {
                                    log::warn!("Monitor start failed: {e}");
                                    summary.set_text("Off");
                                    return glib::Propagation::Stop;
                                }
                            }
                        } else {
                            be.stop_monitor();
                            summary.set_text("Off");
                            let _ = crate::settings::save_monitor_enabled(false);
                        }
                    }
                    glib::Propagation::Proceed
                });

            // Restore the persisted monitor state. load_monitor_enabled()
            // existed but was never called, so the monitor always came up
            // off regardless of how the user left it.
            let want_monitor = crate::settings::load_monitor_enabled();
            {
                let sw = utility.graph.borrow().monitor_switch.clone();
                let summary = utility.monitor.summary.clone();
                let backend_restore = backend.clone();
                let monitor_target = engine_sink.clone();
                sw.set_active(want_monitor);
                if want_monitor {
                    if let Some(be) = backend_restore.borrow_mut().as_mut() {
                        let target = resolve_monitor_target(&monitor_target, be);
                        if !target.is_empty() && be.start_monitor(&target).is_ok() {
                            log::info!("Monitor restored on {target}");
                            summary.set_text("On · Live");
                        }
                    }
                }
                let _ = summary;
            }
        }

        // Output Controls (Headroom panel): per-output-device auto-preset.
        // Fallback = default preset for unmatched outputs; Link to Output =
        // auto-load the current preset for the active output device. Both
        // act on the currently-selected preset (from the Preset panel).
        {
            let presets = utility.presets.clone();
            let fallback_label = utility.fallback_label.clone();
            utility.fallback_button.connect_clicked(move |_| {
                match presets.borrow().current_preset_name() {
                    Some(name) => {
                        if let Err(e) = crate::core::set_output_preset_fallback_name(&name) {
                            log::warn!("Set fallback preset failed: {e}");
                        } else {
                            fallback_label.set_text(&name);
                            log::info!("Fallback preset set to {name}");
                        }
                    }
                    None => log::info!("No preset selected to set as fallback"),
                }
            });

            let presets = utility.presets.clone();
            let link_label = utility.link_label.clone();
            utility.link_button.connect_clicked(move |_| {
                match presets.borrow().current_preset_name() {
                    Some(name) => {
                        // Key by the sink the EQ is actually feeding. The old
                        // code wrote the literal "default", so there was one
                        // undifferentiated entry no matter how many outputs
                        // existed -- and nothing read it anyway.
                        let key = crate::core::output_preset_key_for_sink(&engine_sink.borrow());
                        if key.is_empty() {
                            log::info!("Link preset: no output device known yet");
                            return;
                        }
                        if let Err(e) = crate::core::set_output_preset_link(&key, &name) {
                            log::warn!("Link preset to output failed: {e}");
                        } else {
                            link_label.set_text(&name);
                            log::info!("Linked preset {name} to output");
                        }
                    }
                    None => log::info!("No preset selected to link to output"),
                }
            });
        }

        {
            let headroom = utility.headroom.clone();
            let band_faders = band_faders.clone();
            utility
                .headroom
                .borrow()
                .set_safe_button
                .connect_clicked(move |_| {
                    let bands: Vec<crate::core::EqBand> = band_faders
                        .iter()
                        .map(|f| {
                            let fader = f.borrow();
                            crate::core::EqBand {
                                index: fader.index,
                                frequency: fader.frequency,
                                gain_db: fader.gain_db,
                                q: fader.q_value,
                                filter_type: fader.filter_type,
                                mute: fader.muted,
                                solo: fader.soloed,
                                coefficients: crate::core::BiquadCoefficients::identity(),
                            }
                        })
                        .collect();
                    let panel = headroom.borrow();
                    let peak = crate::core::estimate_response_peak_db(
                        &bands,
                        panel.preamp_value(),
                        crate::core::SAMPLE_RATE,
                    );
                    if peak <= 0.5 {
                        return;
                    }
                    panel.set_preamp_value(panel.preamp_value() - peak - 1.0);
                });
        }

        // Setup preset panel callbacks
        {
            let presets = utility.presets.clone();
            let band_faders = band_faders.clone();
            let default_sig = crate::core::preset_payload_state_signature(
                &crate::core::preset_payload(&crate::core::default_bands(), 0.0),
            );
            let apply_band_faders = band_faders.clone();
            let apply_headroom = utility.headroom.clone();
            let apply_refresh = refresh_editor.clone();
            let reset_band_faders = band_faders.clone();
            let reset_headroom = utility.headroom.clone();
            let reset_refresh = refresh_editor.clone();
            let sig_band_faders = band_faders.clone();
            let sig_headroom = utility.headroom.clone();
            let state_band_faders = band_faders.clone();
            let state_headroom = utility.headroom.clone();
            presets.borrow_mut().set_callbacks(
                Some(Box::new(move |bands, preamp| {
                    // Upstream re-syncs every fader from the loaded bands
                    // (`update_band_fader`), so mute/solo come from the preset
                    // rather than from the pre-load UI state.
                    let solo_active = crate::core::bands_have_solo(&bands);
                    apply_headroom.borrow().set_preamp_value(preamp);
                    for (i, band) in bands.iter().enumerate() {
                        if let Some(fader) = apply_band_faders.get(i) {
                            let frequency = band.frequency.clamp(
                                crate::core::EQ_FREQUENCY_MIN_HZ,
                                crate::core::EQ_FREQUENCY_MAX_HZ,
                            );
                            let q = band.q.clamp(crate::core::EQ_Q_MIN, crate::core::EQ_Q_MAX);
                            let mut f = fader.borrow_mut();
                            let selected = f.selected;
                            f.set_band_state(
                                band.gain_db,
                                frequency,
                                crate::window_band_fader::format_frequency_label(frequency),
                                q,
                                crate::window_band_fader::format_q_label(q),
                                band.filter_type,
                                crate::band_fader::filter_type_short_label(band.filter_type).into(),
                                selected,
                                band.filter_type != crate::core::FilterType::Off,
                                band.mute,
                                band.solo,
                                solo_active,
                            );
                            f.drawing_area.queue_draw();
                        }
                    }
                    recompute_solo_active(&apply_band_faders);
                    apply_refresh();
                })),
                Some(Box::new(move || {
                    // Upstream `reset_state` restores `default_bands()` (the
                    // first DEFAULT_ACTIVE_BANDS as neutral *Bell*s), NOT all
                    // Off. Setting Off left the bands inert so moving a fader
                    // produced no curve change ("EQ stays off").
                    let default_bands = crate::core::default_bands();
                    // Upstream `reset_state` also zeroes the preamp.
                    reset_headroom.borrow().set_preamp_value(0.0);
                    for (i, fader) in reset_band_faders.iter().enumerate() {
                        let mut f = fader.borrow_mut();
                        let band =
                            default_bands
                                .get(i)
                                .cloned()
                                .unwrap_or_else(|| crate::core::EqBand {
                                    index: i,
                                    frequency: 1000.0,
                                    gain_db: 0.0,
                                    q: 1.0,
                                    filter_type: crate::core::FilterType::Off,
                                    mute: false,
                                    solo: false,
                                    coefficients: crate::core::BiquadCoefficients::identity(),
                                });
                        let frequency = band.frequency;
                        let q = band.q;
                        let filter_type = band.filter_type;
                        let selected = f.selected;
                        f.set_band_state(
                            0.0,
                            frequency,
                            crate::window_band_fader::format_frequency_label(frequency),
                            q,
                            crate::window_band_fader::format_q_label(q),
                            filter_type,
                            crate::band_fader::filter_type_short_label(filter_type).into(),
                            selected,
                            i < crate::core::DEFAULT_ACTIVE_BANDS,
                            false,
                            false,
                            false,
                        );
                        f.drawing_area.queue_draw();
                    }
                    reset_refresh();
                })),
                Some(Box::new(move || {
                    let bands: Vec<crate::core::EqBand> = sig_band_faders
                        .iter()
                        .map(|f| {
                            let fader = f.borrow();
                            crate::core::EqBand {
                                index: fader.index,
                                frequency: fader.frequency,
                                gain_db: fader.gain_db,
                                q: fader.q_value,
                                filter_type: fader.filter_type,
                                mute: fader.muted,
                                solo: fader.soloed,
                                coefficients: crate::core::BiquadCoefficients::identity(),
                            }
                        })
                        .collect();
                    crate::core::preset_payload_state_signature(&crate::core::preset_payload(
                        &bands,
                        sig_headroom.borrow().preamp_value(),
                    ))
                })),
                // Live EQ state, for "Save preset". Same source as the
                // signature above -- the faders, not the loaded preset -- so
                // what gets written is the curve on screen.
                Some(Box::new(move || {
                    let bands: Vec<crate::core::EqBand> = state_band_faders
                        .iter()
                        .map(|f| {
                            let fader = f.borrow();
                            crate::core::EqBand {
                                index: fader.index,
                                frequency: fader.frequency,
                                gain_db: fader.gain_db,
                                q: fader.q_value,
                                filter_type: fader.filter_type,
                                mute: fader.muted,
                                solo: fader.soloed,
                                coefficients: crate::core::BiquadCoefficients::identity(),
                            }
                        })
                        .collect();
                    (bands, state_headroom.borrow().preamp_value())
                })),
            );
            presets.borrow_mut().set_default_signature(default_sig);

            // --- AutoEq import entry point.
            //
            // `window_autoeq.rs` builds a complete dialog (search, results,
            // curve preview) but nothing constructed it, so the feature was
            // unreachable despite README advertising it. The dialog is built
            // once and re-presented on later clicks.
            let autoeq_dialog: Rc<RefCell<Option<Rc<crate::window_autoeq::AutoEqDialog>>>> =
                Rc::new(RefCell::new(None));
            let autoeq_dialog_for_click = autoeq_dialog.clone();
            let presets_for_autoeq = utility.presets.clone();
            let window_for_autoeq = window.clone();
            presets.borrow_mut().set_autoeq_callback(Box::new(move || {
                let existing = autoeq_dialog_for_click.borrow().clone();
                if let Some(dlg) = existing {
                    dlg.show();
                    return;
                }

                let dlg = Rc::new(crate::window_autoeq::AutoEqDialog::new(
                    &window_for_autoeq,
                    // `load_autoeq_entries` appends "autoeq/entries.json"
                    // itself, so this must be the *app* config dir. Passing
                    // `user_config_dir()` put the cache in `~/.config/autoeq/`
                    // instead of `~/.config/mini-eq/autoeq/`.
                    crate::core::app_config_dir(),
                ));

                // Import = save as a preset and load it, matching the APO
                // file-import flow so both routes behave the same way.
                let presets_for_import = presets_for_autoeq.clone();
                dlg.set_import_callback(Box::new(move |bands, preamp, profile_name| {
                    let list_box = presets_for_import.borrow().list_box.clone();
                    // Never overwrite an existing preset: AutoEq names repeat
                    // across measurement sources.
                    let sanitized = crate::window_presets::unique_preset_name(&profile_name);
                    let dest = crate::core::preset_path_for_name(&sanitized);
                    match crate::core::save_preset_to_file(&dest, &bands, preamp) {
                        Ok(()) => {
                            crate::window_presets::refresh_preset_list(&list_box);
                            if let Err(e) = presets_for_import
                                .borrow_mut()
                                .load_library_preset(&sanitized)
                            {
                                log::warn!("AutoEq import: could not load {sanitized}: {e}");
                            } else {
                                log::info!("AutoEq imported as preset {sanitized}");
                            }
                        }
                        Err(e) => log::warn!("AutoEq import: could not save {sanitized}: {e}"),
                    }
                }));

                *autoeq_dialog_for_click.borrow_mut() = Some(dlg);
            }));
        }

        Self {
            window,
            toolbar_view,
            utility,
            split_view,
            band_scrolled,
            band_faders,
        }
    }

    pub fn present(&self) {
        self.window.present();
    }
}

/// Apply one command queued by the D-Bus remote-control interface.
///
/// Called from the 33 ms tick, on the GTK main thread, so it can touch both
/// widgets and the PipeWire backend directly.
///
/// Design note: for the two switches we set the widget rather than calling
/// the backend ourselves. `set_active` does not emit `state-set`, so the
/// existing `connect_state_set` handler is skipped — the routing is therefore
/// performed here, and the widget's own notify handler pushes the resulting
/// state back into `AppState`. That keeps one code path for "apply routing"
/// regardless of whether the change came from the UI or from D-Bus.
fn apply_remote_command(
    cmd: &crate::remote_control::RemoteCommand,
    app_state: &Arc<crate::remote_control::AppState>,
    backend: &Rc<RefCell<Option<PipeWireBackend>>>,
    engine_sink: &Rc<RefCell<String>>,
    route_switch: &gtk4::Switch,
    bypass_switch: &gtk4::Switch,
    presets: &Rc<RefCell<crate::window_presets::PresetPanel>>,
    window: &adw::ApplicationWindow,
) {
    use crate::remote_control::RemoteCommand;
    match cmd {
        RemoteCommand::SetRouting(on) => {
            if route_switch.is_active() != *on {
                route_switch.set_active(*on);
            }
            if let Some(be) = backend.borrow_mut().as_mut() {
                let result = if *on {
                    be.auto_route_to_sink(crate::core::VIRTUAL_SINK_BASE)
                } else {
                    be.unroute_all(Some(&engine_sink.borrow()))
                };
                if let Err(e) = result {
                    log::warn!("D-Bus SetRoutingEnabled({on}) failed: {e}");
                }
            }
            app_state.emit_state_changed();
        }
        RemoteCommand::SetEqEnabled(on) => {
            // `eq_enabled` means "EQ active", which is the inverse of the
            // A/B compare (bypass) switch. The debounced tick reads the
            // switch and pushes `eq_enabled` to the filter chain.
            let bypass = !*on;
            if bypass_switch.is_active() != bypass {
                bypass_switch.set_active(bypass);
            }
            *app_state.eq_enabled.lock().unwrap() = *on;
            app_state.emit_state_changed();
        }
        RemoteCommand::SetPreset(name) => match presets.borrow_mut().load_library_preset(name) {
            Ok(()) => app_state.emit_presets_changed(),
            Err(e) => log::warn!("D-Bus SetPreset({name}) failed: {e}"),
        },
        // `preset_name` itself is synced by the caller immediately after this
        // returns, so the `presets` borrow is released first.
        RemoteCommand::PresentWindow => {
            window.present();
        }
        RemoteCommand::Quit => {
            app_state.set_shutting_down(true);
            window.close();
        }
    }
}

fn create_menu_model() -> gio::Menu {
    let menu = gio::Menu::new();

    // NOTE the `win.` prefix: these actions are registered on the
    // ApplicationWindow, not the GApplication. `app.` resolves against the
    // GApplication's action group, so with `app.` GTK found no action and
    // rendered every item insensitive (disabled).
    let file_section = gio::Menu::new();
    file_section.append(Some("Preferences"), Some("win.preferences"));
    file_section.append(Some("Quit"), Some("win.quit"));
    menu.append_section(None, &file_section);

    let help_section = gio::Menu::new();
    help_section.append(Some("About mini-eq RR"), Some("win.about"));
    menu.append_section(None, &help_section);

    menu
}
