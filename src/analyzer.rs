//! Spectrum analyzer DSP core.
//!
//! Pure-Rust port of the original Python `analyzer.py` math: log-band
//! FFT spectrum computation, power smoothing, and dB conversion. Also
//! provides LUFS loudness snapshot types backed by the `ebur128` crate.
//!
//! The PipeWire audio-capture plumbing (`OutputSpectrumAnalyzer` in the
//! Python original) is handled by the `pipewire_backend` module; this file
//! focuses on the testable DSP pipeline.

use crate::core::clamp;
use crate::ebur128::{Ebur128Meter, ebur128_default_mode};
use log::{debug, info, warn};
use pipewire::{Error, core::CoreRc, properties::properties, stream::StreamBox};
use rustfft::FftPlanner;
use std::sync::{Arc, Mutex};

// ---------------------------------------------------------------------------
// Output Spectrum Analyzer — PipeWire capture plumbing
// ---------------------------------------------------------------------------

/// Target captured by the monitor stream: the processed output sink,
/// matching upstream (which monitors `output_sink`, i.e. what you hear).
pub const ANALYZER_CAPTURE_RATE: u32 = 48000;
pub const ANALYZER_CAPTURE_CHANNELS: usize = 2;

/// Levels shared from the realtime loop thread to the UI thread: log-band
/// dB values plus the latest loudness snapshot.
#[derive(Debug, Default)]
pub struct MonitorShared {
    pub levels_db: Mutex<Vec<f64>>,
    pub loudness: Mutex<Option<AnalyzerLoudnessSnapshot>>,
    pub frames_captured: Mutex<u64>,
    pub peak_sample: Mutex<f32>,
    pub mean_sample: Mutex<f32>,
    /// Accumulates the peak absolute sample since the last `take_window_peak`
    /// call (reset each read), giving a windowed current peak for the
    /// clipping warning. Separate from `peak_sample` (the all-time max used
    /// by diagnostics).
    pub window_peak: Mutex<f32>,
    /// Smoothing (analyzer response speed), shared rather than owned by
    /// `OutputSpectrumAnalyzer`.
    ///
    /// It is read on the realtime capture thread and written by the UI slider,
    /// and it used to live on the analyzer struct, where the capture callback
    /// captured it BY VALUE when the stream was created. Moving the slider
    /// then did nothing until the monitor was toggled off and on, because only
    /// a fresh `start_capture` re-read the field. `display_gain_db` never had
    /// this problem: it is applied in `display_levels()`, i.e. on the way to the
    /// UI, not inside the capture callback.
    pub response_speed: Mutex<f64>,
}

impl MonitorShared {
    /// Shared state with the smoothing seeded to the analyzer's own default,
    /// so a capture thread started before any UI interaction behaves the same
    /// as one started after.
    pub fn new() -> Self {
        Self {
            response_speed: Mutex::new(ANALYZER_RESPONSE_DEFAULT),
            ..Default::default()
        }
    }

    /// Written by the UI (Smoothing slider), read by the realtime callback on
    /// every captured buffer. Shared rather than owned by
    /// `OutputSpectrumAnalyzer` precisely so that a slider move does not need a
    /// capture restart to take effect.
    pub fn set_response_speed(&self, speed: f64) {
        *self.response_speed.lock().unwrap() =
            speed.clamp(ANALYZER_RESPONSE_MIN, ANALYZER_RESPONSE_MAX);
    }

    pub fn response_speed(&self) -> f64 {
        *self.response_speed.lock().unwrap()
    }
}

/// DSP state owned by the capture callbacks (loop thread only, via Mutex).
struct MonitorProcessor {
    ring_mono: Vec<f32>,
    prev_powers: Vec<f64>,
    meter: Option<Ebur128Meter>,
    last_lufs_emit: std::time::Instant,
}

/// Captures audio from the processed output and performs FFT spectrum
/// analysis. Port of the Python `OutputSpectrumAnalyzer`: the stream
/// mirrors upstream's `mini-eq-analyzer` capture (F32 stereo, monitor of
/// the output sink), buffers feed the log-band FFT + LUFS meter, and the
/// UI reads snapshots via [`OutputSpectrumAnalyzer::display_levels`].
pub struct OutputSpectrumAnalyzer {
    pub core: CoreRc,
    pub capture_stream: Option<StreamBox<'static>>,
    pub stream_listener: Option<pipewire::stream::StreamListener<()>>,
    pub format_pod_bytes: Vec<u8>,
    pub format_answer_bytes: Arc<Vec<u8>>,
    /// Fixed PortConfig answer, same lifetime rules as above.
    pub port_config_answer_bytes: Arc<Vec<u8>>,
    pub shared: Arc<MonitorShared>,
    processor: Arc<Mutex<MonitorProcessor>>,
    pub sample_rate: f64,
    pub fft_size: usize,
    pub enabled: bool,
    pub display_gain_db: f64,
    pub levels: Vec<f64>,
    pub loudness_snapshot: Option<AnalyzerLoudnessSnapshot>,
}

/// Build an EnumFormat pod offering planar... interleaved F32 stereo,
/// matching upstream's requested format.
fn build_capture_enum_format(rate: u32) -> Vec<u8> {
    use pipewire::spa::param::ParamType;
    use pipewire::spa::param::audio::AudioFormat;
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Id, SpaTypes};

    let obj = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property {
                key: FormatProperties::MediaType.0,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaType::Audio.as_raw())),
            },
            Property {
                key: FormatProperties::MediaSubtype.0,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
            },
            Property {
                key: FormatProperties::AudioFormat.0,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(AudioFormat::F32LE.as_raw()),
                        alternatives: vec![Id(AudioFormat::F32LE.as_raw())],
                    },
                ))),
            },
            Property {
                key: FormatProperties::AudioRate.0,
                flags: PropertyFlags::empty(),
                value: Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: rate as i32,
                        min: 1,
                        max: 192000,
                    },
                ))),
            },
            Property {
                key: FormatProperties::AudioChannels.0,
                flags: PropertyFlags::empty(),
                value: Value::Int(ANALYZER_CAPTURE_CHANNELS as i32),
            },
            Property {
                key: FormatProperties::AudioPosition.0,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(pipewire::spa::pod::ValueArray::Id(vec![
                    Id(CHANNEL_POSITION_FL),
                    Id(CHANNEL_POSITION_FR),
                ])),
            },
        ],
    });
    serialize_pod_value(&obj)
}

