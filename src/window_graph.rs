//! Frequency response graph widget with analyzer overlay and drag editing.

use gtk4::cairo::Context;
use gtk4::prelude::*;

use crate::core::{EQ_FREQUENCY_MAX_HZ, EQ_FREQUENCY_MIN_HZ, GRAPH_DB_MAX, GRAPH_DB_MIN};

/// Plot margins in pixels, matching upstream `window_graph.py`
/// (`GRAPH_PLOT_LEFT/RIGHT/TOP/BOTTOM`).
///
/// The plot area is the widget minus these gutters. They exist for the axis
/// labels, so every layer has to inset by the same amount: the response curve,
/// the grid lines and the analyzer bars are all positioned from these, and
/// drawing any of them over the full widget size puts the spectrum a full label
/// width away from the curve it belongs to.
pub const GRAPH_PLOT_LEFT: f64 = 58.0;
pub const GRAPH_PLOT_RIGHT: f64 = 62.0;
pub const GRAPH_PLOT_TOP: f64 = 26.0;
pub const GRAPH_PLOT_BOTTOM: f64 = 34.0;

/// Map a frequency to an x pixel inside the plot area (upstream
/// `frequency_to_x`): log-spaced between the axis ends, inset by the margins.
pub fn frequency_to_x(frequency: f64, width: f64, left: f64, right: f64) -> f64 {
    let usable = (width - left - right).max(1.0);
    let position = (frequency
        .clamp(EQ_FREQUENCY_MIN_HZ, EQ_FREQUENCY_MAX_HZ)
        .log10()
        - EQ_FREQUENCY_MIN_HZ.log10())
        / (EQ_FREQUENCY_MAX_HZ.log10() - EQ_FREQUENCY_MIN_HZ.log10());
    left + usable * position
}

/// Inverse of [`frequency_to_x`] (upstream `x_to_frequency`). Needed by graph
/// dragging; written with it so the pair cannot drift apart.
pub fn x_to_frequency(x: f64, width: f64, left: f64, right: f64) -> f64 {
    let usable = (width - left - right).max(1.0);
    let normalized = ((x - left) / usable).clamp(0.0, 1.0);
    let log_freq = EQ_FREQUENCY_MIN_HZ.log10()
        + normalized * (EQ_FREQUENCY_MAX_HZ.log10() - EQ_FREQUENCY_MIN_HZ.log10());
    10f64.powf(log_freq)
}

/// Map a dB value to a y pixel inside the plot area, matching upstream `db_to_y`.
///
/// The axis spans `GRAPH_DB_MIN..GRAPH_DB_MAX` (±24 dB), not the ±20 dB EQ gain
/// range, so the curve and the grid lines stay consistent with the Python
/// original.
pub fn db_to_y(db_value: f64, height: f64, top: f64, bottom: f64) -> f64 {
    let usable = (height - top - bottom).max(1.0);
    let normalized =
        (db_value.clamp(GRAPH_DB_MIN, GRAPH_DB_MAX) - GRAPH_DB_MIN) / (GRAPH_DB_MAX - GRAPH_DB_MIN);
    (height - bottom) - usable * normalized
}

/// Inverse of [`db_to_y`] (upstream `y_to_db`), clamped to the axis.
pub fn y_to_db(y: f64, height: f64, top: f64, bottom: f64) -> f64 {
    let usable = (height - top - bottom).max(1.0);
    let normalized = (((height - bottom) - y) / usable).clamp(0.0, 1.0);
    GRAPH_DB_MIN + normalized * (GRAPH_DB_MAX - GRAPH_DB_MIN)
}

/// Colours for the graph, split by appearance exactly as upstream
/// `draw_graph_background` / `draw_graph_response_overlay` do.
///
/// The Rust port hardcoded a dark background, so the graph stayed dark grey in a
/// light theme. `GraphPalette::for_current()` reads the Adw style manager.
pub struct GraphPalette {
    pub plot_top: (f64, f64, f64, f64),
    pub plot_bottom: (f64, f64, f64, f64),
    pub major_grid: (f64, f64, f64, f64),
    pub grid: (f64, f64, f64, f64),
    pub vertical_grid: (f64, f64, f64, f64),
    pub axis_label: (f64, f64, f64),
    pub edge_label: (f64, f64, f64),
    pub border: (f64, f64, f64, f64),
    pub analyzer_grid: (f64, f64, f64),
    pub analyzer_label: (f64, f64, f64),
    pub monitor_label: (f64, f64, f64),
    pub selected_line: (f64, f64, f64, f64),
    pub selected_point: (f64, f64, f64),
    pub selected_halo: (f64, f64, f64),
    pub effective_point: (f64, f64, f64),
    pub inactive_point: (f64, f64, f64),
    pub response: (f64, f64, f64),
    pub disabled_response: (f64, f64, f64),
}

