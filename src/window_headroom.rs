//! Headroom meter and preamp control widget.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::cairo::Context;
use gtk4::prelude::*;

/// Meter axis bounds, matching upstream `window_headroom`.
pub const HEADROOM_METER_MIN_DB: f64 = -12.0;
pub const HEADROOM_METER_MAX_DB: f64 = 24.0;
pub const HEADROOM_SAFE_LIMIT_DB: f64 = -3.0;
pub const HEADROOM_RISK_LIMIT_DB: f64 = 0.0;

/// Target output peak (dBFS) that Auto-Safe keeps the curve under. Matches the
/// one-shot "Set Safe" margin (`peak + 1.0` ⇒ peak lands at −1 dBFS).
pub const AUTO_SAFE_TARGET_DBFS: f64 = -1.0;

/// Fixed width of the Set Safe button so label changes never reflow the row.
/// Both clipping buttons are the same width so the pair reads as one shape.
/// Was 96px when Fix still said "Clip-Safe"/"Set Safe".
const CLIP_BUTTON_WIDTH_PX: i32 = 56;
/// CSS class marking Fix as actionable (red).
pub const CLIP_FIX_NEEDED: &str = "clip-fix-needed";
/// CSS class marking Auto as on (green).
pub const CLIP_AUTO_ON: &str = "clip-auto-on";
/// Char width of the numeric peak readout, sized for the widest
/// string it can render so text changes never resize the row.
const PEAK_LABEL_WIDTH_CHARS: i32 = 11;
/// Width of the smooth spread slide bar inside the popover.
const SMOOTH_WIDTH_SCALE_W_PX: i32 = 150;

/// Compute the preamp that keeps the EQ curve's peak at or below
/// `target_dbfs`, never boosting above 0 dB. `raw_peak_db` is the curve peak
/// with preamp = 0. This is the continuous form of the one-shot "Set Safe":
/// sliding the EQ up auto-lowers the preamp; sliding it down lets the preamp
/// rise back toward 0 (but never above).
pub fn auto_safe_preamp_db(raw_peak_db: f64, target_dbfs: f64) -> f64 {
    if !raw_peak_db.is_finite() {
        return 0.0;
    }
    (target_dbfs - raw_peak_db).clamp(crate::core::EQ_PREAMP_MIN_DB, 0.0)
}

/// Position of `value_db` along the meter, in `0.0..=1.0`.
pub fn headroom_meter_norm(value_db: f64) -> f64 {
    let span = HEADROOM_METER_MAX_DB - HEADROOM_METER_MIN_DB;
    ((value_db - HEADROOM_METER_MIN_DB) / span).clamp(0.0, 1.0)
}

