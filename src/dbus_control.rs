//! D-Bus remote control interface for Mini EQ.
//!
//! Mirrors `dbus_control.py` from the original Python project. Registers a
//! D-Bus service at `io.github.mrproject72.mini_eq_rr` / `/io/github/mrproject72/mini_eq_rr/Control`
//! with the `io.github.mrproject72.MiniEqRR.Control` interface.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;

use crate::analyzer::analyzer_level_to_display_norm;
use crate::core::{APP_ID, sanitize_preset_name};
use crate::window_presets::list_preset_names;

// ── Constants ────────────────────────────────────────────────────────

pub const BUS_NAME: &str = APP_ID;
pub const OBJECT_PATH: &str = "/io/github/mrproject72/mini_eq_rr/Control";
pub const INTERFACE_NAME: &str = "io.github.mrproject72.MiniEqRR.Control";
pub const PANEL_ANALYZER_BINS: usize = 10;
pub const API_VERSION: u32 = 1;

pub const CAPABILITIES: &[&str] = &[
    "present-window",
    "quit",
    "background-mode",
    "start-at-login",
    "start-active-at-login",
    "set-routing",
    "set-preset",
    "set-output-mode",
    "set-output-sink",
    "set-monitor-enabled",
    "set-monitor-sink",
    "output-presets",
    "output-mode",
    "monitor-sink",
    "analyzer-levels",
    "startup-notification",
];

// ── D-Bus introspection XML ─────────────────────────────────────────

pub const INTROSPECTION_XML: &str = r#"<node>
  <interface name="io.github.mrproject72.MiniEqRR.Control">
    <method name="GetState">
      <arg name="state" type="a{sv}" direction="out"/>
    </method>
    <method name="ListPresets">
      <arg name="presets" type="as" direction="out"/>
    </method>
    <method name="SetEqEnabled">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetRoutingEnabled">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetOutputMode">
      <arg name="mode" type="s" direction="in"/>
    </method>
    <method name="SetOutputSink">
      <arg name="name" type="s" direction="in"/>
    </method>
    <method name="SetMonitorEnabled">
      <arg name="enabled" type="b" direction="in"/>
    </method>
    <method name="SetMonitorSink">
      <arg name="name" type="s" direction="in"/>
    </method>
    <method name="SetPreset">
      <arg name="name" type="s" direction="in"/>
    </method>
    <method name="PresentWindow"/>
    <method name="PresentWindowWithStartupId">
      <arg name="startup_id" type="s" direction="in"/>
    </method>
    <method name="Quit"/>
    <signal name="StateChanged">
      <arg name="state" type="a{sv}"/>
    </signal>
    <signal name="AnalyzerLevelsChanged">
      <arg name="levels" type="ad"/>
    </signal>
    <signal name="PresetsChanged"/>
  </interface>
</node>
"#;

// ── Application state trait ──────────────────────────────────────────