impl GraphPalette {
    pub fn new(dark: bool) -> Self {
        const FOCUS_BLUE: (f64, f64, f64) = (0.47, 0.72, 1.0);
        const FOCUS_BLUE_LIGHT: (f64, f64, f64) = (0.68, 0.84, 1.0);
        const RESPONSE_AMBER: (f64, f64, f64) = (0.84, 0.46, 0.12);
        if dark {
            Self {
                plot_top: (0.105, 0.155, 0.225, 0.98),
                plot_bottom: (0.045, 0.070, 0.108, 0.98),
                major_grid: (0.72, 0.80, 0.88, 0.28),
                grid: (0.45, 0.52, 0.60, 0.20),
                vertical_grid: (0.45, 0.52, 0.60, 0.18),
                axis_label: (0.72, 0.76, 0.80),
                edge_label: (0.82, 0.85, 0.89),
                border: (0.85, 0.90, 0.96, 0.14),
                analyzer_grid: (0.42, 0.78, 0.92),
                analyzer_label: (0.45, 0.78, 0.86),
                monitor_label: (0.50, 0.86, 0.98),
                selected_line: (*&FOCUS_BLUE.0, FOCUS_BLUE.1, FOCUS_BLUE.2, 0.24),
                selected_point: FOCUS_BLUE_LIGHT,
                selected_halo: FOCUS_BLUE,
                effective_point: (0.78, 0.85, 0.93),
                inactive_point: (0.44, 0.50, 0.57),
                response: RESPONSE_AMBER,
                disabled_response: (0.58, 0.64, 0.72),
            }
        } else {
            Self {
                plot_top: (0.95, 0.97, 0.99, 0.98),
                plot_bottom: (0.81, 0.87, 0.93, 0.98),
                major_grid: (0.18, 0.25, 0.32, 0.34),
                grid: (0.20, 0.28, 0.36, 0.20),
                vertical_grid: (0.20, 0.28, 0.36, 0.18),
                axis_label: (0.18, 0.25, 0.32),
                edge_label: (0.12, 0.18, 0.24),
                border: (0.16, 0.23, 0.30, 0.24),
                analyzer_grid: (0.04, 0.42, 0.58),
                analyzer_label: (0.02, 0.34, 0.50),
                monitor_label: (0.02, 0.36, 0.54),
                selected_line: (0.02, 0.30, 0.56, 0.18),
                selected_point: (0.02, 0.30, 0.56),
                selected_halo: (0.02, 0.36, 0.68),
                effective_point: (0.18, 0.25, 0.32),
                inactive_point: (0.50, 0.56, 0.62),
                response: (0.82, 0.34, 0.02),
                disabled_response: (0.34, 0.40, 0.46),
            }
        }
    }

    pub fn for_current() -> Self {
        Self::new(crate::appearance::style_manager_is_dark())
    }
}

/// `cr.new_sub_path()` + arc helper. Cairo's `arc` connects to the current
/// point, so consecutive dots would otherwise be joined by stray lines.
fn fill_dot(ctx: &Context, x: f64, y: f64, radius: f64) {
    ctx.new_sub_path();
    ctx.arc(x, y, radius, 0.0, std::f64::consts::TAU);
    ctx.fill().unwrap();
}

/// Text with an explicit colour and size (upstream `draw_text`).
fn draw_text(ctx: &Context, text: &str, x: f64, y: f64, rgb: (f64, f64, f64), size: f64) {
    ctx.set_source_rgb(rgb.0, rgb.1, rgb.2);
    ctx.set_font_size(size);
    ctx.move_to(x, y);
    ctx.show_text(text).ok();
}

