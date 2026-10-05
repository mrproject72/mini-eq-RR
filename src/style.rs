//! CSS styling for the mini-eq GTK4 application.

use gtk4::CssProvider;
use gtk4::gdk;

const STYLE_CSS: &str = r#"
:root {
    --window-fg-color: rgb(235, 236, 237);
    --window-bg-color: rgb(30, 30, 32);
    --panel-bg-color: rgb(40, 41, 44);
    --panel-border-color: rgb(60, 61, 65);
    --accent-color: rgb(120, 160, 220);
    --accent-color-active: rgb(140, 180, 240);
    --danger-color: rgb(220, 80, 80);
    --success-color: rgb(80, 180, 120);
    --warning-color: rgb(220, 180, 80);
    --text-muted: rgb(150, 151, 155);
    --toolbar-bg-color: rgb(35, 36, 38);
    --mini-border-subtle: rgba(255, 255, 255, 0.08);
    --mini-toggle-checked-color: rgb(255, 255, 255);
    --mini-toggle-checked-bg: rgb(120, 160, 220);
    --mini-toggle-checked-border: rgb(140, 180, 240);
    --mini-text-dim: rgb(150, 151, 155);
}

.mini-eq-dark {
    --window-fg-color: rgb(235, 236, 237);
    --window-bg-color: rgb(30, 30, 32);
    --panel-bg-color: rgb(40, 41, 44);
    --panel-border-color: rgb(60, 61, 65);
}

.mini-eq-light {
    --window-fg-color: rgb(40, 40, 42);
    --window-bg-color: rgb(240, 241, 243);
    --panel-bg-color: rgb(255, 255, 255);
    --panel-border-color: rgb(200, 201, 205);
}

.panel-card {
    background-color: var(--panel-bg-color);
    border-radius: 12px;
    padding: 12px;
}

.graph-shell-panel {
    background-color: var(--panel-bg-color);
    border-radius: 12px;
    padding: 12px;
}

.quick-view-shell {
    background-color: var(--panel-bg-color);
    border-radius: 12px;
    padding: 12px;
}

.toolbar-row {
    background-color: var(--toolbar-bg-color);
    border-radius: 10px;
    padding: 6px 10px;
}

.heading {
    font-weight: 600;
    color: var(--window-fg-color);
}

.dim-label {
    color: var(--text-muted);
}

.numeric {
    font-variant-numeric: tabular-nums;
}

.fader-title-label {
    font-size: 1.1em;
}

.eq-band-box {
    background-color: var(--panel-bg-color);
    border-radius: 8px;
    padding: 4px;
    border: 1px solid transparent;
}

.eq-band-box-selected {
    border-color: var(--accent-color);
    box-shadow: 0 0 0 1px var(--accent-color);
}

.eq-band-box-muted {
    opacity: 0.5;
}

.band-editor {
    margin-top: 0;
    padding: 4px 7px;
    border-radius: 10px;
    background-color: var(--panel-bg-color);
    border: 1px solid var(--mini-border-subtle);
}

.metric-title {
    font-size: 9.5pt;
    font-weight: 700;
    letter-spacing: 0;
    color: var(--mini-text-dim);
}

.band-editor-title {
    color: var(--window-fg-color);
    font-size: 9.5pt;
    font-weight: 800;
}

.band-editor-selected {
    min-width: 88px;
}

.band-editor-state {
    padding: 0 2px;
}

.band-editor-field {
    margin-left: 2px;
}

.band-editor-input {
    min-height: 32px;
}

/* Compact the +/- buttons inside the editor SpinButtons. Their default
   min-width (~28px each) is what makes each spin ~113-127px wide and
   forces the editor row to ~800px. Shrinking them lets the editor stay
   on a single line at the 640px minimum window width. */
.band-editor-input button {
    min-width: 0;
    min-height: 0;
    padding: 0 2px;
    margin: 0;
}

.band-editor-input entry {
    min-width: 0;
}

.band-editor-toggle {
    min-width: 24px;
    min-height: 30px;
    padding: 0;
    border-radius: 9px;
    font-weight: 800;
    color: var(--window-fg-color);
    background-color: rgba(128, 128, 128, 0.12);
    border: 1px solid var(--mini-border-subtle);
}

.band-editor-toggle:checked {
    color: var(--mini-toggle-checked-color);
    background-color: var(--mini-toggle-checked-bg);
    border-color: var(--mini-toggle-checked-border);
}

.utility-section {
    background-color: var(--panel-bg-color);
    border-radius: 10px;
    padding: 10px;
    margin-bottom: 8px;
}

.utility-pane-shell {
    background-color: var(--panel-bg-color);
    border-radius: 12px;
    padding: 8px;
}

.preset-state-chip {
    border-radius: 10px;
    padding: 2px 8px;
    font-size: 0.85em;
}

