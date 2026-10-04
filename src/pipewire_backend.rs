use std::cell::RefCell;
use std::ffi::CString;
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::sync::{Arc, Mutex};

use log::{debug, info, warn};
use pipewire::node::Node;
use pipewire::spa::param::ParamType;
use pipewire::spa::pod::Pod;
use pipewire::spa::pod::builder::Builder;
use pipewire::{Error, context::ContextRc, core::CoreRc, loop_::Timeout, main_loop::MainLoopRc};
use pipewire_sys as pw_sys;

use crate::core::{
    EQ_PREAMP_MAX_DB, EQ_PREAMP_MIN_DB, EqBand, FILTER_OUTPUT_SUFFIX, VIRTUAL_SINK_BASE,
};
use crate::filter_chain;
use crate::routing::{OutputRoute, RoutingEngine};

/// Owns a `pw_impl_module` loaded through `pw_context_load_module`.
///
/// PipeWire's Rust bindings do not expose module loading, so the handle is kept
/// as a raw pointer and destroyed on drop.
struct ModuleHandle(*mut pw_sys::pw_impl_module);

impl Drop for ModuleHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer came from `pw_context_load_module` and is
            // destroyed exactly once.
            unsafe { pw_sys::pw_impl_module_destroy(self.0) };
            self.0 = std::ptr::null_mut();
        }
    }
}

pub struct PipeWireBackend {
    mainloop: MainLoopRc,
    context: ContextRc,
    core: CoreRc,
    filter_chain_module: Option<ModuleHandle>,
    bands: Vec<EqBand>,
    preamp_gain: f64,
    running: Arc<Mutex<bool>>,
    routing: RoutingEngine,
    /// Live proxy for the filter-chain virtual sink node (`mini_eq_sink`).
    /// Captured by the registry listener once the module creates it; used by
    /// `apply_live_controls` to push `SPA_PARAM_Props` without a reload.
    filter_node: Rc<RefCell<Option<Node>>>,
    /// Kept alive so the registry `global` listener stays registered for the
    /// backend's lifetime (listeners unregister themselves when dropped).
    _registry_listener: Option<pipewire::registry::Listener>,
    /// Live output spectrum analyzer (monitor). Owns the capture stream that
    /// taps the output sink's monitor ports. Mirrors upstream
    /// `output_analyzer`. Started/stopped via `start_monitor`/`stop_monitor`.
    analyzer: crate::analyzer::OutputSpectrumAnalyzer,
    /// Target sink for a monitor start whose port-linking is still pending
    /// negotiation. Driven forward by `pump_monitor_link` from the update
    /// loop so the GTK UI never blocks on the (multi-second) link wait.
    pending_monitor_target: Option<String>,
}

/// `SPA_PROP_params` — the `SPA_PROP_START_Other` (0x80000) entry that carries
/// control values on a props object, so its value is 0x80001.
///
/// `spa/include/spa/param/props.h` documents its payload as
/// `Struct((String : key, Pod : value)*)` — a flat run of string/value pairs.
/// `spa/plugins/filter-graph/filter-graph.c:parse_params()` reads it with a
/// sequential `spa_pod_parser`: `push_struct`, then repeatedly
/// `get_string(name)` followed by a value. There is **no item count and no
/// dict flag**, and the loop `break`s on the first field it cannot read as a
/// string.
///
/// That last detail matters: an earlier attempt "fixed" this by emitting
/// `SPA_POD_PROP_FLAG_HINT_DICT` and a leading `Int: n_items`, on the theory
/// that upstream's `GLib.Variant("a{sd}", …)` implied a dict. Both were wrong.
/// The leading `Int` made `get_string` fail on field zero, so the parser
/// aborted immediately and *every* control was dropped — which presented as the
/// EQ becoming completely inert once systemwide routing was on.
const PARAM_PROPS: u32 = 0x80001;