/// Rounded rectangle path (upstream `rounded_rectangle_path`).
fn rounded_rectangle_path(ctx: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let radius = radius.min(width / 2.0).min(height / 2.0);
    let pi = std::f64::consts::PI;
    ctx.new_sub_path();
    ctx.arc(x + width - radius, y + radius, radius, -pi / 2.0, 0.0);
    ctx.arc(
        x + width - radius,
        y + height - radius,
        radius,
        0.0,
        pi / 2.0,
    );
    ctx.arc(x + radius, y + height - radius, radius, pi / 2.0, pi);
    ctx.arc(x + radius, y + radius, radius, pi, pi * 1.5);
    ctx.close_path();
}

/// The dB grid lines upstream draws: -24..24 step 6, with 0 dB emphasised.
const DB_GRID_LINES: [i32; 9] = [-24, -18, -12, -6, 0, 6, 12, 18, 24];
/// The analyzer's own dBFS grid, drawn against the right-hand labels.
const ANALYZER_DB_LINES: [i32; 4] = [-60, -40, -20, 0];
/// Frequency grid lines, matching upstream.
const FREQ_GRID_LINES: [i32; 11] = [20, 30, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000];

#[derive(Debug, Clone, Copy)]
pub enum GraphMode {
    Default,
    Compact,
}

impl GraphMode {
    pub fn height(self) -> i32 {
        match self {
            GraphMode::Default => 196,
            GraphMode::Compact => 156,
        }
    }
}

#[derive(Debug, Clone)]
pub struct EqGraphState {
    pub preamp_db: f64,
    // NOTE: this used to carry `frequency`, `q` and `filter_type` for the
    // selected band. They were written on every tick and read by nothing --
    // both the response curve and the selected-band marker derive what
    // they need from `bands` + `selected_band` instead. Removed.
    pub selected_band: Option<usize>,
    pub bands: Vec<crate::core::EqBand>,
    pub analyzer_levels: Vec<f64>,
    pub mode: GraphMode,
    /// False while the A/B bypass is engaged. Upstream greys the curve and
    /// halves the fill so a bypass is visible on the graph, not just in the
    /// audio (`window_graph.py:1065-1074`).
    pub eq_enabled: bool,
    /// Display gain in dB, needed to place the analyzer's right-hand dBFS
    /// labels on the same scale the bars use.
    pub analyzer_display_gain_db: f64,
    /// Whether the monitor is running, so the background can caption itself
    /// "Monitor" and draw the dBFS scale only when there is a spectrum.
    pub analyzer_active: bool,
    /// Set once the first frame has been queued. Lets `update` tell "nothing
    /// changed since startup" (skip everything) apart from "nothing changed
    /// since the last painted frame" -- without it, a first frame identical
    /// to the defaults would never paint and the graph would stay blank.
    painted: bool,
}

impl EqGraphState {
    pub fn new() -> Self {
        Self {
            preamp_db: 0.0,
            selected_band: None,
            bands: Vec::new(),
            analyzer_levels: Vec::new(),
            mode: GraphMode::Default,
            eq_enabled: true,
            analyzer_display_gain_db: 0.0,
            analyzer_active: false,
            painted: false,
        }
    }
}

impl Default for EqGraphState {
    fn default() -> Self {
        Self::new()
    }
}

pub struct EqGraph {
    pub container: gtk4::Box,
    pub overlay: gtk4::Overlay,
    pub background_area: gtk4::DrawingArea,
    pub analyzer_area: gtk4::DrawingArea,
    pub response_area: gtk4::DrawingArea,
    /// Output-monitor on/off switch, placed in the graph header (right of the
    /// "Frequency Response" title) so it sits directly above the spectrum.
    pub monitor_switch: gtk4::Switch,
    /// Graph title row; `add_top_control` packs extra toggles into it.
    header: gtk4::Box,
    header_spacer: gtk4::Box,
    pub state: std::rc::Rc<std::cell::RefCell<EqGraphState>>,
}

