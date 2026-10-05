//! Main layout for the mini-eq application window.

use gtk4::prelude::*;

use crate::core::{DEFAULT_ACTIVE_BANDS, MAX_BANDS};
use crate::window_band_fader::WindowBandFader;
use crate::window_utility::UtilityPane;

use std::cell::RefCell;
use std::rc::Rc;

/// Build the band fader row layout.
///
/// `selection_changed_callback` receives the index of the band the user just
/// selected; the owner is responsible for clearing the other faders (a fader
/// cannot do that itself without holding borrows on its siblings).
///
/// `gain_changed` is the owner-authoritative gain request handed to every
/// fader: it returns the gain the band may actually use. It MUST be threaded
/// all the way down — a stub here would silently disable peak safety for all
/// fader gestures (drag/scroll/keys) while leaving the editor spin clamped,
/// which is exactly how stacked shelves escaped the limit.
pub fn build_band_faders(
    visible_bands: usize,
    gain_changed: Rc<dyn Fn(usize, f64) -> f64>,
    selection_changed_callback: Rc<dyn Fn(usize)>,
) -> (
    gtk4::ScrolledWindow,
    Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>,
) {
    let scrolled = gtk4::ScrolledWindow::new();
    scrolled.set_hexpand(true);
    scrolled.set_vexpand(true);
    // Horizontal: NEVER scroll. The fader row is homogeneous + hexpand, so it
    // is forced to the viewport width and shrinks to fit. This prevents the
    // row from keeping its natural (72*10=738px) width and overflowing the
    // window — which was pushing the overlay side panel outside the viewport.
    scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
    scrolled.set_min_content_height(200);

    let band_box = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    band_box.set_css_classes(&["band-fader-container"]);
    band_box.set_hexpand(true);
    band_box.set_vexpand(true);
    // Equal-width faders that share the available horizontal space, so the
    // row shrinks to fit a narrow window instead of overflowing it.
    band_box.set_homogeneous(true);

    let mut faders = Vec::new();
    let count = visible_bands.clamp(1, MAX_BANDS);
    // Upstream `compute_log_spaced_band_defaults` derives both frequency and Q
    // from the band count, so the row spans 20 Hz..20 kHz across `count` bands.
    let defaults = crate::core::compute_log_spaced_band_defaults(count);
    for (i, (frequency, q)) in defaults.into_iter().enumerate() {
        // Active bands must default to Bell (matching `core::default_bands()`).
        // With `Off` the wet/dry mix is 0, so dragging the fader changes gain
        // on a bypassed biquad and produces no audible effect.
        let filter_type = if i < DEFAULT_ACTIVE_BANDS {
            crate::core::FilterType::Bell
        } else {
            crate::core::FilterType::Off
        };
        let band = WindowBandFader::new(
            i,
            frequency,
            0.0,
            q,
            filter_type,
            i < DEFAULT_ACTIVE_BANDS,
            gain_changed.clone(),
            selection_changed_callback.clone(),
        );
        faders.push(band.fader.clone());
        band_box.append(band.widget());
    }

    scrolled.set_child(Some(&band_box));
    (scrolled, faders)
}

/// Fixed cell widths for the output row (see `fixed_cell`).
//
// These are MINIMUM cell widths: `set_size_request` is a floor, not a cap, so
// a cell holding a caption is as wide as the caption plus its control. They
// exist so that showing or hiding a control -- the Fix button going
// insensitive, the peak number appearing -- cannot slide the rest of the row.
const CELL_W_SMOOTH: i32 = 96;
const CELL_W_PREAMP: i32 = 96;
const CELL_W_STATUS: i32 = 108;
/// One cell for the whole clipping group: [Clip] [Fix] [Auto].
const CELL_W_CLIP: i32 = 148;