/// Serialise control values into the `SPA_PARAM_Props` pod that PipeWire's
/// filter-graph parses.
///
/// The payload shape is dictated by `parse_params()` in
/// `spa/plugins/filter-graph/filter-graph.c`:
///
/// ```text
/// Object(Props)
///   prop key   = SPA_PROP_params (0x80001)
///   prop flags = 0
///   prop value = Struct( (String : key, Double : value)* )
/// ```
///
/// `parse_params` walks the struct with `spa_pod_parser` and `break`s as soon
/// as a field is not a string, so the payload must start with a string and
/// must contain no count or type tag.
///
/// Split out of `apply_live_controls` so the wire format can be pinned by a
/// unit test; this module previously had no tests at all.
fn build_props_controls_pod_bytes(controls: &[(String, f64)]) -> Option<Vec<u8>> {
    let mut data: Vec<u8> = Vec::new();
    let built = {
        let mut builder = Builder::new(&mut data);
        let mut obj_frame = MaybeUninit::zeroed();
        let mut struct_frame = MaybeUninit::zeroed();
        let mut ok = true;

        // SAFETY: both frames stay alive until popped, and the builder owns
        // `data` and is dropped before it is returned.
        unsafe {
            ok &= builder
                .push_object(
                    &mut obj_frame,
                    pipewire::spa::utils::SpaTypes::ObjectParamProps.as_raw(),
                    ParamType::Props.as_raw(),
                )
                .is_ok();
            ok &= builder.add_prop(PARAM_PROPS, 0).is_ok();
            ok &= builder.push_struct(&mut struct_frame).is_ok();
            // Must begin with a string: parse_params breaks on the first
            // non-string field, so no count or type tag may precede the pairs.
            if ok {
                for (name, value) in controls {
                    ok &= builder.add_string(name).is_ok();
                    ok &= builder.add_double(*value).is_ok();
                }
            }
            builder.pop(struct_frame.assume_init_mut());
            builder.pop(obj_frame.assume_init_mut());
            ok
        }
    };

    built.then_some(data)
}

impl PipeWireBackend {
    pub fn new(bands: Vec<EqBand>) -> Result<Self, Error> {
        info!("Initializing PipeWire backend");

        pipewire::init();

        let mainloop = MainLoopRc::new(None)?;
        let context = ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        info!("Connected to PipeWire server");

        let routing = RoutingEngine::new(core.clone(), mainloop.clone());
        let analyzer =
            crate::analyzer::OutputSpectrumAnalyzer::new(core.clone(), crate::core::SAMPLE_RATE)?;

        let backend = PipeWireBackend {
            mainloop,
            context,
            core,
            filter_chain_module: None,
            bands,
            preamp_gain: 0.0,
            running: Arc::new(Mutex::new(true)),
            routing,
            filter_node: Rc::new(RefCell::new(None)),
            _registry_listener: None,
            analyzer,
            pending_monitor_target: None,
        };

        let registry_listener = backend.setup_registry_listener()?;
        let backend = PipeWireBackend {
            _registry_listener: Some(registry_listener),
            ..backend
        };

        Ok(backend)
    }

    fn setup_registry_listener(&self) -> Result<pipewire::registry::Listener, Error> {
        let registry = self.core.get_registry_rc()?;
        let filter_node = self.filter_node.clone();
        let registry_for_cb = registry.clone();
        let listener = registry.add_listener_local();
        let listener = listener.global(move |global| {
            debug!("Registry global: id={} type={:?}", global.id, global.type_);
            if global.type_.to_str() != pipewire::types::ObjectType::Node.to_str() {
                return;
            }
            let is_eq_sink = global
                .props
                .as_ref()
                .and_then(|p| p.get("node.name"))
                .map(|n| n == VIRTUAL_SINK_BASE)
                .unwrap_or(false);
            if is_eq_sink && filter_node.borrow().is_none() {
                match registry_for_cb.bind::<Node, _>(global) {
                    Ok(node) => {
                        info!(
                            "Captured live filter node proxy: {} (id={})",
                            VIRTUAL_SINK_BASE, global.id
                        );
                        *filter_node.borrow_mut() = Some(node);
                    }
                    Err(e) => warn!("Failed to bind filter node: {}", e),
                }
            }
        });
        let _listener = listener.register();
        Ok(_listener)
    }

    // ---------------------------------------------------------------------
    // Output monitor (spectrum analyzer + loudness meter)
    // ---------------------------------------------------------------------

    /// Start monitoring the processed output for the spectrum analyzer and
    /// loudness meter, mirroring upstream `ensure_output_analyzer`.
    ///
    /// Non-blocking: starts the capture stream and records the target so the
    /// port-linking (which must wait for stream negotiation) is driven forward
    /// by [`pump_monitor_link`] from the window's update loop. This keeps the
    /// GTK UI responsive instead of freezing on the multi-second link wait.
    pub fn start_monitor(&mut self, target_sink_name: &str) -> Result<(), Error> {
        info!("Starting output monitor of {target_sink_name}");
        self.analyzer.start_capture(target_sink_name, None)?;
        self.pending_monitor_target = Some(target_sink_name.to_string());
        Ok(())
    }

