//! Analyzer panel widget with FFT spectrum visualization.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::cairo::Context;
use gtk4::prelude::*;

use crate::analyzer::analyzer_level_to_display_norm;
use crate::core::clamp_level;

pub struct AnalyzerPanel {
    pub container: gtk4::Box,
    pub drawing_area: gtk4::DrawingArea,
    pub enabled_toggle: gtk4::ToggleButton,
    pub smoothing_scale: gtk4::Scale,
    pub display_gain_scale: gtk4::Scale,
    pub freeze_switch: gtk4::Switch,
    pub levels: Rc<RefCell<Vec<f64>>>,
    /// Set by the Freeze switch: `update()` drops incoming frames so the
    /// displayed spectrum holds still.
    pub frozen: Rc<std::cell::Cell<bool>>,
}

/// One labelled control row: a title on the left, the control on the right,
/// and a dim value readout between them.
///
/// Upstream uses `Adw.ActionRow(title=...)` with the control and a value label
/// as suffixes (`window_utility.py`, "Smoothing" / "Display Gain" / "Freeze").
/// The previous Rust version crammed two bare sliders and a bare switch into a
/// single unlabelled horizontal box, so nothing on screen said what any of them
/// did — only a tooltip, which needs the pointer.
fn labelled_row<C: IsA<gtk4::Widget>>(
    title_text: &str,
    tooltip: Option<&str>,
    control: &C,
    value_label: Option<&gtk4::Label>,
) -> gtk4::ListBoxRow {
    let row = gtk4::ListBoxRow::new();
    row.set_activatable(false);
    row.set_selectable(false);
    if let Some(t) = tooltip {
        row.set_tooltip_text(Some(t));
    }

    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 12);
    box_.set_margin_top(6);
    box_.set_margin_bottom(6);
    box_.set_margin_start(12);
    box_.set_margin_end(12);

    let title = gtk4::Label::new(Some(title_text));
    title.set_hexpand(true);
    title.set_xalign(0.0);
    title.set_width_chars(12);
    box_.append(&title);

    if let Some(label) = value_label {
        label.set_css_classes(&["dim-label"]);
        label.set_width_chars(6);
        label.set_xalign(1.0);
        box_.append(label);
    }

    let control = control.upcast_ref::<gtk4::Widget>();
    control.set_valign(gtk4::Align::Center);
    box_.append(control);
    row.set_child(Some(&box_));
    row
}

