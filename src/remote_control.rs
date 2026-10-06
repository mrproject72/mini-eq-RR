//! Shared application state for the D-Bus remote-control interface.
//!
//! This lives in the library rather than in `main.rs` because the GTK window
//! has to write into it on every state mutation while the D-Bus handlers read
//! from it — and `window.rs` cannot reference the binary crate.
//!
//! ## Why there is a command queue
//!
//! `MiniEqAppHandler` is bounded by `Send + Sync` because the `gio` vtable
//! closure that dispatches method calls requires it. GTK objects, however, are
//! main-thread-only and cannot cross that boundary. So the handlers never touch
//! the window directly: they update the cached state (so `GetState` answers
//! immediately and correctly) and *post* a [`RemoteCommand`]. The window's
//! 33 ms tick drains the queue and applies the commands to the real widgets
//! and to the PipeWire backend.
//!
//! Before this existed, six of the handler methods were stubs — `SetEqEnabled`
//! flipped a flag nothing read, `SetPreset` / `PresentWindow` / `Quit` were
//! no-ops, and `AnalyzerLevels` always returned an empty array.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use gtk4 as gtk;

use crate::dbus_control::MiniEqAppHandler;

/// A mutation requested over D-Bus, to be applied by the window.
#[derive(Debug, Clone, PartialEq)]
pub enum RemoteCommand {
    SetEqEnabled(bool),
    SetRouting(bool),
    SetOutputMode(OutputModeArg),
    SetOutputSink(String),
    SetMonitorEnabled(bool),
    SetMonitorSink(String),
    SetPreset(String),
    PresentWindow,
    Quit,
}

/// Argument to `SetOutputMode`: the mode as a string, so the D-Bus signature
/// is `s` and the same `selected`/`reroute` values the UI buttons use can be
/// passed by hand.
#[derive(Debug, Clone, PartialEq)]
pub struct OutputModeArg(pub crate::core::OutputRoutingMode);

/// Minimum interval between `AnalyzerLevelsChanged` emissions.
///
/// Time-based, matching upstream `CONTROL_ANALYZER_EMIT_INTERVAL_SECONDS =
/// 0.10`. An earlier attempt here compared the level vectors and emitted only
/// on a difference, which starves a remote panel: silence and steady tones
/// produce an identical spectrum every frame, so the signal never fired at all.
pub const CONTROL_ANALYZER_EMIT_INTERVAL: std::time::Duration =
    std::time::Duration::from_millis(100);

/// Live application state, shared between the D-Bus handlers and the window.
pub struct AppState {
    pub eq_enabled: Mutex<bool>,
    pub routed: Mutex<bool>,
    pub preset_name: Mutex<Option<String>>,
    pub output_sink: Mutex<Option<String>>,
    pub background_mode: Mutex<bool>,
    pub start_at_login: Mutex<bool>,
    pub start_active_at_login: Mutex<bool>,
    pub analyzer_enabled: Mutex<bool>,
    running: Mutex<bool>,

    // Published by the window on every tick so `GetState` and the
    // `AnalyzerLevelsChanged` signal read real values.
    analyzer_levels: Mutex<Vec<f64>>,
    analyzer_display_gain_db: Mutex<f64>,
    window_visible: Mutex<bool>,
    shutting_down: Mutex<bool>,

    /// Which streams the EQ reaches. Published by the window from the mode
    /// buttons; the D-Bus handler reads it for `GetState`.
    output_mode: Mutex<crate::core::OutputRoutingMode>,
    /// The sink the monitor is pinned to, or `None` to follow the EQ output.
    monitor_sink: Mutex<Option<String>>,
    /// The preset linked to the current output device, if any.
    output_preset: Mutex<Option<String>>,

    /// Throttle state for `AnalyzerLevelsChanged`.
    last_analyzer_emit: Mutex<Option<std::time::Instant>>,

    /// Commands waiting for the window to apply them.
    pending: Mutex<VecDeque<RemoteCommand>>,

    /// The registered D-Bus connection, so the window can emit signals.
    connection: Mutex<Option<gtk::gio::DBusConnection>>,
}

