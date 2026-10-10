//! Window state persistence (size, position).

use gtk4::prelude::*;

/// Smallest the window may be shrunk to. Chosen to stay friendly to low-res
/// displays (classic VGA) while still fitting the graph + faders and the
/// inline utility panel without cutting content.
/// The header toolbar's natural minimum is 644 px (the route switch + the
/// panel buttons + the menu + the title, incl. spacing). A window minimum
/// below that makes AdwToolbarView overflow at the smallest size
/// ("exceeds AdwApplicationWindow width: requested 644 px, 640 px
/// available"). 648 gives 4 px of slack over the toolbar's need.
pub const MIN_WINDOW_WIDTH: i32 = 648;

// Vertical budget, assembled from the real rows so the floor moves when the
// layout changes instead of silently cutting content.
//
// Two rows were added since this value was last set: the graph's switch row
// (Monitor + A/B) and the output control row, which now WRAPS to a second
// line at narrow widths via `adw::WrapBox`. Without raising the floor the
// band editor at the bottom got pushed out of view.
const HEADER_BAR_H: i32 = 46;
const GRAPH_SWITCH_ROW_H: i32 = 34;
const GRAPH_DEFAULT_H: i32 = 196;
/// Two wrapped lines plus the WrapBox inter-line spacing.
const OUTPUT_ROW_WRAPPED_H: i32 = 78;
/// Title bar (36) + the control row of the inline band editor.
const BAND_EDITOR_H: i32 = 70;
/// Margins and inter-box spacing.
const CHROME_H: i32 = 24;
/// Floor for the band fader strip itself, so it stays usable.
const FADERS_MIN_H: i32 = 120;

pub const MIN_WINDOW_HEIGHT: i32 = HEADER_BAR_H
    + GRAPH_SWITCH_ROW_H
    + GRAPH_DEFAULT_H
    + OUTPUT_ROW_WRAPPED_H
    + BAND_EDITOR_H
    + CHROME_H
    + FADERS_MIN_H;

pub fn initial_window_default_size() -> (i32, i32) {
    let display = gtk4::gdk::Display::default();
    if let Some(display) = display {
        let monitors = display.monitors();
        if monitors.n_items() > 0 {
            if let Some(obj) = monitors.item(0) {
                if let Ok(monitor) = obj.downcast::<gtk4::gdk::Monitor>() {
                    let geometry = monitor.geometry();
                    let width = (geometry.width() as f64 * 0.85).round() as i32;
                    let height = (geometry.height() as f64 * 0.85).round() as i32;
                    let width = width.max(MIN_WINDOW_WIDTH.max(980));
                    // Tied to MIN_WINDOW_HEIGHT so raising the floor can
                    // never leave the default smaller than the minimum.
                    let height = height.max(MIN_WINDOW_HEIGHT.max(600));
                    return (width.min(1360), height.min(720));
                }
            }
        }
    }
    (1360, 720)
}

pub fn bind_window_state(window: &impl IsA<gtk4::Window>) {
    let window = window.as_ref();
    let settings = crate::appearance::AppearanceSettings::load();
    if let Some(width) = settings.window_width {
        window.set_default_width(width);
    }
    if let Some(height) = settings.window_height {
        window.set_default_height(height);
    }

    let _ = window.connect_notify(Some("default-width"), move |win, _| {
        let width = win.default_width();
        let mut settings = crate::appearance::AppearanceSettings::load();
        settings.window_width = Some(width);
        settings.save();
    });

    let _ = window.connect_notify(Some("default-height"), move |win, _| {
        let height = win.default_height();
        let mut settings = crate::appearance::AppearanceSettings::load();
        settings.window_height = Some(height);
        settings.save();
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn min_window_height_covers_every_fixed_row() {
        // The floor must leave real room for the faders, not just barely
        // reach the sum of the fixed rows. Regression guard for the
        // wrapped output row pushing the band editor out of view.
        let fixed = HEADER_BAR_H
            + GRAPH_SWITCH_ROW_H
            + GRAPH_DEFAULT_H
            + OUTPUT_ROW_WRAPPED_H
            + BAND_EDITOR_H
            + CHROME_H;
        let room_for_faders = MIN_WINDOW_HEIGHT - fixed;
        assert!(
            room_for_faders >= FADERS_MIN_H,
            "min height {MIN_WINDOW_HEIGHT} leaves only {room_for_faders}px for faders, want >= {FADERS_MIN_H}"
        );
    }

    #[test]
    fn wrapped_output_row_budget_is_more_than_one_line() {
        // If someone shrinks the wrapped allowance back to a single line
        // the editor gets clipped again, so pin it above one line's height.
        let single_line = 36.0;
        let budget = OUTPUT_ROW_WRAPPED_H as f64;
        assert!(
            budget > single_line * 1.5,
            "output row budget {budget} is not enough for a wrapped second line"
        );
    }
}
