use std::cell::Cell;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use log::{debug, info, warn};
use pipewire::{
    Error,
    core::{CoreRc, PW_ID_CORE},
    link::Link,
    loop_::Timeout,
    main_loop::MainLoopRc,
    metadata::Metadata,
    properties::properties,
    proxy::ProxyT,
    stream::StreamBox,
    types::ObjectType,
};

use crate::core::{FILTER_OUTPUT_SUFFIX, OUTPUT_CLIENT_NAME, SAMPLE_RATE, VIRTUAL_SINK_BASE};

/// Parse a PipeWire metadata value that is a JSON object carrying a `name`
/// field (e.g. the `default.audio.sink` value
/// `{"name":"alsa_output.pci-...","priority":...}`) into the bare node
/// name. Falls back to the raw string if it is not JSON, and returns
/// `None` for empty/non-object payloads. Mirrors upstream
/// `parse_metadata_node_name`.
/// PipeWire's media class for a sink node. Upstream compares against the same
/// string (`AUDIO_SINK`).
pub const AUDIO_SINK_MEDIA_CLASS: &str = "Audio/Sink";

/// Best available human label for a sink: its description, else its node name.
pub fn display_label(description: &str, name: &str) -> String {
    let d = description.trim();
    if !d.is_empty() {
        return d.to_string();
    }
    name.to_string()
}

pub fn parse_metadata_node_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    if value.is_empty() {
        return None;
    }
    match serde_json::from_str::<serde_json::Value>(value) {
        Ok(serde_json::Value::Object(map)) => map
            .get("name")
            .and_then(|n| n.as_str())
            .map(|s| s.to_string()),
        Ok(_) => None,
        Err(_) => Some(value.to_string()),
    }
}

#[derive(Debug, Clone)]
pub struct OutputRoute {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct RouteInfo {
    pub route_id: u32,
    pub source_node: u32,
    pub target_node: u32,
    pub link_id: u32,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub id: u32,
    pub node_id: u32,
    pub name: String,
    pub media_type: String,
    pub media_role: String,
    pub channels: u32,
    pub rate: u32,
    pub target_sink: Option<String>,
    pub active: bool,
}

/// One playback stream, with the properties upstream's router filter on.
///
/// Upstream reads `node.name`, `application.name`, `media.role` and
/// `node.dont-move` for every stream (`PipeWireNode` +
/// `iter_routable_output_streams`). The port used to collect only `node.name`,
/// which is why it could not tell a desktop sound from programme material.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamNode {
    pub id: u32,
    pub node_name: String,
    pub app_name: String,
    pub media_role: String,
    /// `node.dont-move`: upstream refuses to route these
    /// (`move_stream_to_target` raises on it).
    pub dont_move: bool,
}

pub struct RoutingEngine {
    core: CoreRc,
    mainloop: MainLoopRc,
    routes: Arc<Mutex<Vec<OutputRoute>>>,
    links: Arc<Mutex<Vec<RouteInfo>>>,
    streams: Arc<Mutex<HashMap<u32, StreamInfo>>>,
    routing_table: Arc<Mutex<HashMap<u32, u32>>>,
    auto_route: bool,
    /// Whether the playback streams are currently routed through the EQ.
    ///
    /// Distinct from `current_sink`, which is where the *chain* points. Upstream
    /// keeps the same distinction (`RoutingEngine.routed` vs `output_sink`) and
    /// uses it to gate the re-route after an engine restart
    /// (`restart_engine` only re-routes `if self.routed`).
    routed: bool,
    /// Which streams the EQ reaches when it is on. See
    /// [`crate::core::OutputRoutingMode`]. Stored separately from `routed`:
    /// `routed` is the on/off of the machinery, this is *which* streams it takes.
    /// Toggling the switch off and back on restores the remembered mode rather
    /// than always re-routing everything.
    output_mode: crate::core::OutputRoutingMode,
    current_sink: Option<String>,
    /// Physical sinks the user switched EQ on for (per-device on/off).
    /// A device stays here until its EQ is switched off, even with zero
    /// streams: streams that appear later are adopted event-driven (see
    /// `streams_dirty` / `adopt_new_streams`). Without this, enabling EQ on
    /// an empty device silently did nothing forever -- the reported "EQ on
    /// but the late stream is not processed" bug.
    eq_wanted: HashSet<String>,
    /// Physical sink currently selected in the UI. Under Reroute, late
    /// streams go into this device's chain.
    selected_sink: Option<String>,
    /// Set when streams may need (re-)adoption: a playback node appears in
    /// the registry, or a `target.node`/`target.object` property changes
    /// (user moved a stream in a mixer, WirePlumber re-homed one, or we
    /// wrote one ourselves). The window's pump tick drains it into a single
    /// `adopt_new_streams` pass -- event-driven, zero cost when idle, no
    /// polling timer anywhere.
    streams_dirty: Rc<Cell<bool>>,
    /// Streams routed by the most recent auto-route call (see
    /// `last_routed_count`). Plain integer: only ever touched on the GTK
    /// thread via `&mut self` routing calls.
    last_routed_count: usize,
    /// Virtual sink node names of live per-device EQ chains
    /// (`core::eq_virtual_sink_for`), registered by the backend as chains
    /// are created/destroyed. Lets scope/allowed logic cover every device
    /// sink, not just the legacy singleton.
    eq_sinks: HashMap<String, String>,
    /// The PHYSICAL sink the filter chain plays out to (what you hear).
    ///
    /// Distinct from `current_sink`, which `auto_route_to_sink` points at the
    /// VIRTUAL sink (the streams' entry point) while routing is on. Reading
    /// the monitor tap from `current_sink` therefore resolved to `mini_eq_sink`
    /// itself whenever the EQ was on -- the monitor listened to the virtual
    /// sink instead of the device, and a device switch followed by re-enable
    /// left it there. Only physical sinks are recorded here.
    chain_output_sink: Option<String>,
    virtual_sink_name: String,
    /// Handle to the PipeWire `default` metadata object, bound from the
    /// registry global whose `metadata.name == "default"`. Used to set each
    /// stream's `target.node`/`target.object` so WirePlumber performs the
    /// routing (the pavucontrol mechanism). Mirrors upstream
    /// `Pwg.Metadata.new(core, "default")` + `set_stream_target`.
    default_metadata: Option<Metadata>,
    /// The user's current default audio sink node name, captured from the
    /// `default` metadata key `default.audio.sink` (a JSON object whose
    /// `name` field is the sink's `node.name`). This is the portable,
    /// machine-agnostic way to learn the real default output on any
    /// PipeWire system — mirrors upstream `DEFAULT_AUDIO_SINK_KEY`.
    default_audio_sink: Rc<RefCell<Option<String>>>,
    /// The user's *configured* default sink (`default.configured.audio.sink`),
    /// used as a fallback when the runtime default is unset.
    configured_audio_sink: Rc<RefCell<Option<String>>>,
    /// Set by the metadata `property` listener the instant
    /// `default.audio.sink` changes. The value is already in
    /// `default_audio_sink` above; this flag is the signal. It is the only
    /// reason the 500 ms default-sink poll exists -- the listener fires in
    /// real time, so polling is pure waste and the pump in
    /// `refresh_default_audio_sink_name` is a 50 ms stall per tick that
    /// cannot observe anything the listener has not already delivered.
    default_sink_changed: Rc<RefCell<bool>>,
    /// Keeps the metadata `property` listeners alive (a dropped listener
    /// unregisters itself, so these must outlive the bind callback).
    metadata_listeners: Rc<RefCell<Vec<pipewire::metadata::MetadataListener>>>,
    /// Every `target.*` property we see, as `(subject, key) -> (type, value)`.
    /// The server replays current properties when our listener binds, so this
    /// holds each stream's existing routing target before we touch it. The only
    /// way to get at that is upstream's `metadata.dup_value(subject, key)`; the
    /// Rust `Metadata` wrapper has no getter, so the listener is the reader.
    target_cache: Arc<Mutex<HashMap<(u32, String), (Option<String>, Option<String>)>>>,
    /// What each stream pointed at before we routed it into the EQ, captured at
    /// route time. Upstream keeps this in `PipeWireStreamRouter`
    /// (`routed_stream_targets`) and restores it verbatim on disable; clearing
    /// the properties instead leaves the destination to WirePlumber's policy,
    /// which re-resolves from scratch and takes a visible moment of silence.
    routed_targets: Arc<Mutex<HashMap<u32, StreamTarget>>>,
    /// Which of our EQ sinks a stream is link-held into (stream id -> EQ
    /// sink node name). Metadata writes are silently discarded for sandboxed
    /// clients, so without real links the adoption would be bookkeeping only.
    /// Links are created with the `pw-link` tool (port-explicit refs, the
    /// same mechanism as the monitor taps): in-process link-factory objects
    /// for stream ports demonstrably never instantiate server-side, while
    /// `pw-link` links go active and persist for 30+ minutes, including from
    /// inside the sandbox. Destroyed when the stream is handed back,
    /// unrouted, or the chain rebuilds. The EQ name travels with the entry
    /// so per-device teardown only touches its own chain's links. Link ids
    /// are deliberately NOT tracked: teardown destroys whatever is live for
    /// the stream into that EQ (ours -- the session manager never links
    /// there while our metadata is absent).
    fallback_links: Arc<Mutex<HashMap<u32, String>>>,
    /// Streams whose metadata write is awaiting read-back confirmation:
    /// stream id -> (EQ node id, EQ serial, EQ name, act no earlier than).
    /// Our own write's property echo has been observed to arrive after the
    /// write's roundtrip completed, so acting on the first mismatch linked
    /// spuriously (and severed the session manager's in-flight path). Entries
    /// are re-checked on the pump tick and only linked once the deadline
    /// passes unmatched. Cleared together with `fallback_links` on every
    /// release path -- a handed-back stream must never be linked afterwards.
    pending_verify: Vec<(u32, u32, String, String, std::time::Instant)>,
    /// Last full heal pass. Heal does registry snapshots, so it runs at most
    /// ~1/s even though the pump tick calls it every 10 ms; the orphan-drop
    /// stays unthrottled (one lock check).
    last_heal: Rc<Cell<Option<std::time::Instant>>>,
    /// `(node id, object serial)` the EQ was last routed to. Lets `unroute_all`
    /// tell "nothing is on the EQ any more" from "streams are still there" by
    /// reading their targets, so a repeated call is a no-op instead of another
    /// round of writes.
    last_route_target: Arc<Mutex<Option<(u32, String)>>>,
}

/// A stream's routing target as stored in the `default` metadata: the node id
/// and the object serial, each with its own metadata type.
///
/// The types are part of the value. Restoring `target.node` without the
/// `Spa:Id` type it was written with makes WirePlumber read the restored target
/// as something else, so a "faithful" restore still ends up re-resolving.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct StreamTarget {
    pub target_node: Option<String>,
    pub target_node_type: Option<String>,
    pub target_object: Option<String>,
    pub target_object_type: Option<String>,
}

impl StreamTarget {
    /// True when nothing was recorded, i.e. the stream had no explicit target
    /// and may be sent back to the default.
    pub fn is_empty(&self) -> bool {
        self.target_node.is_none() && self.target_object.is_none()
    }
}

impl RoutingEngine {
    pub fn new(core: CoreRc, mainloop: MainLoopRc) -> Self {
        info!("Initializing RoutingEngine");

        RoutingEngine {
            core,
            mainloop,
            routes: Arc::new(Mutex::new(Vec::new())),
            links: Arc::new(Mutex::new(Vec::new())),
            streams: Arc::new(Mutex::new(HashMap::new())),
            routing_table: Arc::new(Mutex::new(HashMap::new())),
            auto_route: false,
            routed: false,
            current_sink: None,
            chain_output_sink: None,
            eq_wanted: HashSet::new(),
            selected_sink: None,
            streams_dirty: Rc::new(Cell::new(false)),
            last_routed_count: 0,
            eq_sinks: HashMap::new(),
            output_mode: crate::core::OutputRoutingMode::Selected,
            virtual_sink_name: format!("{}.source", VIRTUAL_SINK_BASE),
            default_metadata: None,
            default_audio_sink: Rc::new(RefCell::new(None)),
            configured_audio_sink: Rc::new(RefCell::new(None)),
            default_sink_changed: Rc::new(RefCell::new(false)),
            metadata_listeners: Rc::new(RefCell::new(Vec::new())),
            target_cache: Arc::new(Mutex::new(HashMap::new())),
            routed_targets: Arc::new(Mutex::new(HashMap::new())),
            fallback_links: Arc::new(Mutex::new(HashMap::new())),
            pending_verify: Vec::new(),
            last_heal: Rc::new(Cell::new(None)),
            last_route_target: Arc::new(Mutex::new(None)),
        }
    }

    /// Pump the main loop until the server acknowledges a `sync` roundtrip.
    ///
    /// Registry `global` events are queued on the PipeWire socket, so a listener
    /// that is registered and then immediately inspected sees nothing. This is
    /// the barrier that makes `detect_routes`/`scan_streams`/`create_link`
    /// actually observe the objects they registered callbacks for.
    fn roundtrip(&self) -> Result<(), Error> {
        let done = Rc::new(Cell::new(false));
        let pending = self.core.sync(0)?;

        let done_clone = done.clone();
        let loop_clone = self.mainloop.clone();
        let _listener = self
            .core
            .add_listener_local()
            .done(move |id, seq| {
                if id == PW_ID_CORE && seq == pending {
                    done_clone.set(true);
                    loop_clone.quit();
                }
            })
            .register();

        // The server may already be idle; the finite timeout bounds the wait so
        // a missing `done` event cannot hang the caller forever.
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while !done.get() && std::time::Instant::now() < deadline {
            self.mainloop
                .loop_()
                .iterate(Timeout::Finite(Duration::from_millis(100)));
        }

        if !done.get() {
            warn!("PipeWire roundtrip timed out");
        }

        Ok(())
    }

    pub fn with_auto_route(mut self, auto: bool) -> Self {
        self.auto_route = auto;
        self
    }