.preset-state-chip-saved {
    background-color: color-mix(in srgb, var(--success-color) 20%, transparent);
    color: var(--success-color);
}

.preset-state-chip-modified {
    background-color: color-mix(in srgb, var(--warning-color) 20%, transparent);
    color: var(--warning-color);
}

.preset-state-chip-unsaved {
    background-color: color-mix(in srgb, var(--accent-color) 20%, transparent);
    color: var(--accent-color);
}

.preset-state-chip-neutral {
    background-color: color-mix(in srgb, var(--text-muted) 20%, transparent);
    color: var(--text-muted);
}

/* Blinking clipping alert on the Fix button while the EQ curve peak exceeds the
   -1 dBFS target. Toggled by a 500 ms timer (GTK4 CSS has no @keyframes); the
   colour transition smooths each toggle into a pulse. Deliberately on the button
   that FIXES the problem, not the header icon.

   The blink used to be visible because the button rested GREEN, so red-on-green
   was an unmistakable change. Fix is now red the whole time it is actionable
   (see clip-fix-needed below), so the pulse has to come from something else: it
   drops the fill and leaves an outline, alternating outlined/solid red. */
button.headroom-warning,
button.headroom-warning image {
    color: var(--danger-color);
    background-color: transparent;
}

button.headroom-warning {
    transition: color 250ms ease-in-out, background-color 250ms ease-in-out;
}

/* The clipping pair: [Fix] [Auto]. Both are buttons with a fixed label, so
   colour is the ONLY thing that carries their state:
     - Fix: red when the curve peak is over the target and the button is live,
       plain grey otherwise.
     - Auto: green while it is on, plain grey while it is off.
   The explicit opacity override matters for Fix: it is insensitive when there
   is nothing to do, and GTK dims insensitive controls so hard that without
   this the button reads as missing rather than as "nothing to do". */
button.clip-fix-needed,
button.clip-fix-needed:disabled {
    opacity: 1.0;
    color: var(--danger-color);
    background-color: color-mix(in srgb, var(--danger-color) 22%, transparent);
    border-color: var(--danger-color);
}

button.clip-auto-on,
button.clip-auto-on:disabled {
    opacity: 1.0;
    color: var(--success-color);
    background-color: color-mix(in srgb, var(--success-color) 22%, transparent);
    border-color: var(--success-color);
}

/* Smooth engaged: the dropdown lights up the same green so the active mode
   is obvious without opening the popover.
   NOTE: gtk4::MenuButton renders as a `menubutton` CSS node wrapping an
   internal `button`. The class lands on the `menubutton`, so a
   `button.smooth-on` selector NEVER matches -- that is why an earlier
   attempt here appeared to do nothing. Target both the node and its
   internal button. */
menubutton.smooth-on > button,
menubutton.smooth-on button {
    color: var(--success-color);
    background-color: color-mix(in srgb, var(--success-color) 22%, transparent);
    border-color: var(--success-color);
}

/* Main-window output control row: Auto-Safe, A/B compare, preamp, live
   peak meter and Set Safe, between the spectrum and the fader strip. */
.output-control-row {
    padding: 2px 6px;
}

.output-control-row label.metric-title {
    color: var(--text-muted);
    font-size: 0.85em;
}


.system-state-chip {
    border-radius: 10px;
    padding: 2px 8px;
    font-size: 0.85em;
}

.system-state-chip-live {
    background-color: color-mix(in srgb, var(--success-color) 20%, transparent);
    color: var(--success-color);
}

.system-state-chip-bypass {
    background-color: color-mix(in srgb, var(--danger-color) 20%, transparent);
    color: var(--danger-color);
}

.headroom-panel-safe {
    background-color: var(--panel-bg-color);
    border-radius: 8px;
    padding: 8px;
}

.headroom-panel-risk {
    background-color: color-mix(in srgb, var(--danger-color) 15%, transparent);
    border-radius: 8px;
    padding: 8px;
}

.headroom-panel-tight {
    background-color: color-mix(in srgb, var(--warning-color) 15%, transparent);
    border-radius: 8px;
    padding: 8px;
}

.headroom-peak-chip {
    border-radius: 6px;
    padding: 1px 6px;
    font-size: 0.8em;
}

.graph-header-title {
    font-size: 0.95em;
}

.graph-stage {
    background-color: rgb(20, 20, 22);
    border-radius: 8px;
}

.route-box {
    background-color: var(--toolbar-bg-color);
    border-radius: 8px;
    padding: 4px 8px;
}

.utility-pane-scroller {
    background-color: var(--panel-bg-color);
    border-radius: 12px;
}
"#;

/// Load the mini-eq CSS stylesheet into the GTK default display.
pub fn load_style() {
    let provider = CssProvider::new();
    provider.load_from_data(STYLE_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}