/// Trait representing the application state that the D-Bus interface can query
/// and control. Mirrors the Python protocols.
pub trait MiniEqAppHandler: Send + Sync + 'static {
    /// Whether the EQ engine (upstream: the controller) exists. Upstream
    /// reports `controller is not None`.
    fn running(&self) -> bool {
        true
    }
    fn eq_enabled(&self) -> bool;
    fn routed(&self) -> bool;
    fn output_sink(&self) -> Option<String>;

    /// Which streams the EQ reaches: `selected` or `reroute`. Orthogonal to
    /// `routed`, which is the on/off of the machinery.
    fn output_mode(&self) -> crate::core::OutputRoutingMode {
        crate::core::OutputRoutingMode::Selected
    }
    /// The preset linked to the current output device, if any.
    fn output_preset(&self) -> Option<String> {
        None
    }
    /// The sink the monitor is pinned to, or `None` to follow the EQ output.
    fn monitor_sink(&self) -> Option<String> {
        None
    }

    fn set_eq_enabled(&self, enabled: bool);
    fn route_system_audio(&self, enabled: bool);
    fn set_output_mode(&self, mode: crate::core::OutputRoutingMode);
    /// Retarget the EQ output to a concrete sink (`""` = follow the system
    /// default). Exists so the live test — and the Shell extension — can
    /// drive the same path as the Output dropdown without a display.
    fn set_output_sink(&self, sink: &str);
    /// Start/stop the output monitor (spectrum + loudness tap).
    fn set_monitor_enabled(&self, enabled: bool);
    /// Pin the monitor to a concrete sink (`""` = follow the EQ output).
    fn set_monitor_sink(&self, sink: &str);

    fn current_preset_name(&self) -> Option<String> {
        None
    }
    fn background_mode(&self) -> bool {
        false
    }
    fn start_at_login(&self) -> bool {
        false
    }
    fn start_active_at_login(&self) -> bool {
        false
    }
    fn analyzer_enabled(&self) -> bool {
        false
    }
    fn analyzer_levels(&self) -> Vec<f64> {
        Vec::new()
    }
    fn analyzer_display_gain_db(&self) -> f64 {
        0.0
    }
    fn window_visible(&self) -> bool {
        false
    }
    fn ui_shutting_down(&self) -> bool {
        false
    }

    fn present_main_window(&self, startup_id: Option<&str>);
    fn quit_fully(&self);
    fn load_library_preset(&self, name: &str);

    fn app_version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }
}

// ── State building ──────────────────────────────────────────────────

/// Build the state dictionary returned by `GetState`.
pub fn build_state(handler: &dyn MiniEqAppHandler) -> HashMap<String, glib::Variant> {
    let mut state = HashMap::new();
    state.insert("api_version".to_string(), glib::Variant::from(API_VERSION));
    state.insert(
        "app_version".to_string(),
        glib::Variant::from(handler.app_version()),
    );
    state.insert(
        "capabilities".to_string(),
        glib::Variant::from(CAPABILITIES.to_vec()),
    );
    state.insert(
        "running".to_string(),
        glib::Variant::from(handler.running()),
    );
    state.insert(
        "eq_enabled".to_string(),
        glib::Variant::from(handler.eq_enabled()),
    );
    state.insert("routed".to_string(), glib::Variant::from(handler.routed()));
    state.insert(
        "preset_name".to_string(),
        glib::Variant::from(handler.current_preset_name().unwrap_or_default()),
    );
    state.insert(
        "output_sink".to_string(),
        glib::Variant::from(handler.output_sink().unwrap_or_default()),
    );
    state.insert(
        "output_mode".to_string(),
        glib::Variant::from(handler.output_mode().as_str()),
    );
    state.insert(
        "output_preset".to_string(),
        glib::Variant::from(handler.output_preset().unwrap_or_default()),
    );
    state.insert(
        "monitor_sink".to_string(),
        glib::Variant::from(handler.monitor_sink().unwrap_or_default()),
    );
    state.insert(
        "background_mode".to_string(),
        glib::Variant::from(handler.background_mode()),
    );
    state.insert(
        "start_at_login".to_string(),
        glib::Variant::from(handler.start_at_login()),
    );
    state.insert(
        "start_active_at_login".to_string(),
        glib::Variant::from(handler.start_active_at_login()),
    );
    state.insert(
        "analyzer_enabled".to_string(),
        glib::Variant::from(handler.analyzer_enabled()),
    );
    state.insert(
        "window_visible".to_string(),
        glib::Variant::from(handler.window_visible()),
    );
    state
}

/// Convert a `HashMap<String, glib::Variant>` into a `{a{sv}}` Variant.
pub fn state_to_variant(state: &HashMap<String, glib::Variant>) -> glib::Variant {
    let dict = glib::VariantDict::new(None);
    for (key, value) in state {
        dict.insert(key, value);
    }
    dict.end()
}