    /// Advance the pending monitor port-linking. Called every tick from the
    /// update loop. Once the capture stream has negotiated (Paused/Streaming)
    /// and the sink + our node are both visible in the registry, link the
    /// sink's monitor ports to our analyzer inputs with `pw-link` (the
    /// session manager otherwise routes capture to the default source, e.g.
    /// a microphone), then drop any foreign links it made meanwhile.
    ///
    /// Does a bounded amount of work per call so it never blocks the UI for
    /// long. Returns `true` when linking is finished (success or giving up).
    pub fn pump_monitor_link(&mut self) -> bool {
        let Some(target) = self.pending_monitor_target.clone() else {
            return true;
        };
        // Wait until the capture stream has finished negotiating.
        let negotiated = matches!(
            self.analyzer.stream_state(),
            Some(pipewire::stream::StreamState::Paused)
                | Some(pipewire::stream::StreamState::Streaming)
        );
        if !negotiated {
            return false;
        }
        let our_id = self.analyzer.stream_node_id();
        if our_id == 0 {
            return false;
        }
        let nodes = registry_node_names(&self.mainloop, &self.core).unwrap_or_default();
        let Some(hw_id) = nodes
            .iter()
            .find(|(name, _)| name == &target)
            .map(|(_, id)| *id)
        else {
            return false;
        };
        let snapshot = registry_port_snapshot(&self.mainloop, &self.core).unwrap_or_default();
        // Port ids collide per direction (playback_FL and monitor_FL are both
        // port.id 0), so link by port NAME.
        let hw_mon: Vec<String> = snapshot
            .iter()
            .filter(|p| p.node_id == hw_id && p.direction == "out" && p.path.contains("monitor"))
            .map(|p| p.port_name.clone())
            .collect();
        let our_in: Vec<String> = snapshot
            .iter()
            .filter(|p| {
                p.node_id == our_id && p.direction == "in" && p.port_name.starts_with("input")
            })
            .map(|p| p.port_name.clone())
            .collect();
        if hw_mon.len() < 2 || our_in.len() < 2 {
            return false;
        }
        for (out_port, in_port) in hw_mon.iter().zip(our_in.iter()).take(2) {
            let out_ref = format!("{target}:{out_port}");
            let in_ref = format!("{}:{in_port}", crate::analyzer::ANALYZER_NODE_NAME);
            let owned = [out_ref.clone(), in_ref.clone()];
            let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
            match run_pw_link(&self.mainloop, &refs, std::time::Duration::from_secs(10)) {
                Some(o) if o.status.success() => info!("Linked monitor {out_ref} -> {in_ref}"),
                Some(o) => warn!(
                    "pw-link failed: {} {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                ),
                None => warn!("pw-link timed out"),
            }
        }
        self.drop_foreign_monitor_links(our_id, hw_id);
        self.pending_monitor_target = None;
        true
    }

    /// Destroy links into our analyzer inputs that don't come from the
    /// monitored sink (e.g. session-manager microphone links).
    fn drop_foreign_monitor_links(&self, our_id: u32, hw_id: u32) {
        let links = registry_link_snapshot(&self.mainloop, &self.core).unwrap_or_default();
        for link in links {
            if link.input_node == our_id && link.output_node != hw_id {
                let id = link.id.to_string();
                let args = ["destroy", id.as_str()];
                if let Some(o) =
                    run_pw_cli_pumped(&self.mainloop, &args, std::time::Duration::from_secs(10))
                    && o.status.success()
                {
                    info!("Dropped foreign monitor link {}", link.id);
                }
            }
        }
    }

    /// Move the output monitor to a different sink. Stop + start is the
    /// safe way to do this: the monitor is an independent capture stream,
    /// so restarting it cannot interrupt the EQ audio path.
    pub fn retarget_monitor(&mut self, new_sink: &str) -> Result<(), Error> {
        info!("Retargeting output monitor -> {new_sink}");
        self.stop_monitor();
        self.start_monitor(new_sink)
    }

    /// Stop the output monitor.
    pub fn stop_monitor(&mut self) {
        self.pending_monitor_target = None;
        self.analyzer.stop_capture();
    }

    /// Normalized 0..1 spectrum levels from live captured audio.
    pub fn monitor_levels(&self) -> Vec<f64> {
        self.analyzer.display_levels()
    }

    /// Map the UI smoothing percentage (0.15..0.95, matching upstream's
    /// slider scale) onto the analyzer's `response_speed`.
    ///
    /// The two are inverse: more smoothing means a SLOWER response. The
    /// speed range spans 0.02..15 (750x), so the interpolation is done in
    /// log space -- a linear map would make the slider almost useless at
    /// the fast end.
    ///
    /// Calibration: 30% smoothing lands on ANALYZER_RESPONSE_DEFAULT (2.0),
    /// so the panel default reproduces the analyzer's own default.
    pub fn set_analyzer_smoothing(&mut self, smoothing: f64) {
        self.analyzer
            .set_response_speed(crate::analyzer::smoothing_percent_to_response_speed(
                smoothing,
            ));
    }

    /// Display gain in dB applied to the spectrum drawing only (does not
    /// affect audio).
    pub fn set_analyzer_display_gain(&mut self, gain_db: f64) {
        self.analyzer.set_display_gain(gain_db);
    }

    /// Windowed output peak in dBFS (from the live monitor). Returns None
    /// when the monitor is off OR no audio was captured in the window (e.g.
    /// the capture stream was orphaned by an engine restart). Returning None
    /// (rather than -inf) lets callers fall back to the estimated peak.
    pub fn monitor_peak_dbfs(&self) -> Option<f64> {
        if !self.analyzer.is_enabled() {
            return None;
        }
        let lin = self.analyzer.take_window_peak();
        if lin > 0.0 {
            Some(20.0 * (lin as f64).log10())
        } else {
            None
        }
    }

    /// Latest loudness snapshot from live captured audio, if any.
    pub fn monitor_loudness(&self) -> Option<crate::analyzer::AnalyzerLoudnessSnapshot> {
        self.analyzer.display_loudness()
    }

    /// Monitor diagnostics: (frames, active bands, peak, mean).
    pub fn monitor_stats(&self) -> (u64, usize, f32, f32) {
        self.analyzer.monitor_stats()
    }

    /// Whether the monitor capture is currently enabled.
    pub fn monitor_enabled(&self) -> bool {
        self.analyzer.is_enabled()
    }

    /// Push the current band/preamp state to the live filter node via
    /// `SPA_PARAM_Props`, mirroring upstream `set_node_params` /
    /// `apply_state_to_engine`. This changes the DSP in milliseconds without
    /// tearing down (and re-linking) the graph.
    ///
    /// Returns `Ok(false)` if the live node proxy is not available yet (caller
    /// should fall back to a module reload).
    /// True once the live filter node proxy has been captured. The proxy
    /// arrives asynchronously AFTER the module load completes, so callers
    /// must not treat "no proxy" as a live-push failure.
    pub fn has_live_node(&self) -> bool {
        self.filter_node.borrow().is_some()
    }

    pub fn apply_live_controls(&self, eq_enabled: bool) -> Result<bool, Error> {
        let node_borrow = self.filter_node.borrow();
        let node = match node_borrow.as_ref() {
            Some(n) => n,
            None => return Ok(false),
        };

        let controls = filter_chain::bq_raw_control_values(
            &self.bands,
            self.preamp_gain,
            eq_enabled,
            crate::core::SAMPLE_RATE,
        );
        if controls.is_empty() {
            return Ok(true);
        }

        let data = build_props_controls_pod_bytes(&controls).ok_or(Error::CreationFailed)?;

        let pod = Pod::from_bytes(&data).ok_or(Error::CreationFailed)?;
        node.set_param(ParamType::Props, 0, pod);
        debug!(
            "apply_live_controls: pushed {} control(s) to {}",
            controls.len(),
            VIRTUAL_SINK_BASE
        );
        Ok(true)
    }

    /// Update the DSP for new bands WITHOUT a reload when the live node is
    /// available; otherwise fall back to a full module reload.
    ///
    /// A filter-type change is a graph topology change (the biquad `label`
    /// is fixed at module-load time), so it forces a restart instead of a
    /// live push — matching upstream `set_filter_controls`.
    pub fn update_state_live_or_reload(
        &mut self,
        output_sink: &str,
        eq_enabled: bool,
    ) -> Result<(), Error> {
        // With the `bq_raw` coefficient strategy the filter TYPE lives in the
        // coefficients, not the node label, so a type change is a live push
        // just like Freq/Q/Gain. The graph topology never changes and the
        // engine is never restarted, so the sink node id (and therefore the
        // app streams' routing) stays stable across every edit.
        //
        // Startup grace: the live node proxy is captured asynchronously after
        // the module load. Previously a push that landed before the proxy
        // existed fell through to a full module unload+reload, cutting the
        // audio a *second* time right after startup. The module was just
        // loaded with the correct bands, so there is nothing to redo.
        if !self.has_live_node() {
            return Ok(());
        }
        // `eq_enabled` is the A/B compare / D-Bus `SetEqEnabled` flag. It was
        // previously hardcoded to `true`, which left the A/B compare switch
        // wired to nothing: the widget had no handler and the push always ran
        // the bands wet.
        match self.apply_live_controls(eq_enabled) {
            Ok(true) => Ok(()),
            _ => {
                let bands = self.bands.clone();
                self.update_band_coefficients(&bands, output_sink)
            }
        }
    }

    /// Build the filter-chain argument string for the current bands.
    pub fn filter_chain_args(&self, output_sink: &str, eq_enabled: bool) -> String {
        filter_chain::build_filter_chain_module_args(
            &self.bands,
            self.preamp_gain,
            eq_enabled,
            VIRTUAL_SINK_BASE,
            &format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX),
            output_sink,
            // bq_raw (raw biquad coefficients) = upstream default. The
            // filter type lives in the coefficients, so type edits stay
            // live and never force a topology reload.
            false,
        )
    }

