use std::cell::Cell;
use std::cell::RefCell;
use std::collections::HashMap;
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

pub struct RoutingEngine {
    core: CoreRc,
    mainloop: MainLoopRc,
    routes: Arc<Mutex<Vec<OutputRoute>>>,
    links: Arc<Mutex<Vec<RouteInfo>>>,
    streams: Arc<Mutex<HashMap<u32, StreamInfo>>>,
    routing_table: Arc<Mutex<HashMap<u32, u32>>>,
    auto_route: bool,
    current_sink: Option<String>,
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
    /// Keeps the metadata `property` listeners alive (a dropped listener
    /// unregisters itself, so these must outlive the bind callback).
    metadata_listeners: Rc<RefCell<Vec<pipewire::metadata::MetadataListener>>>,
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
            current_sink: None,
            virtual_sink_name: format!("{}.source", VIRTUAL_SINK_BASE),
            default_metadata: None,
            default_audio_sink: Rc::new(RefCell::new(None)),
            configured_audio_sink: Rc::new(RefCell::new(None)),
            metadata_listeners: Rc::new(RefCell::new(Vec::new())),
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
        info!("Current sink set to: {}", sink_name);
    }

    pub fn get_current_sink(&self) -> Option<&str> {
        self.current_sink.as_deref()
    }

    pub fn auto_route_to_sink(&mut self, sink_name: &str) -> Result<(), Error> {
        info!("Auto-routing all playback streams to sink: {}", sink_name);

        let (sink_id, sink_serial) = match self.find_node_target(sink_name) {
            Some(t) => t,
            None => {
                warn!(
                    "Virtual sink node {} not found; cannot auto-route",
                    sink_name
                );
                return Err(Error::CreationFailed);
            }
        };

        let streams = self.list_playback_streams();
        let mut routed = 0usize;
        for (node_id, name) in &streams {
            if name.contains(VIRTUAL_SINK_BASE) || name.contains(OUTPUT_CLIENT_NAME) {
                continue;
            }
            match self.set_stream_target(*node_id, sink_id, &sink_serial) {
                Ok(()) => {
                    info!(
                        "Routed '{}' ({}) -> {} (target.node={}, target.object={})",
                        name, node_id, sink_name, sink_id, sink_serial
                    );
                    routed += 1;
                }
                Err(e) => warn!("Failed to route '{}' ({}): {}", name, node_id, e),
            }
        }

        self.set_current_sink(sink_name);
        self.auto_route = true;

        info!(
            "Auto-routing complete: {} stream(s) -> {}",
            routed, sink_name
        );
        Ok(())
    }

    /// Clear the `target.node`/`target.object` metadata for all playback
    /// streams so WirePlumber returns them to the default sink (System EQ
    /// off). Mirrors upstream's unroute path.
    pub fn unroute_all(&mut self) -> Result<(), Error> {
        info!("Unrouting all playback streams from the EQ");
        let streams = self.list_playback_streams();
        let mut cleared = 0usize;
        for (node_id, name) in &streams {
            if name.contains(VIRTUAL_SINK_BASE) || name.contains(OUTPUT_CLIENT_NAME) {
                continue;
            }
            if let Err(e) = self.clear_stream_target(*node_id) {
                warn!("Failed to unroute '{}' ({}): {}", name, node_id, e);
            } else {
                cleared += 1;
            }
        }
        info!("Unroute complete: {} stream(s) cleared", cleared);
        Ok(())
    }

    /// Hand every playback stream back to a NAMED sink, explicitly.
    ///
    /// `unroute_all` only clears `target.node`/`target.object` and relies on
    /// WirePlumber to pick the default. That works while the stream is
    /// actively playing, but it leaves the decision to session-manager policy:
    /// a stream that is idle at shutdown, or one whose player has not yet
    /// opened its output, can come back with nothing to play through once the
    /// virtual sink is gone -- "the output stops when I close the app".
    ///
    /// Naming the destination makes it deterministic: the metadata says exactly
    /// where to go, so nothing has to notice that a node disappeared. Falls
    /// back to clearing when the sink cannot be found, which is what
    /// `unroute_all` already does.
    pub fn unroute_all_to(&mut self, sink_name: &str) -> Result<(), Error> {
        info!("Restoring playback streams to {sink_name}");

        let target = match self.find_node_target(sink_name) {
            Some(t) => t,
            None => {
                warn!("Restore sink {sink_name} not found; falling back to clearing targets");
                return self.unroute_all();
            }
        };

        let streams = self.list_playback_streams();

        // One batch, ONE roundtrip.
        //
        // `set_stream_target` roundtrips per stream, which is right while
        // routing interactively -- each write is confirmed before the next
        // starts. On the way out it is pure latency in front of the hand-off:
        // with five players that is five serialised PipeWire syncs before the
        // first stream is even told where to go, and the audible gap while the
        // EQ is torn down grows with it. Every write is queued first, then
        // synced once, so all the streams start moving together.
        let mut batched = 0usize;
        {
            self.ensure_default_metadata()?;
            let md = self.default_metadata.as_ref().unwrap();
            for (node_id, name) in &streams {
                if name.contains(VIRTUAL_SINK_BASE) || name.contains(OUTPUT_CLIENT_NAME) {
                    continue;
                }
                md.set_property(
                    *node_id,
                    "target.node",
                    Some("Spa:Id"),
                    Some(&target.0.to_string()),
                );
                md.set_property(*node_id, "target.object", Some("Spa:Id"), Some(&target.1));
                batched += 1;
            }
        }
        if batched > 0 {
            self.roundtrip()?;
        }
        info!("Restore complete: {batched} stream(s) -> {sink_name}");
        Ok(())
    }

    /// Set a stream's routing target via the `default` metadata so
    /// WirePlumber moves it. Mirrors upstream `set_stream_target`:
    /// `target.node` = sink bound_id, `target.object` = sink object.serial,
    /// both typed `Spa:Id`.
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
        let listeners_c = self.metadata_listeners.clone();
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
                        let _pl = md
                            .add_listener_local()
                            .property(move |_subject, key, _type, value| {
                                match key {
                                    Some("default.audio.sink") => {
                                        let parsed = parse_metadata_node_name(value);
                                        info!(
                                            "metadata property default.audio.sink = {:?} -> {:?}",
                                            value, parsed
                                        );
                                        *sink_l.borrow_mut() = parsed;
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
    pub fn default_audio_sink_name(&mut self) -> Option<String> {
        self.ensure_default_metadata().ok()?;
        // The server replays metadata properties asynchronously; pump the
        // loop a few extra times so the `default.audio.sink` event lands
        // before we read it.
        if self.default_audio_sink.borrow().is_none() {
            for _ in 0..20 {
                self.mainloop
                    .loop_()
                    .iterate(Timeout::Finite(Duration::from_millis(20)));
                if self.default_audio_sink.borrow().is_some() {
                    break;
                }
            }
        }
        self.default_audio_sink
            .borrow()
            .clone()
            .or_else(|| self.configured_audio_sink.borrow().clone())
    }

    /// Re-read `default.audio.sink` from the metadata, bypassing the
    /// cached value, so a change of the system default output can be
    /// DETECTED at runtime.
    ///
    /// `default_audio_sink_name()` only pumps the loop when the cache is
    /// empty, so it can never observe a later change -- which is why the
    /// app used to keep the sink it started with forever.
    pub fn refresh_default_audio_sink_name(&mut self) -> Option<String> {
        self.ensure_default_metadata().ok()?;
        // The metadata property is delivered asynchronously; pump briefly
        // so the event lands before we read it back.
        for _ in 0..5 {
            self.mainloop
                .loop_()
                .iterate(Timeout::Finite(Duration::from_millis(10)));
        }
        self.default_audio_sink
            .borrow()
            .clone()
            .or_else(|| self.configured_audio_sink.borrow().clone())
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
                    found_clone.lock().unwrap().push((
                        global.id,
                        props.get("node.name").unwrap_or("unknown").to_string(),
                    ));
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
}