/// Peak text formatting, mirroring upstream `format_headroom_peak_db`.
pub fn format_headroom_peak_db(peak_db: f64) -> String {
    if peak_db > HEADROOM_METER_MAX_DB {
        format!(">{:+.0} dB", HEADROOM_METER_MAX_DB)
    } else if peak_db < HEADROOM_METER_MIN_DB {
        format!("<{:.0} dB", HEADROOM_METER_MIN_DB.abs())
    } else if peak_db < HEADROOM_RISK_LIMIT_DB {
        format!("{:.1} dB", peak_db.abs())
    } else {
        format!("{:+.1} dB", peak_db)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadroomState {
    Safe,
    Tight,
    Risk,
    Bypass,
}

impl HeadroomState {
    pub fn css_class(self) -> &'static str {
        match self {
            HeadroomState::Safe => "headroom-panel-safe",
            HeadroomState::Tight => "headroom-panel-tight",
            HeadroomState::Risk => "headroom-panel-risk",
            HeadroomState::Bypass => "headroom-panel-bypass",
        }
    }
}

pub struct HeadroomPanel {
    pub container: gtk4::Box,
    /// Compact preamp trim. A SpinButton cost ~110px because of its
    /// +/- buttons; a value-less Scale does the same job in ~96px, with the
    /// number kept available in the tooltip.
    /// Preamp uses the +/- spin: precise entry matters more here
    /// than it does for the smooth spread.
    pub preamp_spin: gtk4::SpinButton,
    pub peak_label: gtk4::Label,
    pub state_label: gtk4::Label,
    /// Small status LED replacing the old bar meter: colour carries
    /// the state, the numeric peak label carries the value.
    pub led_area: gtk4::DrawingArea,
    /// Colour of the status LED. Live-driven while the monitor is running,
    /// curve-driven otherwise. Kept separate from `state` because `state`
    /// also gates the Set Safe button, which must stay a property of the
    /// EQ settings rather than of whatever material happens to be playing.
    led_state: Rc<std::cell::Cell<HeadroomState>>,
    pub detail_label: gtk4::Label,
    pub set_safe_button: gtk4::Button,
    /// Auto-Safe as a toggle BUTTON, not a switch: it sits next to the manual
    /// Fix button in the same cell and the two have to read as one pair. Same
    /// fixed label either way -- only the colour says whether Auto is on.
    pub auto_safe_button: gtk4::ToggleButton,
    pub auto_safe: Rc<std::cell::Cell<bool>>,
    /// Smooth (coupled) band editing: dragging one band drags its
    /// neighbours by a decaying fraction so the curve stays smooth across
    /// the adjacent bands on both sides.
    pub smooth_switch: gtk4::Switch,
    pub smooth: Rc<std::cell::Cell<bool>>,
    /// Smooth spread control (Gaussian sigma, in bands) and the live value
    /// read by the Smooth override. Min = 0.45 moves ONLY the dragged band.
    /// Width control lives inside the Smooth popover as a +/- spin.
    /// Width inside the Smooth popover uses a slide bar.
    pub smooth_width_scale: gtk4::Scale,
    /// Single menu button owning the Smooth switch and its width spin, so
    /// the output row spends ONE cell on Smooth instead of two.
    pub smooth_menu: gtk4::MenuButton,
    pub smooth_spread_bands: Rc<std::cell::Cell<f64>>,
    pub state: Rc<RefCell<HeadroomState>>,
    pub peak_value: Rc<RefCell<f64>>,
}

impl HeadroomPanel {
    pub fn new() -> Self {
        let preamp_adj = gtk4::Adjustment::new(
            0.0,
            crate::core::EQ_PREAMP_MIN_DB,
            crate::core::EQ_PREAMP_MAX_DB,
            0.5,
            1.0,
            0.0,
        );
        let preamp_spin = gtk4::SpinButton::new(Some(&preamp_adj), 0.5, 1);
        preamp_spin.set_digits(1);
        preamp_spin.set_width_chars(5);
        preamp_spin.set_max_width_chars(5);
        preamp_spin.set_valign(gtk4::Align::Center);
        preamp_spin.set_tooltip_text(Some("Preamp gain (dB)"));

        // No "Peak: " prefix -- the LED beside it already says what this is,
        // and the prefix was costing ~40px of row width.
        let peak_label = gtk4::Label::new(Some("-- dB"));
        peak_label.set_css_classes(&["numeric"]);
        // The text changes every tick and by different amounts
        // ("-- dB" / "-12.3 dBFS" / "+6.0 dB"). With no width floor the
        // label resized itself each time, which changed the status cell
        // width and -- because the output row is centred -- slid every
        // other control sideways. A fixed char width pins it.
        peak_label.set_width_chars(PEAK_LABEL_WIDTH_CHARS);
        peak_label.set_max_width_chars(PEAK_LABEL_WIDTH_CHARS);
        peak_label.set_xalign(0.0);
        peak_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        peak_label.set_tooltip_text(Some("Estimated output peak (dBFS)"));

        let state_label = gtk4::Label::new(Some("Safe"));
        state_label.set_css_classes(&["headroom-state-label", "headroom-panel-safe"]);

        // A compact LED instead of the old bar meter. The bar needed ~90px
        // and pushed the whole output row onto a second line once every
        // switch was active; the LED costs 18px and the numeric peak label
        // already carries the actual value.
        let led_area = gtk4::DrawingArea::new();
        led_area.set_size_request(18, 18);
        led_area.set_hexpand(false);
        led_area.set_valign(gtk4::Align::Center);

        let detail_label = gtk4::Label::new(Some(""));
        detail_label.set_css_classes(&["numeric"]);

        // Always present. Hiding it made every other item in the output row
        // slide sideways whenever the risk state changed, so it now stays put
        // and is merely insensitive when there is nothing to do. The width
        // is fixed to the widest label so swapping the text cannot move the
        // neighbours either.
        // Labelled "Fix" only. It used to alternate between "Clip-Safe" at rest
        // and "Set Safe" when the curve was over the target, which is a
        // persistent state readout rather than an action, and it cost ~96px of
        // row width. The LED beside it already says whether the peak is safe.
        let set_safe_button = gtk4::Button::with_label("Fix");
        // GTK4 widgets do NOT inherit visibility from their parent, and a
        // freshly built Button starts with visible == false. Nothing else in
        // this codebase ever showed it, which is why the button was absent
        // even though the label, CSS class and layout cell were all correct.
        set_safe_button.set_visible(true);
        set_safe_button.set_sensitive(false);
        set_safe_button.set_size_request(CLIP_BUTTON_WIDTH_PX, -1);
        set_safe_button.set_halign(gtk4::Align::Start);

        let auto_safe_button = gtk4::ToggleButton::with_label("Auto");
        auto_safe_button.set_valign(gtk4::Align::Center);
        auto_safe_button.set_size_request(CLIP_BUTTON_WIDTH_PX, -1);
        auto_safe_button.set_tooltip_text(Some(
            "Automatically keep the output peak under -1 dBFS as you adjust the EQ",
        ));
        let auto_safe = Rc::new(std::cell::Cell::new(false));
        {
            let auto_safe = auto_safe.clone();
            let preamp_ctl = preamp_spin.clone();
            auto_safe_button.connect_toggled(move |btn| {
                let on = btn.is_active();
                auto_safe.set(on);
                // The auto algorithm owns the preamp while enabled, so the
                // manual control is disabled to avoid fighting it.
                preamp_ctl.set_sensitive(!on);
                if on {
                    btn.add_css_class(CLIP_AUTO_ON);
                } else {
                    btn.remove_css_class(CLIP_AUTO_ON);
                }
            });
        }

        let smooth_switch = gtk4::Switch::new();
        smooth_switch.set_valign(gtk4::Align::Center);
        smooth_switch.set_tooltip_text(Some(
            "Couple adjacent bands: dragging one band smoothly drags its neighbours on both sides",
        ));
        let smooth = Rc::new(std::cell::Cell::new(false));
        {
            let smooth = smooth.clone();
            smooth_switch.connect_state_set(move |_sw, on| {
                smooth.set(on);
                glib::Propagation::Proceed
            });
        }

        // Bounds derive from the real band layout so the control can never
        // be dragged into the underlap zone (width < ~0.9x band spacing),
        // which breaks the curve into separate bumps with troughs between.
        let (w_min, w_max, w_default) = crate::core::smooth_spread_bounds_bands();
        let smooth_width_adj = gtk4::Adjustment::new(w_default, w_min, w_max, 0.05, 0.1, 0.0);
        let smooth_width_scale =
            gtk4::Scale::new(gtk4::Orientation::Horizontal, Some(&smooth_width_adj));
        smooth_width_scale.set_digits(2);
        smooth_width_scale.set_draw_value(true);
        smooth_width_scale.set_value_pos(gtk4::PositionType::Right);
        smooth_width_scale.set_size_request(SMOOTH_WIDTH_SCALE_W_PX, -1);
        smooth_width_scale.set_valign(gtk4::Align::Center);
        smooth_width_scale.set_tooltip_text(Some(
            "How many bands move when you drag one.\n\
             Minimum = only the dragged band (1 band).\n\
             Higher  = more of the spectrum moves together.\n\
             The bell width follows automatically so the bump stays smooth.",
        ));
        let smooth_spread_bands = Rc::new(std::cell::Cell::new(w_default));
        {
            let smooth_spread_bands = smooth_spread_bands.clone();
            smooth_width_scale.connect_value_changed(move |s| {
                smooth_spread_bands.set(s.value());
            });
        }
        // The width only matters while Smooth is active.
        smooth_width_scale.set_sensitive(false);

        // --- Smooth popover -------------------------------------------
        // The switch and its width control live inside one menu button.
        // This removes an entire cell from the output row, which is what
        // was overflowing at the minimum window width.
        let smooth_menu = gtk4::MenuButton::builder().label("Smooth").build();
        {
            let pop = gtk4::Box::new(gtk4::Orientation::Vertical, 10);
            pop.set_margin_start(14);
            pop.set_margin_end(14);
            pop.set_margin_top(12);
            pop.set_margin_bottom(12);

            let sw_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            let sw_label = gtk4::Label::new(Some("Smooth band editing"));
            sw_label.set_halign(gtk4::Align::Start);
            sw_label.set_hexpand(true);
            sw_row.append(&sw_label);
            sw_row.append(&smooth_switch);
            pop.append(&sw_row);

            pop.append(&gtk4::Separator::new(gtk4::Orientation::Horizontal));

            let w_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
            let w_label = gtk4::Label::new(Some("Width (bands)"));
            w_label.set_halign(gtk4::Align::Start);
            w_label.set_hexpand(true);
            w_row.append(&w_label);
            w_row.append(&smooth_width_scale);
            pop.append(&w_row);

            let popover = gtk4::Popover::new();
            popover.set_child(Some(&pop));
            smooth_menu.set_popover(Some(&popover));
        }

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        container.set_css_classes(&["headroom-panel-safe"]);
        container.set_margin_bottom(8);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Headroom"));
        title.set_css_classes(&["heading"]);
        header.append(&title);
        header.set_hexpand(true);
        header.append(&state_label);
        container.append(&header);

        // NOTE: the Auto-Safe switch, the preamp control, the peak label,
        // the peak meter and the Set Safe button are NOT appended here. They
        // live in the MAIN window's output control row (see
        // `window_layout::build_output_control_row`), which reparents them.
        // GTK4 will NOT move a widget out of an existing parent, so they must
        // be left unparented here or `gtk_box_append` asserts.
        //
        // `detail_label` is likewise not shown in the sidebar; `update_peak`
        // surfaces its text as the Set Safe button's tooltip instead.

        let state = Rc::new(RefCell::new(HeadroomState::Safe));
        let peak_value = Rc::new(RefCell::new(f64::NEG_INFINITY));
        let led_state = Rc::new(std::cell::Cell::new(HeadroomState::Safe));
        let led_draw_state = led_state.clone();
        led_area.set_draw_func(move |_area, ctx, width, height| {
            Self::draw_led(ctx, width, height, led_draw_state.get());
        });

        Self {
            container,
            preamp_spin,
            peak_label,
            state_label,
            led_area,
            led_state,
            detail_label,
            set_safe_button,
            auto_safe_button,
            auto_safe,
            smooth_switch,
            smooth,
            smooth_width_scale,
            smooth_menu,
            smooth_spread_bands,
            state,
            peak_value,
        }
    }

    pub fn set_state(&self, state: HeadroomState) {
        *self.state.borrow_mut() = state;
        // Curve-driven default; `apply_live_peak` overrides this whenever
        // the monitor is delivering samples.
        self.led_state.set(state);
        self.state_label.set_label(match state {
            HeadroomState::Safe => "Safe",
            HeadroomState::Tight => "Tight",
            HeadroomState::Risk => "Risk",
            HeadroomState::Bypass => "Bypass",
        });
        self.state_label
            .set_css_classes(&["headroom-state-label", state.css_class()]);
        self.container.set_css_classes(&[state.css_class()]);
        self.set_safe_button
            .set_visible(state == HeadroomState::Risk);
        self.led_area.queue_draw();
    }

    pub fn update_peak(&mut self, peak_db: f64) {
        *self.peak_value.borrow_mut() = peak_db;

        // Upstream classifies the *estimated curve peak* (not a live signal
        // level): clipping risk above +0.5 dB, tight between -0.5 and +0.5 dB,
        // safe below that.
        let state = if peak_db > 0.5 {
            HeadroomState::Risk
        } else if peak_db > -0.5 {
            HeadroomState::Tight
        } else {
            HeadroomState::Safe
        };
        self.set_state(state);

        let peak_text = format_headroom_peak_db(peak_db);
        self.peak_label.set_label(&peak_text);

        let detail = if peak_db > 0.5 {
            if self.auto_safe_enabled() {
                // Auto-Safe's only lever is the preamp; if it is pinned at the
                // floor and the curve still exceeds 0 dBFS there is nothing
                // left to lower — the user must reduce band gains instead.
                "Auto-Safe maxed out \u{2014} reduce band gains.".to_string()
            } else {
                format!("Lower preamp by {:.1} dB.", peak_db + 1.0)
            }
        } else if peak_db > -0.5 {
            "Small boosts may clip.".to_string()
        } else {
            "Curve stays below 0 dBFS.".to_string()
        };
        self.detail_label.set_label(&detail);
        // The detail text is not shown in the sidebar any more, so surface it
        // on the control it refers to.
        self.set_safe_button.set_tooltip_text(Some(&detail));

        // Upstream only surfaces "Set Safe" when the curve is actually at
        // risk. With Auto-Safe on there is no safe click to make: the preamp
        // is already where Auto-Safe (or its floor) puts it, so the button
        // would be a no-op that re-opens the same Risk state.
        let needs_fix = peak_db > 0.5 && !self.auto_safe_enabled();
        // A transient action, not a state readout: insensitive (and green)
        // unless there is something to fix, at which point the blink timer adds
        // `headroom-warning`. Kept in the row rather than hidden when there is
        // nothing to do, because the row is centred: a control appearing and
        // disappearing slid every other control sideways.
        self.set_safe_button.set_visible(true);
        self.set_safe_button.set_sensitive(needs_fix);
        // Red when a fix is required, plain grey otherwise. No resting green:
        // the green in this pair means "Auto is on", and reusing it for "nothing
        // to do here" made two different things look like the same state.
        if needs_fix {
            self.set_safe_button.add_css_class(CLIP_FIX_NEEDED);
        } else {
            self.set_safe_button.remove_css_class(CLIP_FIX_NEEDED);
        }
    }

    /// Recompute the estimated curve peak from the live band state and refresh
    /// the panel, mirroring upstream `update_status_summary`.
    pub fn update_curve_peak(&mut self, bands: &[crate::core::EqBand], preamp_db: f64) {
        let peak =
            crate::core::estimate_response_peak_db(bands, preamp_db, crate::core::SAMPLE_RATE);
        self.update_peak(peak);
    }

    /// Show the **live** output peak when the monitor is running.
    ///
    /// Why this exists: `estimate_response_peak_db` is a property of the EQ
    /// *settings* — "how much boost can this curve apply" — not a
    /// measurement of the audio. Two consequences of labelling it as a
    /// peak:
    ///
    /// 1. It only changes when a band or the preamp changes, so with the
    ///    monitor on the number sat frozen until the user moved a slider.
    /// 2. Compared against any real level meter it looks far too loud, e.g.
    ///    "+6.0 dB" of available boost vs a program actually peaking at
    ///    -14 dBFS. Different quantities, same label.
    ///
    /// So: live audio -> real dBFS. No monitor -> say plainly that the
    /// number is the curve's maximum boost, not a level.
    pub fn apply_live_peak(&mut self, live_dbfs: Option<f64>) {
        match live_dbfs {
            Some(db) if db.is_finite() => {
                self.peak_label.set_label(&format!("{db:.1} dBFS"));
                self.peak_label
                    .set_tooltip_text(Some("Live output peak (dBFS)"));
                // The LED goes live with the monitor: clipping is a property
                // of the actual signal, so showing the curve's worst case
                // while real audio is available would be the less honest
                // choice. Thresholds mirror the Auto-Safe target.
                self.led_state.set(if db > AUTO_SAFE_TARGET_DBFS {
                    HeadroomState::Risk
                } else if db > AUTO_SAFE_TARGET_DBFS - 3.0 {
                    HeadroomState::Tight
                } else {
                    HeadroomState::Safe
                });
            }
            _ => {
                let est = *self.peak_value.borrow();
                self.peak_label.set_label(&format!("{est:+.1} dB"));
                self.peak_label.set_tooltip_text(Some(
                    "Maximum boost of the EQ curve — not a live level.\n\
                     Turn on Monitor to see the real output peak.",
                ));
            }
        }
    }

    pub fn preamp_value(&self) -> f64 {
        self.preamp_spin.value()
    }

    /// Whether the Auto-Safe continuous preamp clamp is enabled.
    pub fn auto_safe_enabled(&self) -> bool {
        self.auto_safe.get()
    }

    /// Whether smooth (neighbour-coupled) band editing is enabled.
    pub fn smooth_enabled(&self) -> bool {
        self.smooth.get()
    }

    pub fn set_preamp_value(&self, preamp_db: f64) {
        self.preamp_spin.set_value(preamp_db);
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.container
    }

    /// Paint the status LED: a lamp in a dark socket, coloured by headroom
    /// state. No bar, no scale — the numeric label next to it carries the
    /// value, so this only has to answer "am I safe?".
    fn draw_led(ctx: &Context, width: i32, height: i32, state: HeadroomState) {
        let w = width as f64;
        let h = height as f64;
        let cx = w / 2.0;
        let cy = h / 2.0;
        let r = (w.min(h) / 2.0 - 2.5).max(3.0);

        let color = match state {
            HeadroomState::Safe => (0.24, 0.80, 0.40),
            HeadroomState::Tight => (0.96, 0.74, 0.22),
            HeadroomState::Risk => (0.93, 0.27, 0.23),
            HeadroomState::Bypass => (0.46, 0.50, 0.55),
        };

        // Dark socket ring so it reads as a lamp rather than a bare dot.
        ctx.set_source_rgba(0.0, 0.0, 0.0, 0.55);
        ctx.arc(cx, cy, r + 1.6, 0.0, std::f64::consts::TAU);
        ctx.fill().unwrap();

        // Lamp body.
        ctx.set_source_rgb(color.0, color.1, color.2);
        ctx.arc(cx, cy, r, 0.0, std::f64::consts::TAU);
        ctx.fill().unwrap();

        // Specular highlight.
        ctx.set_source_rgba(1.0, 1.0, 1.0, 0.38);
        ctx.arc(
            cx - r * 0.28,
            cy - r * 0.30,
            r * 0.34,
            0.0,
            std::f64::consts::TAU,
        );
        ctx.fill().unwrap();
    }
}

impl Default for HeadroomPanel {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_auto_safe_preamp_db() {
        // Curve peaks +6 dB -> preamp -7 dB keeps it at -1 dBFS.
        assert!((auto_safe_preamp_db(6.0, -1.0) - (-7.0)).abs() < 1e-9);
        // Curve already low (-3 dB) -> no boost, preamp clamped to 0.
        assert!((auto_safe_preamp_db(-3.0, -1.0) - 0.0).abs() < 1e-9);
        // Exactly at target -> 0 preamp would put peak at target already,
        // so a tiny cut keeps the -1 dBFS margin.
        assert!((auto_safe_preamp_db(0.0, -1.0) - (-1.0)).abs() < 1e-9);
        // Huge peak clamps at the preamp floor.
        assert!((auto_safe_preamp_db(100.0, -1.0) - crate::core::EQ_PREAMP_MIN_DB).abs() < 1e-9);
        // Non-finite peak -> unity (0 dB) preamp.
        assert_eq!(auto_safe_preamp_db(f64::NEG_INFINITY, -1.0), 0.0);
    }

    #[test]
    fn test_headroom_meter_norm_endpoints_and_clamp() {
        assert!((headroom_meter_norm(HEADROOM_METER_MIN_DB) - 0.0).abs() < 1e-9);
        assert!((headroom_meter_norm(HEADROOM_METER_MAX_DB) - 1.0).abs() < 1e-9);
        assert!((headroom_meter_norm(HEADROOM_METER_MIN_DB - 50.0) - 0.0).abs() < 1e-9);
        assert!((headroom_meter_norm(HEADROOM_METER_MAX_DB + 50.0) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn test_format_headroom_peak_db_matches_upstream() {
        assert_eq!(
            format_headroom_peak_db(HEADROOM_METER_MAX_DB + 1.0),
            ">+24 dB"
        );
        assert_eq!(
            format_headroom_peak_db(HEADROOM_METER_MIN_DB - 1.0),
            "<12 dB"
        );
        assert_eq!(format_headroom_peak_db(-4.5), "4.5 dB");
        assert_eq!(format_headroom_peak_db(0.0), "+0.0 dB");
        assert_eq!(format_headroom_peak_db(2.5), "+2.5 dB");
    }
}