    /// Load `libpipewire-module-filter-chain`, which creates the virtual sink
    /// (capture side), the DSP graph and the playback node as a single module.
    ///
    /// This replaces the previous per-node `create_object` calls: the
    /// filter-chain is a module, not an object factory, and the sink/output
    /// nodes are declared in its `capture.props`/`playback.props` sections.
    pub fn create_filter_chain(&mut self, output_sink: &str) -> Result<(), Error> {
        info!("Loading filter-chain module -> {}", output_sink);

        let args = self.filter_chain_args(output_sink, true);
        let c_name = CString::new(filter_chain::FILTER_CHAIN_MODULE_NAME)
            .map_err(|_| Error::CreationFailed)?;
        let c_args = CString::new(args).map_err(|_| Error::CreationFailed)?;

        // SAFETY: `self.context` outlives the module (the module is destroyed in
        // `unload_filter_chain_module` before the context drops), and both
        // strings are NUL-terminated for the duration of the call.
        let module = unsafe {
            pw_sys::pw_context_load_module(
                self.context.as_raw_ptr(),
                c_name.as_ptr(),
                c_args.as_ptr(),
                std::ptr::null_mut(),
            )
        };

        if module.is_null() {
            warn!("pw_context_load_module returned NULL");
            return Err(Error::CreationFailed);
        }

        self.filter_chain_module = Some(ModuleHandle(module));
        info!("Filter-chain module loaded");
        Ok(())
    }