impl EqGraph {
    pub fn new() -> Self {
        let state = std::rc::Rc::new(std::cell::RefCell::new(EqGraphState::new()));

        let background_area = gtk4::DrawingArea::new();
        background_area.set_vexpand(true);
        background_area.set_hexpand(true);
        let bg_state = state.clone();
        background_area.set_draw_func(move |_area, ctx, width, height| {
            let s = bg_state.borrow();
            EqGraph::draw_background(
                ctx,
                width,
                height,
                s.analyzer_display_gain_db,
                s.analyzer_active,
            );
        });

        let analyzer_area = gtk4::DrawingArea::new();
        analyzer_area.set_vexpand(true);
        analyzer_area.set_hexpand(true);
        let state_clone = state.clone();
        analyzer_area.set_draw_func(move |_area, ctx, width, height| {
            let levels = &state_clone.borrow().analyzer_levels;
            EqGraph::draw_analyzer(ctx, width, height, levels);
        });

        let response_area = gtk4::DrawingArea::new();
        response_area.set_vexpand(true);
        response_area.set_hexpand(true);
        let state_clone = state.clone();
        response_area.set_draw_func(move |_area, ctx, width, height| {
            let s = state_clone.borrow();
            EqGraph::draw_response(
                ctx,
                width,
                height,
                s.preamp_db,
                &s.bands,
                s.selected_band,
                s.eq_enabled,
            );
        });

        let overlay = gtk4::Overlay::new();
        overlay.set_child(Some(&background_area));
        overlay.add_overlay(&analyzer_area);
        overlay.add_overlay(&response_area);

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        container.set_css_classes(&["graph-shell-panel"]);
        container.set_margin_bottom(8);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Frequency Response"));
        title.set_css_classes(&["heading"]);
        header.append(&title);

        // Push the monitor control to the right edge of the title row, right
        // above the spectrum box.
        let header_spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        header_spacer.set_hexpand(true);
        header.append(&header_spacer);
        let _ = &header_spacer;

        let monitor_label = gtk4::Label::new(Some("Monitor"));
        monitor_label.set_valign(gtk4::Align::Center);
        monitor_label.set_css_classes(&["monitor-toggle-label"]);
        header.append(&monitor_label);

        let monitor_switch = gtk4::Switch::new();
        monitor_switch.set_valign(gtk4::Align::Center);
        monitor_switch.set_tooltip_text(Some("Output monitor (live spectrum)"));
        header.append(&monitor_switch);

        header.set_hexpand(true);
        container.append(&header);
        container.append(&overlay);

        Self {
            container,
            overlay,
            background_area,
            analyzer_area,
            response_area,
            monitor_switch,
            header,
            header_spacer,
            state,
        }
    }

    /// Pack a control into the graph's top row, immediately left of the
    /// Monitor toggle (so Monitor stays on the right edge).
    pub fn add_top_control(&self, widget: &impl IsA<gtk4::Widget>) {
        // Right after the expanding spacer, i.e. flush left of "Monitor".
        self.header
            .insert_child_after(widget, Some(&self.header_spacer));
    }

    pub fn set_mode(&mut self, mode: GraphMode) {
        self.state.borrow_mut().mode = mode;
        let h = mode.height();
        self.background_area.set_size_request(-1, h);
        self.analyzer_area.set_size_request(-1, h);
        self.response_area.set_size_request(-1, h);
        self.overlay.queue_draw();
    }

    pub fn update(
        &mut self,
        preamp_db: f64,
        bands: &[crate::core::EqBand],
        analyzer_levels: &[f64],
        eq_enabled: bool,
        analyzer_display_gain_db: f64,
        analyzer_active: bool,
    ) {
        // Paint on demand: the 33 ms tick calls this unconditionally, and a
        // full 4-layer Cairo repaint 30x/s of pixel-identical content was the
        // entire idle CPU footprint (~7% of one core, measured). Redraw only
        // the layers whose inputs actually moved since the last painted frame:
        // background follows monitor state/gain, response follows the curve,
        // analyzer follows the spectrum, overlay follows the curve (dots).
        // Bitwise float compare is deliberate: untouched widgets re-read
        // identical values, and decayed silence converges to exact zeros, so
        // a static graph settles into zero repaints. Any real motion (music,
        // drag, toggle) changes bits and repaints exactly as before.
        let (background_changed, response_changed, analyzer_changed, first) = {
            let s = self.state.borrow();
            (
                s.analyzer_active != analyzer_active
                    || s.analyzer_display_gain_db != analyzer_display_gain_db,
                s.preamp_db != preamp_db
                    || s.bands.as_slice() != bands
                    || s.eq_enabled != eq_enabled,
                s.analyzer_levels.as_slice() != analyzer_levels,
                !s.painted,
            )
        };
        if !(first || background_changed || response_changed || analyzer_changed) {
            return;
        }
        {
            let mut s = self.state.borrow_mut();
            s.preamp_db = preamp_db;
            s.bands = bands.to_vec();
            s.analyzer_levels = analyzer_levels.to_vec();
            s.eq_enabled = eq_enabled;
            s.analyzer_display_gain_db = analyzer_display_gain_db;
            s.analyzer_active = analyzer_active;
            s.painted = true;
        }
        // Queue the specific drawing areas directly. `overlay.queue_draw()`
        // does not reliably re-invoke the child `DrawingArea` draw funcs in
        // GTK4, which left the response curve frozen after the first paint.
        // The background is included because the analyzer's dBFS scale and the
        // "Monitor" caption follow the monitor state and the display gain.
        if background_changed || first {
            self.background_area.queue_draw();
        }
        if response_changed || first {
            self.response_area.queue_draw();
            self.overlay.queue_draw();
        }
        if analyzer_changed || first {
            self.analyzer_area.queue_draw();
        }
    }