/// Build a fixed Format pod answering the server's EnumFormat offer.
fn build_capture_format(rate: u32) -> Vec<u8> {
    use pipewire::spa::param::ParamType;
    use pipewire::spa::param::audio::AudioFormat;
    use pipewire::spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Id, SpaTypes};

    let obj = Value::Object(Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: ParamType::Format.as_raw(),
        properties: vec![
            Property {
                key: FormatProperties::MediaType.0,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaType::Audio.as_raw())),
            },
            Property {
                key: FormatProperties::MediaSubtype.0,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(MediaSubtype::Raw.as_raw())),
            },
            Property {
                key: FormatProperties::AudioFormat.0,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(AudioFormat::F32LE.as_raw())),
            },
            Property {
                key: FormatProperties::AudioRate.0,
                flags: PropertyFlags::empty(),
                value: Value::Int(rate as i32),
            },
            Property {
                key: FormatProperties::AudioChannels.0,
                flags: PropertyFlags::empty(),
                value: Value::Int(ANALYZER_CAPTURE_CHANNELS as i32),
            },
            Property {
                key: FormatProperties::AudioPosition.0,
                flags: PropertyFlags::empty(),
                value: Value::ValueArray(pipewire::spa::pod::ValueArray::Id(vec![
                    Id(CHANNEL_POSITION_FL),
                    Id(CHANNEL_POSITION_FR),
                ])),
            },
        ],
    });
    serialize_pod_value(&obj)
}

/// Build a fixed PortConfig pod answering the server's EnumPortConfig
/// offer: capture direction, DSP mode, monitor tap — matching what
/// `pw-record` negotiates on this daemon.
fn build_capture_port_config() -> Vec<u8> {
    use pipewire::spa::param::ParamType;
    use pipewire::spa::pod::{Object, Property, PropertyFlags, Value};
    use pipewire::spa::utils::{Id, SpaTypes};

    // Property keys of SPA_TYPE_OBJECT_ParamPortConfig.
    const PORT_CONFIG_DIRECTION: u32 = 1;
    const PORT_CONFIG_MODE: u32 = 2;
    const PORT_CONFIG_MONITOR: u32 = 3;
    // SPA_DIRECTION_INPUT = 0, SPA_PARAM_PORT_CONFIG_MODE_dsp = 3.
    const DIRECTION_INPUT: u32 = 0;
    const PORT_CONFIG_MODE_DSP: u32 = 3;

    let obj = Value::Object(Object {
        type_: SpaTypes::ObjectParamPortConfig.as_raw(),
        id: ParamType::PortConfig.as_raw(),
        properties: vec![
            Property {
                key: PORT_CONFIG_DIRECTION,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(DIRECTION_INPUT)),
            },
            Property {
                key: PORT_CONFIG_MODE,
                flags: PropertyFlags::empty(),
                value: Value::Id(Id(PORT_CONFIG_MODE_DSP)),
            },
            Property {
                key: PORT_CONFIG_MONITOR,
                flags: PropertyFlags::empty(),
                value: Value::Bool(true),
            },
        ],
    });
    serialize_pod_value(&obj)
}

fn serialize_pod_value(value: &pipewire::spa::pod::Value) -> Vec<u8> {
    pipewire::spa::pod::serialize::PodSerializer::serialize(std::io::Cursor::new(Vec::new()), value)
        .map(|(cursor, _)| cursor.into_inner())
        .unwrap_or_default()
}

use pipewire::spa::pod::ChoiceValue;

/// SPA channel positions for stereo (SPA_AUDIO_CHANNEL_FL/FR).
const CHANNEL_POSITION_FL: u32 = 3;
const CHANNEL_POSITION_FR: u32 = 4;

impl OutputSpectrumAnalyzer {
    pub fn new(core: CoreRc, sample_rate: f64) -> Result<Self, Error> {
        let fft_size = analyzer_fft_size(sample_rate);
        let levels = vec![ANALYZER_DB_FLOOR; ANALYZER_BIN_COUNT];

        Ok(Self {
            core,
            capture_stream: None,
            stream_listener: None,
            format_pod_bytes: build_capture_enum_format(ANALYZER_CAPTURE_RATE),
            format_answer_bytes: Arc::new(build_capture_format(ANALYZER_CAPTURE_RATE)),
            port_config_answer_bytes: Arc::new(build_capture_port_config()),
            shared: Arc::new(MonitorShared::new()),
            processor: Arc::new(Mutex::new(MonitorProcessor {
                ring_mono: Vec::new(),
                prev_powers: vec![0.0; ANALYZER_BIN_COUNT],
                meter: None,
                last_lufs_emit: std::time::Instant::now(),
            })),
            sample_rate,
            fft_size,
            enabled: false,
            display_gain_db: ANALYZER_DISPLAY_GAIN_DEFAULT,
            levels,
            loudness_snapshot: None,
        })
    }