    /// Every real output sink: nodes with `media.class == "Audio/Sink"`,
    /// excluding our own virtual sink.
    ///
    /// The previous implementation matched on `node.name` containing
    /// "audio.sink" / "output" / "analog". That is wrong in both directions: it
    /// misses devices whose names contain none of those substrings (USB,
    /// bluetooth, network sinks), and it matches *input* nodes such as
    /// `alsa_input.pci-....analog-stereo`, which is a microphone. Upstream filters
    /// on `media.class` exactly (`is_audio_sink` -> `media.class == "Audio/Sink"`),
    /// which is what PipeWire actually defines.
    pub fn list_output_sinks(&self) -> Vec<OutputRoute> {
        let Ok(registry) = self.core.get_registry() else {
            return Vec::new();
        };
        let found = Arc::new(Mutex::new(Vec::new()));
        let found_clone = found.clone();
        let virtual_prefix = self.virtual_sink_name.clone();

        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = &global.props else {
                    return;
                };
                if props.get("media.class").unwrap_or("") != AUDIO_SINK_MEDIA_CLASS {
                    return;
                }
                let name = props.get("node.name").unwrap_or("").to_string();
                // Our own sink is an implementation detail, not a device the
                // user can pick; upstream excludes it the same way.
                if name.starts_with(VIRTUAL_SINK_BASE) || name.starts_with(&virtual_prefix) {
                    return;
                }
                found_clone.lock().unwrap().push(OutputRoute {
                    id: global.id,
                    description: props.get("node.description").unwrap_or("").to_string(),
                    name,
                    active: false,
                });
            })
            .register();

        // Registry globals are queued, so a listener sees nothing until the
        // server has been roundtripped.
        let _ = self.roundtrip();

        let mut result = found.lock().unwrap().clone();
        // Stable, human-friendly ordering: description first, then node name.
        result.sort_by(|a, b| {
            let ka = display_label(&a.description, &a.name);
            let kb = display_label(&b.description, &b.name);
            ka.cmp(&kb).then_with(|| a.name.cmp(&b.name))
        });
        debug!("Found {} output sink(s)", result.len());
        result
    }

    /// Kept for API compatibility; now backed by the same `media.class` filter.
    pub fn detect_routes(&self) -> Result<Vec<OutputRoute>, Error> {
        Ok(self.list_output_sinks())
    }

    pub fn get_routes(&self) -> Vec<OutputRoute> {
        self.routes.lock().unwrap().clone()
    }

    pub fn get_active_route(&self) -> Option<OutputRoute> {
        self.routes
            .lock()
            .unwrap()
            .iter()
            .find(|r| r.active)
            .cloned()
    }

    pub fn set_current_sink(&mut self, sink_name: &str) {
        self.current_sink = Some(sink_name.to_string());
        // Record the chain's physical destination for the monitor resolve
        // path. ANY of our virtual sinks (legacy or per-device) is the
        // streams' entry point, not the device being equalised -- recording
        // one sends the monitor to `mini_eq_sink*` itself instead of the
        // device. No physical sink starts with our prefix (pinned by unit
        // test), so the prefix check is the correct guard.
        if !sink_name.starts_with(VIRTUAL_SINK_BASE) {
            self.chain_output_sink = Some(sink_name.to_string());
        }
        info!("Current sink set to: {}", sink_name);
    }

    pub fn get_current_sink(&self) -> Option<&str> {
        self.current_sink.as_deref()
    }

    /// Register an EQ virtual sink for a physical device (called as device
    /// chains are created/destroyed, and once at startup for the legacy
    /// singleton). Scope and allowed-target logic cover every registered
    /// sink.
    pub fn register_eq_sink(&mut self, eq_sink_name: &str, physical_sink: &str) {
        self.eq_sinks
            .insert(eq_sink_name.to_string(), physical_sink.to_string());
    }

    /// The physical sink a registered EQ virtual sink plays to.
    pub fn physical_for_eq_sink(&self, eq_sink_name: &str) -> Option<String> {
        self.eq_sinks.get(eq_sink_name).cloned()
    }

    /// Shared handle for the streams-dirty flag (see field docs). The backend
    /// hands a clone to the registry listener so node arrivals mark it.
    pub fn streams_dirty_flag(&self) -> Rc<Cell<bool>> {
        self.streams_dirty.clone()
    }

    /// Mark streams for a scope re-check on the next pump tick.
    pub fn mark_streams_dirty(&self) {
        self.streams_dirty.set(true);
    }

    /// Take the streams-dirty flag (true when a re-check is due).
    pub fn take_streams_dirty(&self) -> bool {
        self.streams_dirty.replace(false)
    }

    /// Record that this device's EQ was switched on/off by the user.
    pub fn set_device_eq_wanted(&mut self, physical_sink: &str, on: bool) {
        if on {
            self.eq_wanted.insert(physical_sink.to_string());
        } else {
            self.eq_wanted.remove(physical_sink);
        }
    }

    /// Record the UI-selected output device (Reroute target for late streams).
    pub fn set_selected_sink(&mut self, physical_sink: &str) {
        self.selected_sink = Some(physical_sink.to_string());
    }

    /// Drop a per-device virtual sink from the known set.
    pub fn unregister_eq_sink(&mut self, eq_sink_name: &str) {
        self.eq_sinks.remove(eq_sink_name);
    }

    /// All known EQ virtual sinks (registered device chains).
    pub fn eq_sink_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.eq_sinks.keys().cloned().collect();
        names.sort();
        names
    }

    /// The physical sink the filter chain plays out to, if known. See the
    /// field docs: this stays physical while `current_sink` follows the
    /// virtual sink during routing.
    pub fn chain_output_sink(&self) -> Option<String> {
        self.chain_output_sink.clone()
    }

    /// True while the playback streams are routed through the EQ.
    pub fn is_routed(&self) -> bool {
        self.routed
    }

    /// Which streams the EQ reaches when it is on.
    pub fn output_mode(&self) -> crate::core::OutputRoutingMode {
        self.output_mode
    }

    pub fn set_output_mode(&mut self, mode: crate::core::OutputRoutingMode) {
        self.output_mode = mode;
    }

    /// True when the stream is ours or must never be touched.
    ///
    /// Upstream `_is_internal_stream`: our own client, the filter chain's own
    /// output, UI/event sounds by `media.role`, and the desktop/speech
    /// applications it blocklists by name.
    fn is_internal_stream(&self, stream: &StreamNode) -> bool {
        if stream.app_name == OUTPUT_CLIENT_NAME {
            return true;
        }
        if crate::core::BLOCKLIST_MEDIA_ROLES.contains(&stream.media_role.as_str()) {
            return true;
        }
        if crate::core::BLOCKLIST_STREAM_NAMES.contains(&stream.node_name.as_str())
            || crate::core::BLOCKLIST_STREAM_NAMES.contains(&stream.app_name.as_str())
        {
            return true;
        }
        let internal_output = format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX);
        if stream.node_name == internal_output {
            return true;
        }
        stream.node_name.starts_with(VIRTUAL_SINK_BASE)
            || stream
                .node_name
                .starts_with(&format!("{}.", self.virtual_sink_name))
    }

    /// The `target.object` values that belong to our own signal path: the
    /// virtual sink, the chain's output, and the real output sink we feed.
    ///
    /// Upstream `_target_object_matches_processing_path`. A stream whose
    /// `target.object` is anything else was deliberately pointed at another
    /// device -- by the user in pavucontrol, or by the app itself -- and
    /// hijacking it is exactly the "disrupt the user's audio" case.
    fn processing_path_targets(&self) -> Vec<String> {
        // What a stream may legitimately be pointed at while it is under our
        // control: our virtual sink, the chain's own output node, the real sink
        // the EQ feeds -- and, crucially, whatever we last routed to, since
        // that is the serial actually sitting in the streams' `target.object`.
        //
        // The last-routed serial is not optional bookkeeping. Resolving the
        // virtual sink by name is not enough: if that lookup fails for any
        // reason, every stream we routed looks like it has a "foreign" target,
        // and then the restore skips them and they are left pointing at a sink
        // that is about to disappear. That is the silent-app bug again.
        let mut allowed: Vec<String> = self
            .last_route_target
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, serial)| serial.clone())
            .into_iter()
            .collect();

        let mut names = vec![
            VIRTUAL_SINK_BASE.to_string(),
            self.virtual_sink_name.clone(),
            format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX),
        ];
        // Every registered per-device EQ sink (+ its playback node): streams
        // sitting in ANY device's chain are ours-in-waiting, never foreign.
        for eq in self.eq_sinks.keys() {
            names.push(eq.clone());
            names.push(format!("{eq}{FILTER_OUTPUT_SUFFIX}"));
        }
        if let Some(sink) = self.default_audio_sink.borrow().clone() {
            names.push(sink);
        }
        // The chosen chain output: unroute sends streams there as the
        // fallback, so a stream sitting on it is ours-in-waiting, not a
        // foreign choice the user made. Without this, an off-to-fallback
        // followed by re-on routes 0 streams under Selected.
        if let Some(sink) = self.chain_output_sink.clone() {
            names.push(sink);
        }
        for name in names {
            allowed.push(name.clone());
            // The node's `object.serial` is what lands in `target.object`; the
            // node name is accepted too because both forms appear in the wild.
            if let Some((_, serial)) = self.find_node_target(&name) {
                allowed.push(serial);
            }
        }
        allowed
    }

    /// Whether a stream's routing target puts it in scope for the chosen device
    /// under `mode`. Pure: it takes the target string rather than a stream, so
    /// the predicate is testable without a live PipeWire connection.
    ///
    /// - **Selected**: in scope when the stream has no explicit target (the
    ///   default is the device we picked) or its `target.object` matches the
    ///   chosen sink's serial. A stream deliberately pointed at another device
    ///   is out of scope -- that is the user's mixer deciding, and it stays
    ///   absolute.
    /// - **Reroute**: everything is in scope, including streams with a foreign
    ///   target. This is the explicit opt-out from the foreign-target rule.
    ///
    /// NOTE: the no-explicit-target arm assumes the caller already resolved
    /// "default" to the chosen device (see `is_in_scope`, which maps an
    /// explicit target on the system default back to default-routed first).
    /// Calling this directly with a raw target over-admits target-less
    /// streams when the chosen device is not the default.
    pub fn target_in_scope(
        target_object: Option<&str>,
        sink_serial: &str,
        mode: crate::core::OutputRoutingMode,
    ) -> bool {
        if mode == crate::core::OutputRoutingMode::Reroute {
            return true;
        }
        match target_object {
            // No explicit target: WirePlumber is choosing the default, which is
            // the device we picked. In scope.
            None | Some("") => true,
            Some(target) => target == sink_serial,
        }
    }

    /// Whether a stream is in scope for the chosen device under `mode`.
    ///
    /// Called after the internal/blocklist filters, so the only question left is
    /// *which device* the stream is aimed at.
    ///
    /// The chosen sink is identified by its `object.serial`, which is the value
    /// WirePlumber matches on in `target.object`. Comparing against the serial
    /// rather than the node name is what makes "this stream is already going to
    /// the device I picked" true.
    /// Scope rule with an explicit system default: pure and unit-tested.
    ///
    /// `target` is the stream's `target.object` (None/empty = WirePlumber's
    /// choice), `sink_serial` the chosen sink, `default_serial` the system
    /// default sink if known. An explicit target on the default counts as
    /// default-routed (WirePlumber writes explicit targets even for default
    /// playback); default playback is in scope iff the chosen sink IS the
    /// default. Anything explicitly aimed elsewhere must match the chosen
    /// sink. Reroute takes everything.
    fn scope_allows(
        target: Option<&str>,
        sink_serial: &str,
        default_serial: Option<&str>,
        mode: crate::core::OutputRoutingMode,
    ) -> bool {
        if mode == crate::core::OutputRoutingMode::Reroute {
            return true;
        }
        let on_default = match target {
            None | Some("") => true,
            Some(t) => default_serial.is_some_and(|d| d == t),
        };
        if on_default {
            return default_serial.is_some_and(|d| d == sink_serial);
        }
        target.is_some_and(|t| t == sink_serial)
    }

    fn is_in_scope(
        &self,
        stream: &StreamNode,
        sink_serial: &str,
        default_serial: Option<&str>,
        mode: crate::core::OutputRoutingMode,
    ) -> bool {
        let target = self.stream_target(stream.id).target_object;
        Self::scope_allows(target.as_deref(), sink_serial, default_serial, mode)
    }

    /// Streams we may route: not ours, not blocklisted, and not already
    /// pointed at a device the user chose.
    ///
    /// Upstream `iter_routable_output_streams`.
    /// The id of our own filter-chain output stream, `mini_eq_sink_output`.
    ///
    /// Upstream `output_stream_by_name(filter_output_name)`. This is the stream
    /// whose target decides where the EQ chain plays out, and moving it is the
    /// live alternative to rebuilding the module.
    pub fn output_stream_id_by_name(&self, node_name: &str) -> Option<u32> {
        self.list_stream_nodes()
            .into_iter()
            .find(|s| s.node_name == node_name)
            .map(|s| s.id)
    }

    /// Move a stream and report where it actually ended up.
    ///
    /// The write is not the outcome: the earlier assumption that a metadata
    /// write to a `node.passive` client cannot move it was wrong, and it cost an
    /// engine rebuild -- and the interruption that comes with one -- on every
    /// output switch. Upstream moves this stream live and only falls back to
    /// `restart_engine` when the move raises. Reading the value back is what
    /// makes the choice between the two paths evidence-based rather than a
    /// guess in a comment.
    pub fn move_stream_to_target_checked(
        &mut self,
        stream_id: u32,
        sink_id: u32,
        sink_serial: &str,
    ) -> bool {
        if self
            .set_stream_target(stream_id, sink_id, sink_serial)
            .is_err()
        {
            return false;
        }
        // The metadata property event arrives during the roundtrip above; allow
        // a bounded moment for the cache to reflect it before calling the move a
        // failure and rebuilding the engine.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        loop {
            if self.stream_target(stream_id).target_object.as_deref() == Some(sink_serial) {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            let _ = self.roundtrip();
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// Serials of every live output sink, by `object.serial`.
    ///
    /// `target.object` is a serial, so a serial that no longer belongs to a
    /// live sink is a target left over from a previous run of this app. Such a
    /// value is treated as "no explicit target" rather than as a deliberate
    /// choice: the sink it names is gone, so honouring it would leave the
    /// stream unrouted forever.
    fn live_sink_serials(&self) -> Vec<String> {
        let registry = match self.core.get_registry() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let found = Arc::new(Mutex::new(Vec::new()));
        let found_clone = found.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node
                    && let Some(props) = &global.props
                    && props.get("media.class").unwrap_or("") == "Audio/Sink"
                    && let Some(serial) = props.get("object.serial")
                {
                    found_clone.lock().unwrap().push(serial.to_string());
                }
            })
            .register();
        let _ = self.roundtrip();
        found.lock().unwrap().clone()
    }

    pub fn routable_output_streams(&self) -> Vec<StreamNode> {
        self.routable_output_streams_for(
            self.current_sink.as_deref().unwrap_or(VIRTUAL_SINK_BASE),
            self.output_mode,
        )
    }

    /// Filtered view for an explicit sink + mode, so the scope predicate can be
    /// exercised on its own (see `tests::mode_scope_predicate`).
    pub fn routable_output_streams_for(
        &self,
        sink_name: &str,
        mode: crate::core::OutputRoutingMode,
    ) -> Vec<StreamNode> {
        let allowed = self.processing_path_targets();
        let live_serials = self.live_sink_serials();
        let sink_serial = self
            .find_node_target(sink_name)
            .map(|(_, serial)| serial)
            .unwrap_or_default();
        // Resolved once per computation (not per stream): an explicit target
        // on this serial counts as default-routed in `is_in_scope`.
        let default_serial: Option<String> = self
            .default_audio_sink
            .borrow()
            .clone()
            .and_then(|n| self.find_node_target(&n))
            .map(|(_, serial)| serial);
        if std::env::var("MINI_EQ_DEBUG_ROUTING").is_ok() {
            info!("routable filter: allowed targets = {allowed:?}");
            for s in self.list_stream_nodes() {
                let t = self.stream_target(s.id);
                info!(
                    "  stream {} name={:?} app={:?} role={:?} dont_move={} target={:?} internal={} => {}",
                    s.id,
                    s.node_name,
                    s.app_name,
                    s.media_role,
                    s.dont_move,
                    t.target_object,
                    self.is_internal_stream(&s),
                    if self.is_internal_stream(&s) {
                        "skip (internal)"
                    } else {
                        match t.target_object.as_deref() {
                            Some(x)
                                if !x.is_empty()
                                    && !allowed.iter().any(|a| a == x)
                                    && live_serials.iter().any(|serial| serial == x) =>
                            {
                                "skip (foreign target)"
                            }
                            Some(x) if !x.is_empty() && !allowed.iter().any(|a| a == x) => {
                                "ROUTABLE (stale target, sink gone)"
                            }
                            _ => {
                                if self.is_in_scope(
                                    &s,
                                    &sink_serial,
                                    default_serial.as_deref(),
                                    mode,
                                ) {
                                    "ROUTABLE"
                                } else {
                                    "skip (out of scope)"
                                }
                            }
                        }
                    }
                );
            }
        }
        self.list_stream_nodes()
            .into_iter()
            .filter(|s| !self.is_internal_stream(s))
            .filter(|s| {
                // No explicit target is fine: WirePlumber is choosing the
                // default, which is what we want to override. An explicit
                // foreign target is not -- unless the mode says to take
                // everything, in which case `is_in_scope` returns true below.
                match self.stream_target(s.id).target_object.as_deref() {
                    Some(target) if !target.is_empty() => {
                        allowed.iter().any(|a| a == target)
                            // Stale serial from a previous instance: the sink it
                            // names does not exist, so it is not a deliberate
                            // choice and must not keep the stream out of the
                            // EQ forever.
                            || !live_serials.iter().any(|serial| serial == target)
                    }
                    _ => true,
                }
            })
            .filter(|s| self.is_in_scope(s, &sink_serial, default_serial.as_deref(), mode))
            .collect()
    }

    /// Candidates for unroute/suspend: everything routable, PLUS streams
    /// already pointed into our processing path even when the mode scope
    /// excludes them.
    ///
    /// Without the second half, switching the output device (which points
    /// `current_sink` at the physical sink) and then switching the EQ off
    /// leaves EQ-pointed streams behind: under Selected they are out of scope
    /// for the physical sink, so the restore never sees them and they keep
    /// pointing at `mini_eq_sink` -- audio stops when the app exits. A re-route
    /// then records the EQ target as "where it came from", cementing it.
    /// Reproduced by tests-live/live_test.sh (T7-off restored 0 streams).
    pub fn restorable_output_streams(&self) -> Vec<StreamNode> {
        let recorded_ids: std::collections::HashSet<u32> = self
            .routed_targets
            .lock()
            .unwrap()
            .keys()
            .copied()
            .collect();
        let allowed = self.processing_path_targets();
        let sink_serial = self
            .current_sink
            .as_deref()
            .and_then(|n| self.find_node_target(n))
            .map(|(_, serial)| serial)
            .unwrap_or_default();
        let mode = self.output_mode;
        self.list_stream_nodes()
            .into_iter()
            .filter(|s| !self.is_internal_stream(s))
            .filter(|s| {
                if recorded_ids.contains(&s.id) {
                    return true;
                }
                let target = self.stream_target(s.id);
                let obj = target.target_object.as_deref().unwrap_or("");
                // Pointed into our processing path: ours by definition.
                if !obj.is_empty() && allowed.iter().any(|a| a == obj) {
                    return true;
                }
                Self::target_in_scope(target.target_object.as_deref(), &sink_serial, mode)
            })
            .collect()
    }

    /// True while at least one playback stream is pointed into the EQ.
    ///
    /// Gates manual output switches under Selected: moving the chain away
    /// would orphan these streams, so the switch is refused instead (the
    /// chain follows the active device). Internal streams are excluded, like
    /// everywhere else.
    pub fn has_routed_streams(&self) -> bool {
        let mut serials: Vec<String> = self
            .last_route_target
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, serial)| serial.clone())
            .into_iter()
            .collect();
        if let Some((_, serial)) = self.find_node_target(VIRTUAL_SINK_BASE) {
            if !serials.contains(&serial) {
                serials.push(serial);
            }
        }
        if serials.is_empty() {
            return false;
        }
        self.list_stream_nodes().iter().any(|s| {
            !self.is_internal_stream(s)
                && self
                    .stream_target(s.id)
                    .target_object
                    .as_deref()
                    .is_some_and(|t| serials.iter().any(|x| x == t))
        })
    }

    /// Reconcile routed streams with the current output device (or mode).
    ///
    /// Call after the chain moved to another sink, or after narrowing to
    /// Selected, while routing is on. Under Reroute everything stays. Under
    /// Selected a stream stays in the EQ only when it is *effectively* aimed
    /// at the new device: its recorded target, or -- when it never had an
    /// explicit one -- the system default (WirePlumber's choice for
    /// target-less streams). Anything else is handed back, so changing the
    /// Output dropdown never drags audio off the device it is playing on;
    /// streams explicitly aimed at the new device that were never taken are
    /// adopted. Returns `(pruned, adopted)`.
    ///
    /// No-op when not routed, under Reroute, or when a device cannot be
    /// resolved (safer to keep the status quo than to guess).
    pub fn rescope_routing(&mut self) -> Result<(usize, usize), Error> {
        if !self.routed || self.output_mode == crate::core::OutputRoutingMode::Reroute {
            return Ok((0, 0));
        }
        let chosen = self.chain_output_sink.clone().unwrap_or_default();
        self.rescope_scoped(VIRTUAL_SINK_BASE, &chosen)
    }

    /// Device-scoped reconcile: streams effectively aimed elsewhere are
    /// handed back, streams explicitly aimed at this device are (re)routed
    /// into ITS virtual sink. Unlike the legacy `rescope_routing` this does
    /// not require the global `routed` flag: per-device chains are adopted
    /// whenever their device's EQ is asked to be consistent.
    pub fn rescope_device(&mut self, physical_sink: &str) -> Result<(usize, usize), Error> {
        if self.output_mode == crate::core::OutputRoutingMode::Reroute {
            // Reroute takes everything in the selected chain; there is
            // nothing to prune per device.
            return Ok((0, 0));
        }
        let eq = crate::core::eq_virtual_sink_for(physical_sink);
        self.rescope_scoped(&eq, physical_sink)
    }

    /// Shared body of `rescope_routing`/`rescope_device`.
    ///
    /// `eq_sink_name` is the virtual sink the streams get routed into,
    /// `physical_name` the real device they are effectively aimed at.
    fn rescope_scoped(
        &mut self,
        eq_sink_name: &str,
        physical_name: &str,
    ) -> Result<(usize, usize), Error> {
        let chosen_serial_and_name = self.find_node_target(physical_name);
        let Some((_, chosen_serial)) = chosen_serial_and_name else {
            return Ok((0, 0));
        };
        let default_serial: Option<String> = self
            .default_audio_sink
            .borrow()
            .clone()
            .and_then(|n| self.find_node_target(&n))
            .map(|(_, serial)| serial);
        let Some((virt_id, virt_serial)) = self.find_node_target(eq_sink_name) else {
            return Ok((0, 0));
        };
        // A stream's effective device: its recorded origin, falling back to
        // the system default for streams that never had an explicit target.
        let effective = |recorded: &StreamTarget| -> Option<String> {
            recorded
                .target_object
                .as_deref()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .or_else(|| default_serial.clone())
        };
        let mut pruned = 0usize;
        let mut adopted = 0usize;
        let mut wrote = false;
        let mut pruned_ids: Vec<u32> = Vec::new();
        let mut verified_ids: Vec<u32> = Vec::new();
        self.ensure_default_metadata()?;
        // 1. Hand back recorded streams that are effectively aimed elsewhere.
        // 2. (Re-)route recorded in-scope streams that are not currently on
        //    the EQ (post-suspend / post-rebuild state).
        let recorded = self.routed_targets.lock().unwrap().clone();
        {
            let md = self.default_metadata.as_ref().unwrap();
            for (id, rec) in &recorded {
                match effective(rec) {
                    Some(eff) if eff != chosen_serial => {
                        md.set_property(
                            *id,
                            "target.node",
                            rec.target_node_type.as_deref(),
                            rec.target_node.as_deref(),
                        );
                        md.set_property(
                            *id,
                            "target.object",
                            rec.target_object_type.as_deref(),
                            rec.target_object.as_deref(),
                        );
                        self.routed_targets.lock().unwrap().remove(id);
                        info!("Rescope: handed stream {id} back (aimed elsewhere)");
                        pruned += 1;
                        pruned_ids.push(*id);
                        wrote = true;
                    }
                    _ => {
                        // In scope (or undecidable): make sure it is actually
                        // on the EQ. A `None` effective target means neither a
                        // recorded origin nor a known default -- leave it.
                        let on_eq = self.stream_target(*id).target_object.as_deref()
                            == Some(virt_serial.as_str());
                        if !on_eq && effective(rec).as_deref() == Some(chosen_serial.as_str()) {
                            md.set_property(
                                *id,
                                "target.node",
                                Some("Spa:Id"),
                                Some(&virt_id.to_string()),
                            );
                            md.set_property(
                                *id,
                                "target.object",
                                Some("Spa:Id"),
                                Some(&virt_serial),
                            );
                            info!("Rescope: re-routed stream {id} into the EQ");
                            adopted += 1;
                            verified_ids.push(*id);
                            wrote = true;
                        }
                    }
                }
            }
        }
        // 3. Adopt unrecorded streams explicitly aimed at the new device --
        //    and target-less ones when the new device IS the default.
        //    `dont-move` is respected exactly like the initial auto-route.
        //    Live links backstop metadata (direct node.target links leave no
        //    metadata trace).
        {
            let md = self.default_metadata.as_ref().unwrap();
            let link_targets = self.stream_link_targets();
            for s in self.list_stream_nodes() {
                if self.is_internal_stream(&s)
                    || s.dont_move
                    || self.routed_targets.lock().unwrap().contains_key(&s.id)
                {
                    continue;
                }
                let cur = self.stream_target(s.id);
                let cur_obj = cur.target_object.as_deref().unwrap_or("");
                let ours = !cur_obj.is_empty() && cur_obj == chosen_serial;
                let linked_here = link_targets
                    .get(&s.id)
                    .is_some_and(|v| v.iter().any(|n| n == physical_name));
                let linked_into_own_eq = link_targets
                    .get(&s.id)
                    .is_some_and(|v| v.iter().any(|n| n == eq_sink_name));
                // Explicit matches and live links decide; a bare
                // follows-default with neither is a brand-new stream whose
                // target has not arrived yet (see adopt_unrecorded_into) --
                // leave it for the pump-driven adoption pass, which fires on
                // the metadata/link arrival. Claiming it here would record it
                // for the wrong chain with no later rescope to fix it.
                if ours || linked_here || linked_into_own_eq {
                    md.set_property(
                        s.id,
                        "target.node",
                        Some("Spa:Id"),
                        Some(&virt_id.to_string()),
                    );
                    md.set_property(s.id, "target.object", Some("Spa:Id"), Some(&virt_serial));
                    self.routed_targets.lock().unwrap().insert(s.id, cur);
                    info!("Rescope: adopted stream '{}' ({})", s.node_name, s.id);
                    adopted += 1;
                    verified_ids.push(s.id);
                    wrote = true;
                }
            }
        }
        *self.last_route_target.lock().unwrap() = Some((virt_id, virt_serial.clone()));
        if wrote {
            self.roundtrip()?;
        }
        // Handed-back streams must not keep fallback links feeding the old
        // chain; (re-)routed ones get read-back verification with link
        // fallback where the metadata write did not take.
        self.drop_fallback_links(&pruned_ids);
        self.clear_pending_for(&pruned_ids);
        // Deferred read-back verification (see `defer_verify`): acting on
        // the first mismatch linked spuriously on broadcast lag.
        self.defer_verify(&verified_ids, virt_id, &virt_serial, eq_sink_name);
        self.heal_fallback_links();
        if pruned > 0 || adopted > 0 {
            info!("Rescope complete: {pruned} handed back, {adopted} (re-)routed");
        }
        Ok((pruned, adopted))
    }

    /// True while at least one stream is pointed into the given device's EQ
    /// virtual sink. Unknown sink (chain not created yet / already dropped)
    /// is false -- cannot be "on" it.
    pub fn has_routed_streams_for(&self, physical_sink: &str) -> bool {
        let eq = crate::core::eq_virtual_sink_for(physical_sink);
        let Some((_, serial)) = self.find_node_target(&eq) else {
            return false;
        };
        self.list_stream_nodes().iter().any(|s| {
            !self.is_internal_stream(s)
                && self.stream_target(s.id).target_object.as_deref() == Some(serial.as_str())
        })
    }

    /// Number of non-internal playback streams verifiably flowing into one
    /// of our EQ sinks right now -- by `target.object` metadata OR by live
    /// links into the sink node.
    ///
    /// Unlike `last_routed_count` (intent: what the last route pass asked
    /// for) and `is_routed` (machinery on/off), this is read-back truth: in
    /// a sandbox that silently discards metadata writes it stays 0 while the
    /// streams audibly bypass the EQ, which is exactly the "EQ on but
    /// nothing happens" state the old flag could not distinguish. Surfaced
    /// as GetState `eq_flowing` / `flowing_streams`.
    ///
    /// Deliberately heavier than the flag reads (a few registry passes), so
    /// callers must throttle it -- the window recomputes at most every
    /// ~500 ms, never on the 33 ms tick directly.
    pub fn flowing_count(&self) -> usize {
        let eq_names = self.owned_eq_sink_names();
        let mut serials = Vec::with_capacity(eq_names.len());
        for name in &eq_names {
            if let Some((_, serial)) = self.find_node_target(name) {
                serials.push(serial);
            }
        }
        if serials.is_empty() {
            return 0;
        }
        let link_targets = self.stream_link_targets();
        self.list_stream_nodes()
            .iter()
            .filter(|s| {
                if self.is_internal_stream(s) {
                    return false;
                }
                if self
                    .stream_target(s.id)
                    .target_object
                    .as_deref()
                    .is_some_and(|t| serials.iter().any(|x| x == t))
                {
                    return true;
                }
                link_targets
                    .get(&s.id)
                    .is_some_and(|v| v.iter().any(|n| eq_names.iter().any(|e| e == n)))
            })
            .count()
    }

    /// Hand back every stream pointed into `physical_sink`'s EQ chain.
    ///
    /// Streams with a recorded origin get that verbatim; unrecorded ones fall
    /// back to `fallback_sink` (clearing their target when no fallback is
    /// available). After this, no stream should reference the chain's virtual
    /// sink -- so the chain can be dropped or its EQ switched "off" without
    /// silencing the apps that used it.
    pub fn unroute_device(
        &mut self,
        physical_sink: &str,
        fallback_sink: Option<&str>,
    ) -> Result<(), Error> {
        let eq = crate::core::eq_virtual_sink_for(physical_sink);
        let Some((_, eq_serial)) = self.find_node_target(&eq) else {
            return Ok(());
        };
        self.ensure_default_metadata()?;
        let streams = self.list_stream_nodes();
        let recorded = self.routed_targets.lock().unwrap().clone();
        let mut restored = 0usize;
        let mut to_fallback = 0usize;
        let mut wrote = false;
        {
            let md = self.default_metadata.as_ref().unwrap();
            for s in &streams {
                if self.is_internal_stream(s) {
                    continue;
                }
                let cur = self.stream_target(s.id);
                if cur.target_object.as_deref() != Some(eq_serial.as_str()) {
                    continue;
                }
                match recorded.get(&s.id) {
                    Some(rec) => {
                        md.set_property(
                            s.id,
                            "target.node",
                            rec.target_node_type.as_deref(),
                            rec.target_node.as_deref(),
                        );
                        md.set_property(
                            s.id,
                            "target.object",
                            rec.target_object_type.as_deref(),
                            rec.target_object.as_deref(),
                        );
                        self.routed_targets.lock().unwrap().remove(&s.id);
                        info!("UnrouteDevice: restored '{}' ({})", s.node_name, s.id);
                        restored += 1;
                    }
                    None => {
                        match fallback_sink.and_then(|n| self.find_node_target(n)) {
                            Some((node_id, serial)) => {
                                md.set_property(
                                    s.id,
                                    "target.node",
                                    Some("Spa:Id"),
                                    Some(&node_id.to_string()),
                                );
                                md.set_property(
                                    s.id,
                                    "target.object",
                                    Some("Spa:Id"),
                                    Some(&serial),
                                );
                            }
                            None => {
                                md.set_property(s.id, "target.node", None, None);
                                md.set_property(s.id, "target.object", None, None);
                            }
                        }
                        info!("UnrouteDevice: fallback for '{}' ({})", s.node_name, s.id);
                        to_fallback += 1;
                    }
                }
                wrote = true;
            }
        }
        if wrote {
            self.roundtrip()?;
        }
        // Streams leaving this device's ownership must not keep fallback
        // links feeding its chain: drop ours for this EQ only, so sibling
        // device chains keep playing. Pending verifications for it die too.
        // BUT the restore metadata writes above are discarded by a sandboxed
        // daemon, so a stream held by a fallback link into this EQ would go
        // pathless the moment its link is dropped. Restore each held stream
        // with real port links into its recorded original device (or the
        // device being unrouted) FIRST -- link before drop, so the stream is
        // never pathless -- and forget the entry only when the links are
        // live. A failed restore keeps the link (and the entry): the heal
        // pass retries, and a pathless stream is worse than a bypassed one.
        {
            let device_default = fallback_sink
                .map(|n| n.to_string())
                .unwrap_or_else(|| physical_sink.to_string());
            let held: Vec<u32> = self
                .fallback_links
                .lock()
                .unwrap()
                .iter()
                .filter(|(_, eq_name)| eq_name.as_str() == eq)
                .map(|(id, _)| *id)
                .collect();
            for id in held {
                let device_name = match recorded.get(&id) {
                    Some(r) if r.target_node_type.as_deref() != Some("Spa:Id") => r
                        .target_node
                        .clone()
                        .unwrap_or_else(|| device_default.clone()),
                    _ => device_default.clone(),
                };
                if self.unlink_stream_to_device(id, &device_name) {
                    // Device links are live: destroy the stream's links into
                    // the EQ (matched against the live graph; the tracking
                    // entry is about to go, so drop_fallback_links_for_eq
                    // would miss them) -- else the stream doubles into both
                    // paths.
                    let link_details = self.stream_link_details();
                    if let Some(links) = link_details.get(&id) {
                        for (lid, sink) in links {
                            if sink == &eq {
                                Self::destroy_link_object(*lid);
                            }
                        }
                    }
                    info!(
                        "UnrouteDevice: stream {id} restored into {device_name} via fallback links"
                    );
                    self.fallback_links.lock().unwrap().remove(&id);
                } else {
                    warn!(
                        "UnrouteDevice: stream {id} could not be restored into {device_name}; keeping its fallback link"
                    );
                }
            }
        }
        self.drop_fallback_links_for_eq(&eq);
        self.clear_pending_for_eq(&eq);
        if self.routed_targets.lock().unwrap().is_empty() {
            self.routed = false;
            *self.last_route_target.lock().unwrap() = None;
        }
        info!("UnrouteDevice({physical_sink}): {restored} restored, {to_fallback} to fallback");
        Ok(())
    }

    /// Route streams that appeared after EQ was enabled into the right
    /// device chain, without touching anything else.
    ///
    /// This is the event-driven counterpart of the initial auto-route: the
    /// registry/metadata listeners set `streams_dirty` whenever nodes arrive
    /// or targets move, and the pump tick drains it here. Only UNRECORDED
    /// streams are candidates, so already-routed (or deliberately moved)
    /// streams are never yanked between chains by a background pass.
    ///
    /// - Selected: a stream joins the chain of the device it is effectively
    ///   aimed at (explicit target, or the default for target-less streams),
    ///   and only when that device's EQ is wanted.
    /// - Reroute: everything unrecorded joins the SELECTED device's chain,
    ///   and only while routing is active at all (`routed`).
    pub fn adopt_new_streams(&mut self) -> Result<usize, Error> {
        if self.output_mode == crate::core::OutputRoutingMode::Reroute {
            if !self.routed {
                return Ok(0);
            }
            let Some(sel) = self.selected_sink.clone() else {
                return Ok(0);
            };
            return self.adopt_unrecorded_into(&sel, true);
        }
        let wanted: Vec<String> = self.eq_wanted.iter().cloned().collect();
        let mut total = 0usize;
        for dev in wanted {
            total += self.adopt_unrecorded_into(&dev, false)?;
        }
        Ok(total)
    }

    /// Audio ports per node: node id -> (port global id, port name, direction).
    /// One registry pass; loop-thread only like the other snapshots.
    fn stream_audio_ports(&self) -> HashMap<u32, Vec<(u32, String, String)>> {
        let ports: Rc<RefCell<HashMap<u32, Vec<(u32, String, String)>>>> =
            Rc::new(RefCell::new(HashMap::new()));
        let ports_clone = ports.clone();
        let registry = match self.core.get_registry() {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Port {
                    if let Some(props) = &global.props {
                        let get = |k: &str| props.get(k).unwrap_or("").to_string();
                        if let Ok(node) = get("node.id").parse::<u32>() {
                            ports_clone.borrow_mut().entry(node).or_default().push((
                                global.id,
                                get("port.name"),
                                get("port.direction"),
                            ));
                        }
                    }
                }
            })
            .register();
        let _ = self.roundtrip();
        ports.borrow().clone()
    }

    /// Channel key of a port name (`output_FL` -> `FL`): what pairs a stream
    /// output to a sink input.
    fn port_channel(name: &str) -> &str {
        name.rsplit('_').next().unwrap_or(name)
    }

    /// Pair `out_id`'s output ports to `in_id`'s input ports by channel
    /// suffix (FL->FL, FR->FR); a lone output fans out to every input (mono
    /// upmix). Returns global (out-port, in-port) id pairs.
    fn pair_stream_ports(&self, out_id: u32, in_id: u32) -> Vec<(u32, u32)> {
        let ports = self.stream_audio_ports();
        let outs: Vec<(u32, String)> = ports
            .get(&out_id)
            .map(|v| {
                v.iter()
                    .filter(|(_, _, d)| d == "out")
                    .map(|(id, n, _)| (*id, n.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let ins: Vec<(u32, String)> = ports
            .get(&in_id)
            .map(|v| {
                v.iter()
                    .filter(|(_, _, d)| d == "in")
                    .map(|(id, n, _)| (*id, n.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let mut pairs: Vec<(u32, u32)> = Vec::new();
        let mut used_in: HashSet<u32> = HashSet::new();
        for (oid, oname) in &outs {
            if let Some((iid, _)) = ins.iter().find(|(iid, iname)| {
                !used_in.contains(iid) && Self::port_channel(iname) == Self::port_channel(oname)
            }) {
                pairs.push((*oid, *iid));
                used_in.insert(*iid);
            }
        }
        if pairs.is_empty() && outs.len() == 1 {
            for (iid, _) in &ins {
                pairs.push((outs[0].0, *iid));
            }
        }
        pairs
    }

    /// `pw-link` ref list for port pairs: global port ids, not `node:port`
    /// names -- duplicate stream names (two players, two test tones) make
    /// names ambiguous, ids exact.
    fn port_id_refs(pairs: &[(u32, u32)]) -> Vec<String> {
        let mut refs: Vec<String> = Vec::with_capacity(pairs.len() * 2);
        for (oport, iport) in pairs {
            refs.push(oport.to_string());
            refs.push(iport.to_string());
        }
        refs
    }

    /// Restore a fallback-held stream into `device` with real port links
    /// (the sandboxed daemon discards restore metadata writes), verifying
    /// the links are live before the caller drops the EQ link. Link-first
    /// discipline: the stream is never pathless. Returns true when links
    /// into `device` are observably live.
    fn unlink_stream_to_device(&self, stream_id: u32, device: &str) -> bool {
        if !self.list_stream_nodes().iter().any(|s| s.id == stream_id) {
            return false;
        }
        let Some(dev_id) = self.find_node_id_by_name(device) else {
            warn!("UnrouteDevice: device {device} not found for stream {stream_id}");
            return false;
        };
        let pairs = self.pair_stream_ports(stream_id, dev_id);
        if pairs.is_empty() {
            warn!("UnrouteDevice: no channel pairing between stream {stream_id} and {device}");
            return false;
        }
        let refs = Self::port_id_refs(&pairs);
        if !Self::run_pw_link_refs(&refs) {
            return false;
        }
        // Verify before the caller drops the EQ link: a "success" exit with
        // no live links into the device means the stream would go silent.
        self.stream_link_details()
            .get(&stream_id)
            .is_some_and(|v| v.iter().any(|(_, n)| n == device))
    }

    /// Map each playback stream to the sink node names it is actually linked
    /// into, from live Link objects.
    ///
    /// Ground truth of audio flow, complementing the `default` metadata:
    /// direct `node.target` links (e.g. `pw-play --target`) never produce
    /// metadata entries, and metadata can hold stale serials -- while links
    /// show where sound really goes. One registry pass; used as a fallback
    /// signal by adoption when metadata says nothing.
    fn stream_link_targets(&self) -> HashMap<u32, Vec<String>> {
        self.stream_link_details()
            .into_iter()
            .map(|(s, v)| (s, v.into_iter().map(|(_, name)| name).collect()))
            .collect()
    }

    /// Same as [`Self::stream_link_targets`] but keeping each link's id, so
    /// the fallback can destroy the competing direct links it supersedes.
    /// Map: stream node id -> (link id, sink node name).
    fn stream_link_details(&self) -> HashMap<u32, Vec<(u32, String)>> {
        let links: Rc<RefCell<Vec<(u32, u32, u32)>>> = Rc::new(RefCell::new(Vec::new()));
        let links_clone = links.clone();
        let names: Rc<RefCell<HashMap<u32, String>>> = Rc::new(RefCell::new(HashMap::new()));
        let names_clone = names.clone();
        let registry = match self.core.get_registry() {
            Ok(r) => r,
            Err(_) => return HashMap::new(),
        };
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Link {
                    if let Some(props) = &global.props {
                        let get = |k: &str| props.get(k).unwrap_or("").to_string();
                        if let (Some(out), Some(inp)) = (
                            get("link.output.node").parse::<u32>().ok(),
                            get("link.input.node").parse::<u32>().ok(),
                        ) {
                            links_clone.borrow_mut().push((global.id, out, inp));
                        }
                    }
                } else if global.type_ == ObjectType::Node {
                    if let Some(props) = &global.props {
                        if props.get("media.class").unwrap_or("") == "Audio/Sink" {
                            if let Some(name) = props.get("node.name") {
                                names_clone.borrow_mut().insert(global.id, name.to_string());
                            }
                        }
                    }
                }
            })
            .register();
        let _ = self.roundtrip();
        let names = names.borrow();
        let mut map: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
        for (lid, out, inp) in links.borrow().iter() {
            if let Some(sink) = names.get(inp) {
                map.entry(*out).or_default().push((*lid, sink.clone()));
            }
        }
        map
    }

    /// Destroy a link object by id via `pw-cli`. Used for our own fallback
    /// links on teardown and for the competing direct links a fallback
    /// supersedes. Best effort with loud logging: failure here means double
    /// audio (fallback + direct) or a leak, both of which must be visible.
    /// Returns true when the daemon accepted the destroy.
    fn destroy_link_object(link_id: u32) -> bool {
        let id = link_id.to_string();
        let out = std::process::Command::new("pw-cli")
            .args(["destroy", id.as_str()])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                info!("Destroyed link {link_id}");
                true
            }
            Ok(o) => {
                warn!(
                    "pw-cli destroy {link_id} failed: {} {}",
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                false
            }
            Err(e) => {
                warn!("pw-cli destroy {link_id} could not spawn: {e}");
                false
            }
        }
    }

    /// Destroy our fallback links for `stream_ids` and forget them. Called
    /// whenever streams leave our ownership (hand-back, unroute, rebuild):
    /// a surviving fallback link would keep feeding a dead chain or double
    /// the audio once metadata routing works again. Destroys whatever is
    /// live for the stream into its recorded EQ (no id tracking: ids go
    /// stale across daemon restarts, live-matching does not).
    fn drop_fallback_links(&self, stream_ids: &[u32]) {
        if stream_ids.is_empty() {
            return;
        }
        let mut map = self.fallback_links.lock().unwrap();
        let link_details = self.stream_link_details();
        for id in stream_ids {
            if let Some(eq) = map.remove(id) {
                if let Some(links) = link_details.get(id) {
                    for (lid, sink) in links {
                        if sink == &eq {
                            Self::destroy_link_object(*lid);
                        }
                    }
                }
            }
        }
    }

    /// Drop fallback links for streams nobody owns any more (left
    /// `routed_targets` without teardown). Without this, orphan links keep
    /// feeding audio through a dead chain forever: the heal pass sees live
    /// links and keeps them, and nothing else revisits them. Called from the
    /// heal path itself, so orphans converge to silence-free teardown.
    fn drop_orphan_fallback_links(&self) {
        let owned: Vec<u32> = self
            .fallback_links
            .lock()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        if owned.is_empty() {
            return;
        }
        let recorded = self.routed_targets.lock().unwrap();
        let orphans: Vec<u32> = owned
            .into_iter()
            .filter(|id| !recorded.contains_key(id))
            .collect();
        drop(recorded);
        self.drop_fallback_links(&orphans);
    }

    /// [`Self::drop_fallback_links`], restricted to entries feeding
    /// `eq_name`: per-device teardown must not touch sibling chains.
    fn drop_fallback_links_for_eq(&self, eq_name: &str) {
        let ids: Vec<u32> = self
            .fallback_links
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, name)| *name == eq_name)
            .map(|(id, _)| *id)
            .collect();
        self.drop_fallback_links(&ids);
    }

    /// True when `stream_id` is observably on the EQ: metadata target matches
    /// `virt_serial`, or live links feed `eq_name`. Shared by verification
    /// and healing so both agree on what "landed" means.
    fn stream_on_eq(
        &self,
        stream_id: u32,
        virt_serial: &str,
        eq_name: &str,
        link_details: &HashMap<u32, Vec<(u32, String)>>,
    ) -> bool {
        self.stream_target(stream_id).target_object.as_deref() == Some(virt_serial)
            || link_details
                .get(&stream_id)
                .is_some_and(|v| v.iter().any(|(_, n)| n == eq_name))
    }

    /// Grace period between a metadata write and link-fallback action.
    /// Our own write's property echo has been observed to arrive after the
    /// write's roundtrip completed; acting on the first mismatch linked
    /// spuriously and severed the session manager's in-flight path. Waiting
    /// costs nothing: audio already flows via the metadata path where it
    /// works, and the fallback only matters where it never will.
    const PENDING_VERIFY_GRACE: Duration = Duration::from_millis(1500);

    /// Queue `stream_ids` for read-back verification instead of verifying
    /// inline (see [`Self::process_pending_verifies`]). Replaces any pending
    /// entry for the same stream, so re-routes refresh the deadline instead
    /// of stacking.
    fn defer_verify(&mut self, stream_ids: &[u32], virt_id: u32, virt_serial: &str, eq_name: &str) {
        let deadline = std::time::Instant::now() + Self::PENDING_VERIFY_GRACE;
        self.pending_verify
            .retain(|(id, _, _, _, _)| !stream_ids.contains(id));
        for id in stream_ids {
            self.pending_verify.push((
                *id,
                virt_id,
                virt_serial.to_string(),
                eq_name.to_string(),
                deadline,
            ));
        }
    }

    /// Forget pending verifications for `stream_ids` (call on every release
    /// path: a handed-back stream must never be linked afterwards).
    fn clear_pending_for(&mut self, stream_ids: &[u32]) {
        if !stream_ids.is_empty() {
            self.pending_verify
                .retain(|(id, _, _, _, _)| !stream_ids.contains(id));
        }
    }

    /// Forget pending verifications feeding `eq_name` (per-device teardown).
    fn clear_pending_for_eq(&mut self, eq_name: &str) {
        self.pending_verify.retain(|(_, _, _, eq, _)| eq != eq_name);
    }

    /// Drain due pending verifications: re-check each stream, link the ones
    /// still unmatched past their deadline, drop the landed ones. Called on
    /// every pump tick (cheap early-out when empty) so no event is needed to
    /// revisit a stream whose broadcast lagged.
    pub fn process_pending_verifies(&mut self) {
        if self.pending_verify.is_empty() {
            return;
        }
        let now = std::time::Instant::now();
        let link_details = self.stream_link_details();
        let alive: HashSet<u32> = self.list_stream_nodes().iter().map(|s| s.id).collect();
        // Take (not retain) so the re-checks below can borrow freely.
        let pending = std::mem::take(&mut self.pending_verify);
        let mut due: Vec<(u32, u32, String, String)> = Vec::new();
        for (id, virt_id, serial, eq, deadline) in pending {
            if !alive.contains(&id) {
                continue;
            }
            if self.stream_on_eq(id, &serial, &eq, &link_details) {
                self.drop_fallback_links(&[id]);
                continue;
            }
            if now >= deadline {
                due.push((id, virt_id, serial, eq));
            } else {
                self.pending_verify
                    .push((id, virt_id, serial, eq, deadline));
            }
        }
        for (id, virt_id, serial, eq) in &due {
            self.link_stream_fallback(*id, *virt_id, serial, eq);
        }
        self.heal_fallback_links();
    }

    /// Wire real port links stream -> EQ sink and sever the competing direct
    /// links, recording ours for teardown. Returns true when our path is in
    /// place. Link first, sever second: the stream must never be pathless in
    /// between.
    /// Spawn `pw-link` with port refs, waiting up to 10 s. Mirrors the
    /// monitor-tap path (same proven mechanism): in-process link-factory
    /// objects for stream ports never instantiate server-side, while
    /// `pw-link` links go active and persist, including from the sandbox.
    fn run_pw_link_refs(refs: &[String]) -> bool {
        let out = std::process::Command::new("pw-link")
            .args(refs)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output();
        match out {
            Ok(o) if o.status.success() => {
                info!("pw-link linked {}", refs.join(" "));
                true
            }
            Ok(o) => {
                warn!(
                    "pw-link {} failed: {} {}",
                    refs.join(" "),
                    o.status,
                    String::from_utf8_lossy(&o.stderr).trim()
                );
                false
            }
            Err(e) => {
                warn!("pw-link could not spawn: {e}");
                false
            }
        }
    }

    /// Wire real port links stream -> EQ sink via `pw-link` (see
    /// [`Self::run_pw_link_refs` for why not link-factory]) and sever the
    /// competing direct links. Records the EQ name for teardown. Returns
    /// true when links into the EQ are observably live afterwards.
    /// Link first, sever second: the stream is never pathless in between.
    fn link_stream_fallback(
        &self,
        stream_id: u32,
        virt_id: u32,
        virt_serial: &str,
        eq_name: &str,
    ) -> bool {
        // Re-check under the current graph: a concurrent session-manager
        // move may have landed the stream while we were queuing.
        let link_details = self.stream_link_details();
        if self.stream_on_eq(stream_id, virt_serial, eq_name, &link_details) {
            self.drop_fallback_links(&[stream_id]);
            return true;
        }
        if !self.list_stream_nodes().iter().any(|s| s.id == stream_id) {
            return false;
        };
        let pairs = self.pair_stream_ports(stream_id, virt_id);
        if pairs.is_empty() {
            warn!("Stream {stream_id}: no channel pairing between stream and EQ ports");
            return false;
        }
        warn!(
            "Stream {stream_id}: metadata write did not take (sandboxed daemon?) -- linking into {eq_name} directly"
        );
        let refs = Self::port_id_refs(&pairs);
        if !Self::run_pw_link_refs(&refs) {
            return false;
        }
        // Verify: only record (and only sever) when the links are live.
        // A "success" exit with no links means the daemon is humouring us.
        let link_details = self.stream_link_details();
        if !link_details
            .get(&stream_id)
            .is_some_and(|v| v.iter().any(|(_, n)| n == eq_name))
        {
            warn!(
                "Stream {stream_id}: pw-link reported success but no links into {eq_name} appeared"
            );
            return false;
        }
        self.fallback_links
            .lock()
            .unwrap()
            .insert(stream_id, eq_name.to_string());
        self.sever_direct_links(stream_id, &link_details);
        true
    }

    /// Destroy live links from `stream_id` straight into non-EQ sinks. Only
    /// ever called for streams we own (recorded in `routed_targets`) right
    /// after wiring our own path in, so the stream cannot play around the EQ
    /// -- and, with our link in place first, cannot go silent either. Links
    /// the session manager rebuilds later are re-severed by
    /// [`Self::heal_fallback_links`].
    fn sever_direct_links(&self, stream_id: u32, link_details: &HashMap<u32, Vec<(u32, String)>>) {
        let Some(links) = link_details.get(&stream_id) else {
            return;
        };
        for (lid, sink) in links {
            if sink.starts_with(VIRTUAL_SINK_BASE) {
                continue;
            }
            warn!(
                "Stream {stream_id}: severing direct link {lid} -> {sink} (superseded by EQ path)"
            );
            Self::destroy_link_object(*lid);
        }
    }

    /// Our EQ sink node names: the legacy base plus one per wanted device.
    /// Used to recognise our own side of live links (see
    /// `heal_fallback_links` and `flowing_count`).
    fn owned_eq_sink_names(&self) -> Vec<String> {
        let mut names = vec![VIRTUAL_SINK_BASE.to_string()];
        for dev in &self.eq_wanted {
            names.push(crate::core::eq_virtual_sink_for(dev));
        }
        names
    }

    /// Re-check streams we hold via fallback links: destroy our links where
    /// metadata has since won (avoids doubling on a slow session manager),
    /// and re-sever direct links the session manager rebuilt. No-op when no
    /// fallback links exist -- one lock check. Called on the adoption pump so
    /// it runs exactly when the graph moved.
    fn heal_fallback_links(&self) {
        // Orphans first, always: links for streams nobody owns must go before
        // any keep/drop decision below, or they self-perpetuate (live links
        // look healthy to the checks that follow).
        self.drop_orphan_fallback_links();
        // Then throttle the snapshot work: the pump tick calls every 10 ms,
        // but a full registry pass that often is pure burn.
        let now = std::time::Instant::now();
        if self
            .last_heal
            .get()
            .is_some_and(|t| now.duration_since(t) < Duration::from_secs(1))
        {
            return;
        }
        self.last_heal.set(Some(now));
        let owned: Vec<(u32, String)> = self
            .fallback_links
            .lock()
            .unwrap()
            .iter()
            .map(|(id, eq)| (*id, eq.clone()))
            .collect();
        if owned.is_empty() {
            return;
        }
        let link_details = self.stream_link_details();
        let alive: HashSet<u32> = self.list_stream_nodes().iter().map(|s| s.id).collect();
        for (id, eq_name) in owned {
            if !alive.contains(&id) {
                // Stream is gone; the daemon dropped its links already.
                self.fallback_links.lock().unwrap().remove(&id);
                continue;
            }
            // EQ-specific serial only: the broad processing-path set also
            // contains physical/default serials, and matching those once
            // dropped good fallback links for a stream merely sitting on its
            // original device ("metadata path live" lie -> silence).
            let eq_target = self.find_node_target(&eq_name);
            let meta_on_eq = eq_target.as_ref().is_some_and(|(_, serial)| {
                self.stream_target(id).target_object.as_deref() == Some(serial.as_str())
            });
            let linked_to_us = link_details
                .get(&id)
                .is_some_and(|v| v.iter().any(|(_, n)| n == &eq_name));
            if meta_on_eq {
                // Metadata path live: exactly one path may feed the EQ, so
                // destroy whatever links the stream holds into it and forget
                // the entry. Where metadata works the session manager
                // re-establishes its own links from the target within a
                // tick (one brief handoff blip, only in the race where we
                // linked first); where it never takes (sandbox) this branch
                // never fires. Either way no double, no orphan.
                if let Some(links) = link_details.get(&id) {
                    for (lid, sink) in links {
                        if sink == &eq_name {
                            Self::destroy_link_object(*lid);
                        }
                    }
                }
                self.fallback_links.lock().unwrap().remove(&id);
            } else if linked_to_us {
                // Link-held with metadata still elsewhere (sandbox steady
                // state): re-sever anything rebuilt direct.
                self.sever_direct_links(id, &link_details);
            } else if let Some((virt_id, serial)) = eq_target {
                // Our links died and metadata never took (chain rebuild,
                // daemon dropped them, or the verify raced a slow broadcast
                // and never linked). Re-wire rather than forget: the stream
                // is still recorded as ours, so adoption would skip it and
                // it would sit pathless forever.
                self.link_stream_fallback(id, virt_id, &serial, &eq_name);
            } else {
                // Chain itself is gone; forget the entry (teardown for
                // dead chains runs on the rebuild path).
                self.fallback_links.lock().unwrap().remove(&id);
            }
        }
    }

    /// Route unrecorded, non-internal streams into `physical_sink`'s chain.
    /// With `take_all` (Reroute) every unrecorded stream qualifies; otherwise
    /// only streams effectively aimed at the device. Streams already carrying
    /// a record are left alone even if they would qualify -- they belong to
    /// whoever routed them.
    fn adopt_unrecorded_into(
        &mut self,
        physical_sink: &str,
        take_all: bool,
    ) -> Result<usize, Error> {
        let eq = crate::core::eq_virtual_sink_for(physical_sink);
        let Some((virt_id, virt_serial)) = self.find_node_target(&eq) else {
            return Ok(0);
        };
        let chosen_serial: Option<String> = self
            .find_node_target(physical_sink)
            .map(|(_, serial)| serial);
        let Some(chosen_serial) = chosen_serial else {
            return Ok(0);
        };
        self.ensure_default_metadata()?;
        let mut adopted = 0usize;
        let mut adopted_ids: Vec<u32> = Vec::new();
        let mut wrote = false;
        {
            let md = self.default_metadata.as_ref().unwrap();
            // Live link topology as a fallback signal: direct `node.target`
            // links (e.g. `pw-play --target`) never produce metadata entries,
            // so a stream audibly playing on this device would otherwise be
            // invisible here.
            let link_targets = self.stream_link_targets();
            for s in self.list_stream_nodes() {
                if self.is_internal_stream(&s)
                    || s.dont_move
                    || self.routed_targets.lock().unwrap().contains_key(&s.id)
                {
                    continue;
                }
                let linked_here = link_targets
                    .get(&s.id)
                    .is_some_and(|v| v.iter().any(|n| n == physical_sink));
                let linked_into_own_eq = link_targets
                    .get(&s.id)
                    .is_some_and(|v| v.iter().any(|n| n == &eq));
                let qualifies = if take_all {
                    true
                } else {
                    let cur = self.stream_target(s.id);
                    match cur.target_object.as_deref() {
                        // No metadata target: only adopt on link evidence.
                        // The old `default == chosen` arm also fired for
                        // brand-new streams whose metadata simply had not
                        // arrived yet (pw-play et al. set node properties
                        // first, metadata follows), arming them for the
                        // default device before their real target was known
                        // -- and recording them, so the correct chain skipped
                        // them forever (late-stream race). A still-linkless
                        // paused stream waits too; its first links re-flag
                        // dirty and the next pass decides with full
                        // information. Reroute takes everything regardless.
                        None | Some("") => linked_here || linked_into_own_eq,
                        Some(t) => t == chosen_serial,
                    }
                };
                if !qualifies && !linked_here && !linked_into_own_eq {
                    continue;
                }
                let cur = self.stream_target(s.id);
                md.set_property(
                    s.id,
                    "target.node",
                    Some("Spa:Id"),
                    Some(&virt_id.to_string()),
                );
                md.set_property(s.id, "target.object", Some("Spa:Id"), Some(&virt_serial));
                self.routed_targets.lock().unwrap().insert(s.id, cur);
                info!("Adopted stream '{}' ({}) into {eq}", s.node_name, s.id);
                adopted += 1;
                adopted_ids.push(s.id);
                wrote = true;
            }
        }
        if wrote {
            *self.last_route_target.lock().unwrap() = Some((virt_id, virt_serial.clone()));
            self.roundtrip()?;
        }
        // Read-back verification with link fallback (no-op where metadata
        // took), plus healing for streams adopted on earlier passes.
        // Deferred read-back verification (see `defer_verify`).
        self.defer_verify(&adopted_ids, virt_id, &virt_serial, &eq);
        self.heal_fallback_links();
        Ok(adopted)
    }

    /// Put the routed streams back on their own targets WITHOUT forgetting that
    /// they are routed.
    ///
    /// This is the "before" half of upstream's `restart_engine`: it restores
    /// the streams, stops the engine, and re-routes once the new engine is
    /// ready. The record and the routed flag are deliberately kept, so the
    /// re-route that follows the rebuild still knows these streams are ours and
    /// still knows where each one came from.
    ///
    /// Needed because a filter-chain reload destroys `mini_eq_sink`: streams
    /// left pointed at it are pointed at nothing, and WirePlumber's recovery
    /// from that is both slower and less predictable than a deliberate
    /// live-to-live move.
    pub fn suspend_routing(&mut self) -> Result<(), Error> {
        if !self.routed {
            return Ok(());
        }
        // Restorable, not just routable: a recorded stream pointed at the
        // virtual sink must be moved off it even when the mode scope would
        // not route it (see restorable_output_streams).
        let streams = self.restorable_output_streams();
        let recorded = self.routed_targets.lock().unwrap().clone();
        if recorded.is_empty() {
            return Ok(());
        }
        let mut moved = 0usize;
        {
            self.ensure_default_metadata()?;
            let md = self.default_metadata.as_ref().unwrap();
            for node in &streams {
                let Some(target) = recorded.get(&node.id) else {
                    continue;
                };
                md.set_property(
                    node.id,
                    "target.node",
                    target.target_node_type.as_deref(),
                    target.target_node.as_deref(),
                );
                md.set_property(
                    node.id,
                    "target.object",
                    target.target_object_type.as_deref(),
                    target.target_object.as_deref(),
                );
                moved += 1;
            }
        }
        if moved > 0 {
            self.roundtrip()?;
            info!("Suspended routing for {moved} stream(s) across the engine rebuild");
        }
        // The rebuild destroys the EQ nodes, and the daemon drops every link
        // into them -- including our fallbacks. Forget the ids now; the
        // re-route after the rebuild re-verifies each stream from scratch.
        self.fallback_links.lock().unwrap().clear();
        self.pending_verify.clear();
        Ok(())
    }

    pub fn auto_route_to_sink(&mut self, sink_name: &str) -> Result<(), Error> {
        self.auto_route_to_sink_with_mode(sink_name, self.output_mode)
    }

    /// Route under an explicit mode. The switch handler passes the engine's
    /// mode; the scope predicate (`is_in_scope`) is what differs between
    /// Selected and Reroute, and it is only meaningful against a real sink.
    pub fn auto_route_to_sink_with_mode(
        &mut self,
        sink_name: &str,
        mode: crate::core::OutputRoutingMode,
    ) -> Result<(), Error> {
        info!("Auto-routing to sink: {sink_name} (mode {:?})", mode);

        let (sink_id, sink_serial) = {
            // The virtual sink node can be missing if we just created its
            // chain: the registry has not delivered the global yet. `find_node_target`
            // does one roundtrip, so poll it a few times with a short pump
            // before declaring failure -- bounded so a genuinely missing node
            // cannot hang the caller.
            let deadline = std::time::Instant::now() + Duration::from_millis(800);
            let mut found = self.find_node_target(sink_name);
            while found.is_none() && std::time::Instant::now() < deadline {
                self.mainloop
                    .loop_()
                    .iterate(Timeout::Finite(Duration::from_millis(50)));
                found = self.find_node_target(sink_name);
            }
            match found {
                Some(t) => t,
                None => {
                    warn!("Virtual sink node {sink_name} not found; cannot auto-route");
                    return Err(Error::CreationFailed);
                }
            }
        };

        // Scope against the PHYSICAL chain output as well as the virtual
        // sink. `sink_name` here is the virtual sink (the route destination),
        // but Selected means "streams already aimed at the chosen DEVICE":
        // a stream we just sent to the fallback (explicit real-device target)
        // would otherwise never be re-routed under Selected -- the re-on
        // after an off routed 0 streams in the live test.
        //
        // When `sink_name` is a registered device sink, its owning physical
        // device is the scope; the legacy singleton falls back to the global
        // chain output.
        let scope_physical = self
            .physical_for_eq_sink(sink_name)
            .or_else(|| self.chain_output_sink.clone());
        let scope_sinks: Vec<String> = {
            let mut v = vec![sink_name.to_string()];
            if let Some(physical) = scope_physical {
                if physical != sink_name {
                    v.push(physical);
                }
            }
            v
        };
        let mut streams: Vec<StreamNode> = Vec::new();
        for scope in &scope_sinks {
            for s in self.routable_output_streams_for(scope, mode) {
                if !streams.iter().any(|x| x.id == s.id) {
                    streams.push(s);
                }
            }
        }
        let mut routed = 0usize;
        let mut routed_ids: Vec<u32> = Vec::new();
        let mut skipped_dont_move = 0usize;
        for node in &streams {
            let node_id = &node.id;
            let name = &node.node_name;
            // `node.dont-move` is the stream owner's own instruction not to
            // move it. Upstream raises on it; here it is counted and left alone.
            if node.dont_move {
                debug!("not routing '{name}' ({node_id}): node.dont-move");
                skipped_dont_move += 1;
                continue;
            }
            // Remember where this stream pointed BEFORE we move it, exactly as
            // upstream's `PipeWireStreamRouter` does. This is what makes the
            // way back faithful: a stream that was deliberately pointed at
            // another device, or one with no explicit target at all, goes back
            // to that, not to whatever WirePlumber picks for "no target".
            //
            // Never record our own virtual sink as the origin: a stream that
            // is already EQ-pointed (failed unroute, previous instance) would
            // otherwise cement the EQ target as "where it came from" and keep
            // pointing at `mini_eq_sink` past unroute and quit.
            {
                let mut recorded = self.routed_targets.lock().unwrap();
                if !recorded.contains_key(node_id) {
                    let cur = self.stream_target(*node_id);
                    let obj = cur.target_object.as_deref().unwrap_or("");
                    let ours = !obj.is_empty()
                        && (Some(obj) == Some(sink_serial.as_str())
                            || self
                                .find_node_target(crate::core::VIRTUAL_SINK_BASE)
                                .map(|(_, serial)| serial)
                                .as_deref()
                                == Some(obj));
                    if ours {
                        debug!("not recording '{name}' ({node_id}): already on the EQ path");
                    } else {
                        recorded.insert(*node_id, cur);
                    }
                }
            }
            *self.last_route_target.lock().unwrap() = Some((sink_id, sink_serial.clone()));
            match self.set_stream_target(*node_id, sink_id, &sink_serial) {
                Ok(()) => {
                    info!(
                        "Routed '{}' ({}) -> {} (target.node={}, target.object={})",
                        name, node_id, sink_name, sink_id, sink_serial
                    );
                    routed += 1;
                    routed_ids.push(*node_id);
                }
                Err(e) => warn!("Failed to route '{}' ({}): {}", name, node_id, e),
            }
        }

        // Read-back verification with link fallback where the metadata write
        // did not take (sandboxed daemon). No-op where it did.
        // Deferred read-back verification (see `defer_verify`).
        self.defer_verify(&routed_ids, sink_id, &sink_serial, sink_name);
        self.heal_fallback_links();

        self.set_current_sink(sink_name);
        self.auto_route = true;
        self.routed = true;
        self.last_routed_count = routed;

        if skipped_dont_move > 0 {
            info!("Left {} stream(s) alone: node.dont-move", skipped_dont_move);
        }
        if routed == 0 {
            // Loud on purpose: EQ on with nothing routed is exactly the
            // "EQ doesn't do anything" report, and every quieter signal was
            // missed. The window mirrors this as a toast.
            warn!(
                "Auto-routing to {sink_name}: no streams in scope -- nothing will play through the EQ"
            );
        }
        info!(
            "Auto-routing complete: {} stream(s) -> {}",
            routed, sink_name
        );
        Ok(())
    }

    /// Streams routed by the most recent `auto_route_to_sink*` call. The
    /// window uses it for the "EQ on but nothing to equalize" toast.
    pub fn last_routed_count(&self) -> usize {
        self.last_routed_count
    }

    /// Clear the `target.node`/`target.object` metadata for all playback
    /// streams so WirePlumber returns them to the default sink (System EQ
    /// off). Mirrors upstream's unroute path.
    /// Hand the playback streams back to where they were before the EQ took
    /// them.
    ///
    /// Every stream routed by `auto_route_to_sink` had its previous target
    /// recorded, so it is put back verbatim -- node id, object serial and their
    /// metadata types, exactly as upstream's
    /// `PipeWireStreamRouter.restore_output_streams` does. Anything not in that
    /// record (the EQ was enabled before this process started, or a stream
    /// appeared mid-session) falls back to `fallback_sink`, or to clearing the
    /// target when there is nothing better.
    ///
    /// The difference from just clearing the properties is the whole point:
    /// clearing hands the choice of destination back to WirePlumber, which then
    /// re-resolves the stream's target from scratch, and the silence while it
    /// does is what made toggling the switch audibly disruptive.
    pub fn unroute_all(&mut self, fallback_sink: Option<&str>) -> Result<(), Error> {
        // Restorable, not just routable: streams we pointed at the virtual
        // sink stay restore candidates even when the mode scope excludes
        // them (device switch points current_sink at the physical sink, which
        // would otherwise hide every EQ-pointed stream under Selected).
        let streams = self.restorable_output_streams();

        // Is anything still pointed at the EQ? Check every chain we own (all
        // registered serials, not just the latest), since a stream left in a
        // per-device chain would otherwise slip past the early return.
        let route_target = self.last_route_target.lock().unwrap().clone();
        let our_serials: HashSet<String> = self.processing_path_targets().into_iter().collect();
        let still_on_eq = {
            let mut found = route_target.as_ref().is_some_and(|(_, serial)| {
                streams.iter().any(|node| {
                    self.stream_target(node.id).target_object.as_deref() == Some(serial.as_str())
                })
            });
            if !found {
                found = streams.iter().any(|node| {
                    self.stream_target(node.id)
                        .target_object
                        .as_deref()
                        .is_some_and(|t| our_serials.contains(t))
                });
            }
            found
        };
        let recorded_empty = self.routed_targets.lock().unwrap().is_empty();
        if !still_on_eq && recorded_empty {
            info!("Unroute: nothing is pointed at the EQ; nothing to restore");
            return Ok(());
        }

        // One batch, one sync: the streams must all start moving together, or
        // the hand-off is serialised and the gap grows with the stream count.
        let recorded = self.routed_targets.lock().unwrap().clone();
        let mut restored_ids: HashSet<u32> = HashSet::new();
        let mut restored = 0usize;
        {
            self.ensure_default_metadata()?;
            for node in &streams {
                let (node_id, name) = (&node.id, &node.node_name);
                if let Some(target) = recorded.get(node_id) {
                    self.restore_stream_target(*node_id, target)?;
                    restored_ids.insert(*node_id);
                    info!("Restored '{name}' ({node_id}) to its own target");
                    restored += 1;
                }
            }
        }
        if restored > 0 {
            self.roundtrip()?;
        }

        // Everything else we could legitimately have routed: nothing was
        // recorded, so we do not know where it came from. Fall back to the sink
        // the EQ was feeding, then to clearing the target.
        //
        // Built from `restorable_output_streams()` deliberately: a stream we
        // refuse to route (a blocklisted desktop or speech app, or one the user
        // pointed at another device) must not be written to on the way out
        // either -- sending it to the fallback would be exactly the disruption
        // the routability filter just stopped us causing. Streams already
        // pointed into our own processing path ARE included (they are ours),
        // so an unrecorded one still lands on the fallback instead of dangling.
        let leftovers: Vec<(u32, String)> = streams
            .iter()
            .filter(|node| !restored_ids.contains(&node.id))
            .map(|node| (node.id, node.node_name.clone()))
            .collect();
        let mut moved = 0usize;
        if !leftovers.is_empty() {
            // Same batching as the recorded pass: write everything, sync once.
            let fallback = fallback_sink.and_then(|name| self.find_node_target(name));
            let mut wrote = false;
            {
                self.ensure_default_metadata()?;
                let md = self.default_metadata.as_ref().unwrap();
                for (stream_id, name) in &leftovers {
                    match &fallback {
                        Some((node_id, serial)) => {
                            md.set_property(
                                *stream_id,
                                "target.node",
                                Some("Spa:Id"),
                                Some(&node_id.to_string()),
                            );
                            md.set_property(
                                *stream_id,
                                "target.object",
                                Some("Spa:Id"),
                                Some(serial),
                            );
                        }
                        None => {
                            md.set_property(*stream_id, "target.node", None, None);
                            md.set_property(*stream_id, "target.object", None, None);
                        }
                    }
                    info!("No recorded target for '{name}' ({stream_id}); sent to the fallback");
                    wrote = true;
                    moved += 1;
                }
            }
            if wrote {
                self.roundtrip()?;
            }
        }

        self.routed_targets.lock().unwrap().clear();
        *self.last_route_target.lock().unwrap() = None;
        self.routed = false;
        // Every chain is down: no stream may keep a fallback link anywhere,
        // and no pending verification may fire afterwards. Drop the whole
        // map, not just restored ids: entries for vanished streams would
        // otherwise orphan (nobody owns them any more, so heal would keep
        // their links forever).
        let released: Vec<u32> = restored_ids.into_iter().collect();
        self.drop_fallback_links(&released);
        self.pending_verify.clear();
        self.fallback_links.lock().unwrap().clear();
        info!(
            "Unroute complete: {restored} stream(s) restored to their own target, {moved} to the fallback"
        );
        Ok(())
    }

    pub fn set_stream_target(
        &mut self,
        stream_id: u32,
        sink_bound_id: u32,
        sink_serial: &str,
    ) -> Result<(), Error> {
        self.ensure_default_metadata()?;
        let md = self.default_metadata.as_ref().unwrap();
        md.set_property(
            stream_id,
            "target.node",
            Some("Spa:Id"),
            Some(&sink_bound_id.to_string()),
        );
        md.set_property(
            stream_id,
            "target.object",
            Some("Spa:Id"),
            Some(sink_serial),
        );
        self.roundtrip()
    }

    /// A stream's routing target as it currently stands in the `default`
    /// metadata, read from the property cache.
    ///
    /// Upstream: `PipeWireBackend.stream_target`. Returns an empty
    /// `StreamTarget` for a stream that had no explicit target, which is the
    /// normal case -- WirePlumber picks the default for those.
    pub fn stream_target(&self, stream_id: u32) -> StreamTarget {
        let cache = self.target_cache.lock().unwrap();
        // The cache stores `(type, value)`; the fields are value-and-type.
        // Getting this backwards makes every stream look like it is pointed at
        // the literal string "Spa:Id", so the foreign-target check rejects
        // everything and nothing is ever routed.
        let get = |key: &str| {
            cache
                .get(&(stream_id, key.to_string()))
                .cloned()
                .unwrap_or((None, None))
        };
        let (target_node_type, target_node) = get("target.node");
        let (target_object_type, target_object) = get("target.object");
        StreamTarget {
            target_node,
            target_node_type,
            target_object,
            target_object_type,
        }
    }

    /// Write a stream's recorded target back, types included.
    ///
    /// Upstream: `PipeWireBackend.restore_stream_target`.
    fn restore_stream_target(
        &mut self,
        stream_id: u32,
        target: &StreamTarget,
    ) -> Result<(), Error> {
        self.ensure_default_metadata()?;
        let md = self.default_metadata.as_ref().unwrap();
        md.set_property(
            stream_id,
            "target.node",
            target.target_node_type.as_deref(),
            target.target_node.as_deref(),
        );
        md.set_property(
            stream_id,
            "target.object",
            target.target_object_type.as_deref(),
            target.target_object.as_deref(),
        );
        Ok(())
    }

    /// Look up a sink's node id and `object.serial`, both of which
    /// `retarget_filter_output` needs.
    pub fn sink_id_and_serial(&self, sink_name: &str) -> Option<(u32, String)> {
        let registry = self.core.get_registry().ok()?;
        let found = Arc::new(Mutex::new(None));
        let found_clone = found.clone();
        let target = sink_name.to_string();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ != ObjectType::Node {
                    return;
                }
                let Some(props) = &global.props else { return };
                if props.get("node.name").unwrap_or("") != target {
                    return;
                }
                let serial = props.get("object.serial").unwrap_or("").to_string();
                if !serial.is_empty() {
                    *found_clone.lock().unwrap() = Some((global.id, serial));
                }
            })
            .register();
        let _ = self.roundtrip();
        found.lock().unwrap().clone()
    }

    /// Re-point the EQ's own output at a different sink.
    ///
    /// `create_filter_chain` sets `playback.props.target.object` at module load
    /// time, so the output client is born pointing at one device. Writing
    /// metadata is the supported way to move it afterwards, and must use the
    /// same keys and the same **object.serial** that `set_stream_target` uses for
    /// playback streams — a device *name* is silently ignored.
    ///
    /// Returns the metadata subject that was written, so callers can verify the
    /// move actually happened rather than assuming it.
    pub fn retarget_filter_output(
        &mut self,
        sink_bound_id: u32,
        sink_serial: &str,
    ) -> Result<u32, Error> {
        self.ensure_default_metadata()?;
        // The output client's *node name* is `<VIRTUAL_SINK_BASE>_output`
        // (see `create_filter_chain`). `OUTPUT_CLIENT_NAME` is its
        // node.description, which is why looking that up found nothing.
        let output_node_name = format!("{VIRTUAL_SINK_BASE}_output");
        let subject = self
            .find_node_id_by_name(&output_node_name)
            .ok_or(Error::CreationFailed)?;
        let md = self.default_metadata.as_ref().unwrap();
        md.set_property(
            subject,
            "target.node",
            Some("Spa:Id"),
            Some(&sink_bound_id.to_string()),
        );
        md.set_property(subject, "target.object", Some("Spa:Id"), Some(sink_serial));
        self.roundtrip()?;
        Ok(subject)
    }

    /// Clear a stream's routing target so WirePlumber returns it to the
    /// default sink (System EQ off).
    pub fn clear_stream_target(&mut self, stream_id: u32) -> Result<(), Error> {
        self.ensure_default_metadata()?;
        let md = self.default_metadata.as_ref().unwrap();
        md.set_property(stream_id, "target.node", None, None);
        md.set_property(stream_id, "target.object", None, None);
        self.roundtrip()
    }

    /// Bind the `default` metadata now so the default sink is known.
    ///
    /// The binding is otherwise lazy (first routing op), which left
    /// `default_audio_sink_name()` empty at startup: the engine was never
    /// created, the Output dropdown showed only "Default Output" with nothing
    /// behind it, and the monitor tapped nothing. The server replays current
    /// properties when the listener binds, so one call here is enough.
    pub fn prime_default_sink(&mut self) -> Result<(), Error> {
        self.ensure_default_metadata()
    }

    /// Lazily bind (and cache) the PipeWire `default` metadata object from
    /// the registry global whose `metadata.name == "default"`. While binding,
    /// register a `property` listener so the server's replayed properties
    /// populate `default_audio_sink` / `configured_audio_sink`.
    fn ensure_default_metadata(&mut self) -> Result<(), Error> {
        if self.default_metadata.is_some() {
            return Ok(());
        }
        let registry = self.core.get_registry_rc()?;
        let found = Rc::new(RefCell::new(None::<Metadata>));
        let found_c = found.clone();
        let reg_c = registry.clone();
        let sink_c = self.default_audio_sink.clone();
        let cfg_c = self.configured_audio_sink.clone();
        let changed_c = self.default_sink_changed.clone();
        let listeners_c = self.metadata_listeners.clone();
        let cache_c = self.target_cache.clone();
        let dirty_c = self.streams_dirty.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |g| {
                if g.type_ == ObjectType::Metadata
                    && g.props
                        .as_ref()
                        .map(|p| p.get("metadata.name").unwrap_or("") == "default")
                        .unwrap_or(false)
                {
                    if let Ok(md) = reg_c.bind::<Metadata, _>(g) {
                        // Register the property listener BEFORE the roundtrip
                        // completes so the server's initial property replay
                        // is captured. Mirrors upstream
                        // `remember_default_metadata_change`.
                        let sink_l = sink_c.clone();
                        let cfg_l = cfg_c.clone();
                        let cache_l = cache_c.clone();
                        let changed_l = changed_c.clone();
                        let dirty_l = dirty_c.clone();
                        let _pl = md
                            .add_listener_local()
                            .property(move |subject, key, type_, value| {
                                // Record per-stream targets on the way past.
                                // This is the only reader available: the Rust
                                // `Metadata` wrapper has no getter, and upstream
                                // reads the same values with
                                // `metadata.dup_value(subject, key)`. The server
                                // replays current properties when this listener
                                // binds, so a stream's existing target is here
                                // before we touch it.
                                if let Some(k) = key
                                    && matches!(k, "target.node" | "target.object")
                                {
                                    cache_l.lock().unwrap().insert(
                                        (subject, k.to_string()),
                                        (
                                            type_.map(|t| t.to_string()),
                                            value.map(|v| v.to_string()),
                                        ),
                                    );
                                    // A stream moved (by us, the user in a
                                    // mixer, or WirePlumber): re-check scope on
                                    // the next pump tick. Our own writes set it
                                    // too; the follow-up scan then finds
                                    // nothing new, which is cheap.
                                    dirty_l.set(true);
                                }
                                match key {
                                    Some("default.audio.sink") => {
                                        let parsed = parse_metadata_node_name(value);
                                        info!(
                                            "metadata property default.audio.sink = {:?} -> {:?}",
                                            value, parsed
                                        );
                                        *sink_l.borrow_mut() = parsed;
                                        *changed_l.borrow_mut() = true;
                                    }
                                    Some("default.configured.audio.sink") => {
                                        *cfg_l.borrow_mut() = parse_metadata_node_name(value);
                                    }
                                    _ => {
                                        debug!("metadata property {:?} = {:?}", key, value);
                                    }
                                }
                                0
                            })
                            .register();
                        listeners_c.borrow_mut().push(_pl);
                        *found_c.borrow_mut() = Some(md);
                    }
                }
            })
            .register();
        self.roundtrip()?;
        let md = found.borrow_mut().take().ok_or(Error::CreationFailed)?;
        self.default_metadata = Some(md);
        info!(
            "Bound default PipeWire metadata (default.audio.sink={:?})",
            self.default_audio_sink.borrow().as_deref()
        );
        Ok(())
    }

    /// The user's current default audio sink node name, read from the
    /// `default` metadata. Falls back to the configured sink. This is the
    /// sink the filter-chain playback node should target so EQ'd audio
    /// reaches the speakers the user actually hears on — portable across
    /// any PipeWire machine.
    pub fn default_audio_sink_name(&self) -> Option<String> {
        self.default_audio_sink
            .borrow()
            .clone()
            .or_else(|| self.configured_audio_sink.borrow().clone())
    }

    /// The sink the filter chain's output points at, if known.
    pub fn current_sink(&self) -> Option<String> {
        self.current_sink.clone()
    }

    /// The current system default output, updated by the metadata `property`
    /// listener in real time.
    ///
    /// Unlike `default_audio_sink_name` this does not pump the loop: the
    /// listener fires synchronously when the server delivers the property, so
    /// the value is already here. The pump in the old `refresh_*` method was
    /// a 50 ms stall per 500 ms tick that could not observe anything the
    /// listener had not already delivered.
    pub fn current_default_audio_sink(&self) -> Option<String> {
        self.default_audio_sink
            .borrow()
            .clone()
            .or_else(|| self.configured_audio_sink.borrow().clone())
    }

    /// If the system default output changed since the last call, return the
    /// new sink and clear the flag. The metadata `property` listener sets the
    /// flag; this is the single consumer, so a change is acted on exactly once.
    ///
    /// Returns `None` when nothing changed -- which is the common case, so the
    /// 500 ms default-sink poll can be a cheap flag read instead of a pump.
    pub fn take_default_sink_change(&self) -> Option<String> {
        if !*self.default_sink_changed.borrow() {
            return None;
        }
        *self.default_sink_changed.borrow_mut() = false;
        self.current_default_audio_sink()
    }

    /// Find a node's bound id AND its `object.serial` by node.name.
    /// The serial is required for the modern `target.object` metadata key
    /// (WirePlumber policy matches on the object serial, not just the id).
    pub fn find_node_target(&self, name: &str) -> Option<(u32, String)> {
        let registry = self.core.get_registry().ok()?;
        let found = Arc::new(Mutex::new(None::<(u32, String)>));
        let found_clone = found.clone();
        let name = name.to_string();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node
                    && let Some(props) = &global.props
                    && props.get("node.name").unwrap_or("") == name
                {
                    if let Some(serial) = props.get("object.serial") {
                        *found_clone.lock().unwrap() = Some((global.id, serial.to_string()));
                    }
                }
            })
            .register();
        let _ = self.roundtrip();
        found.lock().unwrap().clone()
    }
    ///
    /// `output_node` is the stream producer (e.g. an app playback stream) and
    /// `input_node` is the consumer (e.g. the EQ virtual sink).
    pub fn link_nodes(&self, output_node: u32, input_node: u32) -> Result<u32, Error> {
        let link_props = properties! {
            "link.output.node" => output_node.to_string().as_str(),
            "link.input.node" => input_node.to_string().as_str(),
        };

        let link = self
            .core
            .create_object::<Link>("link-factory", &link_props)
            .inspect_err(|e| warn!("link-factory create failed: {}", e))?;
        let lid = link.upcast().id();

        self.links.lock().unwrap().push(RouteInfo {
            route_id: output_node,
            source_node: output_node,
            target_node: input_node,
            link_id: lid,
        });

        info!("Created link {} ({} -> {})", lid, output_node, input_node);
        Ok(lid)
    }

    /// Find a node id by `node.name`, pumping the loop so registry events
    /// actually arrive before the lookup is read.
    pub fn find_node_id_by_name(&self, name: &str) -> Option<u32> {
        let registry = self.core.get_registry().ok()?;
        let found = Arc::new(Mutex::new(None));
        let found_clone = found.clone();
        let name = name.to_string();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node
                    && let Some(props) = &global.props
                    && props.get("node.name").unwrap_or("") == name
                {
                    *found_clone.lock().unwrap() = Some(global.id);
                }
            })
            .register();
        let _ = self.roundtrip();
        *found.lock().unwrap()
    }

    /// List app playback stream nodes (`media.class = Stream/Output/Audio`).
    pub fn list_playback_streams(&self) -> Vec<(u32, String)> {
        self.list_stream_nodes()
            .into_iter()
            .map(|n| (n.id, n.node_name))
            .collect()
    }

    /// Every playback stream, with the properties the routability filter needs.
    pub fn list_stream_nodes(&self) -> Vec<StreamNode> {
        let registry = match self.core.get_registry() {
            Ok(r) => r,
            Err(_) => return Vec::new(),
        };
        let found = Arc::new(Mutex::new(Vec::new()));
        let found_clone = found.clone();
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == ObjectType::Node
                    && let Some(props) = &global.props
                    && props.get("media.class").unwrap_or("") == "Stream/Output/Audio"
                {
                    found_clone.lock().unwrap().push(StreamNode {
                        id: global.id,
                        node_name: props.get("node.name").unwrap_or("unknown").to_string(),
                        app_name: props
                            .get("application.name")
                            .or_else(|| props.get("node.name"))
                            .unwrap_or("")
                            .to_string(),
                        media_role: props.get("media.role").unwrap_or("").to_string(),
                        dont_move: props.get("node.dont-move").unwrap_or("") == "true",
                    });
                }
            })
            .register();
        let _ = self.roundtrip();
        let result = found.lock().unwrap().clone();
        debug!("Found {} playback stream(s)", result.len());
        result
    }

    pub fn create_link(&self, source_id: u32, target_name: &str) -> Result<u32, Error> {
        info!(
            "Creating link from source {} to target {}",
            source_id, target_name
        );

        let target_id = match self.find_node_id_by_name(target_name) {
            Some(id) => id,
            None => {
                warn!("Could not find target node {} for link", target_name);
                return Err(Error::CreationFailed);
            }
        };

        self.link_nodes(source_id, target_id)
    }

    pub fn remove_link(&self, link_id: u32) -> Result<(), Error> {
        info!("Removing link {}", link_id);

        let mut links = self.links.lock().unwrap();
        links.retain(|l| l.link_id != link_id);

        Ok(())
    }

    pub fn get_links(&self) -> Vec<RouteInfo> {
        self.links.lock().unwrap().clone()
    }

    pub fn route_stream(&self, _stream_id: u32, _target_sink: &str) -> Result<(), Error> {
        info!("Routing stream to sink");
        Ok(())
    }

    pub fn unroute_stream(&self, _stream_id: u32) -> Result<(), Error> {
        info!("Unrouting stream");
        Ok(())
    }

    pub fn get_virtual_sink_name(&self) -> &str {
        &self.virtual_sink_name
    }

    pub fn is_auto_route(&self) -> bool {
        self.auto_route
    }

    pub fn set_auto_route(&mut self, auto: bool) {
        self.auto_route = auto;
    }

    pub fn scan_streams(&self) -> Result<Vec<StreamInfo>, Error> {
        info!("Scanning PipeWire streams");

        let streams = Arc::new(Mutex::new(Vec::new()));
        let registry = self.core.get_registry()?;

        let streams_clone = streams.clone();
        let listener = registry.add_listener_local();
        let listener = listener.global(move |global| {
            if global.type_ == ObjectType::Node
                && let Some(props) = &global.props
            {
                let name = props.get("node.name").unwrap_or("unknown");
                let info = StreamInfo {
                    id: global.id,
                    node_id: global.id,
                    name: name.to_string(),
                    media_type: String::new(),
                    media_role: String::new(),
                    channels: 2,
                    rate: SAMPLE_RATE as u32,
                    target_sink: None,
                    active: true,
                };
                streams_clone.lock().unwrap().push(info);
                debug!("Found stream: {} (id={})", name, global.id);
            }
        });
        let _listener = listener.register();

        // Pump the loop so the `global` events populate the stream snapshot.
        self.roundtrip()?;

        let result = streams.lock().unwrap().clone();
        info!("Scanned {} streams", result.len());
        Ok(result)
    }

    pub fn get_streams(&self) -> Vec<StreamInfo> {
        self.streams.lock().unwrap().values().cloned().collect()
    }

    pub fn route_to_virtual_sink(&self, stream_id: u32) -> Result<(), Error> {
        info!(
            "Routing stream {} to virtual sink {}",
            stream_id, VIRTUAL_SINK_BASE
        );

        let sink_name = format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX);

        let mut routing = self.routing_table.lock().unwrap();
        routing.insert(
            stream_id,
            sink_name.clone().into_bytes().iter().sum::<u8>() as u32,
        );

        {
            let mut streams_guard = self.streams.lock().unwrap();
            if let Some(stream) = streams_guard.get_mut(&stream_id) {
                stream.target_sink = Some(sink_name.clone());
            }
        }

        info!("Stream {} routed to virtual sink", stream_id);
        Ok(())
    }

    pub fn unroute_from_virtual_sink(&self, stream_id: u32) -> Result<(), Error> {
        info!("Unrouting stream {} from virtual sink", stream_id);

        self.routing_table.lock().unwrap().remove(&stream_id);

        {
            let mut streams_guard = self.streams.lock().unwrap();
            if let Some(stream) = streams_guard.get_mut(&stream_id) {
                stream.target_sink = None;
            }
        }

        Ok(())
    }

    pub fn auto_route_all(&mut self) -> Result<(), Error> {
        info!("Auto-routing all streams to virtual sink");

        let streams = self.scan_streams()?;
        let sink_name = format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX);

        for stream in &streams {
            if stream.name.contains(VIRTUAL_SINK_BASE) || stream.name.contains(OUTPUT_CLIENT_NAME) {
                continue;
            }
            self.route_to_virtual_sink(stream.id)?;
        }

        self.auto_route = true;
        self.current_sink = Some(sink_name);

        info!("Auto-routed {} streams", streams.len());
        Ok(())
    }

    pub fn get_virtual_sink_streams(&self) -> Vec<StreamInfo> {
        self.streams
            .lock()
            .unwrap()
            .values()
            .filter(|s| {
                s.target_sink
                    .as_ref()
                    .map(|t| t.starts_with(VIRTUAL_SINK_BASE))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    pub fn get_routing_table(&self) -> HashMap<u32, u32> {
        self.routing_table.lock().unwrap().clone()
    }

    pub fn create_virtual_sink_stream(&self) -> Result<StreamBox<'_>, Error> {
        info!("Creating virtual sink stream");

        let sink_name = format!("{}{}", VIRTUAL_SINK_BASE, FILTER_OUTPUT_SUFFIX);

        let sink_props = properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Stream",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::NODE_NAME => sink_name.as_str(),
            *pipewire::keys::NODE_DESCRIPTION => OUTPUT_CLIENT_NAME,
            *pipewire::keys::MEDIA_CLASS => "Audio/Sink",
            "audio.channels" => "2",
            "audio.rate" => SAMPLE_RATE.to_string().as_str(),
            "audio.format" => "f32le",
        };

        let stream = StreamBox::new(&self.core, &sink_name, sink_props)?;

        info!("Virtual sink stream created");
        Ok(stream)
    }

    pub fn monitor_streams(&self) -> Result<(), Error> {
        info!("Monitoring PipeWire streams for changes");
        Ok(())
    }
}