/// Compact analyzer levels to `PANEL_ANALYZER_BINS` bins.
pub fn panel_analyzer_levels(levels: &[f64], display_gain_db: f64) -> Vec<f64> {
    if PANEL_ANALYZER_BINS == 0 {
        return Vec::new();
    }
    if levels.is_empty() {
        return vec![0.0; PANEL_ANALYZER_BINS];
    }
    let source_count = levels.len();
    let mut compacted = Vec::with_capacity(PANEL_ANALYZER_BINS);
    for index in 0..PANEL_ANALYZER_BINS {
        let start = (index * source_count / PANEL_ANALYZER_BINS).min(source_count - 1);
        let end = ((index + 1) * source_count / PANEL_ANALYZER_BINS).min(source_count);
        let end = if end <= start {
            (start + 1).min(source_count)
        } else {
            end
        };
        let max_level = levels[start..end].iter().cloned().fold(f64::NAN, f64::max);
        if max_level.is_nan() {
            compacted.push(0.0);
        } else {
            compacted.push(analyzer_level_to_display_norm(
                clamp_level(max_level),
                display_gain_db,
            ));
        }
    }
    compacted
}

/// Clamp a level to [0.0, 1.0].
pub fn clamp_level(level: f64) -> f64 {
    level.clamp(0.0, 1.0)
}

// ── D-Bus Control ───────────────────────────────────────────────────

/// D-Bus control object that registers the Mini EQ D-Bus interface.
pub struct MiniEqDBusControl {
    handler: Arc<dyn MiniEqAppHandler>,
    connection: Arc<Mutex<Option<gio::DBusConnection>>>,
    registration_id: Arc<Mutex<Option<gio::RegistrationId>>>,
    /// Owner id for the well-known bus name. Must be retained for as long as
    /// the name should stay owned; dropping it releases the name.
    bus_owner_id: Arc<Mutex<Option<gio::OwnerId>>>,
}

impl MiniEqDBusControl {
    pub fn new(handler: Arc<dyn MiniEqAppHandler>) -> Self {
        Self {
            handler,
            connection: Arc::new(Mutex::new(None)),
            registration_id: Arc::new(Mutex::new(None)),
            bus_owner_id: Arc::new(Mutex::new(None)),
        }
    }

    pub fn register(&self) -> Result<(), String> {
        if self.connection.lock().unwrap().is_some() {
            return Ok(());
        }

        let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
            .map_err(|e| format!("Failed to get session bus: {}", e))?;

        let node_info = gio::DBusNodeInfo::for_xml(INTROSPECTION_XML)
            .map_err(|e| format!("Failed to parse introspection XML: {}", e))?;

        let interface_info = node_info
            .interfaces()
            .iter()
            .next()
            .ok_or("No interfaces in introspection XML")?;

        let handler = self.handler.clone();
        let connection_rc = self.connection.clone();

        let reg_id = connection
            .register_object(OBJECT_PATH, interface_info)
            .method_call(
                move |_conn, _sender, _path, _iface, method, params, invocation| {
                    Self::on_method_call(
                        method,
                        &params,
                        invocation,
                        handler.as_ref(),
                        &connection_rc,
                    );
                },
            )
            .build()
            .map_err(|e| format!("Failed to register D-Bus object: {}", e))?;

        *self.connection.lock().unwrap() = Some(connection.clone());
        *self.registration_id.lock().unwrap() = Some(reg_id);

        // Claim the well-known bus name. Registering the object is not
        // enough: without owning the name, `gdbus call --dest <BUS_NAME>` and
        // the GNOME Shell extension get ServiceUnknown. This used to live only
        // in `acquire_bus_name()`, which nothing called, so the control
        // interface was unreachable in practice.
        let owner_id = gio::bus_own_name_on_connection(
            &connection,
            BUS_NAME,
            gio::BusNameOwnerFlags::REPLACE,
            |_conn, _name| {},
            |_conn, _name| {},
        );
        *self.bus_owner_id.lock().unwrap() = Some(owner_id);
        Ok(())
    }