impl AppState {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            eq_enabled: Mutex::new(true),
            routed: Mutex::new(false),
            preset_name: Mutex::new(None),
            output_sink: Mutex::new(None),
            background_mode: Mutex::new(crate::background::load_background_mode()),
            start_at_login: Mutex::new(crate::background::load_start_at_login()),
            start_active_at_login: Mutex::new(crate::background::load_start_active_at_login()),
            analyzer_enabled: Mutex::new(false),
            running: Mutex::new(true),
            analyzer_levels: Mutex::new(Vec::new()),
            analyzer_display_gain_db: Mutex::new(0.0),
            window_visible: Mutex::new(false),
            shutting_down: Mutex::new(false),
            output_mode: Mutex::new(crate::core::OutputRoutingMode::Selected),
            monitor_sink: Mutex::new(None),
            output_preset: Mutex::new(None),
            last_analyzer_emit: Mutex::new(None),
            pending: Mutex::new(VecDeque::new()),
            connection: Mutex::new(None),
        })
    }

    fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
        m.lock().unwrap_or_else(|e| e.into_inner())
    }

    // ── Command queue ──────────────────────────────────────────────

    fn post(&self, cmd: RemoteCommand) {
        Self::lock(&self.pending).push_back(cmd);
    }

    /// Take every queued command. Called from the window's 33 ms tick.
    pub fn drain_pending(&self) -> Vec<RemoteCommand> {
        Self::lock(&self.pending).drain(..).collect()
    }

    // ── Connection ──────────────────────────────────────────────────

    pub fn set_connection(&self, conn: gtk::gio::DBusConnection) {
        *Self::lock(&self.connection) = Some(conn);
    }

    pub fn connection(&self) -> Option<gtk::gio::DBusConnection> {
        Self::lock(&self.connection).clone()
    }

    /// Emit `StateChanged` if the bus connection is up.
    pub fn emit_state_changed(&self) {
        if let Some(conn) = self.connection() {
            crate::dbus_control::MiniEqDBusControl::emit_state_changed(&conn, self);
        }
    }

    /// Emit `AnalyzerLevelsChanged` with the levels the window last published,
    /// rate-limited to [`CONTROL_ANALYZER_EMIT_INTERVAL`].
    ///
    /// Returns `true` when a signal was actually emitted, so the caller can
    /// skip the work entirely on throttled ticks.
    pub fn maybe_emit_analyzer_levels_changed(&self) -> bool {
        let now = std::time::Instant::now();
        {
            let mut last = Self::lock(&self.last_analyzer_emit);
            if let Some(prev) = *last
                && now.duration_since(prev) < CONTROL_ANALYZER_EMIT_INTERVAL
            {
                return false;
            }
            *last = Some(now);
        }
        if let Some(conn) = self.connection() {
            crate::dbus_control::MiniEqDBusControl::emit_analyzer_levels_changed(&conn, self);
        }
        true
    }

    pub fn emit_presets_changed(&self) {
        if let Some(conn) = self.connection() {
            crate::dbus_control::MiniEqDBusControl::emit_presets_changed(&conn);
        }
    }

    // ── Window-published values ─────────────────────────────────────

    /// Publish the per-tick values the D-Bus interface reports. Called by the
    /// window's update loop.
    ///
    /// `output_sink` is published (not merely set by callers) so GetState can
    /// never disagree with the chain: a refused or failed switch leaves the
    /// engine sink untouched and the next tick re-publishes it.
    pub fn publish(
        &self,
        levels: Vec<f64>,
        display_gain_db: f64,
        visible: bool,
        running: bool,
        analyzer_enabled: bool,
        output_mode: crate::core::OutputRoutingMode,
        output_preset: Option<String>,
        monitor_sink: Option<String>,
        output_sink: Option<String>,
    ) {
        *Self::lock(&self.analyzer_levels) = levels;
        *Self::lock(&self.analyzer_display_gain_db) = display_gain_db;
        *Self::lock(&self.window_visible) = visible;
        *Self::lock(&self.running) = running;
        *Self::lock(&self.analyzer_enabled) = analyzer_enabled;
        *Self::lock(&self.output_mode) = output_mode;
        *Self::lock(&self.output_preset) = output_preset;
        *Self::lock(&self.monitor_sink) = monitor_sink;
        if output_sink.is_some() {
            *Self::lock(&self.output_sink) = output_sink;
        }
    }

    /// Record that the UI started tearing down, so in-flight D-Bus calls stop
    /// trying to touch widgets that are going away.
    pub fn set_shutting_down(&self, value: bool) {
        *Self::lock(&self.shutting_down) = value;
    }
}

impl MiniEqAppHandler for AppState {
    fn eq_enabled(&self) -> bool {
        *Self::lock(&self.eq_enabled)
    }