    /// Start capturing the processed output sink, mirroring upstream's
    /// `mini-eq-analyzer` stream. Must be called on the loop's thread
    /// before the loop moves to a background thread. `target_node_id`
    /// pins capture to the sink's monitor ports (like upstream's
    /// `new_audio_capture(sink, monitor=true)`); without it the session
    /// manager may link an unrelated source (e.g. a microphone).
    pub fn start_capture(
        &mut self,
        target_sink_name: &str,
        target_node_id: Option<u32>,
    ) -> Result<(), Error> {
        use pipewire::spa::param::ParamType;
        use pipewire::stream::StreamFlags;

        info!("Starting spectrum capture of {target_sink_name}");

        self.stop_capture();

        let rate_string = ANALYZER_CAPTURE_RATE.to_string();
        let capture_props = properties! {
            *pipewire::keys::MEDIA_TYPE => "Audio",
            *pipewire::keys::MEDIA_CATEGORY => "Capture",
            *pipewire::keys::MEDIA_ROLE => "Music",
            *pipewire::keys::NODE_NAME => ANALYZER_NODE_NAME,
            *pipewire::keys::NODE_DESCRIPTION => ANALYZER_NODE_DESCRIPTION,
            *pipewire::keys::APP_NAME => "Mini EQ",
            "application.id" => ANALYZER_APPLICATION_ID,
            "media.name" => ANALYZER_NODE_DESCRIPTION,
            *pipewire::keys::MEDIA_CLASS => "Stream/Input/Audio",
            "node.dont-move" => "true",
            "state.restore-props" => "false",
            "state.restore-target" => "false",
            "target.object" => target_sink_name,
            "audio.channels" => "2",
            "audio.rate" => rate_string.as_str(),
            "audio.format" => "f32le",
        };

        let stream = StreamBox::new(&self.core, ANALYZER_NODE_NAME, capture_props)?;
        // SAFETY: the stream lives in `self.capture_stream` until
        // `stop_capture`/drop, outliving the listener that borrows it.
        let stream: StreamBox<'static> =
            unsafe { std::mem::transmute::<StreamBox<'_>, StreamBox<'static>>(stream) };

        let shared = self.shared.clone();
        let processor = self.processor.clone();
        let sample_rate = self.sample_rate;
        let fft_size = self.fft_size;
        let answer_bytes = self.format_answer_bytes.clone();
        let port_config_bytes = self.port_config_answer_bytes.clone();

        // Loudness meter lives with the DSP state on the loop thread.
        match Ebur128Meter::new(ANALYZER_CAPTURE_RATE, 2, ebur128_default_mode()) {
            Ok(meter) => {
                processor.lock().unwrap().meter = Some(meter);
            }
            Err(e) => warn!("Analyzer loudness meter unavailable: {e:?}"),
        }

        let listener = stream
            .add_local_listener::<()>()
            .param_changed(move |stream, _, id, _param| {
                if id == ParamType::EnumFormat.as_raw() {
                    // Answer from struct-owned bytes (see
                    // `format_answer_bytes`): `update_params` dispatches
                    // asynchronously, so callback-local bytes would dangle.
                    if let Some(pod) = pipewire::spa::pod::Pod::from_bytes(&answer_bytes) {
                        if let Err(e) = stream.update_params(&mut [pod]) {
                            warn!("Analyzer failed to set format: {e:?}");
                        }
                    }
                } else if id == ParamType::EnumPortConfig.as_raw() {
                    if let Some(pod) = pipewire::spa::pod::Pod::from_bytes(&port_config_bytes) {
                        if let Err(e) = stream.update_params(&mut [pod]) {
                            warn!("Analyzer failed to set port config: {e:?}");
                        }
                    }
                }
            })
            .process(move |stream, _| {
                process_capture_buffers(stream, &shared, &processor, sample_rate, fft_size);
            })
            .state_changed(|stream, _, old, new| {
                debug!("Analyzer stream state: {old:?} -> {new:?}");
                if matches!(
                    new,
                    pipewire::stream::StreamState::Paused
                        | pipewire::stream::StreamState::Streaming
                ) && !matches!(
                    old,
                    pipewire::stream::StreamState::Paused
                        | pipewire::stream::StreamState::Streaming
                ) {
                    // Negotiation done: activate so buffers start flowing,
                    // mirroring upstream `stream.start()`.
                    if let Err(e) = stream.set_active(true) {
                        warn!("Analyzer failed to activate stream: {e:?}");
                    }
                }
            })
            .register()
            .map_err(|_| Error::CreationFailed)?;

        let enum_pod = pipewire::spa::pod::Pod::from_bytes(&self.format_pod_bytes)
            .ok_or(Error::CreationFailed)?;
        info!("Analyzer connecting to {target_sink_name} (node id {target_node_id:?})");
        stream
            .connect(
                pipewire::spa::utils::Direction::Input,
                target_node_id,
                StreamFlags::MAP_BUFFERS,
                &mut [enum_pod],
            )
            .map_err(|e| {
                warn!("Analyzer stream connect failed: {e:?}");
                Error::CreationFailed
            })?;

        self.capture_stream = Some(stream);
        self.stream_listener = Some(listener);
        self.enabled = true;

        info!("Spectrum capture started");
        Ok(())
    }

    pub fn stop_capture(&mut self) {
        let was_enabled = self.enabled;
        self.stream_listener = None;
        if let Some(stream) = self.capture_stream.take() {
            let _ = stream.disconnect();
        }
        self.enabled = false;
        if was_enabled {
            info!("Spectrum capture stopped");
        }
    }

    /// Normalized 0..1 display levels for the UI (dB + display gain).
    pub fn display_levels(&self) -> Vec<f64> {
        let shared = self.shared.levels_db.lock().unwrap();
        let db: Vec<f64> = if shared.len() == ANALYZER_BIN_COUNT {
            shared.clone()
        } else {
            self.levels.clone()
        };
        spectrum_db_values_to_levels(
            &db.iter()
                .map(|v| v + self.display_gain_db)
                .collect::<Vec<f64>>(),
        )
    }

    /// Latest loudness snapshot, if any audio has been measured.
    pub fn display_loudness(&self) -> Option<AnalyzerLoudnessSnapshot> {
        self.shared.loudness.lock().unwrap().clone()
    }

    /// Our stream's daemon-side node id, once bound (0 until then).
    pub fn stream_node_id(&self) -> u32 {
        self.capture_stream
            .as_ref()
            .map(|s| s.node_id())
            .unwrap_or(0)
    }

    /// Current stream state, if the stream exists.
    pub fn stream_state(&self) -> Option<pipewire::stream::StreamState> {
        self.capture_stream.as_ref().map(|s| s.state())
    }

    /// Capture diagnostics: (audio frames processed, bands with signal,
    /// peak absolute sample seen).
    pub fn monitor_stats(&self) -> (u64, usize, f32, f32) {
        let frames = *self.shared.frames_captured.lock().unwrap();
        let active = self
            .shared
            .levels_db
            .lock()
            .unwrap()
            .iter()
            .filter(|v| **v > ANALYZER_DB_FLOOR)
            .count();
        let peak = *self.shared.peak_sample.lock().unwrap();
        let mean = *self.shared.mean_sample.lock().unwrap();
        (frames, active, peak, mean)
    }

    /// Read the windowed peak absolute sample (linear amplitude) accumulated
    /// since the last call, then reset it. Returns 0.0 when no audio has been
    /// captured in the window (e.g. monitor off). Convert to dBFS with
    /// `20*log10(peak)`.
    pub fn take_window_peak(&self) -> f32 {
        let mut guard = self.shared.window_peak.lock().unwrap();
        let p = *guard;
        *guard = 0.0;
        p
    }

    pub fn analyze(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }

        let powers = samples_to_log_band_powers(samples, self.sample_rate, self.fft_size);
        self.levels = power_values_to_db_values(&powers);
    }

    pub fn get_levels(&self) -> &[f64] {
        &self.levels
    }

    pub fn set_display_gain(&mut self, gain_db: f64) {
        self.display_gain_db = gain_db.clamp(ANALYZER_DISPLAY_GAIN_MIN, ANALYZER_DISPLAY_GAIN_MAX);
    }

    /// Takes effect on the next captured buffer, with no restart: the value is
    /// read out of `shared` inside the realtime callback.
    pub fn set_response_speed(&mut self, speed: f64) {
        self.shared.set_response_speed(speed);
    }

    pub fn get_display_norm(&self, band_index: usize) -> f64 {
        if band_index >= self.levels.len() {
            return 0.0;
        }
        analyzer_level_to_display_norm(self.levels[band_index], self.display_gain_db)
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

/// Drain all queued capture buffers into the FFT + loudness pipeline.
/// Runs on the PipeWire loop thread.
fn process_capture_buffers(
    stream: &pipewire::stream::Stream,
    shared: &Arc<MonitorShared>,
    processor: &Arc<Mutex<MonitorProcessor>>,
    sample_rate: f64,
    fft_size: usize,
) {
    let mut proc = processor.lock().unwrap();
    let mut new_frames = 0usize;

    while let Some(mut buffer) = stream.dequeue_buffer() {
        for data in buffer.datas_mut() {
            // Only the chunk's byte range is valid audio; the rest
            // of the mapping is stale (usually zeros). Copy bounds
            // first to satisfy the borrow checker.
            let (offset, size) = {
                let chunk = data.chunk();
                (chunk.offset() as usize, chunk.size() as usize)
            };
            if let Some(bytes) = data.data() {
                let end = (offset + size).min(bytes.len());
                if offset >= end {
                    continue;
                }
                let valid = &bytes[offset..end];
                // Debug hook: archive raw capture bytes for offline analysis.
                if let Ok(path) = std::env::var("MINI_EQ_DUMP_PCM")
                    && !path.is_empty()
                {
                    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    if size < 2_000_000 {
                        use std::io::Write;
                        if let Ok(mut file) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(&path)
                        {
                            let _ = file.write_all(valid);
                        }
                    }
                }
                let floats = bytes_to_f32_vec(valid);
                if floats.is_empty() {
                    continue;
                }
                let peak = floats.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                {
                    let mut guard = shared.peak_sample.lock().unwrap();
                    if peak > *guard {
                        *guard = peak;
                    }
                }
                {
                    let mut guard = shared.window_peak.lock().unwrap();
                    if peak > *guard {
                        *guard = peak;
                    }
                }
                let mean = floats.iter().sum::<f32>() / floats.len().max(1) as f32;
                {
                    let mut guard = shared.mean_sample.lock().unwrap();
                    // Loudest-mean wins so transients don't get diluted.
                    if mean.abs() > guard.abs() {
                        *guard = mean;
                    }
                }
                // Feed stereo loudness meter with interleaved frames when
                // we have a full channel pair per frame.
                if let Some(meter) = proc.meter.as_mut() {
                    let _ = meter.add_frames(&floats);
                }
                // Mono mix for the FFT ring.
                let channels = 2usize;
                let frames = floats.len() / channels;
                for f in 0..frames {
                    let mut sum = 0.0f32;
                    for c in 0..channels {
                        sum += floats.get(f * channels + c).copied().unwrap_or(0.0);
                    }
                    proc.ring_mono.push(sum / channels as f32);
                }
                new_frames += frames;
            }
        }
    }

    if proc.ring_mono.len() < fft_size || new_frames == 0 {
        return;
    }
    // Keep overlap: retain the newest window plus a small tail.
    let keep_from = proc.ring_mono.len().saturating_sub(fft_size);
    let window: Vec<f32> = proc.ring_mono[keep_from..].to_vec();
    proc.ring_mono.drain(..keep_from);

    // Debug hook: archive signal-bearing FFT inputs for offline analysis.
    if let Ok(path) = std::env::var("MINI_EQ_DUMP_WINDOW")
        && !path.is_empty()
    {
        let peak_in: f32 = window.iter().fold(0.0f32, |m, v| m.max(v.abs()));
        if peak_in > 0.01 {
            use std::io::Write;
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
            {
                for sample in &window {
                    let _ = file.write_all(&sample.to_le_bytes());
                }
            }
        }
    }

    let powers = samples_to_log_band_powers(&window, sample_rate, fft_size);
    if powers.len() == proc.prev_powers.len() && !powers.is_empty() {
        // Read the smoothing the UI last set, not a value captured when the
        // stream was created: the slider must be live.
        let response_speed = shared.response_speed();
        let alpha = analyzer_smoothing_alpha(response_speed, new_frames, sample_rate);
        let smoothed = smooth_power_values(&proc.prev_powers, &powers, alpha);
        proc.prev_powers = smoothed.clone();
        let db = power_values_to_db_values(&smoothed);
        if log::log_enabled!(log::Level::Debug) {
            let peak = db.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
            let peak_idx = db
                .iter()
                .position(|&v| (v - peak).abs() < 1e-9)
                .unwrap_or(999);
            let win_peak: f32 = window.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            log::debug!(
                "FFT window={} frames={} ring={} win_peak={win_peak:.4} peak_db={peak:.1} peak_idx={peak_idx}",
                window.len(),
                new_frames,
                proc.ring_mono.len()
            );
        }
        *shared.levels_db.lock().unwrap() = db;
    }
    *shared.frames_captured.lock().unwrap() += new_frames as u64;

    if proc.last_lufs_emit.elapsed().as_secs_f64() >= LOUDNESS_EMIT_INTERVAL_SECONDS
        && let Some(meter) = proc.meter.as_ref()
    {
        let snapshot = AnalyzerLoudnessSnapshot {
            momentary_lufs: meter.momentary_lufs().unwrap_or(f64::NEG_INFINITY),
            shortterm_lufs: meter.shortterm_lufs().unwrap_or(f64::NEG_INFINITY),
            integrated_lufs: meter.integrated_lufs().unwrap_or(f64::NEG_INFINITY),
        };
        *shared.loudness.lock().unwrap() = Some(snapshot);
        proc.last_lufs_emit = std::time::Instant::now();
    }
}

/// Copy a little-endian f32 byte payload (alignment-safe).
fn bytes_to_f32_vec(bytes: &[u8]) -> Vec<f32> {
    let usable = bytes.len() - (bytes.len() % ANALYZER_SAMPLE_WIDTH_BYTES);
    let mut out = Vec::with_capacity(usable / ANALYZER_SAMPLE_WIDTH_BYTES);
    let (chunks, _) = bytes[..usable].as_chunks::<ANALYZER_SAMPLE_WIDTH_BYTES>();
    for chunk in chunks {
        let word = [chunk[0], chunk[1], chunk[2], chunk[3]];
        out.push(f32::from_le_bytes(word));
    }
    out
}

// ---------------------------------------------------------------------------
// Constants (mirror the Python analyzer.py)
// ---------------------------------------------------------------------------

pub const ANALYZER_BAND_FREQUENCIES: [f64; 30] = [
    25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0,
    630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0, 3150.0, 4000.0, 5000.0, 6300.0, 8000.0,
    10000.0, 12500.0, 16000.0, 20000.0,
];

pub const ANALYZER_BIN_COUNT: usize = ANALYZER_BAND_FREQUENCIES.len();
pub const ANALYZER_DB_FLOOR: f64 = -100.0;
pub const ANALYZER_INTERVAL_MS: f64 = 33.0;
pub const ANALYZER_FFT_WINDOW_SECONDS: f64 = 0.085;
pub const ANALYZER_FFT_MIN_SIZE: usize = 8192;
pub const ANALYZER_FFT_MAX_SIZE: usize = 32768;
pub const ANALYZER_SAMPLE_WIDTH_BYTES: usize = 4;
pub const ANALYZER_QUEUE_WAIT_SECONDS: f64 = 0.005;
pub const ANALYZER_READER_JOIN_TIMEOUT_SECONDS: f64 = 0.2;
pub const ANALYZER_DISPLAY_GAIN_MIN: f64 = -12.0;
pub const ANALYZER_DISPLAY_GAIN_MAX: f64 = 32.0;
pub const ANALYZER_DISPLAY_GAIN_DEFAULT: f64 = 0.0;
pub const ANALYZER_CAPTURE_QUEUE_BLOCKS: usize = 128;
pub const ANALYZER_NODE_NAME: &str = "mini-eq-analyzer";
pub const ANALYZER_NODE_DESCRIPTION: &str = "Mini EQ Monitor";
pub const ANALYZER_APPLICATION_ID: &str = "io.github.mrproject72.mini_eq_rr";
pub const ANALYZER_MEDIA_CLASS: &str = "Stream/Input/Audio/Internal";

pub const ANALYZER_RESPONSE_MIN: f64 = 0.02;
pub const ANALYZER_RESPONSE_MAX: f64 = 15.0;
pub const ANALYZER_RESPONSE_DEFAULT: f64 = 2.0;
pub const ANALYZER_POWER_FLOOR: f64 = 1e-10; // 10^(-100/10)

pub const LOUDNESS_EMIT_INTERVAL_SECONDS: f64 = 0.25;

/// Loudness levels reported by the analyzer.
#[derive(Debug, Clone, Default)]
pub struct AnalyzerLoudnessSnapshot {
    pub momentary_lufs: f64,
    pub shortterm_lufs: f64,
    pub integrated_lufs: f64,
}

// ---------------------------------------------------------------------------
// Display / dB helpers
// ---------------------------------------------------------------------------

pub fn normalize_spectrum_db(db_value: f64) -> f64 {
    clamp(
        (db_value - ANALYZER_DB_FLOOR) / ANALYZER_DB_FLOOR.abs(),
        0.0,
        1.0,
    )
}

pub fn spectrum_level_to_db(level: f64) -> f64 {
    ANALYZER_DB_FLOOR + (clamp(level, 0.0, 1.0) * ANALYZER_DB_FLOOR.abs())
}

/// Map a raw dB level to a normalized [0,1] display deflection.
///
/// Uses the x42-style meter shape: hides very low noise and expands the
/// musical range.
pub fn analyzer_db_to_display_norm(db_value: f64, display_gain_db: f64) -> f64 {
    let display_db = db_value + display_gain_db;

    let deflection = if display_db < -70.0 {
        0.0
    } else if display_db < -60.0 {
        (display_db + 70.0) * 0.25
    } else if display_db < -50.0 {
        ((display_db + 60.0) * 0.5) + 2.5
    } else if display_db < -40.0 {
        ((display_db + 50.0) * 0.75) + 7.5
    } else if display_db < -30.0 {
        ((display_db + 40.0) * 1.5) + 15.0
    } else if display_db < -20.0 {
        ((display_db + 30.0) * 2.0) + 30.0
    } else if display_db < 6.0 {
        ((display_db + 20.0) * 2.5) + 50.0
    } else {
        115.0
    };

    clamp(deflection / 115.0, 0.0, 1.0)
}

pub fn analyzer_level_to_display_norm(level: f64, display_gain_db: f64) -> f64 {
    analyzer_db_to_display_norm(spectrum_level_to_db(level), display_gain_db)
}

pub fn spectrum_db_values_to_levels(db_values: &[f64]) -> Vec<f64> {
    db_values
        .iter()
        .map(|&v| normalize_spectrum_db(v))
        .collect()
}

// ---------------------------------------------------------------------------
// Sizing helpers
// ---------------------------------------------------------------------------

pub fn analyzer_frame_count(sample_rate: f64) -> usize {
    (1.0_f64.max(sample_rate) * ANALYZER_INTERVAL_MS / 1000.0) as usize
}

pub fn next_power_of_two(value: usize) -> usize {
    if value == 0 {
        return 1;
    }
    let mut n = value - 1;
    n |= n >> 1;
    n |= n >> 2;
    n |= n >> 4;
    n |= n >> 8;
    n |= n >> 16;
    n |= n >> 32;
    n + 1
}

pub fn analyzer_fft_size(sample_rate: f64) -> usize {
    let target = (1.0_f64.max(sample_rate) * ANALYZER_FFT_WINDOW_SECONDS) as usize;
    let sized = next_power_of_two(target);
    sized.clamp(ANALYZER_FFT_MIN_SIZE, ANALYZER_FFT_MAX_SIZE)
}

/// Map a UI smoothing percentage (0.15..0.95, upstream's slider scale)
/// onto the analyzer's `response_speed`.
///
/// Inverse: more smoothing = slower response. Interpolated in log space
/// because the speed range spans 0.02..15 (750x); a linear map would make
/// the slider nearly useless at the fast end.
///
/// Calibration: 30% -> ANALYZER_RESPONSE_DEFAULT (2.0).
pub fn smoothing_percent_to_response_speed(smoothing: f64) -> f64 {
    let s = smoothing.clamp(0.15, 0.95);
    let lo = ANALYZER_RESPONSE_MIN.ln();
    let hi = ANALYZER_RESPONSE_MAX.ln();
    (lo + (1.0 - s) * (hi - lo)).exp()
}

pub fn analyzer_smoothing_alpha(response_speed: f64, frame_count: usize, sample_rate: f64) -> f64 {
    let speed = clamp(response_speed, ANALYZER_RESPONSE_MIN, ANALYZER_RESPONSE_MAX);
    1.0 - (-2.0 * std::f64::consts::PI * speed * (1.max(frame_count) as f64)
        / 1.0_f64.max(sample_rate))
    .exp()
}

// ---------------------------------------------------------------------------
// Frequency bin layout
// ---------------------------------------------------------------------------

/// Center frequencies for the analyzer bands.
pub fn analyzer_bin_center_frequencies(
    level_count: usize,
    freq_min: f64,
    freq_max: f64,
) -> Vec<f64> {
    if level_count == ANALYZER_BIN_COUNT
        && (freq_min - crate::core::GRAPH_FREQ_MIN).abs() < 1e-9
        && (freq_max - crate::core::GRAPH_FREQ_MAX).abs() < 1e-9
    {
        return ANALYZER_BAND_FREQUENCIES.to_vec();
    }

    let log_min = freq_min.ln();
    let log_span = (freq_max / freq_min).ln();
    (0..level_count)
        .map(|index| log_min + log_span * (index as f64 + 0.5) / level_count as f64)
        .map(|v| v.exp())
        .collect()
}

/// Band edges (one more than the number of bands).
pub fn analyzer_band_edges(center_frequencies: &[f64]) -> Vec<f64> {
    if center_frequencies.is_empty() {
        return Vec::new();
    }
    if center_frequencies.len() == 1 {
        let center = center_frequencies[0];
        return vec![center / 2.0_f64.sqrt(), center * 2.0_f64.sqrt()];
    }

    let middle_edges: Vec<f64> = center_frequencies
        .windows(2)
        .map(|w| (w[0] * w[1]).sqrt())
        .collect();
    let first_edge = center_frequencies[0] * center_frequencies[0] / middle_edges[0];
    let last_edge = center_frequencies[center_frequencies.len() - 1]
        * center_frequencies[center_frequencies.len() - 1]
        / middle_edges[middle_edges.len() - 1];

    let mut edges = Vec::with_capacity(center_frequencies.len() + 1);
    edges.push(first_edge);
    edges.extend(middle_edges);
    edges.push(last_edge);
    edges
}

// ---------------------------------------------------------------------------
// Sample / byte conversion helpers
// ---------------------------------------------------------------------------

/// Parse little-endian f32 bytes into samples (DC-removed).
pub fn pcm_f32le_bytes_to_samples(payload: &[u8]) -> Vec<f32> {
    let usable = payload.len() - (payload.len() % ANALYZER_SAMPLE_WIDTH_BYTES);
    let sample_count = usable / ANALYZER_SAMPLE_WIDTH_BYTES;
    let mut samples = Vec::with_capacity(sample_count);
    for i in 0..sample_count {
        let offset = i * ANALYZER_SAMPLE_WIDTH_BYTES;
        let bytes = payload[offset..offset + 4].try_into().unwrap();
        samples.push(f32::from_le_bytes(bytes));
    }
    samples
}

/// Convert interleaved f32 bytes to mono by averaging channels.
pub fn interleaved_f32le_bytes_to_mono(payload: &[u8], channels: usize) -> Vec<f32> {
    let usable = payload.len() - (payload.len() % (ANALYZER_SAMPLE_WIDTH_BYTES * channels.max(1)));
    let frame_count = usable / (ANALYZER_SAMPLE_WIDTH_BYTES * channels.max(1));
    let mut mono = Vec::with_capacity(frame_count);
    for i in 0..frame_count {
        let base = i * ANALYZER_SAMPLE_WIDTH_BYTES * channels.max(1);
        let mut sum = 0.0_f32;
        for c in 0..channels.max(1) {
            let offset = base + c * ANALYZER_SAMPLE_WIDTH_BYTES;
            let bytes = payload[offset..offset + 4].try_into().unwrap();
            sum += f32::from_le_bytes(bytes);
        }
        mono.push(sum / channels.max(1) as f32);
    }
    mono
}

// ---------------------------------------------------------------------------
// FFT computation
// ---------------------------------------------------------------------------

fn hanning_window(size: usize) -> Vec<f32> {
    let n = size as f32;
    (0..size)
        .map(|i| {
            // numpy's hanning: 0.5 - 0.5*cos(2*pi*n/(size))
            0.5 - 0.5 * (2.0 * std::f32::consts::PI * i as f32 / n).cos()
        })
        .collect()
}

fn amplitude_normalizer(window: &[f32]) -> f32 {
    window.iter().sum::<f32>() / 2.0
}

/// Overlap-weight structure for mapping FFT bins to log-band powers.
pub struct BandOverlapWeights {
    pub band_indexes: Vec<usize>,
    pub bin_indexes: Vec<usize>,
    pub weights: Vec<f64>,
    pub band_count: usize,
}

pub fn analyzer_fft_band_overlap_weights(
    fft_size: usize,
    sample_rate: f64,
    center_frequencies: &[f64],
) -> BandOverlapWeights {
    let size = fft_size.max(2);
    let sr = sample_rate.max(1.0);
    let band_count = center_frequencies.len();

    if band_count == 0 {
        return BandOverlapWeights {
            band_indexes: Vec::new(),
            bin_indexes: Vec::new(),
            weights: Vec::new(),
            band_count: 0,
        };
    }

    // rfftfreq equivalent: frequency of each bin
    let bin_count = size / 2 + 1; // rfft returns this many bins
    let frequencies: Vec<f64> = (0..bin_count)
        .map(|i| i as f64 * sr / size as f64)
        .collect();

    let bin_width = (sr / size as f64).max(1e-12);
    let mut bin_left: Vec<f64> = frequencies.iter().map(|&f| f - bin_width * 0.5).collect();
    let mut bin_right: Vec<f64> = frequencies.iter().map(|&f| f + bin_width * 0.5).collect();
    bin_left[0] = 0.0;
    let last = bin_right.len() - 1;
    bin_right[last] = (sr * 0.5).min(bin_right[last]);

    let bin_widths: Vec<f64> = bin_right
        .iter()
        .zip(bin_left.iter())
        .map(|(r, l)| (*r - *l).max(1e-12))
        .collect();

    let edges = analyzer_band_edges(center_frequencies);
    let nyquist = sr * 0.5;
    let band_left: Vec<f64> = edges[..edges.len() - 1]
        .iter()
        .map(|&e| e.clamp(0.0, nyquist))
        .collect();
    let band_right: Vec<f64> = edges[1..].iter().map(|&e| e.clamp(0.0, nyquist)).collect();

    let mut band_indexes = Vec::new();
    let mut bin_indexes = Vec::new();
    let mut weights = Vec::new();

    for (b_idx, (&b_left, &b_right)) in band_left.iter().zip(band_right.iter()).enumerate() {
        for (bin_idx, (bl, br)) in bin_left.iter().zip(bin_right.iter()).enumerate() {
            let overlap_left = b_left.max(*bl);
            let overlap_right = b_right.min(*br);
            let overlap = (overlap_right - overlap_left).max(0.0);
            if overlap > 0.0 {
                let weight = overlap / bin_widths[bin_idx];
                band_indexes.push(b_idx);
                bin_indexes.push(bin_idx);
                weights.push(weight);
            }
        }
    }

    BandOverlapWeights {
        band_indexes,
        bin_indexes,
        weights,
        band_count,
    }
}

/// Compute log-band power levels from a sample buffer.
pub fn samples_to_log_band_powers(samples: &[f32], sample_rate: f64, fft_size: usize) -> Vec<f64> {
    if samples.is_empty() {
        return Vec::new();
    }

    let size = fft_size.max(2);
    let window = hanning_window(size);
    let normalizer = amplitude_normalizer(&window);

    // Take last `size` samples, zero-padded
    let mut input: Vec<f32> = vec![0.0; size];
    let copy_len = samples.len().min(size);
    input[size - copy_len..].copy_from_slice(&samples[samples.len() - copy_len..]);

    // Apply window
    let windowed: Vec<f32> = input
        .iter()
        .zip(window.iter())
        .map(|(s, w)| s * w)
        .collect();

    // rfft
    let mut planner = FftPlanner::new();
    let fft = planner.plan_fft_forward(size);

    let mut complex: Vec<num_complex::Complex<f32>> = windowed
        .iter()
        .map(|&s| num_complex::Complex::new(s, 0.0))
        .collect();
    fft.process(&mut complex);

    // bin_powers = (|spectrum| / normalizer)^2, DC = 0
    let bin_count = size / 2 + 1;
    let mut bin_powers: Vec<f64> = vec![0.0; bin_count];
    for (i, c) in complex.iter().enumerate().take(bin_count) {
        if i == 0 {
            bin_powers[i] = 0.0;
        } else {
            let mag = c.norm() as f64 / normalizer as f64;
            bin_powers[i] = mag * mag;
        }
    }

    // Map to log bands
    let overlap = analyzer_fft_band_overlap_weights(size, sample_rate, &ANALYZER_BAND_FREQUENCIES);

    if overlap.band_count == 0 || overlap.bin_indexes.is_empty() {
        return vec![0.0; overlap.band_count];
    }

    let mut band_powers = vec![0.0_f64; overlap.band_count];
    for i in 0..overlap.bin_indexes.len() {
        let bin_idx = overlap.bin_indexes[i];
        let band_idx = overlap.band_indexes[i];
        let weight = overlap.weights[i];
        band_powers[band_idx] += bin_powers[bin_idx] * weight;
    }

    band_powers
}

// ---------------------------------------------------------------------------
// Post-processing
// ---------------------------------------------------------------------------

pub fn smooth_power_values(previous: &[f64], current: &[f64], alpha: f64) -> Vec<f64> {
    if current.is_empty() {
        return previous.to_vec();
    }
    let mix = clamp(alpha, 0.0, 1.0);
    let prev_vec: Vec<f64> = if previous.len() == current.len() {
        previous.to_vec()
    } else {
        vec![0.0; current.len()]
    };
    prev_vec
        .iter()
        .zip(current.iter())
        .map(|(&old, &new)| old + mix * (new - old))
        .collect()
}

pub fn power_values_to_db_values(power_values: &[f64]) -> Vec<f64> {
    power_values
        .iter()
        .map(|&power| 10.0 * (power.max(ANALYZER_POWER_FLOOR)).log10())
        .map(|db| db.max(ANALYZER_DB_FLOOR))
        .collect()
}

pub fn samples_to_log_band_db_values(
    samples: &[f32],
    sample_rate: f64,
    fft_size: usize,
) -> Vec<f64> {
    let powers = samples_to_log_band_powers(samples, sample_rate, fft_size);
    power_values_to_db_values(&powers)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::SAMPLE_RATE;

    #[test]
    fn test_analyzer_bin_count() {
        assert_eq!(ANALYZER_BIN_COUNT, 30);
    }

    #[test]
    fn test_analyzer_fft_size() {
        // 48000 * 0.085 = 4080, next pow2 = 4096, clamped to [8192, 32768] = 8192
        let size = analyzer_fft_size(SAMPLE_RATE);
        assert!(size >= ANALYZER_FFT_MIN_SIZE);
        assert!(size <= ANALYZER_FFT_MAX_SIZE);
        assert!(size.is_power_of_two());
    }

    #[test]
    fn test_next_power_of_two() {
        assert_eq!(next_power_of_two(0), 1);
        assert_eq!(next_power_of_two(1), 1);
        assert_eq!(next_power_of_two(7), 8);
        assert_eq!(next_power_of_two(8), 8);
        assert_eq!(next_power_of_two(9), 16);
        assert_eq!(next_power_of_two(4080), 4096);
    }

    #[test]
    fn test_analyzer_frame_count() {
        assert_eq!(analyzer_frame_count(SAMPLE_RATE), 1584);
    }

    #[test]
    fn test_bin_center_frequencies() {
        let freqs = analyzer_bin_center_frequencies(
            ANALYZER_BIN_COUNT,
            crate::core::GRAPH_FREQ_MIN,
            crate::core::GRAPH_FREQ_MAX,
        );
        assert_eq!(freqs.len(), 30);
        assert!((freqs[0] - 25.0).abs() < 0.01);
        assert!((freqs[29] - 20000.0).abs() < 0.01);
    }

    #[test]
    fn test_band_edges() {
        let edges = analyzer_band_edges(&ANALYZER_BAND_FREQUENCIES);
        assert_eq!(edges.len(), 31);
        assert!(edges.windows(2).all(|w| w[1] > w[0]));
    }

    #[test]
    fn test_normalize_spectrum_db() {
        assert_eq!(normalize_spectrum_db(0.0), 1.0);
        assert_eq!(normalize_spectrum_db(ANALYZER_DB_FLOOR), 0.0);
        assert_eq!(normalize_spectrum_db(-50.0), 0.5);
    }

    #[test]
    fn test_spectrum_level_to_db() {
        assert_eq!(spectrum_level_to_db(1.0), 0.0);
        assert_eq!(spectrum_level_to_db(0.0), ANALYZER_DB_FLOOR);
        assert_eq!(spectrum_level_to_db(0.5), -50.0);
    }

    #[test]
    fn test_analyzer_db_to_display_norm() {
        assert_eq!(analyzer_db_to_display_norm(-70.0, 0.0), 0.0);
        assert_eq!(analyzer_db_to_display_norm(6.0, 0.0), 1.0);
        // with display gain
        assert!(analyzer_db_to_display_norm(-70.0, 10.0) > 0.0);
    }

    /// Regression guard: the Smoothing slider used to do nothing until the
    /// monitor was switched off and on again.
    ///
    /// `response_speed` lived on `OutputSpectrumAnalyzer`, and the capture
    /// callback captured it BY VALUE when the stream was created, so the only
    /// way a new value reached the DSP was a fresh `start_capture`. It now
    /// lives in the `Arc<MonitorShared>` the callback already holds, and these
    /// are the accessors on both sides of that seam: the UI writes through
    /// `set_response_speed`, the realtime thread reads `response_speed()`.
    #[test]
    fn the_smoothing_value_the_ui_writes_is_the_one_the_dsp_reads() {
        let shared = MonitorShared::new();
        assert_eq!(shared.response_speed(), ANALYZER_RESPONSE_DEFAULT);

        let slow = analyzer_smoothing_alpha(shared.response_speed(), 1584, SAMPLE_RATE);
        shared.set_response_speed(ANALYZER_RESPONSE_MAX);
        let fast = analyzer_smoothing_alpha(shared.response_speed(), 1584, SAMPLE_RATE);

        // More response speed == less smoothing == a larger alpha per frame.
        assert!(
            fast > slow,
            "a faster response must smooth less ({fast} should exceed {slow})"
        );

        // The setter's clamp is what the analyzer relies on.
        shared.set_response_speed(1e9);
        assert_eq!(shared.response_speed(), ANALYZER_RESPONSE_MAX);
        shared.set_response_speed(-1e9);
        assert_eq!(shared.response_speed(), ANALYZER_RESPONSE_MIN);
    }

    #[test]
    fn smoothing_percent_mapping_is_monotone_inverse() {
        // More smoothing must always mean a slower (lower) response speed.
        let mut prev = f64::MAX;
        for i in 0..=80 {
            let pct = 0.15 + (i as f64) * 0.01;
            let sp = smoothing_percent_to_response_speed(pct);
            assert!(
                sp < prev,
                "mapping not strictly decreasing at {pct:.2}: {sp} vs {prev}"
            );
            prev = sp;
        }
    }

    #[test]
    fn smoothing_percent_mapping_hits_the_default_at_30_percent() {
        let sp = smoothing_percent_to_response_speed(0.30);
        assert!(
            (sp - ANALYZER_RESPONSE_DEFAULT).abs() < 0.15,
            "30% should land near ANALYZER_RESPONSE_DEFAULT \
             ({ANALYZER_RESPONSE_DEFAULT}), got {sp}"
        );
    }

    #[test]
    fn smoothing_percent_mapping_stays_in_range() {
        for i in 0..=200 {
            let sp = smoothing_percent_to_response_speed(0.1 + (i as f64) * 0.005);
            assert!(
                (ANALYZER_RESPONSE_MIN - 1e-9..=ANALYZER_RESPONSE_MAX + 1e-9).contains(&sp),
                "out of range: {sp}"
            );
        }
    }

    #[test]
    fn test_smoothing_alpha() {
        let alpha = analyzer_smoothing_alpha(ANALYZER_RESPONSE_DEFAULT, 1584, SAMPLE_RATE);
        assert!(alpha > 0.0 && alpha < 1.0);
    }

    #[test]
    fn test_silence_powers_zero() {
        let samples = vec![0.0_f32; 16000];
        let powers =
            samples_to_log_band_powers(&samples, SAMPLE_RATE, analyzer_fft_size(SAMPLE_RATE));
        assert!(powers.iter().all(|&p| p <= 1e-10));
    }

    #[test]
    fn test_sine_waves_peak_at_frequency() {
        // Generate a 1000 Hz sine at 48 kHz, sample for one FFT window
        let fft_size = analyzer_fft_size(SAMPLE_RATE);
        let mut samples = Vec::with_capacity(fft_size);
        for i in 0..fft_size {
            let t = i as f32 / SAMPLE_RATE as f32;
            samples.push((2.0 * std::f32::consts::PI * 1000.0 * t).sin());
        }
        let db_values = samples_to_log_band_db_values(&samples, SAMPLE_RATE, fft_size);
        assert_eq!(db_values.len(), ANALYZER_BIN_COUNT);
        // The 1000 Hz band should have the peak response
        let max_db = db_values.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let max_idx = db_values
            .iter()
            .position(|&v| (v - max_db).abs() < 1e-9)
            .unwrap();
        // 1000 Hz is in the 17th band (index 16) of the standard analyzer frequencies
        assert_eq!(max_idx, 16);
    }

    #[test]
    fn test_pcm_f32le_bytes_to_samples() {
        let bytes: Vec<u8> = vec![0x00, 0x00, 0x80, 0x3f]; // 1.0 in f32 LE
        let samples = pcm_f32le_bytes_to_samples(&bytes);
        assert_eq!(samples.len(), 1);
        assert!((samples[0] - 1.0).abs() < 1e-6);
    }

    #[test]
    fn test_interleaved_to_mono() {
        // 2 channels, 2 samples: L=[1.0, 2.0], R=[3.0, 4.0]
        let mut bytes = Vec::new();
        for v in [1.0_f32, 3.0, 2.0, 4.0] {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        let mono = interleaved_f32le_bytes_to_mono(&bytes, 2);
        assert_eq!(mono.len(), 2);
        assert!((mono[0] - 2.0).abs() < 1e-6); // (1+3)/2
        assert!((mono[1] - 3.0).abs() < 1e-6); // (2+4)/2
    }
}