/// The main-window output control row, between the spectrum and the fader
/// strip: monitor settings, Smooth, the preamp, the live peak readout and the
/// clipping pair.
///
/// Every control is reparented from the sidebar panels, so the sidebar can hold
/// only output-device settings. The captions stay visible at every width: they
/// are the only thing naming the controls, and a row of unlabelled switches
/// and numbers is not something to ship to make a width budget easier. An
/// earlier version hid every caption below 1000px and left only tooltips, which
/// made the row unreadable at ordinary window sizes.
///
/// `Fix` — not the header icon — carries the clipping alert.
fn build_output_control_row(utility: &UtilityPane) -> gtk4::FlowBox {
    // FlowBox rather than adw::WrapBox. WrapBox needs libadwaita >= 1.7,
    // but Ubuntu 24.04 LTS -- a perfectly reasonable target for a desktop
    // EQ, supported until 2029 -- ships 1.5.0. Requiring 1.7 would make
    // the app unbuildable there, so the wrapping is done with GTK4's own
    // container instead and the v1_7 feature is dropped.
    //
    // A plain gtk4::Box is NOT an option: the row is ~630px against a
    // 640px minimum window, so it genuinely needs to wrap rather than
    // clip.
    let row = gtk4::FlowBox::new();
    row.set_selection_mode(gtk4::SelectionMode::None);
    row.set_homogeneous(false);
    row.set_min_children_per_line(1);
    row.set_max_children_per_line(32);
    row.set_column_spacing(8);
    row.set_row_spacing(10);
    row.set_css_classes(&["output-control-row"]);
    row.set_halign(gtk4::Align::Center);
    row.set_hexpand(true);
    row.set_valign(gtk4::Align::Center);

    let headroom = utility.headroom.borrow();
    let mut labels: Vec<gtk4::Label> = Vec::new();

    // Every cell has a FIXED width. Previously the cells sized to their
    // content, so hiding the preamp or the width control slid everything
    // else sideways. Fixed cells mean a control appearing or vanishing never
    // moves its neighbours, and the wrap points are deterministic too.
    // One cell for the monitor settings gear and the Smooth dropdown, with the
    // switch and its width spin inside the menu popover.
    //
    // They share a cell on purpose. The gear used to sit in its own fixed
    // 40 px cell, which cost a whole extra FlowBox gap on top of the icon: at
    // the minimum window width that was enough to push the Set Safe button onto
    // a second line. Two controls that are both small and both about how
    // editing behaves belong in one cell with a 2 px gap.
    let smooth_item = gtk4::Box::new(gtk4::Orientation::Horizontal, 2);
    smooth_item.set_size_request(CELL_W_SMOOTH, -1);
    smooth_item.set_halign(gtk4::Align::Start);
    smooth_item.set_valign(gtk4::Align::Center);
    smooth_item.set_tooltip_text(Some("Smooth: couple overlapping bands while dragging"));
    utility
        .monitor
        .settings_button
        .set_halign(gtk4::Align::Start);
    smooth_item.append(&utility.monitor.settings_button);
    smooth_item.append(&headroom.smooth_menu);
    // "Pre", not "Preamp": the row is width-tight and the caption sits in
    // front of a spin button that is already ~165px wide. The tooltip carries
    // the full name.
    let preamp_item = fixed_cell(
        CELL_W_PREAMP,
        Some(("Pre", "Output preamp trim (dB)")),
        &headroom.preamp_spin,
        &mut labels,
    );

    // Monitor settings, then Smooth: both change how editing feels, and the
    // dropdown leads them.
    row.insert(&smooth_item, -1);
    row.insert(&preamp_item, -1);

    // Status cell: LED + numeric peak, grouped so they never separate on wrap.
    let status_cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    status_cell.set_size_request(CELL_W_STATUS, -1);
    status_cell.set_halign(gtk4::Align::Start);
    status_cell.set_valign(gtk4::Align::Center);
    status_cell.append(&headroom.led_area);
    headroom.peak_label.set_valign(gtk4::Align::Center);
    status_cell.append(&headroom.peak_label);
    row.insert(&status_cell, -1);

    // The clipping group: [Clip] [Fix] [Auto], one cell at the end of the row.
    //
    // `Fix` and `Auto` are two answers to the same question -- what to do about
    // a peak over the target -- so they belong together rather than at opposite
    // ends of the row, and the single caption replaces the two the cells used to
    // carry. It is also narrower than the two cells it replaces, which is what
    // the row needs at the minimum window width.
    //
    // "Auto" keeps a label of its own because a bare switch says nothing; it
    // carries the same metric-title class as the captions, so the compaction
    // tiers drop it exactly like them and leave the switch with its tooltip.
    let clip_cell = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    clip_cell.set_size_request(CELL_W_CLIP, -1);
    clip_cell.set_halign(gtk4::Align::Start);
    clip_cell.set_valign(gtk4::Align::Center);
    clip_cell.set_tooltip_text(Some(
        "Fix trims the preamp by hand; Auto lets it follow the peak",
    ));
    let clip_label = gtk4::Label::new(Some("Clip"));
    clip_label.set_valign(gtk4::Align::Center);
    clip_label.set_css_classes(&["metric-title"]);
    clip_cell.append(&clip_label);
    labels.push(clip_label);
    headroom.set_safe_button.set_valign(gtk4::Align::Center);
    clip_cell.append(&headroom.set_safe_button);
    headroom.auto_safe_button.set_valign(gtk4::Align::Center);
    clip_cell.append(&headroom.auto_safe_button);
    row.insert(&clip_cell, -1);

    // The preamp stays exactly where it is while Auto-Safe owns it and is
    // merely insensitive. Hiding it removed its content from the cell, and
    // even with the cell floor held the row's centred layout shifted as the
    // remaining items re-flowed. Disabling keeps every pixel put.
    {
        let preamp = headroom.preamp_spin.clone();
        headroom.auto_safe_button.connect_toggled(move |btn| {
            preamp.set_sensitive(!btn.is_active());
        });
    }
    headroom
        .preamp_spin
        .set_sensitive(!headroom.auto_safe.get());

    row
}