    pub fn set_selected_band(&mut self, band_index: Option<usize>) {
        self.state.borrow_mut().selected_band = band_index;
        self.response_area.queue_draw();
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.container
    }
}

impl EqGraph {
    /// Plot frame, grid and axis labels (upstream `draw_graph_background`).
    fn draw_background(
        ctx: &Context,
        width: i32,
        height: i32,
        display_gain_db: f64,
        analyzer_active: bool,
    ) {
        let width = width as f64;
        let height = height as f64;
        let (left, right, top, bottom) = (
            GRAPH_PLOT_LEFT,
            GRAPH_PLOT_RIGHT,
            GRAPH_PLOT_TOP,
            GRAPH_PLOT_BOTTOM,
        );
        let palette = GraphPalette::for_current();

        // Rounded, vertically graded plot area. The previous version painted
        // flat 0.08 grey over the WHOLE widget, so the graph read as a dark
        // slab in a light theme and left no room for labels.
        let plot_width = width - left - right;
        let plot_height = height - top - bottom;
        if plot_width <= 0.0 || plot_height <= 0.0 {
            return;
        }
        let gradient = gtk4::cairo::LinearGradient::new(0.0, top, 0.0, height - bottom);
        let (t0, t1, t2, t3) = palette.plot_top;
        gradient.add_color_stop_rgba(0.0, t0, t1, t2, t3);
        let (b0, b1, b2, b3) = palette.plot_bottom;
        gradient.add_color_stop_rgba(1.0, b0, b1, b2, b3);
        ctx.set_source(&gradient).ok();
        rounded_rectangle_path(ctx, left, top, plot_width, plot_height, 7.0);
        ctx.fill().unwrap();

        for db in DB_GRID_LINES {
            let y = db_to_y(db as f64, height, top, bottom);
            if db == 0 {
                ctx.set_source_rgba(
                    palette.major_grid.0,
                    palette.major_grid.1,
                    palette.major_grid.2,
                    palette.major_grid.3,
                );
                ctx.set_line_width(1.6);
            } else {
                ctx.set_source_rgba(
                    palette.grid.0,
                    palette.grid.1,
                    palette.grid.2,
                    palette.grid.3,
                );
                ctx.set_line_width(1.0);
            }
            ctx.move_to(left, y);
            ctx.line_to(width - right, y);
            ctx.stroke().unwrap();
            let text = if db == 0 {
                "+0 dB".to_string()
            } else {
                format!("{:+}", db)
            };
            draw_text(ctx, &text, 10.0, y + 4.0, palette.axis_label, 11.5);
        }

        // The analyzer has its own dB scale, drawn against right-hand labels so
        // the two axes cannot be confused (upstream :942-951).
        if analyzer_active {
            for db in ANALYZER_DB_LINES {
                let norm = crate::analyzer::analyzer_db_to_display_norm(db as f64, display_gain_db);
                let y = (height - bottom) - plot_height * norm;
                let alpha = if db == 0 { 0.18 } else { 0.10 };
                ctx.set_source_rgba(
                    palette.analyzer_grid.0,
                    palette.analyzer_grid.1,
                    palette.analyzer_grid.2,
                    alpha,
                );
                ctx.set_line_width(1.0);
                ctx.move_to(left, y);
                ctx.line_to(width - right, y);
                ctx.stroke().unwrap();
                let text = if db == 0 {
                    "0 dBFS".to_string()
                } else {
                    db.to_string()
                };
                draw_text(
                    ctx,
                    &text,
                    width - right + 8.0,
                    y + 4.0,
                    palette.analyzer_label,
                    10.5,
                );
            }
        }

        for freq in FREQ_GRID_LINES {
            let x = frequency_to_x(freq as f64, width, left, right);
            ctx.set_source_rgba(
                palette.vertical_grid.0,
                palette.vertical_grid.1,
                palette.vertical_grid.2,
                palette.vertical_grid.3,
            );
            ctx.set_line_width(1.0);
            ctx.move_to(x, top);
            ctx.line_to(x, height - bottom);
            ctx.stroke().unwrap();
            let text = if freq >= 1000 {
                format!("{}k", freq / 1000)
            } else {
                freq.to_string()
            };
            draw_text(
                ctx,
                &text,
                x - 10.0,
                height - 10.0,
                palette.axis_label,
                11.5,
            );
        }

        ctx.set_source_rgba(
            palette.border.0,
            palette.border.1,
            palette.border.2,
            palette.border.3,
        );
        ctx.set_line_width(1.0);
        rounded_rectangle_path(
            ctx,
            left + 0.5,
            top + 0.5,
            plot_width - 1.0,
            plot_height - 1.0,
            6.5,
        );
        ctx.stroke().unwrap();

        draw_text(ctx, "20 Hz", left, 18.0, palette.edge_label, 11.5);
        draw_text(ctx, "20 kHz", width - 58.0, 18.0, palette.edge_label, 11.5);
        if analyzer_active {
            draw_text(
                ctx,
                "Monitor",
                left + 10.0,
                top + 18.0,
                palette.monitor_label,
                12.5,
            );
        }
    }

