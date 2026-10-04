//! Custom EQ band fader widget (Cairo-rendered slider).

use std::cell::RefCell;
use std::f64::consts::PI;
use std::rc::Rc;

use gio::glib;
use gtk4::cairo::{Context, FontSlant, FontWeight, LinearGradient};
use gtk4::prelude::*;

use crate::core::{EQ_GAIN_MAX_DB, EQ_GAIN_MIN_DB, FilterType};

const CONTENT_H: i32 = 182;
/// Minimum fader width. The fader is horizontally responsive: it grows to
/// fill the available width (via hexpand + a homogeneous row) and can shrink
/// down to this, so the window doesn't need to be as wide as the full
/// fixed-width row to avoid horizontal cutting.
const FADER_MIN_W: i32 = 44;
const GAIN_STEP_DB: f64 = 0.5;
const GAIN_FINE_STEP_DB: f64 = 0.1;
const GAIN_COARSE_STEP_DB: f64 = 3.0;
const GAIN_PAGE_STEP_DB: f64 = 3.0;
const GAIN_DRAG_FINE_MULTIPLIER: f64 = 0.20;
const GAIN_DRAG_COARSE_MULTIPLIER: f64 = 2.0;
const CLICK_MAX_TRAVEL_PX: f64 = 5.0;
const FADER_DRAG_START_THRESHOLD_PX: f64 = 2.0;
const TICK_GAINS: [f64; 5] = [-24.0, -12.0, 0.0, 12.0, 24.0];
const TICK_ZERO_GAIN: f64 = 0.0;
const TICK_INNER_OFFSET_PX: f64 = 9.0;
const TICK_MINOR_OUTER_OFFSET_PX: f64 = 14.0;
const TICK_ZERO_OUTER_OFFSET_PX: f64 = 20.0;
const TICK_MINOR_LINE_WIDTH: f64 = 1.0;
const TICK_ZERO_LINE_WIDTH: f64 = 1.15;
const DARK_TICK_ZERO_ALPHA: f64 = 0.42;
const DARK_TICK_MINOR_ALPHA: f64 = 0.20;
const LIGHT_TICK_ZERO_ALPHA: f64 = 0.46;
const LIGHT_TICK_MINOR_ALPHA: f64 = 0.32;

const FOCUS_BLUE: (f64, f64, f64) = (0.47, 0.72, 1.0);

/// A single EQ band fader control.
pub struct EqBandFader {
    pub container: gtk4::Box,
    pub drawing_area: gtk4::DrawingArea,
    pub index: usize,
    pub gain_db: f64,
    pub frequency: f64,
    pub frequency_label: String,
    pub q_value: f64,
    pub q_label: String,
    pub filter_type: FilterType,
    pub filter_type_label: String,
    pub selected: bool,
    pub active: bool,
    pub muted: bool,
    pub soloed: bool,
    pub solo_active: bool,
    pub hovered: bool,
    pub focused: bool,
    /// Set while the global Smooth override is active: the band's own filter
    /// type is replaced by the internal `Sin` wide bell, so the fader shows
    /// "Sin" instead of the stored type. The stored type is untouched and
    /// reappears when Smooth is switched off.
    pub smooth_override: bool,
    pub drag_start_gain_db: f64,
    pub dragging_gain: bool,
    /// Owner-authoritative gain request. The fader does NOT decide its own
    /// gain: it asks the owner what gain it may actually use, and the owner
    /// may clamp the request (peak safety). The RETURNED value, not the
    /// requested one, is what gets applied.
    ///
    /// Stored as an `Rc` so a handler can clone it out and call it WITHOUT
    /// holding a borrow on this fader — the owner re-enters the fader to
    /// apply the result, which would panic on a nested `borrow_mut`.
    pub gain_changed_callback: Option<Rc<dyn Fn(usize, f64) -> f64>>,
    /// Invoked when the user selects this band. The owner clears the other
    /// faders and mirrors the selection into the response graph, so this is a
    /// request rather than a direct `selected` mutation. Stored as an `Rc` so
    /// the handler can clone it out and invoke it without holding a borrow on
    /// this fader (the owner re-enters to set `selected`).
    pub selection_changed_callback: Option<Rc<dyn Fn(usize)>>,
}