    fn running(&self) -> bool {
        *Self::lock(&self.running)
    }

    fn routed(&self) -> bool {
        *Self::lock(&self.routed)
    }

    fn output_sink(&self) -> Option<String> {
        Self::lock(&self.output_sink).clone()
    }

    fn output_mode(&self) -> crate::core::OutputRoutingMode {
        *Self::lock(&self.output_mode)
    }

    fn output_preset(&self) -> Option<String> {
        Self::lock(&self.output_preset).clone()
    }

    fn monitor_sink(&self) -> Option<String> {
        Self::lock(&self.monitor_sink).clone()
    }

    fn set_output_mode(&self, mode: crate::core::OutputRoutingMode) {
        *Self::lock(&self.output_mode) = mode;
        self.post(RemoteCommand::SetOutputMode(
            crate::remote_control::OutputModeArg(mode),
        ));
    }

    fn set_eq_enabled(&self, enabled: bool) {
        *Self::lock(&self.eq_enabled) = enabled;
        self.post(RemoteCommand::SetEqEnabled(enabled));
    }

    fn route_system_audio(&self, enabled: bool) {
        *Self::lock(&self.routed) = enabled;
        self.post(RemoteCommand::SetRouting(enabled));
    }

    fn set_output_sink(&self, sink: &str) {
        // Deliberately NOT cached here (unlike routed/eq_enabled): the window
        // may REFUSE the switch (sticky chain under Selected), in which case
        // an eager write would leave GetState reporting a device the chain
        // never moved to. The window publishes the true engine sink every
        // tick instead (see publish), so the worst case is ≤33 ms of staleness
        // rather than a permanent lie. Live-test T6a caught exactly this.
        self.post(RemoteCommand::SetOutputSink(sink.to_string()));
    }

    fn set_monitor_enabled(&self, enabled: bool) {
        *Self::lock(&self.analyzer_enabled) = enabled;
        self.post(RemoteCommand::SetMonitorEnabled(enabled));
    }

    fn set_monitor_sink(&self, sink: &str) {
        *Self::lock(&self.monitor_sink) = if sink.is_empty() {
            None
        } else {
            Some(sink.to_string())
        };
        self.post(RemoteCommand::SetMonitorSink(sink.to_string()));
    }

    fn current_preset_name(&self) -> Option<String> {
        Self::lock(&self.preset_name).clone()
    }

    fn background_mode(&self) -> bool {
        *Self::lock(&self.background_mode)
    }

    fn start_at_login(&self) -> bool {
        *Self::lock(&self.start_at_login)
    }

    fn start_active_at_login(&self) -> bool {
        *Self::lock(&self.start_active_at_login)
    }

    fn analyzer_enabled(&self) -> bool {
        *Self::lock(&self.analyzer_enabled)
    }

    fn analyzer_levels(&self) -> Vec<f64> {
        Self::lock(&self.analyzer_levels).clone()
    }

    fn analyzer_display_gain_db(&self) -> f64 {
        *Self::lock(&self.analyzer_display_gain_db)
    }

    fn window_visible(&self) -> bool {
        *Self::lock(&self.window_visible)
    }

    fn ui_shutting_down(&self) -> bool {
        *Self::lock(&self.shutting_down)
    }

    fn present_main_window(&self, _startup_id: Option<&str>) {
        self.post(RemoteCommand::PresentWindow);
    }

    fn quit_fully(&self) {
        self.post(RemoteCommand::Quit);
    }