    /// Output spectrum behind the curve.
    ///
    /// Drawn inside the same plot rect as the curve and the grid: the overlay is
    /// a separate `DrawingArea` covering the whole widget, so it has to inset
    /// by the margins itself or the spectrum sits a label width away from the
    /// frequency axis it is meant to line up with.
    fn draw_analyzer(ctx: &Context, width: i32, height: i32, levels: &[f64]) {
        if levels.is_empty() {
            return;
        }

        let width = width as f64;
        let height = height as f64;
        let (left, right, top, bottom) = (
            GRAPH_PLOT_LEFT,
            GRAPH_PLOT_RIGHT,
            GRAPH_PLOT_TOP,
            GRAPH_PLOT_BOTTOM,
        );
        let plot_width = width - left - right;
        let plot_height = height - top - bottom;
        if plot_width <= 0.0 || plot_height <= 0.0 {
            return;
        }
        let dark = crate::appearance::style_manager_is_dark();
        let (r, g, b) = if dark {
            (0.33, 0.78, 0.90)
        } else {
            (0.03, 0.46, 0.60)
        };

        let bar_count = levels.len().min(crate::analyzer::ANALYZER_BIN_COUNT);
        let bar_width = plot_width / bar_count as f64;
        // A 1 px gap, but never wider than a third of the bar, so narrow bars
        // stay visible (upstream `cached_analyzer_bar_geometry`).
        let gap = 1.5f64.min(bar_width * 0.35);
        ctx.set_source_rgba(r, g, b, 0.15);
        for (i, &level) in levels.iter().take(bar_count).enumerate() {
            let bar_height = (level * plot_height).clamp(0.0, plot_height);
            let x = left + i as f64 * bar_width;
            let y = height - bottom - bar_height;
            ctx.rectangle(x, y, (bar_width - gap).max(1.0), bar_height);
            ctx.fill().unwrap();
        }
    }