impl EqBandFader {
    pub fn new(
        index: usize,
        gain_changed_callback: Rc<dyn Fn(usize, f64) -> f64>,
        selection_changed_callback: Rc<dyn Fn(usize)>,
    ) -> Rc<RefCell<Self>> {
        let fader = Rc::new(RefCell::new(Self {
            container: gtk4::Box::new(gtk4::Orientation::Vertical, 0),
            drawing_area: gtk4::DrawingArea::new(),
            index,
            gain_db: 0.0,
            frequency: 1000.0,
            frequency_label: "".into(),
            q_value: 0.7,
            q_label: "".into(),
            filter_type: FilterType::Bell,
            filter_type_label: "".into(),
            selected: false,
            active: true,
            muted: false,
            soloed: false,
            solo_active: false,
            hovered: false,
            focused: false,
            smooth_override: false,
            drag_start_gain_db: 0.0,
            dragging_gain: false,
            gain_changed_callback: Some(gain_changed_callback),
            selection_changed_callback: Some(selection_changed_callback),
        }));

        {
            let f = fader.borrow();
            f.container.set_css_classes(&["eq-band-box"]);
            // Responsive width: min FADER_MIN_W, expands to fill (set in the
            // homogeneous band row). Height stays fixed-ish via CONTENT_H.
            f.container.set_size_request(FADER_MIN_W, CONTENT_H);
            f.container.set_hexpand(true);

            let drawing_area = &f.drawing_area;
            // content_width is the drawing area's MINIMUM width request; keep it
            // at FADER_MIN_W so the fader can actually shrink to fit a narrow
            // window. The draw func uses the real allocated width, so it scales.
            drawing_area.set_content_width(FADER_MIN_W);
            drawing_area.set_content_height(CONTENT_H);
            drawing_area.set_hexpand(true);
            drawing_area.set_vexpand(true);
            drawing_area.set_focusable(true);
            drawing_area.set_tooltip_text(Some("Band Gain"));
        }

        // Hover highlight + resize cursor, matching upstream
        // `band_fader.py` (`on_motion_enter`/`on_motion_leave`,
        // `set_cursor_from_name("ns-resize")`). `hovered` was read by the draw
        // function but nothing ever set it: no motion controller was attached,
        // so the highlight could never appear.
        {
            let enter_fader = fader.clone();
            let enter = gtk4::EventControllerMotion::new();
            enter.connect_enter(move |_, _x, _y| {
                let mut f = enter_fader.borrow_mut();
                if f.hovered {
                    return;
                }
                f.hovered = true;
                f.drawing_area.queue_draw();
            });
            fader.borrow().drawing_area.add_controller(enter);

            let leave_fader = fader.clone();
            let leave = gtk4::EventControllerMotion::new();
            leave.connect_leave(move |_| {
                let mut f = leave_fader.borrow_mut();
                if !f.hovered {
                    return;
                }
                f.hovered = false;
                f.drawing_area.queue_draw();
            });
            fader.borrow().drawing_area.add_controller(leave);

            if let Some(cursor) = gtk4::gdk::Cursor::from_name("ns-resize", None) {
                fader.borrow().drawing_area.set_cursor(Some(&cursor));
            }
        }

        // Drag gesture for gain adjustment
        {
            let f1 = fader.clone();
            let f2 = fader.clone();
            let f3 = fader.clone();
            let drag = gtk4::GestureDrag::new();
            drag.connect_begin(move |_gesture, _event_seq| {
                let mut f = f1.borrow_mut();
                f.drag_start_gain_db = f.gain_db;
                f.drawing_area.queue_draw();
            });
            drag.connect_drag_update(move |gesture, _offset_x, offset_y| {
                // Snapshot what we need and RELEASE the borrow before asking
                // the owner, which re-enters this fader.
                let (index, cb, start_gain, height) = {
                    let mut f = f2.borrow_mut();
                    if !f.dragging_gain {
                        let start = gesture.start_point();
                        if start.is_none() {
                            return;
                        }
                        let off = gesture.offset();
                        if off.is_none() {
                            return;
                        }
                        let (ox, oy) = off.unwrap();
                        if f64::hypot(ox, oy) < FADER_DRAG_START_THRESHOLD_PX {
                            return;
                        }
                        f.dragging_gain = true;
                    }
                    (
                        f.index,
                        f.gain_changed_callback.clone(),
                        f.drag_start_gain_db,
                        f.drawing_area.allocated_height() as f64,
                    )
                };
                let state = gesture.current_event_state();
                let multiplier = interaction_multiplier_for_state(state);
                let (_track_top, track_bottom) = track_bounds(height);
                let usable_height = (track_bottom - 56.0).max(1.0);
                let gain = start_gain
                    - (offset_y / usable_height) * (EQ_GAIN_MAX_DB - EQ_GAIN_MIN_DB) * multiplier;
                let gain = (gain.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB) * 10.0).round() / 10.0;
                let effective = apply_gain_request(&cb, index, gain);
                let mut f = f2.borrow_mut();
                if effective != f.gain_db {
                    f.gain_db = effective;
                    f.drawing_area.queue_draw();
                }
            });
            drag.connect_end(move |_gesture, _event_seq| {
                let mut f = f3.borrow_mut();
                f.dragging_gain = false;
                f.drawing_area.queue_draw();
            });
            fader.borrow().drawing_area.add_controller(drag);
        }