    /// Tear down the loaded filter-chain module and its nodes.
    pub fn unload_filter_chain_module(&mut self) {
        if let Some(handle) = self.filter_chain_module.take() {
            // Take the raw pointer out and skip `ModuleHandle::drop`, which
            // would destroy the same module a second time (double free).
            let ptr = handle.0;
            std::mem::forget(handle);
            // SAFETY: `ptr` came from `pw_context_load_module` and is destroyed
            // exactly once, here.
            unsafe { pw_sys::pw_impl_module_destroy(ptr) };
            // Drop the cached node proxy. It is only ever captured when
            // `is_none()`, so leaving it set would keep a dead node forever and
            // every later live push would go nowhere. This matters for the
            // reload path, which is how the output device is changed.
            *self.filter_node.borrow_mut() = None;
            info!("Filter-chain module unloaded");
        }
    }

    /// Biquad control values for the current bands (bq_raw coefficients).
    pub fn native_control_values(&self, eq_enabled: bool) -> Vec<(String, f64)> {
        filter_chain::bq_raw_control_values(
            &self.bands,
            self.preamp_gain,
            eq_enabled,
            crate::core::SAMPLE_RATE,
        )
    }

    pub fn detect_output_routes(&self) -> Result<Vec<OutputRoute>, Error> {
        self.routing.detect_routes()
    }