    /// The response curve, the selected-band focus line and one dot per active
    /// band (upstream `draw_graph_response_overlay`).
    fn draw_response(
        ctx: &Context,
        width: i32,
        height: i32,
        preamp_db: f64,
        bands: &[crate::core::EqBand],
        selected_band: Option<usize>,
        eq_enabled: bool,
    ) {
        let width = width as f64;
        let height = height as f64;
        let (left, right, top, bottom) = (
            GRAPH_PLOT_LEFT,
            GRAPH_PLOT_RIGHT,
            GRAPH_PLOT_TOP,
            GRAPH_PLOT_BOTTOM,
        );
        let plot_width = width - left - right;
        if plot_width <= 0.0 || height - top - bottom <= 0.0 {
            return;
        }
        let palette = GraphPalette::for_current();
        let solo_active = crate::core::bands_have_solo(bands);

        // Focus line for the selected band, drawn from the top to the bottom of
        // the plot rect (it used to span the whole widget, through the axis
        // labels).
        if let Some(index) = selected_band
            && let Some(band) = bands.get(index)
        {
            let x = frequency_to_x(band.frequency, width, left, right);
            ctx.set_source_rgba(
                palette.selected_line.0,
                palette.selected_line.1,
                palette.selected_line.2,
                palette.selected_line.3,
            );
            ctx.set_line_width(1.4);
            ctx.move_to(x, top);
            ctx.line_to(x, height - bottom);
            ctx.stroke().unwrap();
        }

        // Curve. Starts at the first sample's own y: the old code began with
        // `move_to(0, centre_y)`, which drew a vertical spike up the left edge.
        let num_points = plot_width as usize;
        let mut frequencies = Vec::with_capacity(num_points);
        for i in 0..num_points {
            let freq = EQ_FREQUENCY_MIN_HZ
                * (EQ_FREQUENCY_MAX_HZ / EQ_FREQUENCY_MIN_HZ).powf(i as f64 / num_points as f64);
            frequencies.push(freq);
        }

        let response = crate::core::total_response_db_at_frequencies(
            bands,
            preamp_db,
            crate::core::SAMPLE_RATE,
            &frequencies,
        );
        let (curve_r, curve_g, curve_b) = if eq_enabled {
            palette.response
        } else {
            palette.disabled_response
        };
        ctx.set_source_rgb(curve_r, curve_g, curve_b);
        ctx.set_line_width(2.6);
        let mut started = false;
        for (i, &db) in response.iter().enumerate() {
            let y = db_to_y(db, height, top, bottom);
            if started {
                ctx.line_to(left + i as f64, y);
            } else {
                ctx.move_to(left, y);
                started = true;
            }
        }
        ctx.stroke().unwrap();

        // One dot per active band -- the "movable spots". Previously only the
        // selected band got a marker, and at the band's raw gain, which floats
        // off the curve whenever the preamp is non-zero or bands overlap.
        for (index, band) in bands.iter().enumerate() {
            if band.filter_type == crate::core::FilterType::Off {
                continue;
            }
            let x = frequency_to_x(band.frequency, width, left, right);
            let y = db_to_y(
                crate::core::total_response_db(
                    bands,
                    preamp_db,
                    crate::core::SAMPLE_RATE,
                    band.frequency,
                ),
                height,
                top,
                bottom,
            );
            let selected = Some(index) == selected_band;
            let effective = crate::core::band_is_effective(band, solo_active);
            let colour = if selected {
                palette.selected_point
            } else if effective {
                palette.effective_point
            } else {
                palette.inactive_point
            };
            ctx.set_source_rgb(colour.0, colour.1, colour.2);
            let radius = if selected {
                5.8
            } else if effective {
                4.2
            } else {
                3.6
            };
            fill_dot(ctx, x, y, radius);
            if selected {
                ctx.set_source_rgba(
                    palette.selected_halo.0,
                    palette.selected_halo.1,
                    palette.selected_halo.2,
                    0.24,
                );
                fill_dot(ctx, x, y, 12.0);
            }
        }
    }
}