        // Scroll wheel
        {
            let fader_clone = fader.clone();
            let scroll =
                gtk4::EventControllerScroll::new(gtk4::EventControllerScrollFlags::VERTICAL);
            scroll.connect_scroll(move |_controller, _dx, dy| {
                if dy == 0.0 {
                    return glib::Propagation::Proceed;
                }
                let state = _controller.current_event_state();
                let (index, cb, current) = {
                    let f = fader_clone.borrow();
                    (f.index, f.gain_changed_callback.clone(), f.gain_db)
                };
                let step = direct_step_for_state(state);
                let delta = if dy < 0.0 { step } else { -step };
                let gain = (current + delta).clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
                let gain = (gain * 10.0).round() / 10.0;
                let effective = apply_gain_request(&cb, index, gain);
                let mut f = fader_clone.borrow_mut();
                if effective != f.gain_db {
                    f.gain_db = effective;
                    f.drawing_area.queue_draw();
                }
                glib::Propagation::Proceed
            });
            fader.borrow().drawing_area.add_controller(scroll);
        }

        // Click to select
        {
            let fader_clone = fader.clone();
            let click = gtk4::GestureClick::new();
            // Selection is a TOGGLE, so it must fire on a genuine click
            // only. GestureClick::pressed fires the moment the button goes
            // down -- before we know whether the user is dragging -- which
            // meant that grabbing an already-selected fader to move it
            // deselected it first, making the editor vanish on the second
            // drag. Record the press point and decide on release instead.
            let press_xy: Rc<RefCell<Option<(f64, f64)>>> = Rc::new(RefCell::new(None));
            let press_pressed = press_xy.clone();
            click.connect_pressed(move |_gesture, _press_count, x, y| {
                *press_pressed.borrow_mut() = Some((x, y));
            });
            let press_released = press_xy.clone();
            click.connect_released(move |_gesture, _press_count, x, y| {
                let Some((px, py)) = press_released.borrow_mut().take() else {
                    return;
                };
                // Anything past this distance was a drag, not a click.
                if f64::hypot(x - px, y - py) > CLICK_MAX_TRAVEL_PX {
                    return;
                }
                // Also suppress if the drag gesture actually consumed a move,
                // in case the two gestures disagree on the threshold.
                let dragged = fader_clone.borrow().dragging_gain;
                if dragged {
                    return;
                }
                let (index, select) = {
                    let f = fader_clone.borrow();
                    (f.index, f.selection_changed_callback.clone())
                };
                if let Some(cb) = select {
                    cb(index);
                }
            });
            let press_cancel = press_xy.clone();
            click.connect_cancel(move |_, _| {
                press_cancel.borrow_mut().take();
            });
            fader.borrow().drawing_area.add_controller(click);
        }

        // Focus
        {
            let f1 = fader.clone();
            let f2 = fader.clone();
            let focus = gtk4::EventControllerFocus::new();
            focus.connect_enter(move |_controller| {
                let mut f = f1.borrow_mut();
                f.focused = true;
                f.drawing_area.queue_draw();
            });
            focus.connect_leave(move |_controller| {
                let mut f = f2.borrow_mut();
                f.focused = false;
                f.drawing_area.queue_draw();
            });
            fader.borrow().drawing_area.add_controller(focus);
        }

