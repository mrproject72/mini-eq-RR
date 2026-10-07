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

use crate::core::{EqBand, VIRTUAL_SINK_BASE};
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

/// One running EQ chain for one physical output device (multi-chain EQ).
///
/// Phase 1: registry + primitives alongside the legacy single-chain fields;
/// callers migrate in later phases. Each device keeps its own module, live
/// node proxy, bands and preamp, so devices process audio independently and
/// simultaneously.
pub struct DeviceChain {
    /// Physical sink this chain plays out to.
    pub physical_sink: String,
    /// Virtual sink node (`core::eq_virtual_sink_for(physical_sink)`).
    pub virtual_sink: String,
    /// Filter-chain playback node (`core::eq_filter_output_for`).
    pub filter_output: String,
    /// The loaded module (private: unload happens by removing the whole
    /// chain, whose `Drop` destroys the module exactly once).
    module: Option<ModuleHandle>,
    pub bands: Vec<EqBand>,
    pub preamp_gain: f64,
    /// Live proxy for this chain's virtual sink node (same role as the
    /// legacy `filter_node`, per device).
    pub filter_node: Rc<RefCell<Option<Node>>>,
}

pub struct PipeWireBackend {
    mainloop: MainLoopRc,
    context: ContextRc,
    core: CoreRc,
    routing: RoutingEngine,
    /// Per-device EQ chains, keyed by physical sink node name. Shared with
    /// the registry listener (which fills each chain's live node proxy), so
    /// it lives behind `Rc<RefCell>`.
    ///
    /// Borrow rule: never hold `borrow_mut` on this map across a
    /// pump/roundtrip/module-load — the listener callback borrows it to
    /// match newcomers and would panic on a live `borrow_mut`.
    device_chains: Rc<RefCell<std::collections::HashMap<String, DeviceChain>>>,
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
    pub fn new() -> Result<Self, Error> {
        info!("Initializing PipeWire backend");

        pipewire::init();

        let mainloop = MainLoopRc::new(None)?;
        let context = ContextRc::new(&mainloop, None)?;
        let core = context.connect_rc(None)?;

        info!("Connected to PipeWire server");

        let mut routing = RoutingEngine::new(core.clone(), mainloop.clone());
        // Bind the `default` metadata NOW, not on the first routing op:
        // main.rs needs the default sink immediately to create the engine,
        // and the lazy path left it `None` so the engine never started.
        match routing.prime_default_sink() {
            Ok(()) => info!(
                "Default output sink: {:?}",
                routing.default_audio_sink_name()
            ),
            Err(e) => warn!("Could not bind default metadata at startup: {e}"),
        }
        let analyzer =
            crate::analyzer::OutputSpectrumAnalyzer::new(core.clone(), crate::core::SAMPLE_RATE)?;

        let backend = PipeWireBackend {
            mainloop,
            context,
            core,
            routing,
            device_chains: Rc::new(RefCell::new(std::collections::HashMap::new())),
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
        let device_chains = self.device_chains.clone();
        let registry_for_cb = registry.clone();
        let listener = registry.add_listener_local();
        let listener = listener.global(move |global| {
            debug!("Registry global: id={} type={:?}", global.id, global.type_);
            if global.type_.to_str() != pipewire::types::ObjectType::Node.to_str() {
                return;
            }
            // Per-device chains: capture the live node proxy for whichever
            // chain owns this virtual sink name (all of ours start with the
            // legacy `mini_eq_sink` prefix).
            let name = global
                .props
                .as_ref()
                .and_then(|p| p.get("node.name"))
                .unwrap_or("");
            if name.starts_with(VIRTUAL_SINK_BASE) {
                let chains = device_chains.borrow();
                if let Some(chain) = chains.values().find(|c| c.virtual_sink == name) {
                    if chain.filter_node.borrow().is_none() {
                        match registry_for_cb.bind::<Node, _>(global) {
                            Ok(node) => {
                                info!(
                                    "Captured live device filter node proxy: {name} (id={})",
                                    global.id
                                );
                                *chain.filter_node.borrow_mut() = Some(node);
                            }
                            Err(e) => warn!("Failed to bind device filter node: {e}"),
                        }
                    }
                }
            }
        });
        let _listener = listener.register();
        Ok(_listener)
    }

    // ---------------------------------------------------------------------
    // Per-device EQ chains (multi-chain EQ)
    // ---------------------------------------------------------------------

    /// Virtual sink node name for a physical device. Pure constructor so
    /// routing and the window can agree without a backend handle.
    pub fn device_virtual_sink(physical_sink: &str) -> String {
        crate::core::eq_virtual_sink_for(physical_sink)
    }

    /// True while a chain exists for this physical device.
    pub fn has_device_chain(&self, physical_sink: &str) -> bool {
        self.device_chains.borrow().contains_key(physical_sink)
    }

    /// Create the chain for a physical device if missing, loading
    /// `libpipewire-module-filter-chain` with the given bands (fresh devices
    /// start neutral; the window applies the linked preset afterwards).
    ///
    /// Returns `true` when the chain was created by this call. Never pumps
    /// or round-trips while holding the map borrow (see the field docs).
    pub fn ensure_device_chain(
        &mut self,
        physical_sink: &str,
        bands: Vec<EqBand>,
    ) -> Result<bool, Error> {
        if physical_sink.is_empty() {
            return Err(Error::CreationFailed);
        }
        if self.device_chains.borrow().contains_key(physical_sink) {
            return Ok(false);
        }
        // Route must know the new sink before any stream aims at it.
        self.routing.register_eq_sink(
            &crate::core::eq_virtual_sink_for(physical_sink),
            physical_sink,
        );
        let virtual_sink = Self::device_virtual_sink(physical_sink);
        let filter_output = crate::core::eq_filter_output_for(physical_sink);
        let args = filter_chain::build_filter_chain_module_args(
            &bands,
            0.0,
            true,
            &virtual_sink,
            &filter_output,
            physical_sink,
            false,
        );
        let c_name = CString::new(filter_chain::FILTER_CHAIN_MODULE_NAME)
            .map_err(|_| Error::CreationFailed)?;
        let c_args = CString::new(args).map_err(|_| Error::CreationFailed)?;
        // SAFETY: same contract as `create_filter_chain`.
        let module = unsafe {
            pw_sys::pw_context_load_module(
                self.context.as_raw_ptr(),
                c_name.as_ptr(),
                c_args.as_ptr(),
                std::ptr::null_mut(),
            )
        };
        if module.is_null() {
            warn!("pw_context_load_module returned NULL for {physical_sink}");
            return Err(Error::CreationFailed);
        }
        self.device_chains.borrow_mut().insert(
            physical_sink.to_string(),
            DeviceChain {
                physical_sink: physical_sink.to_string(),
                virtual_sink: virtual_sink.clone(),
                filter_output,
                module: Some(ModuleHandle(module)),
                bands,
                preamp_gain: 0.0,
                filter_node: Rc::new(RefCell::new(None)),
            },
        );
        info!("Device filter-chain module loaded for {physical_sink} ({virtual_sink})");
        Ok(true)
    }

    /// Tear down one device's chain. Its streams must have been handed back
    /// first (per-device unroute); anything still pointed at the destroyed
    /// sink is WirePlumber's to rescue.
    /// Physical sinks with a live EQ chain.
    pub fn device_physical_sinks(&self) -> Vec<String> {
        let mut names: Vec<String> = self.device_chains.borrow().keys().cloned().collect();
        names.sort();
        names
    }

    /// One device's stored bands (if its chain exists).
    pub fn device_bands(&self, physical_sink: &str) -> Option<Vec<EqBand>> {
        self.device_chains
            .borrow()
            .get(physical_sink)
            .map(|c| c.bands.clone())
    }

    /// True while at least one physical sink has routed streams.
    pub fn any_eq_active(&self) -> bool {
        self.device_physical_sinks()
            .iter()
            .any(|dev| self.routing.has_routed_streams_for(dev))
    }

    /// Drop a device's chain AND unregister it from the router.
    pub fn drop_device_chain(&mut self, physical_sink: &str) -> bool {
        let removed = match self.device_chains.borrow_mut().remove(physical_sink) {
            Some(chain) => {
                let had_module = chain.module.is_some();
                drop(chain);
                info!(
                    "Device filter-chain module unloaded for {physical_sink} (had module: {had_module})"
                );
                true
            }
            None => false,
        };
        if removed {
            self.routing
                .unregister_eq_sink(&crate::core::eq_virtual_sink_for(physical_sink));
        }
        removed
    }

    /// Replace one device's bands (the DSP push happens via
    /// `device_push_live`, like the legacy path).
    pub fn set_device_bands(&mut self, physical_sink: &str, bands: Vec<EqBand>) {
        if let Some(chain) = self.device_chains.borrow_mut().get_mut(physical_sink) {
            chain.bands = bands;
        }
    }

    /// Replace one device's preamp gain.
    pub fn set_device_preamp(&mut self, physical_sink: &str, preamp_db: f64) {
        if let Some(chain) = self.device_chains.borrow_mut().get_mut(physical_sink) {
            chain.preamp_gain = preamp_db;
        }
    }

    /// True once this device's live filter node proxy has been captured.
    pub fn has_device_live_node(&self, physical_sink: &str) -> bool {
        self.device_chains
            .borrow()
            .get(physical_sink)
            .is_some_and(|c| c.filter_node.borrow().is_some())
    }

    /// Push one device's bands/preamp to its live filter node.
    /// Returns `Ok(false)` when the proxy has not arrived yet (caller should
    /// retry later, like the legacy startup grace).
    pub fn device_push_live(&self, physical_sink: &str, eq_enabled: bool) -> Result<bool, Error> {
        let chains = self.device_chains.borrow();
        let Some(chain) = chains.get(physical_sink) else {
            return Err(Error::CreationFailed);
        };
        let node_borrow = chain.filter_node.borrow();
        let node = match node_borrow.as_ref() {
            Some(n) => n,
            None => return Ok(false),
        };
        let controls = filter_chain::bq_raw_control_values(
            &chain.bands,
            chain.preamp_gain,
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
            "device_push_live: pushed {} control(s) to {}",
            controls.len(),
            chain.virtual_sink
        );
        Ok(true)
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
    /// The sink the monitor taps. A pinned monitor device (from
    /// `output-monitor` in the config) wins; otherwise the chain's current
    /// output, or the system default when the chain has not chosen one yet.
    ///
    /// `pinned` is the sink name the user chose in the Monitor device dropdown,
    /// if any; `None` means "follow the EQ output".
    pub fn resolve_monitor_target(&self, pinned: Option<&str>) -> String {
        if let Some(name) = pinned {
            return name.to_string();
        }
        if let Some(pinned) = crate::core::output_monitor_sink() {
            return pinned;
        }
        // The chain's PHYSICAL output, not `current_sink`: that follows the
        // virtual sink while routing is on, which used to send the monitor to
        // `mini_eq_sink` itself instead of the equalised device.
        let current = self.routing.chain_output_sink().unwrap_or_default();
        if !current.is_empty() {
            return current;
        }
        self.routing
            .current_default_audio_sink()
            .unwrap_or_default()
    }

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

    pub fn detect_output_routes(&self) -> Result<Vec<OutputRoute>, Error> {
        self.routing.detect_routes()
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
    /// Record which physical sink the filter chain is feeding, without
    /// touching any stream. Used once at startup: the chain is created
    /// directly onto the default sink, and the routing engine must know
    /// that so the monitor resolves to a real device instead of nothing.
    pub fn set_current_sink(&mut self, sink_name: &str) {
        self.routing.set_current_sink(sink_name);
    }
    /// The user's current default output sink node name, read from the
    /// PipeWire `default` metadata (`default.audio.sink`). This is the
    /// sink the filter-chain playback node targets so EQ'd audio reaches
    /// the speakers the user actually hears on — portable across any
    /// PipeWire machine (no hardcoded device assumptions).
    pub fn default_output_sink(&self) -> Option<String> {
        self.routing.default_audio_sink_name()
    }

    /// The system default output, updated by the metadata `property`
    /// listener in real time. Returns `None` when nothing changed since the
    /// last call, so the 500 ms default-sink timer can act on a flag read
    /// instead of pumping the PipeWire loop.
    pub fn take_default_sink_change(&self) -> Option<String> {
        self.routing.take_default_sink_change()
    }

    /// Which streams the EQ reaches when it is on.
    pub fn output_mode(&self) -> crate::core::OutputRoutingMode {
        self.routing.output_mode()
    }

    pub fn set_output_mode(&mut self, mode: crate::core::OutputRoutingMode) {
        self.routing.set_output_mode(mode);
    }

    /// True while the playback streams are routed through the EQ.
    pub fn is_routed(&self) -> bool {
        self.routing.is_routed()
    }

    /// True while at least one playback stream is pointed into the EQ.
    /// See [`RoutingEngine::has_routed_streams`].
    pub fn has_routed_streams(&self) -> bool {
        self.routing.has_routed_streams()
    }

    /// Scoped reconcile for one device's chain (used when the routing mode
    /// narrows to Selected).
    pub fn rescope_device(&mut self, physical_sink: &str) -> Result<(usize, usize), Error> {
        self.routing.rescope_device(physical_sink)
    }

    /// Per-device stream count > 0 helper (header switch state).
    pub fn has_routed_streams_for(&self, physical_sink: &str) -> bool {
        self.routing.has_routed_streams_for(physical_sink)
    }

    /// Per-device unroute (EQ off for that device).
    pub fn unroute_device(
        &mut self,
        physical_sink: &str,
        fallback: Option<&str>,
    ) -> Result<(), Error> {
        self.routing.unroute_device(physical_sink, fallback)
    }

    pub fn auto_route_to_sink(&mut self, sink_name: &str) -> Result<(), Error> {
        self.routing.auto_route_to_sink(sink_name)
    }

    /// Streams routed by the most recent auto-route call. See
    /// [`RoutingEngine::last_routed_count`].
    pub fn last_routed_count(&self) -> usize {
        self.routing.last_routed_count()
    }

    /// Reconcile routed streams with the current output device/mode after a
    /// device switch or a narrowing to Selected. See
    /// [`RoutingEngine::rescope_routing`].
    pub fn rescope_routing(&mut self) -> Result<(usize, usize), Error> {
        self.routing.rescope_routing()
    }

    /// Hand playback streams back where they were before the EQ took them
    /// (System EQ off).
    ///
    /// `fallback_sink` is only consulted for streams this process never routed
    /// -- there is no record of where they came from, so the sink the EQ was
    /// feeding is the best available answer.
    pub fn unroute_all(&mut self, fallback_sink: Option<&str>) -> Result<(), Error> {
        self.routing.unroute_all(fallback_sink)
    }

    /// Hand playback streams back to a real output, on the way out.
    ///
    /// The destination is the sink this filter chain was feeding, so the audio
    /// lands where it did before the EQ was routed. If that sink is gone, fall
    /// back to whatever the system default is at this instant, and only then to
    /// clearing the targets.
    ///
    /// This runs on every exit path, including SIGTERM, because the symptom it
    /// prevents is the worst one this app can cause: silence after the user
    /// closes it.
    pub fn restore_routing_on_exit(&mut self, chain_output: Option<&str>) -> Result<(), Error> {
        let candidates: Vec<String> = chain_output
            .map(|s| vec![s.to_string()])
            .unwrap_or_default()
            .into_iter()
            .chain(self.default_output_sink())
            .collect();
        for sink in candidates {
            if sink.is_empty() || sink.contains(crate::core::VIRTUAL_SINK_BASE) {
                continue;
            }
            info!("exit: restoring playback streams (fallback {sink})");
            // Recorded targets win; `sink` is only the fallback.
            return self.routing.unroute_all(Some(&sink));
        }
        warn!("exit: no real output sink to fall back to; restoring recorded targets only");
        self.routing.unroute_all(None)
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