/// A fixed-width cell in the output row. The width is reserved whether or
/// not the caption is showing and whether or not the inner control is
/// visible, which is what keeps the row from reflowing.
fn fixed_cell(
    width: i32,
    caption: Option<(&str, &str)>,
    widget: &impl IsA<gtk4::Widget>,
    labels: &mut Vec<gtk4::Label>,
) -> gtk4::Box {
    let box_ = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
    box_.set_size_request(width, -1);
    box_.set_halign(gtk4::Align::Start);
    box_.set_valign(gtk4::Align::Center);
    if let Some((text, tooltip)) = caption {
        box_.set_tooltip_text(Some(tooltip));
        let label = gtk4::Label::new(Some(text));
        label.set_valign(gtk4::Align::Center);
        label.set_css_classes(&["metric-title"]);
        box_.append(&label);
        labels.push(label);
    }
    box_.append(widget);
    box_
}

/// Build the main content layout with left panel (band faders) and right panel (utility).
pub fn build_main_layout(
    utility: &UtilityPane,
    editor: &crate::window_band_editor::BandEditor,
    visible_bands: usize,
    gain_changed: Rc<dyn Fn(usize, f64) -> f64>,
    selection_changed_callback: Rc<dyn Fn(usize)>,
) -> (
    adw::OverlaySplitView,
    gtk4::ScrolledWindow,
    Vec<Rc<RefCell<crate::band_fader::EqBandFader>>>,
) {
    let main_box = gtk4::Box::new(gtk4::Orientation::Vertical, 4);

    main_box.append(utility.graph.borrow().widget());

    // Output control row: the headroom/bypass controls the user reaches for
    // constantly, on ONE row between the spectrum and the faders. These
    // widgets stay owned by HeadroomPanel/UtilityPane and are reparented
    // here, which frees the sidebar to be a pure Output (device) panel.
    let control_row = build_output_control_row(utility);
    main_box.append(&control_row);

    let (band_scrolled, faders) =
        build_band_faders(visible_bands, gain_changed, selection_changed_callback);
    main_box.append(&band_scrolled);

    main_box.append(editor.widget());

    let split_view = adw::OverlaySplitView::new();
    split_view.set_content(Some(&main_box));
    split_view.set_sidebar(Some(&utility.container));
    // Overlay mode: collapsed=TRUE means the sidebar is shown as an OVERLAY
    // above the content, so the main content is ALWAYS full width. The panel
    // never squeezes the content. Visibility is driven by `show-sidebar`.
    split_view.set_collapsed(true);
    // We control visibility explicitly (via the toggle / F9); don't let the
    // split view change it on its own.
    split_view.set_pin_sidebar(true);
    // Hidden by default: main content uses the full window width.
    split_view.set_show_sidebar(false);
    // Keep the utility panel on the RIGHT at every window size.
    split_view.set_sidebar_position(gtk4::PackType::End);
    split_view.set_sidebar_width_fraction(0.32);
    split_view.set_min_sidebar_width(300.0);
    split_view.set_max_sidebar_width(440.0);

    (split_view, band_scrolled, faders)
}