        // Keyboard
        {
            let fader_clone = fader.clone();
            let key = gtk4::EventControllerKey::new();
            key.connect_key_pressed(move |_controller, key, _keycode, state| {
                // Read-only: every gain mutation goes through the owner so the
                // peak cap is applied uniformly.
                let f = fader_clone.borrow();
                let step = direct_step_for_state(state);
                let mut delta: Option<f64> = None;
                match key {
                    gtk4::gdk::Key::Up
                    | gtk4::gdk::Key::KP_Up
                    | gtk4::gdk::Key::Right
                    | gtk4::gdk::Key::KP_Right => delta = Some(step),
                    gtk4::gdk::Key::Down
                    | gtk4::gdk::Key::KP_Down
                    | gtk4::gdk::Key::Left
                    | gtk4::gdk::Key::KP_Left => delta = Some(-step),
                    gtk4::gdk::Key::Page_Up | gtk4::gdk::Key::KP_Page_Up => {
                        delta = Some(GAIN_PAGE_STEP_DB)
                    }
                    gtk4::gdk::Key::Page_Down | gtk4::gdk::Key::KP_Page_Down => {
                        delta = Some(-GAIN_PAGE_STEP_DB)
                    }
                    _ => {}
                }

                if let Some(d) = delta {
                    let (index, cb, current) =
                        (f.index, f.gain_changed_callback.clone(), f.gain_db);
                    drop(f);
                    let gain = (current + d).clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
                    let gain = (gain * 10.0).round() / 10.0;
                    let effective = apply_gain_request(&cb, index, gain);
                    let mut f = fader_clone.borrow_mut();
                    if effective != f.gain_db {
                        f.gain_db = effective;
                        f.drawing_area.queue_draw();
                    }
                    return glib::Propagation::Proceed;
                }

                match key {
                    gtk4::gdk::Key::_0 | gtk4::gdk::Key::KP_0 | gtk4::gdk::Key::Home => {
                        // Release the borrow before the owner re-enters us.
                        let (index, cb, was_nonzero) =
                            (f.index, f.gain_changed_callback.clone(), f.gain_db != 0.0);
                        let select = f.selection_changed_callback.clone();
                        drop(f);
                        if was_nonzero {
                            let effective = apply_gain_request(&cb, index, 0.0);
                            let mut f = fader_clone.borrow_mut();
                            if f.gain_db != effective {
                                f.gain_db = effective;
                                f.drawing_area.queue_draw();
                            }
                        }
                        if let Some(cb) = select {
                            cb(index);
                        }
                        glib::Propagation::Proceed
                    }
                    gtk4::gdk::Key::Return | gtk4::gdk::Key::KP_Enter | gtk4::gdk::Key::space => {
                        let (index, select) = (f.index, f.selection_changed_callback.clone());
                        drop(f);
                        if let Some(cb) = select {
                            cb(index);
                        }
                        glib::Propagation::Proceed
                    }
                    _ => glib::Propagation::Proceed,
                }
            });
            fader.borrow().drawing_area.add_controller(key);
        }

        // Set the draw function
        {
            let fader_clone = fader.clone();
            fader
                .borrow()
                .drawing_area
                .set_draw_func(move |_area, ctx, width, height| {
                    draw_fader(ctx, width, height, &fader_clone.borrow());
                });
        }

        {
            let f = fader.borrow();
            f.container.append(&f.drawing_area);
        }

        fader
    }

    pub fn set_band_state(
        &mut self,
        gain: f64,
        frequency: f64,
        frequency_label: String,
        q: f64,
        q_label: String,
        filter_type: FilterType,
        filter_type_label: String,
        selected: bool,
        active: bool,
        muted: bool,
        soloed: bool,
        solo_active: bool,
    ) {
        let changed = self.gain_db != gain
            || self.frequency != frequency
            || self.frequency_label != frequency_label
            || self.q_value != q
            || self.q_label != q_label
            || self.filter_type != filter_type
            || self.filter_type_label != filter_type_label
            || self.selected != selected
            || self.active != active
            || self.muted != muted
            || self.soloed != soloed
            || self.solo_active != solo_active;
        self.gain_db = gain.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
        self.frequency = frequency;
        self.frequency_label = frequency_label;
        self.q_value = q;
        self.q_label = q_label;
        self.filter_type = filter_type;
        self.filter_type_label = filter_type_label;
        self.selected = selected;
        self.active = active;
        self.muted = muted;
        self.soloed = soloed;
        self.solo_active = solo_active;
        if changed {
            self.drawing_area.queue_draw();
        }
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.container
    }

    pub fn set_height(&self, height: i32) {
        // Only change the HEIGHT here. Keep the width at the responsive
        // FADER_MIN_W so the compact breakpoint (fired when the window is
        // narrowed) doesn't reset the fader to the fixed CONTENT_W=72 and
        // force the row wider than the window.
        self.container.set_size_request(FADER_MIN_W, height);
        self.drawing_area.set_content_height(height);
        self.drawing_area.queue_draw();
    }
}

fn track_bounds(height: f64) -> (f64, f64) {
    let track_top = 56.0_f64;
    let minimum_track_length = 42.0_f64;
    let bottom_margin = if height >= 170.0 { 44.0_f64 } else { 32.0_f64 };
    let track_bottom = (track_top + minimum_track_length).max(height - bottom_margin);
    (track_top, track_bottom)
}

fn interaction_multiplier_for_state(state: gtk4::gdk::ModifierType) -> f64 {
    if state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        return GAIN_DRAG_FINE_MULTIPLIER;
    }
    if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        return GAIN_DRAG_COARSE_MULTIPLIER;
    }
    1.0
}