impl AnalyzerPanel {
    pub fn new() -> Self {
        let enabled_toggle = gtk4::ToggleButton::new();
        enabled_toggle.set_active(true);
        enabled_toggle.set_tooltip_text(Some("Enable analyzer"));

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Analyzer"));
        title.set_css_classes(&["heading"]);
        header.append(&title);
        header.set_hexpand(true);
        header.append(&enabled_toggle);

        let drawing_area = gtk4::DrawingArea::new();
        drawing_area.set_size_request(-1, 120);
        drawing_area.set_vexpand(true);
        drawing_area.set_hexpand(true);

        // Default 30% maps to ANALYZER_RESPONSE_DEFAULT (2.0) via
        // PipeWireBackend::set_analyzer_smoothing, so the panel default
        // reproduces the analyzer's own default.
        let smoothing_adj = gtk4::Adjustment::new(30.0, 15.0, 95.0, 1.0, 5.0, 0.0);
        let smoothing_scale = gtk4::Scale::new(gtk4::Orientation::Horizontal, Some(&smoothing_adj));
        smoothing_scale.set_size_request(116, -1);
        smoothing_scale.set_tooltip_text(Some(
            "How quickly the spectrum reacts. Lower is smoother and steadier.",
        ));

        let display_gain_adj = gtk4::Adjustment::new(0.0, -12.0, 32.0, 1.0, 4.0, 0.0);
        let display_gain_scale =
            gtk4::Scale::new(gtk4::Orientation::Horizontal, Some(&display_gain_adj));
        display_gain_scale.set_size_request(116, -1);
        display_gain_scale.set_tooltip_text(Some(
            "Display-only boost for the spectrum. Does not change the audio.",
        ));

        let freeze_switch = gtk4::Switch::new();
        freeze_switch.set_valign(gtk4::Align::Center);
        freeze_switch.set_tooltip_text(Some("Hold the current spectrum still"));

        // Live value readouts, so the sliders are not just unlabelled but also
        // unreadable. Upstream shows the same two figures as dim labels.
        let smoothing_value = gtk4::Label::new(Some("30%"));
        let display_gain_value = gtk4::Label::new(Some("+0 dB"));
        {
            let lbl = smoothing_value.clone();
            smoothing_scale.connect_value_changed(move |s| {
                lbl.set_text(&format!("{:.0}%", s.value()));
            });
        }
        {
            let lbl = display_gain_value.clone();
            display_gain_scale.connect_value_changed(move |s| {
                lbl.set_text(&format!("{:+.0} dB", s.value()));
            });
        }

        // NOTE: this panel used to carry its own `lufs_value_label` and
        // `summary_label`, but nothing ever wrote to them -- they sat at a
        // hardcoded "-23 LUFS" forever while the real live readout lives in
        // the monitor strip just below (window.rs updates
        // utility.monitor_loudness_value / monitor_summary). Removed rather
        // than duplicated: two LUFS labels where one is a lie is worse than
        // one that is correct.
        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        container.set_css_classes(&["utility-section"]);
        container.set_margin_bottom(8);
        container.append(&header);
        container.append(&drawing_area);

        // Labelled rows, matching upstream's ActionRow layout. A `ListBox` with
        // `boxed-list` styling is used rather than AdwActionRow because the
        // panel lives in a plain sidebar stack, not an AdwPreferencesGroup.
        let settings = gtk4::ListBox::new();
        settings.set_selection_mode(gtk4::SelectionMode::None);
        settings.add_css_class("boxed-list");
        settings.append(&labelled_row(
            "Smoothing",
            Some("How quickly the spectrum reacts"),
            &smoothing_scale,
            Some(&smoothing_value),
        ));
        settings.append(&labelled_row(
            "Display Gain",
            Some("Display-only boost for the spectrum; does not change the audio"),
            &display_gain_scale,
            Some(&display_gain_value),
        ));
        settings.append(&labelled_row(
            "Freeze",
            Some("Hold the current spectrum still"),
            &freeze_switch,
            None,
        ));
        container.append(&settings);

        let levels = Rc::new(RefCell::new(vec![0.0; 64]));
        let draw_levels = levels.clone();
        drawing_area.set_draw_func(move |_area, ctx, width, height| {
            let levels = draw_levels.borrow();
            Self::draw_spectrum(ctx, width, height, &levels);
        });

        Self {
            container,
            drawing_area,
            enabled_toggle,
            smoothing_scale,
            display_gain_scale,
            freeze_switch,
            levels,
            frozen: Rc::new(std::cell::Cell::new(false)),
        }
    }

    /// Push a new spectrum frame. Ignored while frozen so the user can hold
    /// a frame for inspection.
    pub fn update(&self, levels: &[f64]) {
        if self.frozen.get() {
            return;
        }
        *self.levels.borrow_mut() = levels.to_vec();
        self.drawing_area.queue_draw();
    }

    pub fn set_frozen(&self, frozen: bool) {
        self.frozen.set(frozen);
        if !frozen {
            self.drawing_area.queue_draw();
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.container
    }

    pub fn set_height(&self, height: i32) {
        self.drawing_area.set_size_request(-1, height);
        self.drawing_area.queue_draw();
    }

    fn draw_spectrum(ctx: &Context, width: i32, height: i32, levels: &[f64]) {
        let w = width as f64;
        let h = height as f64;

        ctx.set_source_rgb(0.08, 0.08, 0.08);
        ctx.paint().unwrap();

        ctx.set_source_rgba(0.2, 0.2, 0.2, 0.5);
        ctx.set_line_width(0.5);
        for i in 0..5 {
            let y = h * (i + 1) as f64 / 6.0;
            ctx.move_to(0.0, y);
            ctx.line_to(w, y);
            ctx.stroke().unwrap();
        }

        let bar_count = levels.len().min(64);
        let bar_width = w / bar_count as f64;
        for (i, &level) in levels.iter().take(bar_count).enumerate() {
            let norm = analyzer_level_to_display_norm(clamp_level(level), 0.0);
            let bar_height = norm * h * 0.9;
            let x = i as f64 * bar_width;
            let y = h - bar_height;

            let r = norm;
            let g = 1.0 - norm;
            ctx.set_source_rgb(r, g, 0.0);
            ctx.rectangle(x, y, bar_width - 1.0, bar_height);
            ctx.fill().unwrap();
        }
    }
}

impl Default for AnalyzerPanel {
    fn default() -> Self {
        Self::new()
    }
}
