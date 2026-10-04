//! Utility sidebar: three independently-selectable panels (Preset, Monitor,
//! Output) held in a `gtk4::Stack`. The header's three buttons
//! switch `container`'s visible child, so only one panel shows at a time
//! and the user never has to scroll a single mega-column.

use std::cell::RefCell;
use std::rc::Rc;

use gtk4::prelude::*;

/// Dropdown index 0: follow whatever the system default output is.
pub const FOLLOW_DEFAULT_LABEL: &str = "Default Output";

use crate::window_graph;
use crate::window_headroom;
use crate::window_presets;

/// One labelled control row inside the Monitor Settings popover: title on the
/// left, dim value readout, control on the right.
///
/// Upstream uses `Adw.ActionRow(title=...)` with the control and a value label
/// as suffixes (`window_utility.py`, "Smoothing" / "Display Gain" / "Freeze"). A
/// `ListBox` with `boxed-list` styling stands in for the ActionRow because the
/// popover holds no AdwPreferencesGroup.
fn settings_row<C: IsA<gtk4::Widget>>(
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

/// The monitor's three settings and its loudness readout.
///
/// Smoothing / Display Gain / Freeze live here and nowhere else, exactly as
/// upstream (`window_utility.py`: `analyzer_settings_popover`) — except that the
/// gear button that opens them sits in the **main window's** output control row
/// (just before the Smooth dropdown) instead of in a sidebar panel. The Rust
/// port used to carry two further copies of all three, on an "Analyzer" sidebar
/// page whose Smoothing/Display Gain/Freeze rows were the only wired ones; that
/// page is gone, along with its second copy of the spectrum itself. The single
/// spectrum is the graph overlay (`window_graph::draw_analyzer`).
pub struct MonitorControls {
    /// Gear button + popover holding the three settings. Reparented into the
    /// main window's output control row by `window_layout`, so it is built
    /// standalone rather than wrapped in a panel.
    pub settings_button: gtk4::MenuButton,
    pub smoothing_scale: gtk4::Scale,
    pub smoothing_value: gtk4::Label,
    pub display_gain_scale: gtk4::Scale,
    pub display_gain_value: gtk4::Label,
    pub freeze_switch: gtk4::Switch,
    /// The loudness readout that travels with the settings into the sidebar's
    /// Output page (LUFS value + one-line status).
    pub readout: gtk4::Box,
    pub loudness_value: gtk4::Label,
    pub summary: gtk4::Label,
    /// Set by the Freeze switch. The update loop stops feeding new frames to
    /// the spectrum overlay and the loudness readout while it is set, matching
    /// upstream's `analyzer_frozen` gate on `on_analyzer_levels` and
    /// `on_analyzer_loudness` (`window_analyzer.py`).
    pub frozen: Rc<std::cell::Cell<bool>>,
}

/// Stack page names (used by the header buttons to switch panels).
pub const PAGE_PRESET: &str = "preset";
/// Output-device settings plus the monitor's loudness readout. The headroom /
/// preamp / Auto-Safe / A-B controls sit in the main window's output control
/// row, and the monitor's own settings sit there too, so this page is the only
/// place left that describes the output device.
pub const PAGE_OUTPUT: &str = "output";

/// Utility sidebar holding the two panels in a `Stack`.
pub struct UtilityPane {
    /// The sidebar: a `Stack` whose visible child is the selected panel.
    pub container: gtk4::Stack,
    pub headroom: Rc<RefCell<window_headroom::HeadroomPanel>>,
    pub graph: Rc<RefCell<window_graph::EqGraph>>,
    pub presets: Rc<RefCell<window_presets::PresetPanel>>,
    /// Monitor strip: LUFS readout, summary and the three settings controls.
    pub monitor: MonitorControls,
    /// A/B compare (EQ bypass) switch.
    pub bypass_switch: gtk4::Switch,
    /// Output device dropdown (moved from the header into Output Controls).
    pub output_dropdown: gtk4::DropDown,
    /// Fallback preset action (default preset for unmatched outputs).
    pub fallback_button: gtk4::Button,
    pub fallback_label: gtk4::Label,
    /// Link-to-output action (auto-load current preset for the active output).
    pub link_button: gtk4::Button,
    pub link_label: gtk4::Label,
}

impl UtilityPane {
    pub fn new() -> Self {
        let headroom = Rc::new(RefCell::new(window_headroom::HeadroomPanel::new()));
        let graph = Rc::new(RefCell::new(window_graph::EqGraph::new()));
        let presets = window_presets::PresetPanel::new();
        presets.borrow_mut().start_file_monitoring();

        // --- Preset page: the preset panel (has its own scrollable list).
        let preset_page = Self::scroll_page(presets.borrow().widget());

        // The monitor's gear button goes into the main window's output control
        // row; only its loudness readout stays on a sidebar page.
        let monitor = Self::build_monitor_controls();

        // --- Output page: output-device settings plus the loudness readout.
        // preamp, Auto-Safe and A/B compare now live in the main window's
        // output control row (see `window_layout::build_output_control_row`).
        let bypass_switch = Self::build_bypass_switch();
        let (
            output_controls,
            output_dropdown,
            fallback_button,
            fallback_label,
            link_button,
            link_label,
        ) = Self::build_output_controls();
        let output_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        output_box.set_margin_top(8);
        output_box.set_margin_bottom(8);
        output_box.set_margin_start(8);
        output_box.set_margin_end(8);
        output_box.append(&output_controls);
        output_box.append(&monitor.readout);
        let output_page = Self::scroll_page(&output_box);

        // --- Stack: the two pages, Preset shown by default.
        let stack = gtk4::Stack::new();
        stack.set_hhomogeneous(false);
        stack.set_vhomogeneous(false);
        stack.set_transition_type(gtk4::StackTransitionType::SlideLeftRight);
        stack.add_titled(&preset_page, Some(PAGE_PRESET), "Preset");
        stack.add_titled(&output_page, Some(PAGE_OUTPUT), "Output");
        stack.set_visible_child_name(PAGE_PRESET);

        Self {
            container: stack,
            headroom,
            graph,
            presets,
            monitor,
            bypass_switch,
            output_dropdown,
            fallback_button,
            fallback_label,
            link_button,
            link_label,
        }
    }

    /// Wrap a panel in a vertically-scrolling ScrolledWindow so a tall panel
    /// degrades gracefully instead of overflowing the sidebar.
    fn scroll_page(child: &impl IsA<gtk4::Widget>) -> gtk4::ScrolledWindow {
        let sw = gtk4::ScrolledWindow::new();
        sw.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        sw.set_vexpand(true);
        sw.set_hexpand(true);
        sw.set_child(Some(child));
        sw
    }

    /// A/B compare (EQ bypass) switch. The switch is reparented into the
    /// main window's output control row, so it is built standalone rather
    /// than wrapped in a sidebar row.
    fn build_bypass_switch() -> gtk4::Switch {
        let bypass_switch = gtk4::Switch::new();
        bypass_switch.set_valign(gtk4::Align::Center);
        bypass_switch.set_tooltip_text(Some("Bypass the EQ to compare with/without"));
        bypass_switch
    }

    /// "Output Controls" section for the Headroom page: the output-device
    /// dropdown (moved out of the header) plus the per-output-device
    /// auto-preset actions (Fallback / Link to Output).
    fn build_output_controls() -> (
        gtk4::Box,
        gtk4::DropDown,
        gtk4::Button,
        gtk4::Label,
        gtk4::Button,
        gtk4::Label,
    ) {
        let section = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        section.set_css_classes(&["utility-section"]);

        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Output Controls"));
        title.set_css_classes(&["heading"]);
        header.append(&title);
        section.append(&header);

        // Output device dropdown.
        //
        // This was `StringList::new(&["System Output", "Virtual Sink"])` — two
        // hardcoded labels that named no real device, with no handler and
        // nothing reading the selection. It is now populated from PipeWire
        // (`RoutingEngine::list_output_sinks`, which filters on
        // `media.class == "Audio/Sink"`), with index 0 meaning "follow the
        // system default", matching upstream. The window refills this model
        // whenever the device list changes.
        let output_list = gtk4::StringList::new(&[FOLLOW_DEFAULT_LABEL]);
        let output_dropdown = gtk4::DropDown::new(Some(output_list), None::<gtk4::Expression>);
        output_dropdown.set_hexpand(true);
        output_dropdown.set_tooltip_text(Some("EQ output device"));
        output_dropdown.set_sensitive(false); // enabled once real devices are known
        let output_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let output_label = gtk4::Label::new(Some("Output"));
        output_label.set_css_classes(&["metric-title"]);
        output_row.append(&output_label);
        output_row.append(&output_dropdown);
        section.append(&output_row);

        // Fallback: default preset for unmatched output devices.
        let fallback_button = gtk4::Button::with_label("Set Fallback");
        fallback_button.set_tooltip_text(Some("Use the current preset for unmatched outputs"));
        let fallback_label = gtk4::Label::new(Some("None"));
        fallback_label.set_css_classes(&["dim-label"]);
        fallback_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let fallback_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let fallback_title = gtk4::Label::new(Some("Fallback"));
        fallback_title.set_css_classes(&["metric-title"]);
        fallback_title.set_hexpand(true);
        fallback_row.append(&fallback_title);
        fallback_row.append(&fallback_label);
        fallback_row.append(&fallback_button);
        section.append(&fallback_row);

        // Link to Output: auto-load the current preset for the active output.
        let link_button = gtk4::Button::with_label("Link to Output");
        link_button.set_tooltip_text(Some("Auto-load the current preset for this output device"));
        let link_label = gtk4::Label::new(Some("None"));
        link_label.set_css_classes(&["dim-label"]);
        link_label.set_ellipsize(gtk4::pango::EllipsizeMode::End);
        let link_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let link_title = gtk4::Label::new(Some("Link to Output"));
        link_title.set_css_classes(&["metric-title"]);
        link_title.set_hexpand(true);
        link_row.append(&link_title);
        link_row.append(&link_label);
        link_row.append(&link_button);
        section.append(&link_row);

        (
            section,
            output_dropdown,
            fallback_button,
            fallback_label,
            link_button,
            link_label,
        )
    }

    /// The monitor's gear button (with its three settings) and the loudness
    /// readout that trails it on the sidebar's Output page.
    ///
    /// No panel wraps them: the button is reparented into the main window's
    /// output control row and the readout into the Output page, so neither
    /// needs a container of its own here.
    fn build_monitor_controls() -> MonitorControls {
        let settings_button = gtk4::MenuButton::new();
        settings_button.set_icon_name("preferences-system-symbolic");
        settings_button.set_tooltip_text(Some("Monitor Settings"));
        settings_button.set_valign(gtk4::Align::Center);
        settings_button.set_css_classes(&["toolbar-icon-button", "monitor-settings-button"]);

        let settings_popover = gtk4::Popover::new();
        let settings_box = gtk4::Box::new(gtk4::Orientation::Vertical, 8);
        settings_box.set_margin_top(8);
        settings_box.set_margin_bottom(8);
        settings_box.set_margin_start(8);
        settings_box.set_margin_end(8);

        // Default 30% maps to ANALYZER_RESPONSE_DEFAULT (2.0) via
        // PipeWireBackend::set_analyzer_smoothing, so this reproduces the
        // analyzer's own default (upstream `analyzer_smoothing` default).
        let smoothing_scale =
            gtk4::Scale::with_range(gtk4::Orientation::Horizontal, 15.0, 95.0, 1.0);
        smoothing_scale.set_size_request(116, -1);
        smoothing_scale.set_hexpand(true);
        let smoothing_value = gtk4::Label::new(Some("30%"));

        let display_gain_scale =
            gtk4::Scale::with_range(gtk4::Orientation::Horizontal, -12.0, 32.0, 1.0);
        display_gain_scale.set_size_request(116, -1);
        display_gain_scale.set_hexpand(true);
        let display_gain_value = gtk4::Label::new(Some("+0 dB"));

        let freeze_switch = gtk4::Switch::new();
        freeze_switch.set_valign(gtk4::Align::Center);

        // Live value readouts, as upstream's dim suffix labels.
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

        let settings_list = gtk4::ListBox::new();
        settings_list.set_selection_mode(gtk4::SelectionMode::None);
        settings_list.add_css_class("boxed-list");
        settings_list.append(&settings_row(
            "Smoothing",
            Some("How quickly the spectrum reacts. Lower is smoother and steadier."),
            &smoothing_scale,
            Some(&smoothing_value),
        ));
        settings_list.append(&settings_row(
            "Display Gain",
            Some("Display-only boost for the spectrum; does not change the audio"),
            &display_gain_scale,
            Some(&display_gain_value),
        ));
        settings_list.append(&settings_row(
            "Freeze",
            Some("Hold the current spectrum and loudness readout still"),
            &freeze_switch,
            None,
        ));
        settings_box.append(&settings_list);

        settings_popover.set_child(Some(&settings_box));
        settings_button.set_popover(Some(&settings_popover));

        // Loudness readout. Upstream also draws a small LUFS bar here
        // (`on_loudness_meter_draw`); this port allocated the DrawingArea but
        // never gave it a draw func, so it rendered as an empty box. Only the
        // live numbers are kept until the bar is actually implemented.
        let readout = gtk4::Box::new(gtk4::Orientation::Vertical, 4);
        readout.set_css_classes(&["utility-section"]);
        let readout_header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let readout_title = gtk4::Label::new(Some("Monitor"));
        readout_title.set_css_classes(&["metric-title"]);
        readout_header.append(&readout_title);
        readout.append(&readout_header);

        let detail_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        detail_row.set_css_classes(&["monitor-detail-row"]);
        let loudness_value = gtk4::Label::new(Some("-- LUFS"));
        loudness_value.set_css_classes(&["numeric", "loudness-value-label"]);
        loudness_value.set_width_chars(8);
        detail_row.append(&loudness_value);
        readout.append(&detail_row);

        let summary_label = gtk4::Label::new(Some("Monitor off"));
        summary_label.set_css_classes(&["monitor-summary-label"]);
        summary_label.set_halign(gtk4::Align::Start);
        readout.append(&summary_label);

        MonitorControls {
            settings_button,
            readout,
            loudness_value,
            summary: summary_label,
            smoothing_scale,
            smoothing_value,
            display_gain_scale,
            display_gain_value,
            freeze_switch,
            frozen: Rc::new(std::cell::Cell::new(false)),
        }
    }

    /// Show a specific panel by page name (called by the header buttons).
    pub fn show_page(&self, page: &str) {
        self.container.set_visible_child_name(page);
    }
}

impl Default for UtilityPane {
    fn default() -> Self {
        Self::new()
    }
}