    fn load_library_preset(&self, name: &str) {
        self.post(RemoteCommand::SetPreset(name.to_string()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn setters_post_a_command_the_window_can_drain() {
        let state = AppState::new();
        assert!(state.drain_pending().is_empty());

        state.set_eq_enabled(false);
        state.route_system_audio(true);
        state.set_output_sink("some-sink");
        state.set_monitor_enabled(true);
        state.set_monitor_sink("some-sink");
        state.set_monitor_sink("");
        state.present_main_window(None);
        state.quit_fully();
        state.load_library_preset("rock");

        assert_eq!(
            state.drain_pending(),
            vec![
                RemoteCommand::SetEqEnabled(false),
                RemoteCommand::SetRouting(true),
                RemoteCommand::SetOutputSink("some-sink".to_string()),
                RemoteCommand::SetMonitorEnabled(true),
                RemoteCommand::SetMonitorSink("some-sink".to_string()),
                RemoteCommand::SetMonitorSink(String::new()),
                RemoteCommand::PresentWindow,
                RemoteCommand::Quit,
                RemoteCommand::SetPreset("rock".to_string()),
            ]
        );
        // Drain is destructive: a second call must not replay the commands.
        assert!(state.drain_pending().is_empty());
    }

    /// `SetEqEnabled` and `SetRoutingEnabled` must be visible to `GetState`
    /// immediately, before the window has applied them, so a remote client
    /// that reads back after a call sees its own write.
    #[test]
    fn setters_update_cached_state_immediately() {
        let state = AppState::new();
        assert!(state.running());
        assert!(state.eq_enabled());
        assert!(!state.routed());

        state.set_eq_enabled(false);
        state.route_system_audio(true);

        assert!(!state.eq_enabled());
        assert!(state.routed());
    }

    /// Regression guard for the original defect: every one of these handlers
    /// used to be a no-op, so a remote client could call them and observe no
    /// effect anywhere.
    #[test]
    fn remote_calls_that_were_stubs_now_reach_the_window() {
        let state = AppState::new();

        state.present_main_window(None);
        assert_eq!(state.drain_pending(), vec![RemoteCommand::PresentWindow]);

        state.quit_fully();
        assert_eq!(state.drain_pending(), vec![RemoteCommand::Quit]);

        state.load_library_preset("flat");
        assert_eq!(
            state.drain_pending(),
            vec![RemoteCommand::SetPreset("flat".to_string())]
        );
    }

    /// `AnalyzerLevelsChanged` must fire on a real spectrum update, not on
    /// every 33 ms tick — otherwise a remote panel sees 30 signals/second of
    /// identical data.
    #[test]
    fn publish_updates_the_values_getstate_reports() {
        let state = AppState::new();
        assert!(!state.window_visible());

        state.publish(
            vec![0.25; 4],
            12.0,
            true,
            true,
            true,
            crate::core::OutputRoutingMode::Selected,
            None,
            None,
            Some("sink-a".to_string()),
        );

        assert_eq!(state.analyzer_levels(), vec![0.25; 4]);
        assert_eq!(state.analyzer_display_gain_db(), 12.0);
        assert!(state.window_visible());
        assert!(state.running());
        assert!(state.analyzer_enabled());
        assert_eq!(state.output_sink().as_deref(), Some("sink-a"));
    }

    /// GetState's output_sink tracks the published engine sink, and a
    /// set_output_sink that the window may refuse leaves no trace: the
    /// handler must not cache ahead of the window (live-test T6a caught the
    /// permanent lie this caused).
    #[test]
    fn output_sink_comes_from_publish_not_from_the_setter() {
        let state = AppState::new();
        assert_eq!(state.output_sink(), None);
        state.set_output_sink("sink-b");
        // Queued for the window, but GetState still shows the old truth.
        assert_eq!(state.output_sink(), None);
        assert_eq!(
            state.drain_pending(),
            vec![RemoteCommand::SetOutputSink("sink-b".to_string())]
        );
        state.publish(
            vec![],
            0.0,
            true,
            true,
            false,
            crate::core::OutputRoutingMode::Selected,
            None,
            None,
            Some("sink-a".to_string()),
        );
        assert_eq!(state.output_sink().as_deref(), Some("sink-a"));
    }

    /// The analyzer signal is throttled by TIME, not by value. A
    /// change-detection version starved the signal entirely: silence and steady
    /// tones give an identical spectrum every frame, so nothing was ever sent.
    #[test]
    fn analyzer_signal_is_time_throttled_not_value_gated() {
        let state = AppState::new();
        // No connection is registered, so nothing is actually emitted, but the
        // throttle decision must still be made and must not depend on levels.
        assert!(state.maybe_emit_analyzer_levels_changed());
        // Immediate second call is inside the 100 ms window.
        assert!(!state.maybe_emit_analyzer_levels_changed());
        assert!(!state.maybe_emit_analyzer_levels_changed());

        // Identical levels must not matter either way.
        state.publish(
            vec![0.5; 10],
            0.0,
            true,
            true,
            true,
            crate::core::OutputRoutingMode::Selected,
            None,
            None,
            None,
        );
        assert!(!state.maybe_emit_analyzer_levels_changed());
    }

    #[test]
    fn shutting_down_is_observable() {
        let state = AppState::new();
        assert!(!state.ui_shutting_down());
        state.set_shutting_down(true);
        assert!(state.ui_shutting_down());
    }
}
