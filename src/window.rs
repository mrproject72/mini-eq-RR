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

/// Display label for a sink node name ("Built-in Audio" rather than
/// `alsa_output.pci-...`), falling back to the node name when unknown.
fn pretty_device_label(be: &PipeWireBackend, sink: &str) -> String {
    be.list_output_sinks()
        .into_iter()
        .find(|s| s.name == sink)
        .map(|s| crate::routing::display_label(&s.description, &s.name))
        .unwrap_or_else(|| sink.to_string())
}

/// The curve a device starts with when its EQ is enabled for the first
/// time this session: its linked preset when one is set in the config
/// (fallback names included there), else a neutral curve.
fn device_initial_curve(dev: &str) -> (Vec<crate::core::EqBand>, f64) {
    if let Some(name) = crate::core::output_preset_for_sink(dev) {
        if let Ok((preamp, bands)) =
            crate::core::load_preset_from_file(&crate::core::preset_path_for_name(&name))
        {
            return (bands, preamp);
        }
    }
    (crate::core::default_bands(), 0.0)
}

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
/// Called from the Output dropdown and the default-sink watcher. Both paths
/// retarget the chain first and then load the new device's preset, so the
/// curve that lands is the one the device is linked to.
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
/// The sink the monitor taps. A pinned monitor device (from
/// `output-monitor` in the config) wins; otherwise the chain's current
/// output.
fn resolve_monitor_target(be: &mut PipeWireBackend) -> String {
    be.resolve_monitor_target(None)
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

        // Output mode buttons (Selected / Reroute) live in the sidebar's
        // Output Controls panel (see window_utility.rs), not the header.
        let mode_selected = utility.mode_selected.clone();
        let mode_reroute = utility.mode_reroute.clone();

        // EQ on/off. A plain Off/On switch: "is audio routed through the EQ
        // at all". The mode buttons in Output Controls say *which* streams it
        // takes when it is on.
        let route_switch = gtk4::Switch::new();
        route_switch.set_tooltip_text(Some("Enable/Disable EQ"));
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
        // `state-set` (not `notify::active`) so the toggle itself carries the
        // state change. NOTE: this fires for programmatic `set_active` too,
        // so the D-Bus drain blocks it around its own sync (see
        // `bypass_state_handler`) instead of relying on it not firing.
        //
        // The handler id is kept so the D-Bus drain can block it around a
        // programmatic `set_active`: contrary to an older comment here,
        // GTK4 DOES emit `state-set` for `set_active`, and the live test
        // showed every remote toggle running twice.
        //
        // One cell holds the route switch's `state-set` handler id, so the
        // device-select path can block it while updating the switch to a newly
        // selected device's own on/off state (GTK4 emits `state-set` even for
        // `set_active`, which would otherwise fire a full EQ on/off).
        let route_state_handler: Rc<RefCell<Option<gtk4::glib::SignalHandlerId>>> =
            Rc::new(RefCell::new(None));
        {
            let state_for_bypass = app_state.clone();
            utility
                .bypass_switch
                .connect_state_set(move |_switch, bypassed| {
                    let eq_enabled = !bypassed;
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
                "Turn on the EQ switch first — audio has to be routed \
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

        // Shared output/monitor device state, owned at function scope and
        // cloned into the tick closure (remote drain), the Output dropdown,
        // the default-sink watcher and the monitor blocks below: which output
        // had its preset applied, whether the chain follows the system
        // default, and where the monitor is pinned. One cell for all three
        // drivers so D-Bus commands and the dropdown cannot diverge.
        let output_preset_identity: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
        // Devices whose chain still needs its payload pushed (waiting on the
        // live proxy). Shared by the tick, the per-device ON handlers and the
        // D-Bus drain.
        let device_pending: Rc<RefCell<std::collections::HashSet<String>>> =
            Rc::new(RefCell::new(std::collections::HashSet::new()));
        let device_pending_tick = device_pending.clone();
        let output_follows_default = Rc::new(std::cell::Cell::new(true));
        let monitor_pinned: Rc<RefCell<Option<String>>> =
            Rc::new(RefCell::new(crate::core::output_monitor_sink()));

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
            let device_pending = device_pending_tick.clone();
            let last_push = Rc::new(RefCell::new(
                std::time::Instant::now() - std::time::Duration::from_millis(500),
            ));
            // Handles the remote-control drain needs. `window` is used for
            // PresentWindow/Quit; `route_switch` so a D-Bus SetRoutingEnabled
            // moves the real widget (and the widget's own handler pushes the
            // change back, keeping the two in sync).
            let window_handle = window.clone();
            let route_switch_handle = route_switch.clone();
            let mode_selected_handle = mode_selected.clone();
            let mode_reroute_handle = mode_reroute.clone();
            let bypass_switch_handle = utility.bypass_switch.clone();
            let presets_handle = utility.presets.clone();
            let app_state_handle = app_state.clone();
            // Cloned for the remote drain inside the tick closure; the owners
            // live at function scope (created above) and are also consumed by
            // the dropdown/watcher/monitor blocks further down.
            let output_preset_identity = output_preset_identity.clone();
            let output_follows_default = output_follows_default.clone();
            let monitor_pinned = monitor_pinned.clone();
            let monitor_switch_handle = utility.graph.borrow().monitor_switch.clone();
            let monitor_summary_handle = utility.monitor.summary.clone();
            // Clones of the handler-id cells for the remote drain (the cells
            // themselves live at function scope, created before the bypass
            // switch block above).
            let route_state_handler_tick = route_state_handler.clone();

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
            // Throttle for the verified-flow count published to D-Bus: the
            // computation costs several registry passes, so it runs at most
            // every 15th tick (~500 ms) and the cached value is published in
            // between. Same cadence class as the 500 ms device watcher.
            let flow_tick: Rc<std::cell::Cell<usize>> = Rc::new(std::cell::Cell::new(0));
            let flow_cached: Rc<std::cell::Cell<usize>> = Rc::new(std::cell::Cell::new(0));
            // Auto-write-back debounce: the instant of the last edit that moved
            // the curve away from the linked preset. Cleared when the curve
            // comes back in sync, so a drag rewrites once ~1.5 s after it
            // stops rather than continuously.
            let last_edit_instant: Rc<RefCell<Option<std::time::Instant>>> =
                Rc::new(RefCell::new(None));

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
                        &mode_selected_handle,
                        &mode_reroute_handle,
                        &bypass_switch_handle,
                        &presets_handle,
                        &window_handle,
                        &output_preset_identity,
                        &output_follows_default,
                        &monitor_pinned,
                        &monitor_switch_handle,
                        &monitor_summary_handle,
                        &route_state_handler_tick,
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
                let output_preset_for_state =
                    crate::core::output_preset_for_sink(&engine_sink.borrow());
                let monitor_sink_for_state = crate::core::output_monitor_sink();
                // Verified audio flow for GetState (`eq_flowing` /
                // `flowing_streams`): several registry passes per computation,
                // so recompute at most every 15th tick (~500 ms) and publish
                // the cached value in between.
                flow_tick.set(flow_tick.get().wrapping_add(1));
                if flow_tick.get() % 15 == 0 {
                    flow_cached.set(
                        backend
                            .borrow()
                            .as_ref()
                            .map(|be| be.flowing_count())
                            .unwrap_or(0),
                    );
                }
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
                    backend
                        .borrow()
                        .as_ref()
                        .map(|be| be.output_mode())
                        .unwrap_or(crate::core::OutputRoutingMode::Selected),
                    output_preset_for_state,
                    monitor_sink_for_state,
                    // The chain's true destination, every tick: a refused or
                    // failed output switch must never linger in GetState.
                    Some(engine_sink.borrow().clone()).filter(|s| !s.is_empty()),
                    flow_cached.get(),
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

                // Auto-write-back: when the active output device is linked to a preset and
                // the live curve moves away from it, the curve is written into
                // that preset after a debounce, so a drag does not rewrite the
                // file continuously while the chain is reloading anyway. It
                // only writes into a preset linked from exactly one device --
                // writing into one linked from two would silently change the
                // curve for a device the user did not touch, which is worse
                // than leaving it Modified.
                //
                // The debounce is on the last EDIT, not the last tick: a drag
                // that settles rewrites once, ~1.5 s after it stops.
                {
                    let signature = crate::core::preset_payload_state_signature(
                        &crate::core::preset_payload(&bands, preamp_db),
                    );
                    let saved =
                        crate::core::output_preset_saved_signature_for_sink(&engine_sink.borrow());
                    if saved.as_deref() != Some(signature.as_str()) {
                        // The curve has moved away from the linked preset.
                        *last_edit_instant.borrow_mut() = Some(std::time::Instant::now());
                    } else {
                        // Back in sync: nothing to write.
                        *last_edit_instant.borrow_mut() = None;
                    }
                    if let Some(when) = *last_edit_instant.borrow() {
                        if when.elapsed() >= std::time::Duration::from_millis(1500) {
                            let sink = engine_sink.borrow().clone();
                            let bands_for_write = bands.clone();
                            let preamp_for_write = preamp_db;
                            *last_edit_instant.borrow_mut() = None;
                            if let Err(e) = crate::core::auto_write_output_preset_for_sink(
                                &sink,
                                &bands_for_write,
                                preamp_for_write,
                            ) {
                                log::warn!("Auto write-back failed: {e}");
                            } else {
                                // The panel's chip is recomputed from the live
                                // signature next tick, so it reflects the write.
                                presets_for_chip.borrow_mut().update_state_chip();
                            }
                        }
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
                                // set_text 30x/s with an identical string still
                                // invalidates the label: only touch the widgets
                                // when the displayed value actually moved.
                                let lufs_text = format!("{lufs:.1} LUFS");
                                if monitor_loudness_value.text() != lufs_text {
                                    monitor_loudness_value.set_text(&lufs_text);
                                    monitor_summary.set_text(&format!("On \u{00b7} {lufs_text}"));
                                }
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
                // Per-device EQ: the selected device's edits are the only band
                // payload; every known chain still deserves a wet/bypass push
                // when A/B changes. Anything pushed gets taken off the pending
                // set only when its live proxy has accepted the push (startup
                // grace, like the legacy singleton had).
                let selected = engine_sink.borrow().clone();
                if debounced_payload && !selected.is_empty() {
                    device_pending.borrow_mut().insert(selected.clone());
                }
                if bypass_changed {
                    let devs: Vec<String> = backend
                        .borrow()
                        .as_ref()
                        .map(|be| be.device_physical_sinks())
                        .unwrap_or_default();
                    for dev in devs {
                        device_pending.borrow_mut().insert(dev);
                    }
                }
                {
                    let mut pending = device_pending.borrow_mut();
                    for dev in pending.clone().iter() {
                        let (exists, ready) = {
                            let guard = backend.borrow();
                            match guard.as_ref() {
                                Some(be) => {
                                    (be.has_device_chain(dev), be.has_device_live_node(dev))
                                }
                                None => (false, false),
                            }
                        };
                        if !exists {
                            // Chain gone (failed creation or teardown): nothing
                            // to retry forever.
                            pending.remove(dev);
                            continue;
                        }
                        if !ready {
                            continue; // proxy still arriving; retry next tick
                        }
                        if let Some(be) = backend.borrow_mut().as_mut() {
                            if *dev == selected && !selected.is_empty() {
                                be.set_device_bands(dev, bands.clone());
                                be.set_device_preamp(dev, preamp_db);
                            }
                            // The GLOBAL A/B bypass state (upstream
                            // semantics): the A/B switch is one switch for
                            // all chains, while the route switch is the
                            // per-device engage/disengage. Pushing the
                            // per-device route state here would make the A/B
                            // switch inert; pushing the A/B state per-device
                            // would bypass other devices' chains when it is
                            // global by design.
                            match be.device_push_live(dev, eq_enabled) {
                                Ok(true) => {
                                    pending.remove(dev);
                                }
                                Ok(false) => {} // not ready yet
                                Err(e) => {
                                    pending.remove(dev);
                                    log::warn!("Backend device push failed: {e}");
                                }
                            }
                        }
                    }
                }
                if debounced_payload {
                    *last_pushed_sig.borrow_mut() = payload_sig;
                    *last_push.borrow_mut() = std::time::Instant::now();
                }
                if bypass_changed {
                    *last_pushed_eq_enabled.borrow_mut() = eq_enabled;
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
        // Cloned here too: the select handler below re-syncs the header
        // switch with the newly selected device's own on/off state.
        let route_switch_for_output = route_switch.clone();
        let route_handler_for_output = route_state_handler.clone();
        // Identity of the output whose preset was last applied, and whether
        // the chain follows the system default. Shared by the dropdown, the
        // default-sink watcher and the D-Bus commands; created before the
        // tick closure above, cloned here for these blocks.
        let output_preset_identity = output_preset_identity.clone();
        // True while index 0 ("Default Output") is selected. The default-sink
        // watcher below must not fight an explicitly chosen device.
        let output_follows_default = output_follows_default.clone();

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
                // Cloned, not borrowed: the refusal path below re-enters this
                // handler via `set_selected`, and a live borrow would panic.
                let names = names_for_select.borrow().clone();
                // Index 0 means "follow the system default". That still has to
                // be resolved to a concrete sink: the filter chain's
                // destination is fixed when the module loads, so going back to
                // the default has to rebuild the chain onto whatever the
                // default currently is — the same work as picking a device.
                // The follow flag is only committed once the switch is known
                // to go ahead (a refused switch must leave it untouched).
                let (chosen, want_follow) = if idx == 0 {
                    (
                        backend_for_select
                            .borrow_mut()
                            .as_mut()
                            .and_then(|be| be.default_output_sink())
                            .unwrap_or_default(),
                        true,
                    )
                } else {
                    match names.get(idx) {
                        Some(name) => (name.clone(), false),
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
                // Selecting a device NEVER re-engineers audio: every device has
                // its own chain and stays exactly as the user left it. All that
                // changes is the editing context (faders/preamp) and where the
                // monitor listens.
                follow_default_for_select.set(want_follow);
                *engine_sink_for_select.borrow_mut() = chosen.clone();
                if let Some(be) = backend_for_select.borrow_mut().as_mut() {
                    be.set_selected_sink(&chosen);
                }
                apply_output_preset_for_sink(&chosen, &output_preset_identity, &presets_for_output);
                if state_for_select.output_sink.lock().unwrap().as_deref() != Some(chosen.as_str())
                {
                    *state_for_select.output_sink.lock().unwrap() = Some(chosen.clone());
                    state_for_select.emit_state_changed();
                }
                // The header switch always reflects the SELECTED device's own
                // on/off state, so flipping to a device whose EQ is off shows
                // it off (and vice versa) without touching audio.
                let dev_on = backend_for_select
                    .borrow()
                    .as_ref()
                    .is_some_and(|be| be.has_routed_streams_for(&chosen));
                {
                    let sw = route_switch_for_output.clone();
                    if let Some(id) = route_handler_for_output.borrow().as_ref() {
                        sw.block_signal(id);
                    }
                    if dev_on != sw.is_active() {
                        sw.set_active(dev_on);
                    }
                    if let Some(id) = route_handler_for_output.borrow().as_ref() {
                        sw.unblock_signal(id);
                    }
                }
                if *state_for_select.routed.lock().unwrap() != dev_on {
                    *state_for_select.routed.lock().unwrap() = dev_on;
                    state_for_select.emit_state_changed();
                }
                // The monitor taps the PHYSICAL sink: follow the selection when
                // no device was pinned manually.
                if let Some(be) = backend_for_select.borrow_mut().as_mut() {
                    if be.monitor_enabled() && crate::core::output_monitor_sink().is_none() {
                        match be.retarget_monitor_if_different(&chosen) {
                            Ok(_) => summary_for_select.set_text("On \u{00b7} Live (retargeted)"),
                            Err(e) => log::warn!("Monitor retarget failed: {e}"),
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
            let refresh_outputs_watch = refresh_output_sinks.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(500), move || {
                // Detect the system default output changing and follow it
                // with the EQ itself. `retarget_output` moves the chain's
                // output stream live when it can and only rebuilds the module
                // as a fallback, so this is cheap and does not touch any
                // playback stream.
                //
                // The change is delivered by the metadata `property` listener
                // in real time (`take_default_sink_change`), so this timer is
                // a cheap flag read, not a poll: no PipeWire pump, no 50 ms
                // stall per tick. The 500 ms cadence is only the debounce
                // between acting on it.
                //
                // The chain only follows the default while the user has NOT
                // picked a device: an explicit choice pins the chain, and
                // letting this watcher pull it away would undo the selection.
                // Re-read the output device list on the same cadence. The
                // refresh closure is a no-op unless the list actually changed,
                // so polling is cheap and also catches hotplug.
                refresh_outputs_watch();

                if let Some(be) = backend_sink_watch.borrow_mut().as_mut() {
                    if let Some(now) = be.take_default_sink_change() {
                        let prev = last_default_sink.replace(now.clone());
                        if !prev.is_empty() && prev != now {
                            // With one chain per device there is nothing to
                            // follow: a chain only serves its own device, so a
                            // default change does not move anything. New
                            // streams just resolve to whichever device they
                            // actually play to, and that device's chain handles
                            // them if the user enabled EQ there. We still log
                            // it for visibility.
                            log::info!("Default output changed {prev} -> {now}");
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
                    // Deferred route verifications run on EVERY pump tick, not
                    // just dirty ones: a stream whose property echo lagged the
                    // write needs revisiting without waiting for a new event.
                    // Early-out when nothing is pending.
                    be.process_pending_verifies();
                    // Event-driven adoption: the registry/metadata listeners
                    // flag arrivals and moves; drain the flag into one pass.
                    // Idle cost is a single flag check.
                    if be.take_streams_dirty() {
                        match be.adopt_new_streams() {
                            Ok(n) if n > 0 => {
                                log::info!("Adopted {n} new stream(s) into device chains")
                            }
                            Err(e) => log::warn!("Stream adoption failed: {e}"),
                            _ => {}
                        }
                    }
                }
                ControlFlow::Continue
            });
        }

        // EQ on/off, which also carries the bypass and the routed flag.
        // The mode is read from the buttons at switch-on and remembered, so
        // toggling off and back on restores the mode rather than always
        // re-routing everything. Switching off hands the streams back
        // (unroute), so the EQ truly leaves the signal path.
        {
            let backend_for_switch = backend.clone();
            let state_for_switch = app_state.clone();
            let bypass_for_route = utility.bypass_switch.clone();
            let mode_reroute_for_switch = mode_reroute.clone();
            let engine_sink_for_switch = engine_sink.clone();
            let device_pending_for_switch = device_pending.clone();
            let toast_for_switch = toast_overlay.clone();
            // Handler id kept for the D-Bus drain (see bypass above):
            // `set_active` emits `state-set`, so the drain blocks this while
            // syncing the widget and performs the routing itself.
            *route_state_handler.borrow_mut() = Some(
                route_switch.connect_state_set(move |_switch, on| {
                if !on {
                    bypass_for_route.set_sensitive(false);
                    bypass_for_route.set_tooltip_text(Some(
                        "Turn on the EQ switch first — audio has to be \
                         routed through the EQ for this to have any effect.",
                    ));
                    if let Some(be) = backend_for_switch.borrow_mut().as_mut() {
                        // EQ off for the selected device: its streams are handed
                        // back to their real targets, and only that device is
                        // touched. Every other device's chain keeps running.
                        let dev = engine_sink_for_switch.borrow().clone();
                        if !dev.is_empty() {
                            be.set_device_eq_enabled(&dev, false);
                            if let Err(e) = be.unroute_device(&dev, Some(&dev)) {
                                log::warn!("EQ off: unroute device failed: {e}");
                            }
                        }
                    }
                    if *state_for_switch.routed.lock().unwrap() {
                        *state_for_switch.routed.lock().unwrap() = false;
                        state_for_switch.emit_state_changed();
                    }
                    return glib::Propagation::Proceed;
                }
                if let Some(be) = backend_for_switch.borrow_mut().as_mut() {
                    let mode = if mode_reroute_for_switch.is_active() {
                        crate::core::OutputRoutingMode::Reroute
                    } else {
                        crate::core::OutputRoutingMode::Selected
                    };
                    be.set_output_mode(mode);
                    let dev = engine_sink_for_switch.borrow().clone();
                    if !dev.is_empty() {
                        // The device's linked curve applies on EVERY
                        // route-on, not only at chain creation: a chain
                        // created at startup (persisted eq_enabled) runs the
                        // neutral default, and its linked preset would never
                        // load (the two-device test: one device played flat
                        // while the other device's curve was audible).
                        let (bands, preamp) = device_initial_curve(&dev);
                        if !be.has_device_chain(&dev) {
                            if let Err(e) = be.ensure_device_chain(&dev, bands) {
                                log::warn!("EQ on: failed to start EQ for {dev}: {e}");
                            } else {
                                be.set_device_preamp(&dev, preamp);
                            }
                        } else {
                            be.set_device_bands(&dev, bands);
                            be.set_device_preamp(&dev, preamp);
                        }
                        be.set_current_sink(&dev);
                        // Remember the intent even with zero streams: streams
                        // that appear later are adopted event-driven.
                        be.set_device_eq_enabled(&dev, true);
                        let eq_name = crate::core::eq_virtual_sink_for(&dev);
                        match be.auto_route_to_sink(&eq_name) {
                            Ok(()) => {
                                // Loud when nothing matched: EQ on with zero
                                // routed streams is exactly the "EQ does
                                // nothing" report. The routing layer already
                                // WARNs; mirror it where the user looks.
                                if be.last_routed_count() == 0 {
                                    toast_for_switch.add_toast(adw::Toast::new(&format!(
                                        "EQ is on, but no audio is playing to {} — nothing to equalize",
                                        pretty_device_label(be, &dev)
                                    )));
                                }
                            }
                            Err(e) => log::warn!("EQ on: auto-route failed: {e}"),
                        }
                        device_pending_for_switch.borrow_mut().insert(dev.clone());
                        // Keep the monitor on this device while following
                        // (no pinned device) -- it is freshly audible now.
                        if be.monitor_enabled()
                            && crate::core::output_monitor_sink().is_none()
                        {
                            if let Err(e) = be.retarget_monitor_if_different(&dev) {
                                log::warn!("EQ on: monitor retarget failed: {e}");
                            }
                        }
                    }
                }
                bypass_for_route.set_sensitive(true);
                bypass_for_route.set_tooltip_text(Some(
                    "Compare with/without the EQ. Works because app audio is routed through the EQ.",
                ));
                if *state_for_switch.routed.lock().unwrap() != true {
                    *state_for_switch.routed.lock().unwrap() = true;
                    state_for_switch.emit_state_changed();
                }
                glib::Propagation::Proceed
            }));
        }

        // The two mode buttons are mutually exclusive and the mode is persisted
        // across restarts, so the remembered choice is restored here rather
        // than only shown after the user clicks. The default is Selected, which
        // is also what a missing or version-1 config reads as.
        {
            let backend_restore = backend.clone();
            let mode_selected_restore = mode_selected.clone();
            let mode_reroute_restore = mode_reroute.clone();
            let restore_mode = std::rc::Rc::new(move || {
                let mode = crate::core::output_routing_mode();
                if mode == crate::core::OutputRoutingMode::Reroute {
                    mode_reroute_restore.set_active(true);
                    mode_selected_restore.set_active(false);
                } else {
                    mode_selected_restore.set_active(true);
                    mode_reroute_restore.set_active(false);
                }
                if let Some(be) = backend_restore.borrow_mut().as_mut() {
                    be.set_output_mode(mode);
                }
            });
            // Clicking one turns the other off. The mode is only acted on when
            // the switch is on; turning Reroute on while the EQ is off does not
            // route anything, it only decides what a later switch-on does.
            let mode_reroute_a = mode_reroute.clone();
            let backend_a = backend.clone();
            let state_a = app_state.clone();
            let engine_sink_for_modes = engine_sink.clone();
            let persist_selected = std::rc::Rc::new(move || {
                let mode = if mode_reroute_a.is_active() {
                    crate::core::OutputRoutingMode::Reroute
                } else {
                    crate::core::OutputRoutingMode::Selected
                };
                if let Err(e) = crate::core::set_output_routing_mode(mode) {
                    log::warn!("persist output mode: {e}");
                }
                if let Some(be) = backend_a.borrow_mut().as_mut() {
                    be.set_output_mode(mode);
                    if be.is_routed() {
                        if mode == crate::core::OutputRoutingMode::Reroute {
                            // Reroute: everything moves into the SELECTED
                            // device's chain (its presets/bands apply to all).
                            let sel = engine_sink_for_modes.borrow().clone();
                            if !sel.is_empty() {
                                let eq = crate::core::eq_virtual_sink_for(&sel);
                                match be.auto_route_to_sink(&eq) {
                                    Ok(()) => {
                                        log::info!("Output mode -> {mode:?} (re-routed into {eq})")
                                    }
                                    Err(e) => log::warn!("Output mode re-route failed: {e}"),
                                }
                            }
                        } else {
                            // Narrowing to Selected: hand back the streams that
                            // are effectively aimed elsewhere in every chain.
                            for dev in be.device_physical_sinks() {
                                if let Err(e) = be.rescope_device(&dev) {
                                    log::warn!("Output mode rescope failed for {dev}: {e}");
                                }
                            }
                        }
                    }
                }
                if *state_a.routed.lock().unwrap() {
                    state_a.emit_state_changed();
                }
            });
            let persist_reroute = persist_selected.clone();
            let mode_selected_b = mode_selected.clone();
            let mode_reroute_b = mode_reroute.clone();
            mode_selected.connect_clicked(move |_btn| {
                if !mode_selected_b.is_active() {
                    return;
                }
                mode_reroute_b.set_active(false);
                persist_selected();
            });
            let mode_selected_c = mode_selected.clone();
            let mode_reroute_c = mode_reroute.clone();
            mode_reroute.connect_clicked(move |_btn| {
                if !mode_reroute_c.is_active() {
                    return;
                }
                mode_selected_c.set_active(false);
                persist_reroute();
            });
            restore_mode();
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
            let summary = utility.monitor.summary.clone();
            // Handler id kept for the D-Bus drain and the restore below, both
            // of which sync the widget programmatically (`set_active` emits
            // `state-set`, so they block this while doing so and perform the
            // backend work themselves).
            {
                let state_for_monitor = app_state.clone();
                utility
                    .graph
                    .borrow()
                    .monitor_switch
                    .connect_state_set(move |_switch, on| {
                        if let Some(be) = backend_for_monitor.borrow_mut().as_mut() {
                            if on {
                                let target = resolve_monitor_target(be);
                                if target.is_empty() {
                                    log::warn!("Monitor: no output sink to capture");
                                    summary.set_text("Off · no sink");
                                    return glib::Propagation::Stop;
                                }
                                match be.start_monitor(&target) {
                                    Ok(()) => {
                                        log::info!("Monitor enabled on {target}");
                                        summary.set_text(&format!(
                                            "On \u{00b7} Live{}",
                                            if let Some(pinned) = crate::core::output_monitor_sink()
                                            {
                                                format!(" ({pinned})")
                                            } else {
                                                String::new()
                                            }
                                        ));
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
                        *state_for_monitor.analyzer_enabled.lock().unwrap() = on;
                        state_for_monitor.emit_state_changed();
                        glib::Propagation::Proceed
                    });
            }
            // Restore the persisted monitor state; the widget handler above
            // starts the capture on set_active.
            {
                let sw = utility.graph.borrow().monitor_switch.clone();
                sw.set_active(crate::settings::load_monitor_enabled());
            }
        }

        // Output Controls (Headroom panel): per-output-device auto-preset.
        // Fallback = default preset for unmatched outputs; the Curve dropdown
        // = the preset linked to the active output device. Both act on the
        // currently-selected preset (from the Preset panel).
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
        }

        // Curve: the preset linked to the active output device. Choosing a
        // preset in this dropdown links it to the device immediately, so the
        // reader will use it on the next switch. Unlink drops the link and the
        // fallback applies instead.
        //
        // The model is refilled from the preset library on a 330 ms cadence,
        // so a preset created or deleted elsewhere shows up here without a
        // restart. The active device's linked preset is selected; "(none)"
        // means the fallback applies.
        {
            let curve_dropdown = utility.curve_dropdown.clone();
            let unlink_button = utility.unlink_button.clone();
            let engine_sink_for_curve = engine_sink.clone();
            let presets_for_curve = utility.presets.clone();
            // Guard so programmatic model/selection refreshes do not fire
            // the link handler below (which would re-link + reload a preset
            // on every 330 ms tick).
            let curve_syncing: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
            let curve_known: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
            let curve_syncing_for_refresh = curve_syncing.clone();
            // Full preset list for the Curve dropdown: "(none)" (fallback
            // applies) + built-ins + customs. Previously this only listed
            // custom presets and never showed "(none)", so with a single
            // preset it looked like it only contained the current curve.
            let refresh_curve_model = std::rc::Rc::new(move || {
                let names = crate::window_presets::curve_model_names();
                let active = crate::core::output_preset_for_sink(&engine_sink_for_curve.borrow());
                // Position of the linked preset in the model (0 = "(none)").
                let want: u32 = active
                    .as_ref()
                    .and_then(|name| names.iter().position(|n| n == name))
                    .unwrap_or(0) as u32;
                let selected = curve_dropdown.selected();
                if *curve_known.borrow() == names && selected == want {
                    return;
                }
                *curve_known.borrow_mut() = names.clone();
                let refs: Vec<&str> = names.iter().map(|s| s.as_str()).collect();
                let model = gtk4::StringList::new(&refs);
                curve_syncing_for_refresh.set(true);
                curve_dropdown.set_model(Some(&model));
                curve_dropdown.set_selected(want);
                curve_syncing_for_refresh.set(false);
                unlink_button.set_sensitive(active.is_some());
            });
            let refresh_curve_model_for_tick = refresh_curve_model.clone();
            let curve_dropdown_for_tick = utility.curve_dropdown.clone();
            let unlink_button_for_tick = utility.unlink_button.clone();
            let engine_sink_for_unlink = engine_sink.clone();
            let curve_dropdown_for_unlink = utility.curve_dropdown.clone();
            let unlink_button_for_unlink = utility.unlink_button.clone();
            let presets_for_link = utility.presets.clone();
            let engine_sink_for_link = engine_sink.clone();
            let unlink_button_for_link = utility.unlink_button.clone();
            let curve_dropdown_for_notify = utility.curve_dropdown.clone();
            let unlink_button_for_click = utility.unlink_button.clone();
            let curve_syncing_for_notify = curve_syncing.clone();
            curve_dropdown_for_notify.connect_selected_notify(move |dd| {
                if curve_syncing_for_notify.get() {
                    return;
                }
                let pos = dd.selected();
                if pos == 0 {
                    // "(none)" is not a real preset; treat it as "no link".
                    return;
                }
                let Some(name) = dd
                    .model()
                    .and_then(|m| m.downcast_ref::<gtk4::StringList>().cloned())
                    .and_then(|m| m.string(pos))
                else {
                    return;
                };
                let name = name.to_string();
                let key = crate::core::output_preset_key_for_sink(&engine_sink_for_link.borrow());
                if key.is_empty() {
                    log::info!("Link preset: no output device known yet");
                    return;
                }
                if let Err(e) = crate::core::set_output_preset_link(&key, &name) {
                    log::warn!("Link preset to output failed: {e}");
                } else {
                    log::info!("Linked preset {name} to output {key}");
                    // Loading the linked preset now means the curve on screen
                    // is the one this device will use.
                    if let Err(e) = presets_for_link.borrow_mut().load_library_preset(&name) {
                        log::warn!("Could not load linked preset {name}: {e}");
                    }
                    unlink_button_for_link.set_sensitive(true);
                }
            });
            unlink_button_for_click.connect_clicked(move |_| {
                let key = crate::core::output_preset_key_for_sink(&engine_sink_for_unlink.borrow());
                if key.is_empty() {
                    log::info!("Unlink preset: no output device known yet");
                    return;
                }
                match crate::core::clear_output_preset_link(std::slice::from_ref(&key)) {
                    Ok(true) => {
                        log::info!("Unlinked preset from output {key}");
                        curve_dropdown_for_unlink.set_selected(0);
                        unlink_button_for_unlink.set_sensitive(false);
                    }
                    Ok(false) => {}
                    Err(e) => log::warn!("Unlink preset failed: {e}"),
                }
            });
            // Refill the model on a 330 ms cadence so a preset created or
            // deleted elsewhere is visible here without a restart, and so the
            // selection tracks the active device's linked preset.
            {
                let refresh_for_tick = refresh_curve_model_for_tick.clone();
                let _ = curve_dropdown_for_tick;
                let _ = unlink_button_for_tick;
                let _ = presets_for_curve;
                glib::timeout_add_local(std::time::Duration::from_millis(330), move || {
                    refresh_for_tick();
                    glib::ControlFlow::Continue
                });
            }
            refresh_curve_model();
        }

        // Monitor device: which sink the spectrum and loudness readout tap.
        // `Follow EQ output` (index 0) re-resolves to whatever the chain is
        // playing to; a specific sink pins the monitor to one device so you
        // can equalise on headphones and still watch the speakers. It is
        // listen-only — it does not change where the EQ'd audio goes. The
        // choice is persisted in `output-presets.json` as `monitor`, so a
        // pinned monitor survives a chain rebuild and `Follow EQ output`
        // re-resolves after it.
        {
            let monitor_dropdown = utility.monitor.monitor_dropdown.clone();
            let backend_for_monitor = backend.clone();
            // Pinned monitor device, shared with the D-Bus SetMonitorSink
            // command; created before the tick closure above.
            let monitor_pinned = monitor_pinned.clone();
            let monitor_pinned_for_refresh = monitor_pinned.clone();
            // Parallel node names for the labels above (index 0 = follow,
            // i.e. no name). The model is only rebuilt when the sink set
            // actually changes; rebuilding every tick reset the user's
            // selection and re-fired the notify handler below, which is what
            // dropped the monitor after an output switch.
            let monitor_known: Rc<RefCell<Vec<String>>> = Rc::new(RefCell::new(Vec::new()));
            let monitor_syncing: Rc<std::cell::Cell<bool>> = Rc::new(std::cell::Cell::new(false));
            let monitor_known_for_refresh = monitor_known.clone();
            let monitor_syncing_for_refresh = monitor_syncing.clone();
            let refresh_monitor_model = std::rc::Rc::new(move || {
                let sinks = backend_for_monitor
                    .borrow()
                    .as_ref()
                    .map(|be| be.list_output_sinks())
                    .unwrap_or_default();
                let mut names: Vec<String> = Vec::with_capacity(sinks.len() + 1);
                names.push(String::new());
                let mut labels: Vec<String> = Vec::with_capacity(sinks.len() + 1);
                labels.push("Follow EQ output".to_string());
                for sink in &sinks {
                    names.push(sink.name.clone());
                    labels.push(crate::routing::display_label(&sink.description, &sink.name));
                }
                // Preserve the selection by NODE NAME where possible. On the
                // first refresh the persisted pin decides the initial
                // selection, so a pinned monitor survives a restart even
                // before the dropdown has ever been touched. (Labels are
                // display strings like "Built-in Audio"; they never equal the
                // persisted node name, so matching against them lost the pin.)
                let want = if let Some(pinned) = monitor_pinned_for_refresh.borrow().as_ref() {
                    names
                        .iter()
                        .position(|n| n == pinned.as_str())
                        .map(|i| i as u32)
                        .unwrap_or(0)
                } else {
                    let selected = monitor_dropdown.selected() as usize;
                    let current = monitor_known_for_refresh.borrow().clone();
                    if selected < current.len() && !current[selected].is_empty() {
                        let prev = current[selected].clone();
                        names
                            .iter()
                            .position(|n| n == &prev)
                            .map(|i| i as u32)
                            .unwrap_or(0)
                    } else {
                        0
                    }
                };
                if *monitor_known_for_refresh.borrow() == names
                    && monitor_dropdown.selected() == want
                {
                    monitor_dropdown.set_sensitive(sinks.len() > 1);
                    return;
                }
                *monitor_known_for_refresh.borrow_mut() = names;
                let label_refs: Vec<&str> = labels.iter().map(|s| s.as_str()).collect();
                let model = gtk4::StringList::new(&label_refs);
                monitor_syncing_for_refresh.set(true);
                monitor_dropdown.set_model(Some(&model));
                monitor_dropdown.set_sensitive(sinks.len() > 1);
                monitor_dropdown.set_selected(want);
                monitor_syncing_for_refresh.set(false);
            });
            let refresh_monitor_model_for_tick = refresh_monitor_model.clone();
            let monitor_dropdown_for_notify = utility.monitor.monitor_dropdown.clone();
            let backend_for_notify = backend.clone();
            let summary_for_notify = utility.monitor.summary.clone();
            let monitor_pinned_for_notify = monitor_pinned.clone();
            let monitor_syncing_for_notify = monitor_syncing.clone();
            let monitor_known_for_notify = monitor_known.clone();
            monitor_dropdown_for_notify.connect_notify_local(Some("selected"), move |dd, _| {
                if monitor_syncing_for_notify.get() {
                    return;
                }
                let idx = dd.selected() as usize;
                if idx == 0 {
                    // Follow the chain's current output.
                    *monitor_pinned_for_notify.borrow_mut() = None;
                    if let Err(e) = crate::core::set_output_monitor_sink(None) {
                        log::warn!("persist monitor device: {e}");
                    }
                    if let Some(be) = backend_for_notify.borrow_mut().as_mut() {
                        let target = be.resolve_monitor_target(None);
                        if !target.is_empty() && be.monitor_enabled() {
                            match be.retarget_monitor_if_different(&target) {
                                Ok(_) => summary_for_notify.set_text("On \u{00b7} Live (follow)"),
                                Err(e) => log::warn!("Monitor retarget failed: {e}"),
                            }
                        }
                    }
                    return;
                }
                let name = monitor_known_for_notify
                    .borrow()
                    .get(idx)
                    .cloned()
                    .unwrap_or_default();
                // Fall back to a live listing if the known model is stale.
                let name = if name.is_empty() {
                    backend_for_notify
                        .borrow()
                        .as_ref()
                        .map(|be| be.list_output_sinks())
                        .unwrap_or_default()
                        .get(idx.saturating_sub(1))
                        .map(|s| s.name.clone())
                        .unwrap_or_default()
                } else {
                    name
                };
                if name.is_empty() {
                    return;
                }
                *monitor_pinned_for_notify.borrow_mut() = Some(name.clone());
                if let Err(e) = crate::core::set_output_monitor_sink(Some(&name)) {
                    log::warn!("persist monitor device: {e}");
                }
                if let Some(be) = backend_for_notify.borrow_mut().as_mut() {
                    if be.monitor_enabled() {
                        match be.retarget_monitor_if_different(&name) {
                            Ok(_) => {
                                summary_for_notify.set_text(&format!("On \u{00b7} Live ({name})"));
                            }
                            Err(e) => log::warn!("Monitor retarget to {name} failed: {e}"),
                        }
                    }
                }
            });
            let refresh_for_tick = refresh_monitor_model_for_tick.clone();
            glib::timeout_add_local(std::time::Duration::from_millis(330), move || {
                refresh_for_tick();
                glib::ControlFlow::Continue
            });
            refresh_monitor_model();
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

        // The UI starts out selecting the engine device; record it so
        // Reroute adoption knows where late streams belong from the start.
        {
            let selected = engine_sink.borrow().clone();
            if !selected.is_empty()
                && let Some(be) = backend.borrow_mut().as_mut()
            {
                be.set_selected_sink(&selected);
            }
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
/// Design note: the arms mirror the widget selection into the widget AND
/// perform the backend work themselves, with the widget's own `state-set`
/// handler blocked around the programmatic `set_active` (GTK4 emits
/// `state-set` for `set_active` too -- the live test showed every remote
/// toggle running twice before the blocking was added).
fn apply_remote_command(
    cmd: &crate::remote_control::RemoteCommand,
    app_state: &Arc<crate::remote_control::AppState>,
    backend: &Rc<RefCell<Option<PipeWireBackend>>>,
    engine_sink: &Rc<RefCell<String>>,
    route_switch: &gtk4::Switch,
    mode_selected: &gtk4::ToggleButton,
    mode_reroute: &gtk4::ToggleButton,
    bypass_switch: &gtk4::Switch,
    presets: &Rc<RefCell<crate::window_presets::PresetPanel>>,
    window: &adw::ApplicationWindow,
    output_preset_identity: &Rc<RefCell<Option<String>>>,
    output_follows_default: &Rc<std::cell::Cell<bool>>,
    monitor_pinned: &Rc<RefCell<Option<String>>>,
    monitor_switch: &gtk4::Switch,
    monitor_summary: &gtk4::Label,
    route_state_handler: &Rc<RefCell<Option<gtk4::glib::SignalHandlerId>>>,
) {
    use crate::remote_control::RemoteCommand;
    match cmd {
        RemoteCommand::SetRouting(on) => {
            // The widget handler performs the backend work (per-device
            // unroute on off, device-chain ensure + route on). `set_active`
            // emits `state-set`, so mirror simply and let it run; the D-Bus
            // handler is no different from a manual toggle.
            let want = *on;
            if route_switch.is_active() != want {
                route_switch.set_active(want);
            }
            app_state.emit_state_changed();
        }
        RemoteCommand::SetOutputMode(mode) => {
            // `set_active` does not emit `clicked`, so the button's own handler
            // is skipped -- apply the mode here and mirror it into the widget
            // so the two stay in agreement whichever side moved first.
            // Persisted like the buttons do: without this a D-Bus (or Shell
            // extension) mode change is lost on restart -- the file kept the
            // previous value while GetState reported the new one.
            let want_reroute = mode.0 == crate::core::OutputRoutingMode::Reroute;
            if let Err(e) = crate::core::set_output_routing_mode(mode.0) {
                log::warn!("D-Bus: persist output mode: {e}");
            }
            if mode_reroute.is_active() != want_reroute {
                mode_reroute.set_active(want_reroute);
            }
            if mode_selected.is_active() != !want_reroute {
                mode_selected.set_active(!want_reroute);
            }
            if let Some(be) = backend.borrow_mut().as_mut() {
                be.set_output_mode(mode.0);
                if be.is_routed() {
                    if mode.0 == crate::core::OutputRoutingMode::Reroute {
                        let sel = engine_sink.borrow().clone();
                        if !sel.is_empty() {
                            let eq = crate::core::eq_virtual_sink_for(&sel);
                            match be.auto_route_to_sink(&eq) {
                                Ok(()) => log::info!(
                                    "D-Bus output mode -> {mode:?} (re-routed into {eq})"
                                ),
                                Err(e) => log::warn!("D-Bus output mode re-route failed: {e}"),
                            }
                        }
                    } else {
                        for dev in be.device_physical_sinks() {
                            if let Err(e) = be.rescope_device(&dev) {
                                log::warn!("D-Bus output mode rescope failed for {dev}: {e}");
                            }
                        }
                    }
                }
            }
            app_state.emit_state_changed();
        }
        RemoteCommand::SetEqEnabled(on) => {
            // A/B compare (bypass) switch. The tick reads the switch and
            // pushes `eq_enabled` to the filter chains; the widget handler
            // syncs AppState (fired here via set_active), exactly like a
            // manual toggle.
            let bypass = !*on;
            if bypass_switch.is_active() != bypass {
                bypass_switch.set_active(bypass);
            }
            *app_state.eq_enabled.lock().unwrap() = *on;
            app_state.emit_state_changed();
        }
        RemoteCommand::SetOutputSink(name) => {
            // Same path as the Output dropdown: retarget the chain, load the
            // new device's preset, follow with the monitor. An empty name
            // means "follow the system default".
            // NOTE: the dropdown widget itself is not moved (it is built
            // after the tick closure that runs this); GetState `output_sink`
            // is the source of truth for remote clients.
            let chosen = if name.is_empty() {
                output_follows_default.set(true);
                backend
                    .borrow_mut()
                    .as_mut()
                    .and_then(|be| be.default_output_sink())
                    .unwrap_or_default()
            } else {
                output_follows_default.set(false);
                name.clone()
            };
            if chosen.is_empty() {
                log::warn!("D-Bus SetOutputSink: resolved to no sink");
                return;
            }
            if Some(chosen.as_str()) == Some(engine_sink.borrow().as_str()) {
                log::debug!("D-Bus SetOutputSink: already on {chosen}");
                return;
            }
            // Same as the Output dropdown: selecting a device never moves
            // audio. It only re-targets the edit context (faders/preamp pick
            // up that device's last curve) and where the monitor listens.
            *engine_sink.borrow_mut() = chosen.clone();
            if let Some(be) = backend.borrow_mut().as_mut() {
                be.set_selected_sink(&chosen);
            }
            apply_output_preset_for_sink(&chosen, output_preset_identity, presets);
            if app_state.output_sink.lock().unwrap().as_deref() != Some(chosen.as_str()) {
                *app_state.output_sink.lock().unwrap() = Some(chosen.clone());
                app_state.emit_state_changed();
            }
            // The header switch always reflects the SELECTED device's own on/off.
            let dev_on = backend
                .borrow()
                .as_ref()
                .is_some_and(|b| b.has_routed_streams_for(&chosen));
            {
                let sw = route_switch.clone();
                if let Some(id) = route_state_handler.borrow().as_ref() {
                    sw.block_signal(id);
                }
                if dev_on != sw.is_active() {
                    sw.set_active(dev_on);
                }
                if let Some(id) = route_state_handler.borrow().as_ref() {
                    sw.unblock_signal(id);
                }
            }
            if *app_state.routed.lock().unwrap() != dev_on {
                *app_state.routed.lock().unwrap() = dev_on;
                app_state.emit_state_changed();
            }
            if let Some(be) = backend.borrow_mut().as_mut() {
                if be.monitor_enabled() && crate::core::output_monitor_sink().is_none() {
                    match be.retarget_monitor_if_different(&chosen) {
                        Ok(_) => monitor_summary.set_text("On \u{00b7} Live (retargeted)"),
                        Err(e) => log::warn!("D-Bus: monitor retarget failed: {e}"),
                    }
                }
            }
        }
        RemoteCommand::SetMonitorEnabled(on) => {
            // `set_active` emits `state-set`, so the widget's own handler
            // performs start/stop: mirror the request into the widget and let
            // it do the backend work.
            let want = *on;
            if monitor_switch.is_active() != want {
                monitor_switch.set_active(want);
            }
            app_state.emit_state_changed();
        }
        RemoteCommand::SetMonitorSink(name) => {
            // Empty = follow the EQ output; otherwise pin to the named sink.
            // The dropdown's 330 ms refresh reads the same `monitor_pinned`
            // cell, so the widget follows within a tick.
            if name.is_empty() {
                *monitor_pinned.borrow_mut() = None;
                if let Err(e) = crate::core::set_output_monitor_sink(None) {
                    log::warn!("D-Bus: persist monitor device: {e}");
                }
                if let Some(be) = backend.borrow_mut().as_mut() {
                    let target = be.resolve_monitor_target(None);
                    if !target.is_empty() && be.monitor_enabled() {
                        if let Err(e) = be.retarget_monitor_if_different(&target) {
                            log::warn!("D-Bus: monitor retarget failed: {e}");
                        }
                    }
                }
            } else {
                *monitor_pinned.borrow_mut() = Some(name.clone());
                if let Err(e) = crate::core::set_output_monitor_sink(Some(name)) {
                    log::warn!("D-Bus: persist monitor device: {e}");
                }
                if let Some(be) = backend.borrow_mut().as_mut() {
                    if be.monitor_enabled() {
                        match be.retarget_monitor_if_different(name) {
                            Ok(_) => {
                                monitor_summary.set_text(&format!("On \u{00b7} Live ({name})"))
                            }
                            Err(e) => log::warn!("D-Bus: monitor retarget failed: {e}"),
                        }
                    }
                }
            }
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