    /// Move the EQ's output to a different device.
    ///
    /// Safe to call repeatedly with the same sink. Returns `false` if the
    /// output client could not be found or the metadata write failed.
    pub fn retarget_output(&mut self, sink_name: &str) -> bool {
        if self.routing.get_current_sink() == Some(sink_name) {
            return true;
        }
        // Validated first: reloading the filter chain is disruptive, so do not
        // start one for a device that is not there.
        if !self
            .routing
            .list_output_sinks()
            .iter()
            .any(|s| s.name == sink_name)
        {
            log::warn!("retarget_output: {sink_name} is not an available output sink");
            return false;
        }

        // The filter chain's destination is fixed at module load
        // (`playback.props.target.object`), and the output client is
        // `node.passive`, so PipeWire ignores a later metadata write — verified:
        // the write reports success and `target.object` does not move. The only
        // way is to rebuild the module with the new destination.
        let bands = self.bands.clone();
        log::info!("Reloading filter chain to output on {sink_name}");
        if let Err(e) = self.update_band_coefficients(&bands, sink_name) {
            log::warn!("retarget_output: reload failed: {e}");
            return false;
        }

        // The reload destroys and recreates `mini_eq_sink`, so its
        // `object.serial` changes and every stream we routed to the old one now
        // points at nothing. Re-route them, exactly as System EQ on does.
        //
        // The new node appears asynchronously: `pw_context_load_module` returns
        // before the registry carries the new node, so an immediate re-route
        // looks the sink up, finds nothing, and fails with "Creation failed".
        // Wait for it, bounded so a genuinely missing node cannot hang the UI.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while std::time::Instant::now() < deadline
            && self
                .routing
                .find_node_id_by_name(crate::core::VIRTUAL_SINK_BASE)
                .is_none()
        {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        match self
            .routing
            .auto_route_to_sink(crate::core::VIRTUAL_SINK_BASE)
        {
            Ok(_) => {
                self.routing.set_current_sink(sink_name);
                log::info!("EQ output now on {sink_name}");
                true
            }
            Err(e) => {
                log::warn!("retarget_output: streams not re-routed: {e}");
                false
            }
        }
    }

    /// Every real output sink (`media.class == "Audio/Sink"`), excluding the
    /// EQ's own virtual sink. Used to populate the Output dropdown, which
    /// previously listed two hardcoded labels and nothing else.
    pub fn list_output_sinks(&self) -> Vec<OutputRoute> {
        self.routing.list_output_sinks()
    }

    /// Node name of the system default output sink, if known.
    pub fn default_output_sink_name(&self) -> Option<String> {
        self.routing.get_current_sink().map(|s| s.to_string())
    }

    /// Pump pending PipeWire events without blocking.
    ///
    /// The PipeWire `MainLoop` is not run via `run()` in GUI mode; instead the
    /// UI update loop calls this each tick so registry events, sync roundtrips
    /// and module callbacks are dispatched on the GTK main thread.
    pub fn pump(&self) {
        self.mainloop.loop_().iterate(Timeout::None);
    }

    /// Name of the active physical output sink, falling back to the first
    /// The user's current default output sink node name, read from the
    /// PipeWire `default` metadata (`default.audio.sink`). This is the
    /// sink the filter-chain playback node targets so EQ'd audio reaches
    /// the speakers the user actually hears on — portable across any
    /// PipeWire machine (no hardcoded device assumptions).
    pub fn default_output_sink(&mut self) -> Option<String> {
        self.routing.default_audio_sink_name()
    }

    /// Re-read the system default output sink, bypassing the cache so a
    /// runtime change is observable.
    pub fn refresh_default_audio_sink_name(&mut self) -> Option<String> {
        self.routing.refresh_default_audio_sink_name()
    }

    pub fn auto_route_to_sink(&mut self, sink_name: &str) -> Result<(), Error> {
        self.routing.auto_route_to_sink(sink_name)
    }

    /// Clear routing targets for all playback streams (System EQ off).
    pub fn unroute_all(&mut self) -> Result<(), Error> {
        self.routing.unroute_all()
    }

    /// Update the DSP graph for a new set of bands.
    ///
    /// The native filter-chain computes coefficients at the DSP clock rate, so
    /// live edits are applied by reloading the module with fresh Freq/Q/Gain
    /// control values rather than by pushing raw coefficients.
    pub fn update_band_coefficients(
        &mut self,
        bands: &[EqBand],
        output_sink: &str,
    ) -> Result<(), Error> {
        info!("Updating band coefficients for {} bands", bands.len());

        self.bands = bands.to_vec();
        self.unload_filter_chain_module();
        self.create_filter_chain(output_sink)
    }

    pub fn set_preamp(&mut self, gain_db: f64) -> Result<(), Error> {
        // Use the shared bounds, not a literal: the Auto-Safe budget depends on
        // the floor matching `core::EQ_PREAMP_MIN_DB`.
        self.preamp_gain = gain_db.clamp(EQ_PREAMP_MIN_DB, EQ_PREAMP_MAX_DB);
        info!("Preamp gain set to {} dB", self.preamp_gain);
        Ok(())
    }

    pub fn get_bands(&self) -> &[EqBand] {
        &self.bands
    }

    pub fn get_bands_mut(&mut self) -> &mut Vec<EqBand> {
        &mut self.bands
    }

    pub fn get_preamp(&self) -> f64 {
        self.preamp_gain
    }

    pub fn is_running(&self) -> bool {
        *self.running.lock().unwrap()
    }

    pub fn stop(&mut self) {
        *self.running.lock().unwrap() = false;
        self.unload_filter_chain_module();
        info!("PipeWire backend stopping");
    }

    pub fn run(&mut self) {
        info!("Entering PipeWire main loop");
        self.mainloop.run();
    }

    pub fn quit(&mut self) {
        self.mainloop.quit();
        info!("PipeWire main loop quit");
    }
}

impl Default for PipeWireBackend {
    fn default() -> Self {
        let bands = crate::core::default_bands();
        Self::new(bands).expect("Failed to create PipeWire backend")
    }
}

// ---------------------------------------------------------------------------
// Monitor port-linking helpers (ported from the session branch).
//
// The session manager routes capture streams to the default *source* (e.g. a
// microphone) regardless of `target.object`, so to monitor an output sink we
// must link its monitor ports to our analyzer inputs explicitly with
// `pw-link`, and destroy any foreign links the manager made meanwhile. These
// helpers scrape the registry for the needed ids/names and run the CLI tools
// while pumping our loop so daemon round-trips can complete.
// ---------------------------------------------------------------------------

/// Run `pw-link` while pumping our loop, so daemon round-trips that need our
/// own objects to answer can complete. Loop-thread only.
fn run_pw_link(
    mainloop: &MainLoopRc,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use pipewire::loop_::Timeout;

    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let output = std::process::Command::new("pw-link")
            .args(&refs)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .ok();
        let _ = sender.send(output);
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match receiver.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if std::time::Instant::now() >= deadline {
            warn!("pw-link timed out after {timeout:?}");
            return None;
        }
        mainloop
            .loop_()
            .iterate(Timeout::Finite(std::time::Duration::from_millis(20)));
    }
}