impl Default for RoutingEngine {
    fn default() -> Self {
        let mainloop =
            pipewire::main_loop::MainLoopRc::new(None).expect("Failed to create MainLoop");
        let context =
            pipewire::context::ContextRc::new(&mainloop, None).expect("Failed to create Context");
        let core = context
            .connect_rc(None)
            .expect("Failed to connect to PipeWire");
        RoutingEngine::new(core, mainloop)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sink filter used to match on `node.name` containing "audio.sink" /
    /// "output" / "analog". That both missed devices (USB, bluetooth, network
    /// sinks) and matched inputs (`alsa_input...analog-stereo` is a microphone).
    /// These constants and the `media.class` comparison are what replaced it,
    /// matching upstream's `is_audio_sink`.
    #[test]
    fn audio_sink_media_class_matches_upstream() {
        assert_eq!(AUDIO_SINK_MEDIA_CLASS, "Audio/Sink");
        // Sanity: the name heuristic this replaced would have matched an input.
        assert!(
            "alsa_input.pci-0000_04_00.6.analog-stereo".contains("analog"),
            "the old heuristic matched input nodes; that is why media.class is used"
        );
    }

    #[test]
    fn display_label_prefers_description_then_falls_back_to_name() {
        assert_eq!(
            display_label("Ryzen HD Audio Controller Analog Stereo", "alsa_output.x"),
            "Ryzen HD Audio Controller Analog Stereo"
        );
        assert_eq!(display_label("", "alsa_output.x"), "alsa_output.x");
        assert_eq!(display_label("   ", "alsa_output.x"), "alsa_output.x");
    }

    /// Our own virtual sink must never appear as a selectable output device.
    #[test]
    fn virtual_sink_is_excluded_by_prefix() {
        assert!(VIRTUAL_SINK_BASE.starts_with("mini_eq"));
        assert!("mini_eq_sink".starts_with(VIRTUAL_SINK_BASE));
        assert!("mini_eq_sink.source".starts_with(VIRTUAL_SINK_BASE));
        // A real device must not be caught by the filter.
        assert!(!"alsa_output.pci-0000_04_00.6.analog-stereo".starts_with(VIRTUAL_SINK_BASE));
    }

    /// The scope predicate is the whole point of the mode split. It is pure --
    /// it takes the target string rather than a stream -- so it is testable
    /// without a live PipeWire connection.
    ///
    /// A stream with no explicit target is in scope for the chosen device
    /// (WirePlumber is choosing the default, which is the device we picked);
    /// one aimed at the chosen serial is in scope; one aimed at another
    /// serial is out. Reroute overrides all of that and takes everything.
    /// The blocklist is applied before this predicate, so it is not exercised
    /// here -- `target_in_scope` is only ever asked about a stream that
    /// survived it.
    #[test]
    fn mode_scope_predicate() {
        use crate::core::OutputRoutingMode;

        // Selected: the chosen serial wins, anything else is out, and no
        // target means "the default is the device I picked".
        assert!(RoutingEngine::target_in_scope(
            None,
            "AAA",
            OutputRoutingMode::Selected
        ));
        assert!(RoutingEngine::target_in_scope(
            Some(""),
            "AAA",
            OutputRoutingMode::Selected
        ));
        assert!(RoutingEngine::target_in_scope(
            Some("AAA"),
            "AAA",
            OutputRoutingMode::Selected
        ));
        assert!(!RoutingEngine::target_in_scope(
            Some("BBB"),
            "AAA",
            OutputRoutingMode::Selected
        ));

        // Reroute: the foreign-target rule is the explicit opt-out.
        assert!(RoutingEngine::target_in_scope(
            Some("BBB"),
            "AAA",
            OutputRoutingMode::Reroute
        ));
        assert!(RoutingEngine::target_in_scope(
            None,
            "AAA",
            OutputRoutingMode::Reroute
        ));
    }

    /// Effective-default scope: an explicit target on the system default
    /// counts as default-routed (WirePlumber writes explicit targets even
    /// for default playback). Default playback is in scope iff the chosen
    /// sink IS the default; anything explicitly elsewhere must match.
    #[test]
    fn scope_allows_effective_default() {
        use crate::core::OutputRoutingMode;
        use RoutingEngine as R;

        // Chosen == default: target-less and explicit-default both in.
        assert!(R::scope_allows(
            None,
            "D",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        assert!(R::scope_allows(
            Some(""),
            "D",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        assert!(R::scope_allows(
            Some("D"),
            "D",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        // Chosen != default: default playback (either form) stays out, so
        // enabling EQ on another device never steals it.
        assert!(!R::scope_allows(
            None,
            "B",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        assert!(!R::scope_allows(
            Some("D"),
            "B",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        // Explicitly aimed at the chosen device: in, whatever the default.
        assert!(R::scope_allows(
            Some("B"),
            "B",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        // Explicitly elsewhere: out.
        assert!(!R::scope_allows(
            Some("C"),
            "B",
            Some("D"),
            OutputRoutingMode::Selected
        ));
        // Unknown default: only exact matches (never steal blindly).
        assert!(!R::scope_allows(
            None,
            "B",
            None,
            OutputRoutingMode::Selected
        ));
        assert!(R::scope_allows(
            Some("B"),
            "B",
            None,
            OutputRoutingMode::Selected
        ));
        // Reroute still takes everything.
        assert!(R::scope_allows(
            Some("C"),
            "B",
            Some("D"),
            OutputRoutingMode::Reroute
        ));
    }

    /// The two modes round-trip through the config: a remembered Reroute is
    /// restored on the next startup, and Selected is the default for a missing
    /// or version-1 file.
    #[test]
    fn output_routing_mode_roundtrips() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("output-presets-mode.json");
        let _ = std::fs::remove_file(&path);

        // Default is Selected when nothing is written.
        assert_eq!(
            crate::core::output_routing_mode_at(&path),
            crate::core::OutputRoutingMode::Selected
        );

        crate::core::set_output_routing_mode_at(crate::core::OutputRoutingMode::Reroute, &path)
            .unwrap();
        assert_eq!(
            crate::core::output_routing_mode_at(&path),
            crate::core::OutputRoutingMode::Reroute
        );

        crate::core::set_output_routing_mode_at(crate::core::OutputRoutingMode::Selected, &path)
            .unwrap();
        assert_eq!(
            crate::core::output_routing_mode_at(&path),
            crate::core::OutputRoutingMode::Selected
        );

        let _ = std::fs::remove_file(&path);
    }
}