fn direct_step_for_state(state: gtk4::gdk::ModifierType) -> f64 {
    if state.contains(gtk4::gdk::ModifierType::SHIFT_MASK) {
        return GAIN_FINE_STEP_DB;
    }
    if state.contains(gtk4::gdk::ModifierType::CONTROL_MASK) {
        return GAIN_COARSE_STEP_DB;
    }
    GAIN_STEP_DB
}

/// Route a gain request through the owner and return the gain the fader may
/// actually use. With no owner installed the request is only range-clamped.
///
/// Every gain gesture (drag, scroll, keyboard, zero) MUST go through this so
/// the owner's peak safety applies uniformly. A fader that set its own gain
/// locally would bypass the clamp and let stacked shelves run past what the
/// preamp floor can absorb.
fn apply_gain_request(
    cb: &Option<Rc<dyn Fn(usize, f64) -> f64>>,
    index: usize,
    requested: f64,
) -> f64 {
    match cb {
        Some(cb) => cb(index, requested),
        None => requested.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB),
    }
}

fn rounded_rectangle(ctx: &Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let right = x + width;
    let bottom = y + height;
    let radius = radius.min(width / 2.0).min(height / 2.0);
    ctx.new_sub_path();
    ctx.arc(right - radius, y + radius, radius, -PI / 2.0, 0.0);
    ctx.arc(right - radius, bottom - radius, radius, 0.0, PI / 2.0);
    ctx.arc(x + radius, bottom - radius, radius, PI / 2.0, PI);
    ctx.arc(x + radius, y + radius, radius, PI, 3.0 * PI / 2.0);
    ctx.close_path();
}

fn draw_text(
    ctx: &Context,
    text: &str,
    x: f64,
    y: f64,
    size: f64,
    color: (f64, f64, f64),
    bold: bool,
    center: bool,
) {
    ctx.select_font_face(
        "Sans",
        if bold {
            FontSlant::Normal
        } else {
            FontSlant::Normal
        },
        if bold {
            FontWeight::Bold
        } else {
            FontWeight::Normal
        },
    );
    ctx.set_font_size(size);
    let extents = ctx.text_extents(text).unwrap();
    let text_x = if center {
        x - (extents.width() / 2.0) - extents.x_bearing()
    } else {
        x
    };
    ctx.set_source_rgb(color.0, color.1, color.2);
    ctx.move_to(text_x, y);
    ctx.show_text(text).unwrap();
}

fn draw_state_badge(
    ctx: &Context,
    label: &str,
    x: f64,
    y: f64,
    width: f64,
    color: (f64, f64, f64),
    alpha: f64,
) {
    rounded_rectangle(ctx, x, y, width, 15.0, 6.5);
    ctx.set_source_rgba(color.0, color.1, color.2, 0.22 * alpha);
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(color.0, color.1, color.2, 0.50 * alpha);
    ctx.set_line_width(1.0);
    let _ = ctx.stroke();
    draw_text(
        ctx,
        label,
        x + (width / 2.0),
        y + 10.7,
        7.8,
        (0.94, 0.97, 1.0),
        true,
        true,
    );
}

pub fn filter_type_short_label(ft: FilterType) -> &'static str {
    match ft {
        FilterType::Off => "Off",
        FilterType::Bell => "Bell",
        FilterType::HiPass => "HP",
        FilterType::HiShelf => "HS",
        FilterType::LoPass => "LP",
        FilterType::LoShelf => "LS",
        FilterType::Notch => "Notch",
        FilterType::Allpass => "AP",
        FilterType::Bandpass => "BP",
        FilterType::Resonance => "Res",
        FilterType::LadderPass => "LdP",
        FilterType::LadderRej => "LdR",
        FilterType::Sin => "Sin",
    }
}