/// Run `pw-cli` with a timeout, returning `None` on spawn failure or timeout.
fn run_pw_cli(args: &[&str], timeout: std::time::Duration) -> Option<std::process::Output> {
    let child = std::process::Command::new("pw-cli")
        .args(args)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .ok()?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let result = child.wait_with_output();
        let _ = sender.send(result);
    });
    match receiver.recv_timeout(timeout) {
        Ok(Ok(output)) => Some(output),
        Ok(Err(e)) => {
            warn!("pw-cli failed: {e}");
            None
        }
        Err(_) => {
            warn!("pw-cli timed out after {timeout:?}, leaving child to exit");
            None
        }
    }
}

/// Run `pw-cli` while pumping our loop, for calls whose daemon round-trip
/// needs our own objects to answer (destroy involving our stream). Loop only.
fn run_pw_cli_pumped(
    mainloop: &MainLoopRc,
    args: &[&str],
    timeout: std::time::Duration,
) -> Option<std::process::Output> {
    use pipewire::loop_::Timeout;

    let owned: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let refs: Vec<&str> = owned.iter().map(|s| s.as_str()).collect();
        let result = run_pw_cli(&refs, timeout);
        let _ = sender.send(result);
    });
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match receiver.try_recv() {
            Ok(result) => return result,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => return None,
            Err(std::sync::mpsc::TryRecvError::Empty) => {}
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        mainloop
            .loop_()
            .iterate(Timeout::Finite(std::time::Duration::from_millis(20)));
    }
}

/// (node.name, id) records scraped from a registry snapshot. Loop-thread only:
/// pumps a few iterations so globals arrive before reading.
fn registry_node_names(mainloop: &MainLoopRc, core: &CoreRc) -> Result<Vec<(String, u32)>, Error> {
    use pipewire::loop_::Timeout;
    use std::sync::{Arc, Mutex};

    let names: Arc<Mutex<Vec<(String, u32)>>> = Arc::new(Mutex::new(Vec::new()));
    let names_clone = names.clone();
    let registry = core.get_registry()?;
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pipewire::types::ObjectType::Node {
                if let Some(props) = &global.props {
                    let name = props.get("node.name").unwrap_or("").to_string();
                    if !name.is_empty() {
                        names_clone.lock().unwrap().push((name, global.id));
                    }
                }
            }
        })
        .register();
    for _ in 0..5 {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(std::time::Duration::from_millis(20)));
    }
    Ok(names.lock().unwrap().drain(..).collect())
}

/// Port record scraped from a registry snapshot.
struct PortRecord {
    node_id: u32,
    port_name: String,
    direction: String,
    path: String,
}

/// Snapshot ports from our registry. Loop-thread only.
fn registry_port_snapshot(mainloop: &MainLoopRc, core: &CoreRc) -> Result<Vec<PortRecord>, Error> {
    use pipewire::loop_::Timeout;
    use std::sync::{Arc, Mutex};

    let ports: Arc<Mutex<Vec<PortRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let ports_clone = ports.clone();
    let registry = core.get_registry()?;
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pipewire::types::ObjectType::Port {
                if let Some(props) = &global.props {
                    let get = |k: &str| props.get(k).unwrap_or("").to_string();
                    if let (Some(node_id), Some(_port_id)) = (
                        get("node.id").parse::<u32>().ok(),
                        get("port.id").parse::<u32>().ok(),
                    ) {
                        ports_clone.lock().unwrap().push(PortRecord {
                            node_id,
                            port_name: get("port.name"),
                            direction: get("port.direction"),
                            path: get("object.path"),
                        });
                    }
                }
            }
        })
        .register();
    for _ in 0..5 {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(std::time::Duration::from_millis(20)));
    }
    Ok(ports.lock().unwrap().drain(..).collect())
}

/// Link record scraped from a registry snapshot.
struct LinkRecord {
    id: u32,
    output_node: u32,
    input_node: u32,
}