    /// Shared handle to the registered connection, so `AppState` (and through
    /// it the window) can emit `StateChanged` / `AnalyzerLevelsChanged` when
    /// the UI mutates state on its own.
    pub fn connection_handle(&self) -> Arc<Mutex<Option<gio::DBusConnection>>> {
        self.connection.clone()
    }

    pub fn unregister(&self) {
        let conn = self.connection.lock().unwrap().clone();
        let reg_id = self.registration_id.lock().unwrap().take();
        if let Some(connection) = conn
            && let Some(id) = reg_id
        {
            let _ = connection.unregister_object(id);
        }
        *self.connection.lock().unwrap() = None;
        *self.registration_id.lock().unwrap() = None;
    }

    fn on_method_call(
        method: &str,
        params: &glib::Variant,
        invocation: gio::DBusMethodInvocation,
        handler: &dyn MiniEqAppHandler,
        connection: &Arc<Mutex<Option<gio::DBusConnection>>>,
    ) {
        let conn = connection.lock().unwrap().clone();
        match method {
            "GetState" => {
                let state = build_state(handler);
                let variant = state_to_variant(&state);
                let wrapped = glib::Variant::from((variant,));
                invocation.return_value(Some(&wrapped))
            }
            "ListPresets" => {
                let presets = list_preset_names();
                // Element type pinned to `s` so the array is `as` — see the
                // note on `emit_analyzer_levels_changed`.
                let array = glib::Variant::array_from_iter_with_type(
                    glib::VariantTy::STRING,
                    presets.iter().cloned().map(glib::Variant::from),
                );
                let wrapped = glib::Variant::from((array,));
                invocation.return_value(Some(&wrapped))
            }
            // NOTE on argument extraction: `params` is the whole argument *tuple*, so a
            // one-argument method arrives as `(b)` / `(s)`, never as a bare
            // `b` / `s`. Reading it with `params.get::<bool>()` always failed
            // the type assertion and every setter answered InvalidArguments —
            // invisible until the bus name was actually owned, because nothing
            // could reach the service before.
            "SetEqEnabled" => match params.get::<(bool,)>() {
                Some((enabled,)) => {
                    handler.set_eq_enabled(enabled);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetEqEnabled",
                ),
            },
            "SetRoutingEnabled" => match params.get::<(bool,)>() {
                Some((enabled,)) => {
                    handler.route_system_audio(enabled);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetRoutingEnabled",
                ),
            },
            "SetOutputMode" => match params.get::<(String,)>() {
                Some((mode,)) => {
                    let parsed = crate::core::OutputRoutingMode::from_mode_str(&mode);
                    handler.set_output_mode(parsed);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetOutputMode",
                ),
            },
            "SetOutputSink" => match params.get::<(String,)>() {
                Some((name,)) => {
                    handler.set_output_sink(&name);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetOutputSink",
                ),
            },
            "SetMonitorEnabled" => match params.get::<(bool,)>() {
                Some((enabled,)) => {
                    handler.set_monitor_enabled(enabled);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetMonitorEnabled",
                ),
            },
            "SetMonitorSink" => match params.get::<(String,)>() {
                Some((name,)) => {
                    handler.set_monitor_sink(&name);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetMonitorSink",
                ),
            },
            "SetPreset" => match params.get::<(String,)>() {
                Some((name,)) => {
                    let preset_name = sanitize_preset_name(&name);
                    if preset_name.is_empty() {
                        return invocation.return_dbus_error(
                            &format!("{}.InvalidArguments", INTERFACE_NAME),
                            "preset name is empty",
                        );
                    }
                    handler.load_library_preset(&preset_name);
                    if let Some(ref conn) = conn {
                        Self::emit_state_changed(conn, handler);
                    }
                    invocation.return_value(None)
                }
                None => invocation.return_dbus_error(
                    &format!("{}.InvalidArguments", INTERFACE_NAME),
                    "Invalid arguments for SetPreset",
                ),
            },
            "PresentWindow" => {
                handler.present_main_window(None);
                invocation.return_value(None)
            }
            "PresentWindowWithStartupId" => {
                if let Some((startup_id,)) = params.get::<(String,)>() {
                    handler.present_main_window(Some(&startup_id));
                }
                invocation.return_value(None)
            }
            "Quit" => {
                handler.quit_fully();
                invocation.return_value(None)
            }
            _ => invocation.return_dbus_error(
                &format!("{}.UnknownMethod", INTERFACE_NAME),
                &format!("Unknown method: {}", method),
            ),
        }
    }