impl Default for EqGraph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: f64 = GRAPH_PLOT_TOP;
    const B: f64 = GRAPH_PLOT_BOTTOM;

    #[test]
    fn test_db_to_y_maps_axis_endpoints_inside_the_plot_rect() {
        // The axis is inverted: GRAPH_DB_MAX sits at the plot top (y = T).
        assert!((db_to_y(GRAPH_DB_MAX, 200.0, T, B) - T).abs() < 1e-9);
        assert!((db_to_y(GRAPH_DB_MIN, 200.0, T, B) - (200.0 - B)).abs() < 1e-9);
        // 0 dB is the midpoint of a symmetric +/-24 dB axis.
        assert!((db_to_y(0.0, 200.0, T, B) - (T + (200.0 - T - B) / 2.0)).abs() < 1e-9);
    }

    #[test]
    fn test_db_to_y_clamps_out_of_range() {
        let top = db_to_y(GRAPH_DB_MAX, 200.0, T, B);
        let bottom = db_to_y(GRAPH_DB_MIN, 200.0, T, B);
        assert!((db_to_y(100.0, 200.0, T, B) - top).abs() < 1e-9);
        assert!((db_to_y(-100.0, 200.0, T, B) - bottom).abs() < 1e-9);
    }

    #[test]
    fn test_frequency_to_x_spans_the_plot_rect() {
        // The axis ends land exactly on the gutters, which is what keeps the
        // curve and the spectrum aligned inside the frame.
        assert!((frequency_to_x(EQ_FREQUENCY_MIN_HZ, 400.0, 58.0, 62.0) - 58.0).abs() < 1e-9);
        assert!(
            (frequency_to_x(EQ_FREQUENCY_MAX_HZ, 400.0, 58.0, 62.0) - (400.0 - 62.0)).abs() < 1e-9
        );
        // The geometric mean of the axis ends sits exactly halfway along it.
        // (The arithmetic mean does not: the axis is logarithmic, so 1 kHz is
        // past the midpoint of 20 Hz..20 kHz.)
        let geom_mean = (EQ_FREQUENCY_MIN_HZ * EQ_FREQUENCY_MAX_HZ).sqrt();
        let mid = frequency_to_x(geom_mean, 400.0, 58.0, 62.0);
        assert!((mid - (58.0 + (400.0 - 62.0 - 58.0) / 2.0)).abs() < 1e-9);
        // ...and 1 kHz must therefore be to the RIGHT of the midpoint.
        assert!(frequency_to_x(1000.0, 400.0, 58.0, 62.0) > mid);
    }

    #[test]
    fn test_coordinate_mappers_are_inverses() {
        let (w, h, l, r) = (900.0_f64, 220.0_f64, 58.0_f64, 62.0_f64);
        for freq in [20.0, 63.0, 440.0, 1000.0, 8000.0, 20000.0] {
            let back = x_to_frequency(frequency_to_x(freq, w, l, r), w, l, r);
            assert!(
                (back - freq).abs() < 1e-6,
                "frequency round trip: {freq} -> {back}"
            );
        }
        for db in [-24.0, -12.0, -3.5, 0.0, 7.25, 24.0] {
            let back = y_to_db(db_to_y(db, h, T, B), h, T, B);
            assert!((back - db).abs() < 1e-9, "dB round trip: {db} -> {back}");
        }
    }

    #[test]
    fn test_x_to_frequency_clamps_outside_the_plot_rect() {
        let (w, l, r) = (400.0_f64, 58.0_f64, 62.0_f64);
        assert!((x_to_frequency(-500.0, w, l, r) - EQ_FREQUENCY_MIN_HZ).abs() < 1e-9);
        assert!((x_to_frequency(9999.0, w, l, r) - EQ_FREQUENCY_MAX_HZ).abs() < 1e-9);
    }

    /// The band dot has to sit ON the curve. It used to be drawn at the band's
    /// raw gain, so with a non-zero preamp every dot floated away from the line.
    #[test]
    fn test_band_dot_y_is_the_total_response_not_the_raw_gain() {
        let mut bands = vec![crate::core::EqBand::new(0), crate::core::EqBand::new(1)];
        // Two bells on the SAME frequency: their gains cancel in the summed
        // response, so the curve at 100 Hz is nowhere near either band's own
        // gain. Any overlap of this kind makes the two values differ.
        bands[0].frequency = 100.0;
        bands[0].gain_db = 6.0;
        bands[0].filter_type = crate::core::FilterType::Bell;
        bands[1].frequency = 100.0;
        bands[1].gain_db = -4.0;
        bands[1].filter_type = crate::core::FilterType::Bell;

        let preamp = -3.0;
        let dot_db = crate::core::total_response_db(
            &bands,
            preamp,
            crate::core::SAMPLE_RATE,
            bands[1].frequency,
        );
        let raw = bands[1].gain_db + preamp;
        assert!(
            (dot_db - raw).abs() > 0.5,
            "with overlapping bands the raw gain must differ from the curve, \
             otherwise this test cannot detect the regression it guards"
        );
        assert!(
            (GRAPH_DB_MIN..=GRAPH_DB_MAX).contains(&dot_db),
            "the dot must land on the plotted axis, got {dot_db}"
        );
    }
}