/// Snapshot links from our registry. Loop-thread only.
fn registry_link_snapshot(mainloop: &MainLoopRc, core: &CoreRc) -> Result<Vec<LinkRecord>, Error> {
    use pipewire::loop_::Timeout;
    use std::sync::{Arc, Mutex};

    let links: Arc<Mutex<Vec<LinkRecord>>> = Arc::new(Mutex::new(Vec::new()));
    let links_clone = links.clone();
    let registry = core.get_registry()?;
    let _listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ == pipewire::types::ObjectType::Link {
                if let Some(props) = &global.props {
                    let get = |k: &str| props.get(k).unwrap_or("").to_string();
                    if let (Some(output_node), Some(input_node)) = (
                        get("link.output.node").parse::<u32>().ok(),
                        get("link.input.node").parse::<u32>().ok(),
                    ) {
                        links_clone.lock().unwrap().push(LinkRecord {
                            id: global.id,
                            output_node,
                            input_node,
                        });
                    }
                }
            }
        })
        .register();
    for _ in 0..5 {
        mainloop
            .loop_()
            .iterate(Timeout::Finite(std::time::Duration::from_millis(20)));
    }
    Ok(links.lock().unwrap().drain(..).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard for the silent no-op push.
    ///
    /// The pod used to be built with `flags = 0` and no leading item count, so
    /// PipeWire discarded every control without error and the EQ was frozen at
    /// its load-time curve. `pipewire_backend.rs` had no tests, so nothing
    /// caught it. These assertions pin the wire format that filter-graph
    /// actually parses.
    /// Pins the wire format that `parse_params()` actually parses.
    ///
    /// The payload must be `Struct((String, value)*)` with no count and no
    /// dict flag: `parse_params` does `push_struct` and then reads a string,
    /// and **breaks** on the first field it cannot read as a string. An earlier
    /// version emitted `SPA_POD_PROP_FLAG_HINT_DICT` plus a leading
    /// `Int: n_items`; that leading Int made the parser abort at field zero,
    /// silently dropping every control and leaving the EQ completely inert.
    /// This test exists to stop that shape coming back.
    #[test]
    fn props_pod_is_a_bare_struct_of_string_value_pairs() {
        let controls = vec![
            ("band_l_0:b0".to_string(), 1.5),
            ("band_l_0:b1".to_string(), 0.25),
        ];
        let data = build_props_controls_pod_bytes(&controls).expect("pod should build");
        let pod = Pod::from_bytes(&data).expect("pod bytes should parse");

        let obj = pod.as_object().expect("pod should be an Object");
        let props: Vec<_> = obj.props().collect();
        assert_eq!(props.len(), 1, "expected exactly one property");

        let prop = props[0];
        assert_eq!(
            prop.key().0,
            PARAM_PROPS,
            "property key must be SPA_PROP_params (0x80001)"
        );

        let st = prop
            .value()
            .as_struct()
            .expect("property value should be a Struct");
        let fields: Vec<_> = st.fields().collect();
        assert_eq!(
            fields.len(),
            controls.len() * 2,
            "expected exactly (String, value) pairs"
        );
        // The payload MUST start with a string; parse_params breaks otherwise.
        assert!(
            fields[0].is_string(),
            "first field must be a String control name, got {:?}",
            fields[0].type_()
        );
        assert!(fields[1].is_double(), "value must be a Double");
    }

    /// Every control name must survive serialisation, and none may be preceded
    /// by a count or tag.
    #[test]
    fn props_pod_carries_every_control_name() {
        let controls: Vec<(String, f64)> = (0..16)
            .flat_map(|i| ["l", "r"].map(move |s| (format!("band_{s}_{i}:b0"), i as f64 * 0.5)))
            .collect();
        let data = build_props_controls_pod_bytes(&controls).expect("pod should build");
        let pod = Pod::from_bytes(&data).expect("pod bytes should parse");
        let raw = pod.as_bytes();
        for (name, _) in &controls {
            assert!(
                raw.windows(name.len()).any(|w| w == name.as_bytes()),
                "{name} missing from the serialised payload"
            );
        }
        let obj = pod.as_object().expect("object");
        let prop = obj.props().next().expect("one property");
        let st = prop.value().as_struct().expect("struct");
        let fields: Vec<_> = st.fields().collect();
        assert_eq!(
            fields.len(),
            controls.len() * 2,
            "no count or tag may precede the string/value pairs"
        );
        // Every other field must be the control name: a String pod carrying
        // the expected text.
        for (i, (name, _)) in controls.iter().enumerate() {
            let field = fields[i * 2];
            assert!(field.is_string(), "field {i} should be a String");
            let bytes = field.as_bytes();
            assert!(
                bytes.windows(name.len()).any(|w| w == name.as_bytes()),
                "field {i} does not carry {name:?}"
            );
        }
    }
}