fn draw_fader(ctx: &Context, width: i32, height: i32, fader: &EqBandFader) {
    let width_f = width as f64;
    let height_f = height as f64;
    let center_x = width_f / 2.0;

    let effective = fader.active && !fader.muted && (!fader.solo_active || fader.soloed);
    let alpha = if effective { 1.0 } else { 0.48 };
    let engaged = fader.selected || fader.hovered || fader.focused || fader.dragging_gain;

    let dark = true;
    let (engaged_fill_rgb, selected_fill_alpha, hover_fill_alpha) = if dark {
        ((1.0, 1.0, 1.0), 0.026, 0.045)
    } else {
        ((0.0, 0.0, 0.0), 0.030, 0.045)
    };
    let (
        text_main,
        text_type_selected,
        text_type,
        text_disabled,
        gain_color_selected,
        gain_color_normal,
        gain_color_disabled,
        track_shadow,
        track_gradient_colors,
        selected_fill_gradient_colors,
        fill_gradient_colors,
        tick_color,
        tick_zero_alpha,
        tick_minor_alpha,
        knob_shadow,
        knob_selected,
        knob_normal,
        knob_border,
        knob_highlight,
        overview_freq,
        overview_freq_disabled,
        q_text,
        q_text_disabled,
    ) = if dark {
        (
            (0.82, 0.86, 0.90),
            (0.72, 0.78, 0.84),
            (0.66, 0.72, 0.78),
            (0.50, 0.56, 0.62),
            (0.91, 0.95, 0.99),
            (0.90, 0.94, 0.98),
            (0.62, 0.68, 0.74),
            (0.02, 0.03, 0.045, 0.42),
            ((0.20, 0.26, 0.34, 0.82), (0.10, 0.14, 0.20, 0.82)),
            ((0.56, 0.69, 0.81, 0.56), (0.38, 0.51, 0.64, 0.56)),
            ((0.58, 0.68, 0.78, 0.52), (0.38, 0.48, 0.60, 0.52)),
            (0.82, 0.88, 0.94),
            DARK_TICK_ZERO_ALPHA,
            DARK_TICK_MINOR_ALPHA,
            (0.0, 0.0, 0.0, 0.28),
            (0.54, 0.72, 0.90),
            (0.70, 0.77, 0.84),
            (0.0, 0.0, 0.0, 0.28),
            (1.0, 1.0, 1.0, 0.24),
            (0.76, 0.81, 0.86),
            (0.54, 0.59, 0.64),
            (0.60, 0.66, 0.72),
            (0.46, 0.52, 0.58),
        )
    } else {
        (
            (0.15, 0.20, 0.25),
            (0.18, 0.27, 0.36),
            (0.28, 0.35, 0.42),
            (0.52, 0.58, 0.64),
            (0.12, 0.18, 0.24),
            (0.20, 0.27, 0.34),
            (0.58, 0.63, 0.68),
            (0.0, 0.0, 0.0, 0.20),
            ((0.54, 0.64, 0.74, 0.94), (0.32, 0.43, 0.55, 0.94)),
            ((0.16, 0.45, 0.72, 0.84), (0.08, 0.30, 0.50, 0.84)),
            ((0.24, 0.43, 0.60, 0.76), (0.14, 0.28, 0.44, 0.76)),
            (0.13, 0.19, 0.26),
            LIGHT_TICK_ZERO_ALPHA,
            LIGHT_TICK_MINOR_ALPHA,
            (0.0, 0.0, 0.0, 0.18),
            (0.36, 0.61, 0.84),
            (0.52, 0.64, 0.75),
            (0.0, 0.0, 0.0, 0.20),
            (1.0, 1.0, 1.0, 0.36),
            (0.20, 0.28, 0.36),
            (0.60, 0.65, 0.70),
            (0.34, 0.42, 0.50),
            (0.66, 0.70, 0.74),
        )
    };

    if engaged {
        rounded_rectangle(ctx, 2.0, 2.0, width_f - 4.0, height_f - 4.0, 15.0);
        if fader.selected {
            ctx.set_source_rgba(
                engaged_fill_rgb.0,
                engaged_fill_rgb.1,
                engaged_fill_rgb.2,
                selected_fill_alpha * alpha,
            );
        } else {
            ctx.set_source_rgba(
                engaged_fill_rgb.0,
                engaged_fill_rgb.1,
                engaged_fill_rgb.2,
                hover_fill_alpha * alpha,
            );
        }
        let _ = ctx.fill_preserve();
        let mut border_alpha: f64 = if fader.selected { 0.30 } else { 0.15 };
        if fader.focused {
            border_alpha = border_alpha.max(0.34);
        }
        if fader.selected {
            ctx.set_source_rgba(
                FOCUS_BLUE.0,
                FOCUS_BLUE.1,
                FOCUS_BLUE.2,
                border_alpha * alpha,
            );
        } else {
            ctx.set_source_rgba(0.82, 0.88, 0.94, border_alpha * alpha);
        }
        ctx.set_line_width(1.0);
        let _ = ctx.stroke();
    }

    draw_text(
        ctx,
        &(fader.index + 1).to_string(),
        center_x,
        15.0,
        10.0,
        text_main,
        true,
        true,
    );

    let type_color = if fader.selected {
        text_type_selected
    } else {
        text_type
    };
    let type_color = if !fader.active {
        text_disabled
    } else {
        type_color
    };
    let filter_text = if fader.smooth_override {
        "Sin"
    } else {
        filter_type_short_label(fader.filter_type)
    };
    draw_text(
        ctx,
        filter_text,
        center_x,
        29.5,
        9.0,
        type_color,
        true,
        true,
    );

    let gain_label = format!("{:+.1} dB", fader.gain_db);
    let gain_width = 60.0;
    rounded_rectangle(
        ctx,
        center_x - gain_width / 2.0,
        35.0,
        gain_width,
        18.0,
        8.0,
    );
    if fader.selected {
        ctx.set_source_rgba(
            engaged_fill_rgb.0,
            engaged_fill_rgb.1,
            engaged_fill_rgb.2,
            0.08 * alpha,
        );
    } else {
        ctx.set_source_rgba(
            engaged_fill_rgb.0,
            engaged_fill_rgb.1,
            engaged_fill_rgb.2,
            0.07 * alpha,
        );
    }
    let _ = ctx.fill();
    let gain_color = if fader.selected {
        gain_color_selected
    } else {
        gain_color_normal
    };
    let gain_color = if !fader.active {
        gain_color_disabled
    } else {
        gain_color
    };
    draw_text(
        ctx,
        &gain_label,
        center_x,
        48.1,
        9.3,
        gain_color,
        true,
        true,
    );

    let (track_top, track_bottom) = track_bounds(height_f);
    let track_x = center_x - 3.5;
    let track_width = 7.0;
    let gain_range = EQ_GAIN_MAX_DB - EQ_GAIN_MIN_DB;
    let normalized = (fader.gain_db - EQ_GAIN_MIN_DB) / gain_range;
    let knob_y = track_bottom - ((track_bottom - track_top) * normalized.clamp(0.0, 1.0));
    let zero_y =
        track_bottom - ((track_bottom - track_top) * ((0.0 - EQ_GAIN_MIN_DB) / gain_range));

    rounded_rectangle(
        ctx,
        track_x - 2.0,
        track_top - 1.0,
        track_width + 4.0,
        track_bottom - track_top + 2.0,
        6.0,
    );
    ctx.set_source_rgba(
        track_shadow.0,
        track_shadow.1,
        track_shadow.2,
        track_shadow.3 * alpha,
    );
    let _ = ctx.fill();

    {
        let track_grad = LinearGradient::new(0.0, track_top, 0.0, track_bottom);
        track_grad.add_color_stop_rgba(
            0.0,
            track_gradient_colors.0.0,
            track_gradient_colors.0.1,
            track_gradient_colors.0.2,
            track_gradient_colors.0.3 * alpha,
        );
        track_grad.add_color_stop_rgba(
            1.0,
            track_gradient_colors.1.0,
            track_gradient_colors.1.1,
            track_gradient_colors.1.2,
            track_gradient_colors.1.3 * alpha,
        );
        let _ = ctx.set_source(&track_grad);
    }
    rounded_rectangle(
        ctx,
        track_x,
        track_top,
        track_width,
        track_bottom - track_top,
        3.5,
    );
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(
        track_shadow.0,
        track_shadow.1,
        track_shadow.2,
        track_shadow.3 * alpha,
    );
    ctx.set_line_width(1.0);
    let _ = ctx.stroke();

    let fill_top = knob_y.min(zero_y);
    let fill_bottom = knob_y.max(zero_y);
    let fill_bottom = if fill_bottom - fill_top < 2.0 {
        fill_top + 2.0
    } else {
        fill_bottom
    };
    rounded_rectangle(
        ctx,
        track_x,
        fill_top,
        track_width,
        fill_bottom - fill_top,
        4.0,
    );
    {
        let fill_grad = LinearGradient::new(0.0, fill_top, 0.0, fill_bottom);
        if fader.selected || fader.dragging_gain {
            fill_grad.add_color_stop_rgba(
                0.0,
                selected_fill_gradient_colors.0.0,
                selected_fill_gradient_colors.0.1,
                selected_fill_gradient_colors.0.2,
                selected_fill_gradient_colors.0.3 * alpha,
            );
            fill_grad.add_color_stop_rgba(
                1.0,
                selected_fill_gradient_colors.1.0,
                selected_fill_gradient_colors.1.1,
                selected_fill_gradient_colors.1.2,
                selected_fill_gradient_colors.1.3 * alpha,
            );
        } else {
            fill_grad.add_color_stop_rgba(
                0.0,
                fill_gradient_colors.0.0,
                fill_gradient_colors.0.1,
                fill_gradient_colors.0.2,
                fill_gradient_colors.0.3 * alpha,
            );
            fill_grad.add_color_stop_rgba(
                1.0,
                fill_gradient_colors.1.0,
                fill_gradient_colors.1.1,
                fill_gradient_colors.1.2,
                fill_gradient_colors.1.3 * alpha,
            );
        }
        let _ = ctx.set_source(&fill_grad);
    }
    let _ = ctx.fill_preserve();
    ctx.set_line_width(1.0);
    let _ = ctx.stroke();

    for tick_gain in TICK_GAINS.iter().copied() {
        let is_zero_tick = tick_gain == TICK_ZERO_GAIN;
        let tick_y = track_bottom
            - ((track_bottom - track_top) * ((tick_gain - EQ_GAIN_MIN_DB) / gain_range));
        let tick_alpha = if is_zero_tick {
            tick_zero_alpha
        } else {
            tick_minor_alpha
        };
        ctx.set_source_rgba(tick_color.0, tick_color.1, tick_color.2, tick_alpha * alpha);
        ctx.set_line_width(if is_zero_tick {
            TICK_ZERO_LINE_WIDTH
        } else {
            TICK_MINOR_LINE_WIDTH
        });
        ctx.move_to(center_x + TICK_INNER_OFFSET_PX, tick_y);
        let outer_offset = if is_zero_tick {
            TICK_ZERO_OUTER_OFFSET_PX
        } else {
            TICK_MINOR_OUTER_OFFSET_PX
        };
        ctx.line_to(center_x + outer_offset, tick_y);
        let _ = ctx.stroke();
        if is_zero_tick {
            ctx.move_to(center_x - TICK_ZERO_OUTER_OFFSET_PX, tick_y);
            ctx.line_to(center_x - TICK_INNER_OFFSET_PX, tick_y);
            let _ = ctx.stroke();
        }
    }

    let knob_width = if fader.selected || fader.dragging_gain {
        26.0
    } else {
        24.0
    };
    let knob_height = 16.0;
    let knob_x = center_x - (knob_width / 2.0);
    let knob_y_top = knob_y - (knob_height / 2.0);

    rounded_rectangle(
        ctx,
        knob_x + 1.0,
        knob_y_top + 2.0,
        knob_width,
        knob_height,
        5.0,
    );
    ctx.set_source_rgba(
        knob_shadow.0,
        knob_shadow.1,
        knob_shadow.2,
        knob_shadow.3 * alpha,
    );
    let _ = ctx.fill();

    rounded_rectangle(ctx, knob_x, knob_y_top, knob_width, knob_height, 5.0);
    if fader.selected || fader.dragging_gain {
        ctx.set_source_rgba(
            knob_selected.0,
            knob_selected.1,
            knob_selected.2,
            0.98 * alpha,
        );
    } else {
        ctx.set_source_rgba(knob_normal.0, knob_normal.1, knob_normal.2, 0.98 * alpha);
    }
    let _ = ctx.fill_preserve();
    ctx.set_source_rgba(
        knob_border.0,
        knob_border.1,
        knob_border.2,
        knob_border.3 * alpha,
    );
    ctx.set_line_width(1.0);
    let _ = ctx.stroke();
    ctx.set_source_rgba(
        knob_highlight.0,
        knob_highlight.1,
        knob_highlight.2,
        knob_highlight.3 * alpha,
    );
    ctx.set_line_width(1.0);
    ctx.move_to(center_x - 7.0, knob_y);
    ctx.line_to(center_x + 7.0, knob_y);
    let _ = ctx.stroke();

    let overview_freq_color = if fader.active {
        overview_freq
    } else {
        overview_freq_disabled
    };
    if height_f >= 170.0 {
        let q_color = if fader.active {
            q_text
        } else {
            q_text_disabled
        };
        draw_text(
            ctx,
            &fader.frequency_label,
            center_x,
            height_f - 25.0,
            9.0,
            overview_freq_color,
            false,
            true,
        );
        draw_text(
            ctx,
            &fader.q_label,
            center_x,
            height_f - 11.0,
            8.6,
            q_color,
            false,
            true,
        );
    } else {
        draw_text(
            ctx,
            &fader.frequency_label,
            center_x,
            height_f - 13.0,
            9.0,
            overview_freq_color,
            false,
            true,
        );
    }

    let badge_y = 52.0;
    let badge_right = width_f - 8.0;
    if fader.muted && fader.soloed {
        let badge_width = 24.0;
        draw_state_badge(
            ctx,
            "M/S",
            badge_right - badge_width,
            badge_y,
            badge_width,
            (0.78, 0.65, 0.98),
            alpha,
        );
    } else if fader.muted {
        draw_state_badge(
            ctx,
            "M",
            badge_right - 14.0,
            badge_y,
            14.0,
            (0.94, 0.44, 0.44),
            alpha,
        );
    } else if fader.soloed {
        draw_state_badge(
            ctx,
            "S",
            badge_right - 14.0,
            badge_y,
            14.0,
            FOCUS_BLUE,
            alpha,
        );
    }
}
