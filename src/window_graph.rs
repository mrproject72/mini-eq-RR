//! Frequency response graph widget with analyzer overlay and drag editing.

use gtk4::cairo::Context;
use gtk4::prelude::*;

use crate::core::{
    EQ_FREQUENCY_MAX_HZ, EQ_FREQUENCY_MIN_HZ, GRAPH_DB_MAX, GRAPH_DB_MIN, SAMPLE_RATE,
    total_response_db_at_frequencies,
};

/// Map a dB value to a y pixel, matching upstream `db_to_y`.
///
/// The axis spans `GRAPH_DB_MIN..GRAPH_DB_MAX` (±24 dB), not the ±20 dB EQ gain
/// range, so the curve and the grid lines stay consistent with the Python
/// original.
fn db_to_y(db_value: f64, height: f64) -> f64 {
    let normalized =
        (db_value.clamp(GRAPH_DB_MIN, GRAPH_DB_MAX) - GRAPH_DB_MIN) / (GRAPH_DB_MAX - GRAPH_DB_MIN);
    height * (1.0 - normalized)
}

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
}

impl EqGraphState {
    pub fn new() -> Self {
        Self {
            preamp_db: 0.0,
            selected_band: None,
            bands: Vec::new(),
            analyzer_levels: Vec::new(),
            mode: GraphMode::Default,
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
        background_area.set_draw_func(|_area, ctx, width, height| {
            EqGraph::draw_background(ctx, width, height);
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
            EqGraph::draw_response(ctx, width, height, s.preamp_db, &s.bands, s.selected_band);
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
    ) {
        let mut s = self.state.borrow_mut();
        s.preamp_db = preamp_db;
        s.bands = bands.to_vec();
        s.analyzer_levels = analyzer_levels.to_vec();
        // Queue the specific drawing areas directly. `overlay.queue_draw()`
        // does not reliably re-invoke the child `DrawingArea` draw funcs in
        // GTK4, which left the response curve frozen after the first paint.
        self.response_area.queue_draw();
        self.analyzer_area.queue_draw();
        self.overlay.queue_draw();
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
    fn draw_background(ctx: &Context, width: i32, height: i32) {
        let width = width as f64;
        let height = height as f64;

        ctx.set_source_rgb(0.08, 0.08, 0.08);
        ctx.paint().unwrap();

        let center_y = height / 2.0;

        ctx.set_source_rgba(0.2, 0.2, 0.2, 0.5);
        ctx.set_line_width(0.5);

        for db in [-20, -10, 0, 10, 20].iter() {
            let y = db_to_y(*db as f64, height);
            ctx.move_to(0.0, y);
            ctx.line_to(width, y);
            ctx.stroke().unwrap();
        }

        for &freq in [20, 50, 100, 200, 500, 1000, 2000, 5000, 10000, 20000].iter() {
            let x = ((freq as f64).log10() - EQ_FREQUENCY_MIN_HZ.log10())
                / (EQ_FREQUENCY_MAX_HZ.log10() - EQ_FREQUENCY_MIN_HZ.log10())
                * width;
            ctx.move_to(x, 0.0);
            ctx.line_to(x, height);
            ctx.stroke().unwrap();
        }

        ctx.set_source_rgba(0.4, 0.4, 0.4, 0.8);
        ctx.set_line_width(1.0);
        ctx.move_to(0.0, center_y);
        ctx.line_to(width, center_y);
        ctx.stroke().unwrap();
    }

    fn draw_analyzer(ctx: &Context, width: i32, height: i32, levels: &[f64]) {
        if levels.is_empty() {
            return;
        }

        let width = width as f64;
        let height = height as f64;
        let bar_count = levels.len().min(64);
        let bar_width = width / bar_count as f64;

        ctx.set_source_rgba(0.0, 0.8, 0.0, 0.3);
        for (i, &level) in levels.iter().take(bar_count).enumerate() {
            let bar_height = (level * height).min(height);
            let x = i as f64 * bar_width;
            let y = height - bar_height;
            ctx.rectangle(x, y, bar_width - 1.0, bar_height);
            ctx.fill().unwrap();
        }
    }

    fn draw_response(
        ctx: &Context,
        width: i32,
        height: i32,
        preamp_db: f64,
        bands: &[crate::core::EqBand],
        selected_band: Option<usize>,
    ) {
        let width = width as f64;
        let height = height as f64;
        let center_y = height / 2.0;

        ctx.set_source_rgb(0.0, 0.8, 0.0);
        ctx.set_line_width(2.0);
        ctx.move_to(0.0, center_y);

        let num_points = width as usize;
        let mut frequencies = Vec::with_capacity(num_points);
        for i in 0..num_points {
            let freq = EQ_FREQUENCY_MIN_HZ
                * (EQ_FREQUENCY_MAX_HZ / EQ_FREQUENCY_MIN_HZ).powf(i as f64 / num_points as f64);
            frequencies.push(freq);
        }

        let response =
            total_response_db_at_frequencies(bands, preamp_db, SAMPLE_RATE, &frequencies);

        for (i, &db) in response.iter().enumerate() {
            let y = db_to_y(db, height);
            ctx.line_to(i as f64, y);
        }
        ctx.stroke().unwrap();

        if let Some(band_idx) = selected_band
            && let Some(band) = bands.get(band_idx)
        {
            let x = (band.frequency.log10() - EQ_FREQUENCY_MIN_HZ.log10())
                / (EQ_FREQUENCY_MAX_HZ.log10() - EQ_FREQUENCY_MIN_HZ.log10())
                * width;
            let y = db_to_y(band.gain_db, height);

            ctx.set_source_rgba(1.0, 1.0, 1.0, 0.5);
            ctx.set_line_width(1.0);
            ctx.move_to(x, 0.0);
            ctx.line_to(x, height);
            ctx.stroke().unwrap();

            ctx.arc(x, y, 6.0, 0.0, std::f64::consts::PI * 2.0);
            ctx.set_source_rgba(1.0, 1.0, 1.0, 0.8);
            ctx.fill().unwrap();
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

    #[test]
    fn test_db_to_y_maps_axis_endpoints() {
        // The axis is inverted: GRAPH_DB_MAX sits at the top (y = 0).
        assert!((db_to_y(GRAPH_DB_MAX, 100.0) - 0.0).abs() < 1e-9);
        assert!((db_to_y(GRAPH_DB_MIN, 100.0) - 100.0).abs() < 1e-9);
        // 0 dB is the midpoint of a symmetric ±24 dB axis.
        assert!((db_to_y(0.0, 100.0) - 50.0).abs() < 1e-9);
    }

    #[test]
    fn test_db_to_y_clamps_out_of_range() {
        assert!((db_to_y(100.0, 100.0) - db_to_y(GRAPH_DB_MAX, 100.0)).abs() < 1e-9);
        assert!((db_to_y(-100.0, 100.0) - db_to_y(GRAPH_DB_MIN, 100.0)).abs() < 1e-9);
    }
}