    /// Emit `StateChanged`. Called both from the method handlers below and
    /// from the window whenever it mutates state on its own, so remote clients
    /// see UI-initiated changes too.
    pub(crate) fn emit_state_changed(
        connection: &gio::DBusConnection,
        handler: &dyn MiniEqAppHandler,
    ) {
        let state = build_state(handler);
        let variant = state_to_variant(&state);
        let wrapped = glib::Variant::from((variant,));
        let _ = connection.emit_signal(
            None,
            OBJECT_PATH,
            INTERFACE_NAME,
            "StateChanged",
            Some(&wrapped),
        );
    }

    #[allow(dead_code)]
    pub(crate) fn emit_analyzer_levels_changed(
        connection: &gio::DBusConnection,
        handler: &dyn MiniEqAppHandler,
    ) {
        let levels = panel_analyzer_levels(
            &handler.analyzer_levels(),
            handler.analyzer_display_gain_db(),
        );
        // `array_from_iter_with_type::<f64>` is required: the element type has
        // to be pinned to `d` so the array is `ad`. The obvious
        // `array_from_iter::<Variant>(...map(Variant::from))` infers the
        // element type from `Variant`'s own static type instead and trips an
        // `is_type` assertion at runtime.
        let array = glib::Variant::array_from_iter_with_type(
            glib::VariantTy::DOUBLE,
            levels.iter().copied().map(glib::Variant::from),
        );
        let wrapped = glib::Variant::from((array,));
        let _ = connection.emit_signal(
            None,
            OBJECT_PATH,
            INTERFACE_NAME,
            "AnalyzerLevelsChanged",
            Some(&wrapped),
        );
    }

    #[allow(dead_code)]
    pub(crate) fn emit_presets_changed(connection: &gio::DBusConnection) {
        let _ = connection.emit_signal(None, OBJECT_PATH, INTERFACE_NAME, "PresetsChanged", None);
    }
}

/// Send a D-Bus `PresentWindow` method call to a running Mini EQ instance.
pub fn call_present_window(startup_id: Option<&str>, timeout_ms: i32) -> Result<(), String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|e| format!("Failed to get session bus: {}", e))?;

    if let Some(id) = startup_id {
        let params = glib::Variant::from((id.to_string(),));
        connection.call(
            Some(BUS_NAME),
            OBJECT_PATH,
            INTERFACE_NAME,
            "PresentWindowWithStartupId",
            Some(&params),
            None,
            gio::DBusCallFlags::NONE,
            timeout_ms,
            None::<&gio::Cancellable>,
            |result| {
                let _ = result;
            },
        );
    } else {
        connection.call(
            Some(BUS_NAME),
            OBJECT_PATH,
            INTERFACE_NAME,
            "PresentWindow",
            None,
            None,
            gio::DBusCallFlags::NONE,
            timeout_ms,
            None::<&gio::Cancellable>,
            |result| {
                let _ = result;
            },
        );
    }

    Ok(())
}

/// Acquire the bus name so other clients can find us.
pub fn acquire_bus_name() -> Result<(), String> {
    let connection = gio::bus_get_sync(gio::BusType::Session, None::<&gio::Cancellable>)
        .map_err(|e| format!("Failed to get session bus: {}", e))?;

    let _owner_id = gio::bus_own_name_on_connection(
        &connection,
        BUS_NAME,
        gio::BusNameOwnerFlags::REPLACE,
        |_conn, _name| {},
        |_conn, _name| {},
    );

    Ok(())
}
