use num_complex::Complex64;
use serde::{Deserialize, Serialize};
use std::f64::consts::PI;
use std::path::{Path, PathBuf};

/// Clamp a level to [0.0, 1.0].
pub fn clamp_level(level: f64) -> f64 {
    level.clamp(0.0, 1.0)
}

/// Clamp a value to the range [lower, upper].
pub fn clamp<T: PartialOrd>(value: T, lower: T, upper: T) -> T {
    if value < lower {
        lower
    } else if value > upper {
        upper
    } else {
        value
    }
}

// ── Application ──────────────────────────────────────────────────────────────

pub const APP_NAME: &str = "Mini EQ";
pub const OUTPUT_CLIENT_NAME: &str = "Mini EQ Output";
pub const VIRTUAL_SINK_BASE: &str = "mini_eq_sink";
pub const VIRTUAL_SINK_DESCRIPTION: &str = "Mini-EQ-Sink";
pub const FILTER_OUTPUT_SUFFIX: &str = "_output";

/// Virtual sink node name for a physical output device (multi-chain EQ).
///
/// One EQ chain per device: `mini_eq_sink_<sanitized device name>`, e.g.
/// `mini_eq_sink_alsa_output_pci_0000_04_00_6_analog_stereo`.
/// `VIRTUAL_SINK_BASE` stays the prefix, so every `starts_with` check in
/// routing keeps matching. Deterministic across restarts (derived from the
/// stable PipeWire node name), never empty.
pub fn eq_virtual_sink_for(physical_sink: &str) -> String {
    let mut suffix = String::with_capacity(physical_sink.len());
    for ch in physical_sink.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
            suffix.push(ch);
        } else {
            suffix.push('_');
        }
    }
    let suffix = suffix.trim_matches(['_', '-', '.']);
    let suffix = if suffix.is_empty() {
        "unknown".to_string()
    } else {
        suffix.to_string()
    };
    format!("{VIRTUAL_SINK_BASE}_{suffix}")
}

/// Filter-chain playback node name for a physical output device.
pub fn eq_filter_output_for(physical_sink: &str) -> String {
    format!(
        "{}{FILTER_OUTPUT_SUFFIX}",
        eq_virtual_sink_for(physical_sink)
    )
}

/// Stream media roles that must never be routed through the EQ.
///
/// UI sounds and notifications are not programme material: pushing them through
/// a filter chain adds its latency to every click and alert, and upstream
/// excludes them by role (`BLOCKLIST_MEDIA_ROLES`,
/// `pipewire_stream_router.py`).
pub const BLOCKLIST_MEDIA_ROLES: [&str; 2] = ["event", "Notification"];

/// Applications that must never be routed through the EQ, by `node.name` or
/// `application.name`.
///
/// Desktop shell, media-key handling, the accessibility bus and speech
/// dispatch all own sounds that must stay on the real device and must not be
/// able to be silenced by this app misbehaving. Verbatim from upstream
/// (`BLOCKLIST_STREAM_NAMES`).
pub const BLOCKLIST_STREAM_NAMES: [&str; 7] = [
    "GNOME Shell",
    "Mutter",
    "gsd-media-keys",
    "libcanberra",
    "speech-dispatcher",
    "speech-dispatcher-dummy",
    "speech-dispatcher-espeak-ng",
];

// ── Band Configuration ───────────────────────────────────────────────────────

pub const MAX_BANDS: usize = 32;
pub const DEFAULT_ACTIVE_BANDS: usize = 10;
pub const PRESET_VERSION: i32 = 1;
pub const PRESET_FILE_SUFFIX: &str = ".json";
pub const OUTPUT_PRESET_LINKS_VERSION: i32 = 2;
pub const OUTPUT_PRESET_LINKS_FILE: &str = "output-presets.json";
pub const OUTPUT_PRESET_ROUTE_KEY_PREFIX: &str = "pipewire-route:v1:";

/// Which streams the EQ reaches. Stored in `output-presets.json` as `"mode"`.
///
/// Two modes, named for what they do to the streams rather than for a scope:
/// - **Selected** (default): only streams already aimed at the chosen device
///   are routed into the EQ. Nothing that was deliberately pointed elsewhere is
///   touched. The device you picked is the scope of the EQ.
/// - **Reroute**: every eligible stream is moved into the EQ, whatever device it
///   was on. This is an explicit opt-out from the foreign-target rule.
///
/// `SetRoutingEnabled` ("are streams routed through the EQ at all") is
/// orthogonal to this: it is the on/off of the machinery, while the mode says
/// *which* streams the machinery takes when it is on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputRoutingMode {
    Selected,
    Reroute,
}

impl OutputRoutingMode {
    pub const SELECTED: Self = Self::Selected;
    pub const REROUTE: Self = Self::Reroute;

    pub fn from_mode_str(s: &str) -> Self {
        match s.to_lowercase().as_str() {
            "reroute" => Self::Reroute,
            _ => Self::Selected,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Selected => "selected",
            Self::Reroute => "reroute",
        }
    }
}

/// The persisted shape of `output-presets.json`, version 2.
///
/// Version 1 had only `links` and `default`; version 2 adds `mode` and
/// `monitor`. The reader tolerates version 1 by treating a missing `mode` as
/// `Selected` and a missing `monitor` as `Follow`, and it drops the legacy
/// `"default"` link key (written by the old Link button) on the next write --
/// that key is the fallback, never a device.
pub struct OutputPresetConfig {
    pub links: std::collections::HashMap<String, String>,
    pub default_preset: Option<String>,
    pub mode: OutputRoutingMode,
    /// `Some(name)` to pin the monitor to a device, `None` to follow the EQ
    /// output. Stored as the sink's node name, or the sentinel `"follow"`.
    pub monitor: Option<String>,
}

impl Default for OutputPresetConfig {
    fn default() -> Self {
        Self {
            links: std::collections::HashMap::new(),
            default_preset: None,
            mode: OutputRoutingMode::Selected,
            monitor: None,
        }
    }
}

// ── EQ Modes ─────────────────────────────────────────────────────────────────

pub const EQ_MODES: [&str; 1] = ["Live PipeWire"];
// Upstream `EQ_MODE_APO = 6` (the index into the full 12-entry `FILTER_TYPES`
// map); it is *not* the index into the one-entry `EQ_MODES` map. Band `mode`
// fields persisted by the Python original use this value.
pub const EQ_MODE_APO: i32 = 6;

/// Combo-box index for a selectable filter type, mirroring upstream
/// `FILTER_TYPE_INDEX_BY_VALUE`.
///
/// This is *not* the enum discriminant: `Resonance` (7) is absent from
/// `SELECTABLE_FILTER_TYPES`, so the higher values shift down (`Allpass` 8 -> 7,
/// `Bandpass` 9 -> 8). Indexing a table by discriminant would mis-select
/// `Allpass` and run off the end for `Bandpass`.
pub fn filter_type_combo_index(filter_type: FilterType) -> usize {
    SELECTABLE_FILTER_TYPES
        .iter()
        .position(|candidate| *candidate == filter_type)
        .unwrap_or(0)
}

/// Inverse of [`filter_type_combo_index`].
pub fn filter_type_from_combo_index(index: usize) -> FilterType {
    SELECTABLE_FILTER_TYPES
        .get(index)
        .copied()
        .unwrap_or(FilterType::Off)
}

pub const MODE_ORDER: [&str; 1] = ["Live PipeWire"];
pub const MODE_INDEX_BY_VALUE: &[usize] = &[0];

// ── File paths ───────────────────────────────────────────────────────────────

pub const APP_ID: &str = "io.github.mrproject72.mini_eq_rr";

/// XDG config home or `~/.config`.
pub fn user_config_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME")
        && Path::new(&xdg).is_absolute()
    {
        return PathBuf::from(xdg);
    }
    let home = std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/tmp"));
    home.join(".config")
}

/// `~/.config/mini-eq-rr`
pub fn app_config_dir() -> PathBuf {
    user_config_dir().join("mini-eq")
}

/// `$XDG_CONFIG_HOME/mini-eq/{file_name}`
pub fn app_config_file_path(file_name: &str) -> PathBuf {
    app_config_dir().join(file_name)
}

/// `$XDG_DATA_HOME/mini-eq/{file_name}` or `~/.local/share/mini-eq/{file_name}`
pub fn app_data_file_path(file_name: &str) -> PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| Path::new(p).is_absolute())
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("/tmp"))
                .join(".local/share")
        });
    data_home.join("mini-eq").join(file_name)
}

// ── Filter Types ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum FilterType {
    Off = 0,
    Bell = 1,
    HiPass = 2,
    HiShelf = 3,
    LoPass = 4,
    LoShelf = 5,
    Notch = 6,
    Resonance = 7,
    Allpass = 8,
    Bandpass = 9,
    LadderPass = 10,
    LadderRej = 11,
    /// Internal-only "Sin" smooth bell used by the Smooth override.
    ///
    /// NOT part of `SELECTABLE_FILTER_TYPES`: the user never picks it from
    /// the type dropdown. While the Smooth switch is on it REPLACES each
    /// band's own type (and pins Q at `SMOOTH_BELL_Q`), and the band's real
    /// type/Q are left untouched so they come back when Smooth is turned off.
    ///
    /// Acoustically it is a plain peaking bell; the smoothness comes entirely
    /// from the wide Q. Measured waviness of a coupled multi-band stack:
    ///   Q 1.50 (default bell) -> 2.39 dB   (visible scalloping / "waves")
    ///   Q 0.55 (Sin)          -> 1.14 dB   (ideal Gaussian bump ~1.08 dB)
    Sin = 12,
}

impl FilterType {
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "Off" => Some(Self::Off),
            "Bell" => Some(Self::Bell),
            "Hi-pass" => Some(Self::HiPass),
            "Hi-shelf" => Some(Self::HiShelf),
            "Lo-pass" => Some(Self::LoPass),
            "Lo-shelf" => Some(Self::LoShelf),
            "Notch" => Some(Self::Notch),
            "Resonance" => Some(Self::Resonance),
            "Allpass" => Some(Self::Allpass),
            "Bandpass" => Some(Self::Bandpass),
            "Ladder-pass" => Some(Self::LadderPass),
            "Ladder-rej" => Some(Self::LadderRej),
            "Sin" => Some(Self::Sin),
            _ => None,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Off => "Off",
            Self::Bell => "Bell",
            Self::HiPass => "Hi-pass",
            Self::HiShelf => "Hi-shelf",
            Self::LoPass => "Lo-pass",
            Self::LoShelf => "Lo-shelf",
            Self::Notch => "Notch",
            Self::Resonance => "Resonance",
            Self::Allpass => "Allpass",
            Self::Bandpass => "Bandpass",
            Self::LadderPass => "Ladder-pass",
            Self::LadderRej => "Ladder-rej",
            Self::Sin => "Sin",
        }
    }

    /// SPA biquad label for this filter type.
    ///
    /// Mirrors upstream `NATIVE_BIQUAD_LABELS`; types without a native label
    /// fall back to `bq_peaking` and are bypassed via the mixer wet/dry gain.
    pub fn native_label(&self) -> &'static str {
        match self {
            Self::Off
            | Self::Bell
            | Self::Sin
            | Self::Resonance
            | Self::LadderPass
            | Self::LadderRej => "bq_peaking",
            Self::HiPass => "bq_highpass",
            Self::HiShelf => "bq_highshelf",
            Self::LoPass => "bq_lowpass",
            Self::LoShelf => "bq_lowshelf",
            Self::Notch => "bq_notch",
            Self::Allpass => "bq_allpass",
            Self::Bandpass => "bq_bandpass",
        }
    }

    /// Whether this type maps to a native SPA biquad (upstream
    /// `filter_type in NATIVE_BIQUAD_LABELS`). Types that do not are fully
    /// bypassed by the mixer rather than processed.
    pub fn has_native_biquad(&self) -> bool {
        matches!(
            self,
            Self::Off
                | Self::Bell
                | Self::Sin
                | Self::HiPass
                | Self::HiShelf
                | Self::LoPass
                | Self::LoShelf
                | Self::Notch
                | Self::Allpass
                | Self::Bandpass
        )
    }
}

pub const SELECTABLE_FILTER_TYPES: [FilterType; 9] = [
    FilterType::Off,
    FilterType::Bell,
    FilterType::HiPass,
    FilterType::HiShelf,
    FilterType::LoPass,
    FilterType::LoShelf,
    FilterType::Notch,
    FilterType::Allpass,
    FilterType::Bandpass,
];

// ── EQ Parameters ────────────────────────────────────────────────────────────

pub const SAMPLE_RATE: f64 = 48000.0;
pub const GRAPH_FREQ_MIN: f64 = 20.0;
pub const GRAPH_FREQ_MAX: f64 = 20000.0;
pub const GRAPH_DB_MIN: f64 = -24.0;
pub const GRAPH_DB_MAX: f64 = 24.0;
pub const RESPONSE_PEAK_F_STEP: f64 = 1.02;

pub const EQ_FREQUENCY_MIN_HZ: f64 = 20.0;
pub const EQ_FREQUENCY_MAX_HZ: f64 = 20000.0;
pub const EQ_GAIN_MIN_DB: f64 = -20.0;
pub const EQ_GAIN_MAX_DB: f64 = 20.0;
pub const EQ_Q_MIN: f64 = 0.18248;
pub const EQ_Q_MAX: f64 = 6.0;
/// Upstream `DEFAULT_BAND_Q = 1.0 / math.sqrt(2.0)`.
pub const DEFAULT_BAND_Q: f64 = std::f64::consts::FRAC_1_SQRT_2;
pub const EQ_PREAMP_MIN_DB: f64 = -36.0;
pub const EQ_PREAMP_MAX_DB: f64 = 6.0;

// ── Biquad Coefficients ──────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BiquadCoefficients {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a0: f64,
    pub a1: f64,
    pub a2: f64,
}

impl BiquadCoefficients {
    pub fn identity() -> Self {
        Self {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a0: 1.0,
            a1: 0.0,
            a2: 0.0,
        }
    }

    pub fn is_identity(&self) -> bool {
        (self.b0 - 1.0).abs() < 1e-12
            && self.b1.abs() < 1e-12
            && self.b2.abs() < 1e-12
            && (self.a0 - 1.0).abs() < 1e-12
            && self.a1.abs() < 1e-12
            && self.a2.abs() < 1e-12
    }

    pub fn as_tuple(&self) -> (f64, f64, f64, f64, f64, f64) {
        (self.b0, self.b1, self.b2, self.a0, self.a1, self.a2)
    }

    pub fn as_array(&self) -> [f64; 6] {
        [self.b0, self.b1, self.b2, self.a0, self.a1, self.a2]
    }

    /// Mirror of upstream `scaled_for_control_range`: PipeWire's `bq_raw`
    /// controls must stay within +/-10 or the node fails to configure.
    pub fn scaled_for_control_range(&self, limit: f64) -> Self {
        let max_abs = self
            .as_array()
            .iter()
            .fold(0.0_f64, |acc, v| acc.max(v.abs()));
        if max_abs <= limit || max_abs == 0.0 {
            return self.clone();
        }

        let scale = limit / max_abs;
        Self {
            b0: self.b0 * scale,
            b1: self.b1 * scale,
            b2: self.b2 * scale,
            a0: self.a0 * scale,
            a1: self.a1 * scale,
            a2: self.a2 * scale,
        }
    }
}

pub fn identity_biquad_coefficients(gain: f64) -> BiquadCoefficients {
    if gain == 1.0 {
        return BiquadCoefficients::identity();
    }
    BiquadCoefficients {
        b0: gain,
        b1: 0.0,
        b2: 0.0,
        a0: 1.0,
        a1: 0.0,
        a2: 0.0,
    }
}

// ── EQ Band ──────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct EqBand {
    pub index: usize,
    pub frequency: f64,
    pub gain_db: f64,
    pub q: f64,
    pub filter_type: FilterType,
    pub mute: bool,
    pub solo: bool,
    pub coefficients: BiquadCoefficients,
}

impl EqBand {
    pub fn new(index: usize) -> Self {
        Self {
            index,
            frequency: 1000.0,
            gain_db: 0.0,
            q: 1.0,
            filter_type: FilterType::Off,
            mute: false,
            solo: false,
            coefficients: BiquadCoefficients::identity(),
        }
    }

    pub fn is_effective(&self) -> bool {
        !self.mute && self.filter_type != FilterType::Off
    }
}

// ── Biquad Coefficient Calculation ───────────────────────────────────────────

pub fn band_biquad_coefficients(
    band: &EqBand,
    sample_rate: f64,
    solo_active: bool,
) -> BiquadCoefficients {
    if !band_is_effective(band, solo_active) || !band.filter_type.is_selectable() {
        return BiquadCoefficients::identity();
    }

    // Upstream clamps to the Nyquist limit (`sample_rate / 2 - 1`) and floors Q
    // at 0.0001, not at the UI's EQ_Q_MIN.
    let center_max = EQ_FREQUENCY_MAX_HZ.min((sample_rate * 0.5) - 1.0);
    let f0 = band.frequency.clamp(EQ_FREQUENCY_MIN_HZ, center_max);
    let gain = band.gain_db.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
    let q = band.q.max(0.0001);
    let a = 10.0_f64.powf(gain / 40.0);
    let omega = 2.0 * PI * f0 / sample_rate;
    let sin_omega = omega.sin();
    let cos_omega = omega.cos();
    let alpha = sin_omega / (2.0 * q);

    let (b0, b1, b2, a0, a1, a2) = match band.filter_type {
        // `Sin` is the Smooth override: same peaking math as Bell, the
        // smoothness comes from the wide `SMOOTH_BELL_Q` it is given.
        FilterType::Bell | FilterType::Sin => {
            let b0 = 1.0 + alpha * a;
            let b1 = -2.0 * cos_omega;
            let b2 = 1.0 - alpha * a;
            let a0 = 1.0 + alpha / a;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha / a;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::HiPass => {
            let b0 = (1.0 + cos_omega) / 2.0;
            let b1 = -(1.0 + cos_omega);
            let b2 = (1.0 + cos_omega) / 2.0;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::LoPass => {
            let b0 = (1.0 - cos_omega) / 2.0;
            let b1 = 1.0 - cos_omega;
            let b2 = (1.0 - cos_omega) / 2.0;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::HiShelf => {
            let beta = 2.0 * a.sqrt() * alpha;
            let b0 = a * ((a + 1.0) + (a - 1.0) * cos_omega + beta);
            let b1 = -2.0 * a * ((a - 1.0) + (a + 1.0) * cos_omega);
            let b2 = a * ((a + 1.0) + (a - 1.0) * cos_omega - beta);
            let a0 = (a + 1.0) - (a - 1.0) * cos_omega + beta;
            let a1 = 2.0 * ((a - 1.0) - (a + 1.0) * cos_omega);
            let a2 = (a + 1.0) - (a - 1.0) * cos_omega - beta;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::LoShelf => {
            let beta = 2.0 * a.sqrt() * alpha;
            let b0 = a * ((a + 1.0) - (a - 1.0) * cos_omega + beta);
            let b1 = 2.0 * a * ((a - 1.0) - (a + 1.0) * cos_omega);
            let b2 = a * ((a + 1.0) - (a - 1.0) * cos_omega - beta);
            let a0 = (a + 1.0) + (a - 1.0) * cos_omega + beta;
            let a1 = -2.0 * ((a - 1.0) + (a + 1.0) * cos_omega);
            let a2 = (a + 1.0) + (a - 1.0) * cos_omega - beta;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::Notch => {
            let b0 = 1.0;
            let b1 = -2.0 * cos_omega;
            let b2 = 1.0;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::Allpass => {
            let b0 = 1.0 - alpha;
            let b1 = -2.0 * cos_omega;
            let b2 = 1.0 + alpha;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::Bandpass => {
            let b0 = alpha;
            let b1 = 0.0;
            let b2 = -alpha;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::Resonance => {
            let b0 = 1.0;
            let b1 = -2.0 * cos_omega;
            let b2 = 1.0;
            let a0 = 1.0 + alpha / q;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha / q;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::LadderPass => {
            let b0 = (1.0 - cos_omega) / 2.0;
            let b1 = 1.0 - cos_omega;
            let b2 = (1.0 - cos_omega) / 2.0;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::LadderRej => {
            let b0 = 1.0 + alpha;
            let b1 = -2.0 * cos_omega;
            let b2 = 1.0 - alpha;
            let a0 = 1.0 + alpha;
            let a1 = -2.0 * cos_omega;
            let a2 = 1.0 - alpha;
            (b0, b1, b2, a0, a1, a2)
        }
        FilterType::Off => (1.0, 0.0, 0.0, 1.0, 0.0, 0.0),
    };

    // Normalize by a0
    let norm = a0;
    BiquadCoefficients {
        b0: b0 / norm,
        b1: b1 / norm,
        b2: b2 / norm,
        a0: 1.0,
        a1: a1 / norm,
        a2: a2 / norm,
    }
}

pub fn bands_have_solo(bands: &[EqBand]) -> bool {
    bands.iter().any(|band| band.solo)
}

pub fn db_to_linear(value_db: f64) -> f64 {
    10.0_f64.powf(value_db / 20.0)
}

pub fn band_is_effective(band: &EqBand, solo_active: bool) -> bool {
    !band.mute && band.filter_type != FilterType::Off && (!solo_active || band.solo)
}

impl FilterType {
    fn is_selectable(&self) -> bool {
        matches!(
            self,
            FilterType::Off
                | FilterType::Bell
                | FilterType::Sin
                | FilterType::HiPass
                | FilterType::HiShelf
                | FilterType::LoPass
                | FilterType::LoShelf
                | FilterType::Notch
                | FilterType::Allpass
                | FilterType::Bandpass
        )
    }
}

// ── DSP / Response Helpers ────────────────────────────────────────────────────

pub fn format_frequency(value: f64) -> String {
    if value >= 1000.0 {
        format!("{:.1}k", value / 1000.0)
    } else {
        format!("{}", value.round() as i64)
    }
}

pub fn log_response_frequencies(sample_rate: f64, f_step: f64) -> Vec<f64> {
    let max_frequency = GRAPH_FREQ_MAX.min((sample_rate * 0.5) - 1.0);
    if max_frequency <= GRAPH_FREQ_MIN {
        return vec![1.0_f64.max(max_frequency)];
    }

    let f_step = f_step.max(1.0001);
    let mut frequencies = Vec::new();
    let mut frequency = GRAPH_FREQ_MIN;
    while frequency <= max_frequency {
        frequencies.push(frequency);
        frequency *= f_step;
    }

    if let Some(&last) = frequencies.last() {
        if last < max_frequency {
            frequencies.push(max_frequency);
        }
    }

    frequencies
}

pub fn stepped_response_frequencies(sample_rate: f64, steps: i32) -> Vec<f64> {
    let max_frequency = GRAPH_FREQ_MAX.min((sample_rate * 0.5) - 1.0);
    if max_frequency <= GRAPH_FREQ_MIN {
        return vec![1.0_f64.max(max_frequency)];
    }

    (0..steps.max(2))
        .map(|i| {
            let t = i as f64 / (steps.max(2) - 1) as f64;
            GRAPH_FREQ_MIN * (max_frequency / GRAPH_FREQ_MIN).powf(t)
        })
        .collect()
}

pub fn biquad_response_at_frequency(
    coefficients: &BiquadCoefficients,
    sample_rate: f64,
    frequency: f64,
) -> Complex64 {
    let frequency = frequency.clamp(1.0, (sample_rate * 0.5) - 1.0);
    let omega = 2.0 * PI * frequency / sample_rate;

    let z1 = Complex64::new(omega.cos(), -omega.sin());
    let z2 = z1 * z1;

    let numerator = coefficients.b0 + coefficients.b1 * z1 + coefficients.b2 * z2;
    let denominator = coefficients.a0 + coefficients.a1 * z1 + coefficients.a2 * z2;

    if denominator.norm() < 1e-12 {
        Complex64::new(1.0, 0.0)
    } else {
        numerator / denominator
    }
}

/// Unclamped response in dB — mirrors upstream `total_response_db(..., clamp_output=False)`.
pub fn total_response_db_unclamped(
    bands: &[EqBand],
    preamp_db: f64,
    sample_rate: f64,
    frequency: f64,
) -> f64 {
    let mut response = Complex64::new(1.0, 0.0);
    let solo_active = bands.iter().any(|b| b.solo);

    for band in bands {
        let coeffs = band_biquad_coefficients(band, sample_rate, solo_active);
        response *= biquad_response_at_frequency(&coeffs, sample_rate, frequency);
    }

    let magnitude = response.norm().max(1e-12);
    preamp_db + 20.0 * magnitude.log10()
}

pub fn total_response_db(
    bands: &[EqBand],
    preamp_db: f64,
    sample_rate: f64,
    frequency: f64,
) -> f64 {
    total_response_db_unclamped(bands, preamp_db, sample_rate, frequency)
        .clamp(GRAPH_DB_MIN - 12.0, GRAPH_DB_MAX + 12.0)
}

pub fn total_response_db_at_frequencies(
    bands: &[EqBand],
    preamp_db: f64,
    sample_rate: f64,
    frequencies: &[f64],
) -> Vec<f64> {
    frequencies
        .iter()
        .map(|&f| total_response_db(bands, preamp_db, sample_rate, f))
        .collect()
}

/// Same as [`total_response_db_at_frequencies`] but without the graph-display
/// clamp. Used by [`estimate_response_peak_db`] so stacked boosts are not
/// silently capped at `GRAPH_DB_MAX + 12`.
pub fn total_response_db_at_frequencies_unclamped(
    bands: &[EqBand],
    preamp_db: f64,
    sample_rate: f64,
    frequencies: &[f64],
) -> Vec<f64> {
    frequencies
        .iter()
        .map(|&f| total_response_db_unclamped(bands, preamp_db, sample_rate, f))
        .collect()
}

/// Frequencies used by [`estimate_response_peak_db`], mirroring upstream
/// `response_peak_frequencies`.
///
/// The dense log sweep can straddle a narrow filter's centre and miss its peak,
/// so every effective band's centre frequency is unioned in.
pub fn response_peak_frequencies(bands: &[EqBand], sample_rate: f64) -> Vec<f64> {
    let mut frequencies = log_response_frequencies(sample_rate, RESPONSE_PEAK_F_STEP);

    let solo_active = bands_have_solo(bands);
    let max_frequency = GRAPH_FREQ_MAX.min((sample_rate * 0.5) - 1.0);
    for band in bands {
        if band_is_effective(band, solo_active) {
            frequencies.push(band.frequency.clamp(GRAPH_FREQ_MIN, max_frequency));
        }
    }

    frequencies.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    frequencies.dedup();
    frequencies
}

pub fn estimate_response_peak_db(bands: &[EqBand], preamp_db: f64, sample_rate: f64) -> f64 {
    let frequencies = response_peak_frequencies(bands, sample_rate);
    // Upstream passes clamp_output=False: the headroom/Auto-Safe logic must see
    // the TRUE curve peak, not the graph-display clamp (+36 dB), or it
    // under-compensates when boosts stack well past the preamp range.
    let responses =
        total_response_db_at_frequencies_unclamped(bands, preamp_db, sample_rate, &frequencies);
    responses
        .into_iter()
        .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(preamp_db)
}

/// Smooth curve width, expressed as a **multiple of band spacing**.
///
/// Why relative: the smoothness of a summed multi-band curve is governed by
/// how well each bell bridges the gap to its neighbours. Measured against
/// the real band layout (10 log-spaced bands, spacing 0.997 oct), with a
/// coupled 5-band bump:
///
/// | width (oct) | W/spacing | local extrema |
/// |---|---|---|
/// | 0.50 | 0.50 | **11** — deep troughs between bands |
/// | 0.75 | 0.75 | **11** |
/// | 0.90 | 0.90 | **11** |
/// | 1.10 | 1.10 | 3 |
/// | 1.30 | 1.30 | **1** — smooth |
/// Smooth **spread** limits, in **bands** (the Gaussian kernel's sigma).
///
/// This is what the Width slider actually controls: how many neighbouring
/// bands move when you drag one. Verified on the stock 10-band layout with
/// a +6 dB drag on band 3 (drop threshold 10%, bell Q floored at the
/// bridging width):
///
/// | sigma | bands moved | extrema |
/// |---|---|---|
/// | 0.45 | **1** (dragged band only) | 1 |
/// | 0.80 | 3 | 1 |
/// | 1.00 | 5 | 1 |
/// | 1.60 | 6 | 1 |
/// | 3.00 | 9 | 1 |
pub const SMOOTH_SPREAD_MIN_BANDS: f64 = 0.45;
pub const SMOOTH_SPREAD_MAX_BANDS: f64 = 3.0;
/// Default: 3 bands participate (dragged band + immediate neighbours).
/// Chosen deliberately small — the user wants involvement restricted.
pub const SMOOTH_SPREAD_DEFAULT_BANDS: f64 = 0.8;

/// Minimum bell width, in octaves, needed to bridge adjacent bands.
///
/// Below this the bells underlap and the curve breaks into separate bumps
/// with U-shaped troughs between them. The bell Q is floored here so that
/// a narrow *spread* still produces a rounded bump rather than a spike
/// with valleys.
pub const SMOOTH_BRIDGE_WIDTH_OCT: f64 = 1.3;

/// Bell Q for a given spread.
///
/// Wide spreads need wide bells to bridge the participating region; a
/// narrow spread still needs at least the bridging width. Hence the max.
pub fn smooth_bell_q(spread_bands: f64, spacing_oct: f64) -> f64 {
    let sp = if spacing_oct.is_finite() && spacing_oct > 0.0 {
        spacing_oct
    } else {
        1.0
    };
    let sigma = if spread_bands.is_finite() {
        spread_bands.clamp(SMOOTH_SPREAD_MIN_BANDS, SMOOTH_SPREAD_MAX_BANDS)
    } else {
        SMOOTH_SPREAD_DEFAULT_BANDS
    };
    q_for_smooth_width((sigma * sp).max(SMOOTH_BRIDGE_WIDTH_OCT))
}

/// Spread slider bounds for a given band set (min, max, default) in bands.
pub fn smooth_spread_bounds_bands() -> (f64, f64, f64) {
    (
        SMOOTH_SPREAD_MIN_BANDS,
        SMOOTH_SPREAD_MAX_BANDS,
        SMOOTH_SPREAD_DEFAULT_BANDS,
    )
}

/// Median spacing between consecutive active (non-`Off`) band frequencies,/// in octaves. Falls back to the 10-band default layout if fewer than two
/// bands are active.
pub fn band_spacing_oct(bands: &[EqBand]) -> f64 {
    let mut freqs: Vec<f64> = bands
        .iter()
        .filter(|b| b.filter_type != FilterType::Off && b.frequency > 0.0)
        .map(|b| b.frequency)
        .collect();
    freqs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    if freqs.len() < 2 {
        // Mirror compute_log_spaced_band_defaults for the default layout.
        return 1.0;
    }
    let mut ratios: Vec<f64> = freqs
        .windows(2)
        .map(|w| (w[1] / w[0]).log2())
        .filter(|r| r.is_finite() && *r > 0.0)
        .collect();
    if ratios.is_empty() {
        return 1.0;
    }
    ratios.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    ratios[ratios.len() / 2]
}

/// Smooth curve width limits, in **octaves of half-boost bandwidth**.
///
/// Half-boost width is the span over which the bell sits at least half its
/// peak boost. Measured identical at +3 / +6 / +12 / +18 dB, i.e. it is
/// *gain-independent* — which makes octaves an honest, intuitive control
/// instead of raw Q (where smaller means wider, and the number means
/// nothing to the ear).
pub const SMOOTH_WIDTH_MIN_OCT: f64 = 0.5;
pub const SMOOTH_WIDTH_MAX_OCT: f64 = 4.0;

/// `(half-boost width in octaves, Q)` for a peaking bell.
///
/// Generated by numerically inverting the bell's half-boost bandwidth; the
/// relationship is not a simple inverse so a table is used.
const SMOOTH_WIDTH_TABLE: [(f64, f64); 15] = [
    (0.50, 2.850680),
    (0.75, 1.892359),
    (1.00, 1.406766),
    (1.25, 1.114155),
    (1.50, 0.914949),
    (1.75, 0.772338),
    (2.00, 0.663084),
    (2.25, 0.577069),
    (2.50, 0.507160),
    (2.75, 0.449840),
    (3.00, 0.400695),
    (3.25, 0.359083),
    (3.50, 0.322802),
    (3.75, 0.291155),
    (4.00, 0.263047),
];

/// Map a smooth width in octaves to the bell Q that produces it.
/// Linear interpolation between table points, clamped to the table range.
pub fn q_for_smooth_width(width_oct: f64) -> f64 {
    if !width_oct.is_finite() {
        return q_for_smooth_width(SMOOTH_BRIDGE_WIDTH_OCT);
    }
    let w = width_oct.clamp(SMOOTH_WIDTH_MIN_OCT, SMOOTH_WIDTH_MAX_OCT);
    for pair in SMOOTH_WIDTH_TABLE.windows(2) {
        let (w0, q0) = pair[0];
        let (w1, q1) = pair[1];
        if w <= w1 {
            let t = (w - w0) / (w1 - w0);
            return q0 + (q1 - q0) * t;
        }
    }
    SMOOTH_WIDTH_TABLE[SMOOTH_WIDTH_TABLE.len() - 1].1
}

/// Q at the default smooth width. Kept as a named reference to the
/// measured sweet spot (waviness ~1.14 dB vs 2.39 dB for the Q 1.5
/// default bell, and ~1.08 dB for an ideal Gaussian bump).
pub const SMOOTH_BELL_Q: f64 = 0.55;

/// Apply the Smooth override to a band at a given bell Q.
pub fn smooth_effective_band(band: &EqBand, q: f64) -> EqBand {
    if band.filter_type == FilterType::Off {
        return band.clone();
    }
    let mut b = band.clone();
    b.filter_type = FilterType::Sin;
    b.q = q;
    b
}

/// Neighbour-coupling kernel for smooth (spline-like) band editing.
///
/// A lone Bell raises an isolated hump: the neighbouring bands stay put and
/// the curve looks like a bump sitting on a flat line. With coupling, moving
/// one band drags its neighbours by a decaying fraction of the same delta,
/// so the summed response is one smooth, broad curve that involves the
/// adjacent bands on both sides.
///
/// Geometric decay: half the influence per step away from the dragged band.
pub const SMOOTH_NEIGHBOR_WEIGHTS: [(i64, f64); 4] = [(-2, 0.25), (-1, 0.5), (1, 0.5), (2, 0.25)];

/// Gaussian neighbour-coupling kernel: the delta to apply to every other
/// band when `center` moves by `delta_db`.
///
/// Weight falls off as `exp(-k^2 / 2 sigma^2)` in band-index distance, so
/// a small sigma keeps the edit local (3-5 bands) and a large sigma
/// spreads it across most of the spectrum. Tails below 2% are dropped so
/// far-away bands are not touched at all.
pub fn smooth_kernel_weights(
    center: usize,
    delta_db: f64,
    sigma: f64,
    band_count: usize,
) -> Vec<(usize, f64)> {
    if band_count == 0 || !delta_db.is_finite() || !sigma.is_finite() || sigma <= 0.0 {
        return Vec::new();
    }
    let two_sigma_sq = 2.0 * sigma * sigma;
    (0..band_count)
        .filter_map(|i| {
            if i == center {
                return None;
            }
            let k = (i as i64 - center as i64) as f64;
            let exponent = -(k * k) / two_sigma_sq;
            if exponent < -2.3 {
                return None; // weight < ~10%: don't drag far bands along
            }
            Some((i, delta_db * exponent.exp()))
        })
        .collect()
}

/// Weighted neighbour deltas for a `delta_db` change on `index`, clamped to
/// the existing band range. Off-range neighbours are dropped (no wrapping).
pub fn smooth_neighbor_deltas(index: usize, delta_db: f64, band_count: usize) -> Vec<(usize, f64)> {
    SMOOTH_NEIGHBOR_WEIGHTS
        .iter()
        .filter_map(|(offset, weight)| {
            let other = index as i64 + offset;
            if other >= 0 && (other as usize) < band_count {
                Some((other as usize, delta_db * weight))
            } else {
                None
            }
        })
        .collect()
}

/// Highest raw (preamp = 0) curve peak the interactive gain controls may
/// create.
///
/// Budget rule: the preamp floor is Auto-Safe's only lever, so the floor must
/// be able to pull a capped curve down to the target:
///
/// `EQ_PREAMP_MIN_DB <= AUTO_SAFE_TARGET_DBFS - MAX_SAFE_RAW_PEAK_DB`
///
/// With a -36 dB floor and a -1 dBFS target we may allow +30 dB of raw peak
/// and still have 5 dB of slack for estimation/rounding error. A single band
/// is effectively unrestricted (a max-gain shelf tops out near +23.5 dB);
/// the cap only bites when several boosts stack in the same region, and it is
/// deliberately loose enough that realistic multi-band shaping — e.g. a
/// +12 dB bell plus two shelves — is not refused at the fader.
pub const MAX_SAFE_RAW_PEAK_DB: f64 = 30.0;

/// Clamp a proposed gain for `band_index` so the curve's raw peak stays at
/// or below the effective cap. The effective cap *observes the current
/// line*: it is `max(max_raw_peak_db, peak_of_current_curve)`. An edit can
/// therefore never raise the peak above where it already sits (a preset
/// that loads hot can be tweaked but not made hotter), while normal use is
/// held to the safe cap that Auto-Safe can always absorb.
///
/// The response peak is monotonic in a band's dB gain (more gain ⇒ more
/// boost), so the largest admissible gain is found by bisection.
/// Semantics:
/// - cuts (gain ≤ 0) are always allowed — they can only reduce the peak;
/// - a boost that already fits under the effective cap passes through;
/// - otherwise the gain is bisected down to the cap.
pub fn clamp_gain_for_peak(
    bands: &[EqBand],
    band_index: usize,
    gain_db: f64,
    sample_rate: f64,
    max_raw_peak_db: f64,
) -> f64 {
    let proposed = gain_db.clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB);
    if proposed <= 0.0 {
        return proposed;
    }
    let Some(slot) = bands.iter().position(|b| b.index == band_index) else {
        return proposed;
    };
    // Observe the current line before the edit: never clamp below the
    // status quo, never allow a peak above max(cap, status quo).
    let current_peak = estimate_response_peak_db(bands, 0.0, sample_rate);
    let cap = max_raw_peak_db.max(current_peak);
    let mut trial: Vec<EqBand> = bands.to_vec();
    trial[slot].gain_db = proposed;
    if estimate_response_peak_db(&trial, 0.0, sample_rate) <= cap {
        return proposed;
    }
    // Bisect [0, proposed] for the largest gain whose peak fits. 14 steps
    // resolve a 40 dB range to ~0.002 dB.
    let (mut lo, mut hi) = (0.0, proposed);
    for _ in 0..14 {
        let mid = (lo + hi) / 2.0;
        trial[slot].gain_db = mid;
        if estimate_response_peak_db(&trial, 0.0, sample_rate) <= cap {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    (lo * 10.0).floor() / 10.0
}

// ── Default Band Presets ─────────────────────────────────────────────────────

/// Log-spaced band defaults, mirroring upstream
/// `compute_log_spaced_band_defaults(num_bands)`.
///
/// Returns `(frequency, q)` pairs. Note that the spacing — and therefore the
/// frequencies — depend on `num_bands`, so the active-band defaults are *not*
/// a prefix of the `MAX_BANDS` defaults.
pub fn compute_log_spaced_band_defaults(num_bands: usize) -> Vec<(f64, f64)> {
    if num_bands == 0 {
        return Vec::new();
    }

    let freq_min = GRAPH_FREQ_MIN;
    let freq_max = GRAPH_FREQ_MAX;
    let mut freq0 = freq_min;
    let step = (freq_max / freq_min).powf(1.0 / num_bands as f64);

    let mut defaults = Vec::with_capacity(num_bands);
    for _ in 0..num_bands {
        let freq1 = freq0 * step;
        let freq = freq0 + 0.5 * (freq1 - freq0);
        let width = freq1 - freq0;
        let q_value = freq / width;
        defaults.push((freq, q_value));
        freq0 = freq1;
    }

    defaults
}

/// All `MAX_BANDS` bands, inactive (`Off`), mirroring upstream `inactive_eq_bands()`.
pub fn inactive_eq_bands() -> Vec<EqBand> {
    compute_log_spaced_band_defaults(MAX_BANDS)
        .into_iter()
        .enumerate()
        .map(|(index, (frequency, q_value))| {
            let mut band = EqBand::new(index);
            band.frequency = frequency;
            band.gain_db = 0.0;
            band.q = q_value;
            band.filter_type = FilterType::Off;
            band.coefficients = BiquadCoefficients::identity();
            band
        })
        .collect()
}

/// The default band set: `MAX_BANDS` bands with the first
/// `DEFAULT_ACTIVE_BANDS` enabled as bells, mirroring upstream `default_eq_bands()`.
pub fn default_bands() -> Vec<EqBand> {
    let mut bands = inactive_eq_bands();

    for (index, (frequency, q_value)) in compute_log_spaced_band_defaults(DEFAULT_ACTIVE_BANDS)
        .into_iter()
        .enumerate()
    {
        if let Some(band) = bands.get_mut(index) {
            band.filter_type = FilterType::Bell;
            band.frequency = frequency;
            band.q = q_value;
        }
    }

    bands
}

/// Built-in factory presets that ship with the app and cannot be removed by
/// the user. They live in code (not the preset dir) so they always exist.
pub const BUILTIN_PRESET_NAMES: &[&str] = &["Neutral", "Bass Boost", "Treble Boost"];

/// True if `name` is one of the non-removable built-in presets.
pub fn is_builtin_preset(name: &str) -> bool {
    BUILTIN_PRESET_NAMES.contains(&name)
}

/// Bands + preamp for a built-in preset name, or `None` if not a built-in.
/// Built on top of `default_bands()` (log-spaced active bells) by nudging
/// the low/high bands.
pub fn builtin_preset_bands(name: &str) -> Option<(Vec<EqBand>, f64)> {
    match name {
        "Neutral" => Some((default_bands(), 0.0)),
        "Bass Boost" => {
            let mut bands = default_bands();
            for band in bands.iter_mut().take(3) {
                band.gain_db = 5.0;
            }
            Some((bands, 0.0))
        }
        "Treble Boost" => {
            let mut bands = default_bands();
            let n = DEFAULT_ACTIVE_BANDS;
            if n >= 3 {
                for band in bands.iter_mut().skip(n - 3).take(3) {
                    band.gain_db = 4.0;
                }
            }
            Some((bands, 0.0))
        }
        _ => None,
    }
}

pub fn eq_band_to_dict(band: &EqBand) -> serde_json::Value {
    serde_json::json!({
        "filter_type": band.filter_type as u8,
        "frequency": band.frequency,
        "gain_db": band.gain_db,
        "q": band.q,
        "mute": band.mute,
        "solo": band.solo,
    })
}

pub fn eq_band_from_dict(data: &serde_json::Value, fallback: &EqBand) -> EqBand {
    let raw_filter_type = data
        .get("filter_type")
        .and_then(|v| v.as_f64())
        .unwrap_or(fallback.filter_type as u8 as f64)
        .clamp(0.0, 11.0) as u8;
    // Upstream only accepts the selectable types; Resonance/Ladder-* are
    // coerced to Off when loaded.
    let mut filter_type = match raw_filter_type {
        1 => FilterType::Bell,
        2 => FilterType::HiPass,
        3 => FilterType::HiShelf,
        4 => FilterType::LoPass,
        5 => FilterType::LoShelf,
        6 => FilterType::Notch,
        8 => FilterType::Allpass,
        9 => FilterType::Bandpass,
        _ => FilterType::Off,
    };
    if !filter_type.is_selectable() {
        filter_type = FilterType::Off;
    }

    // Upstream persists `mute`; `enabled` is still accepted (inverted) for
    // presets written by earlier builds of this port.
    let mute = match (
        data.get("mute").and_then(|v| v.as_bool()),
        data.get("enabled").and_then(|v| v.as_bool()),
    ) {
        (Some(mute), _) => mute,
        (None, Some(enabled)) => !enabled,
        (None, None) => fallback.mute,
    };

    EqBand {
        index: fallback.index,
        frequency: data
            .get("frequency")
            .and_then(|v| v.as_f64())
            .unwrap_or(fallback.frequency)
            .clamp(EQ_FREQUENCY_MIN_HZ, EQ_FREQUENCY_MAX_HZ),
        gain_db: data
            .get("gain_db")
            .and_then(|v| v.as_f64())
            .unwrap_or(fallback.gain_db)
            .clamp(EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB),
        q: data
            .get("q")
            .and_then(|v| v.as_f64())
            .unwrap_or(fallback.q)
            .clamp(EQ_Q_MIN, EQ_Q_MAX),
        filter_type,
        mute,
        solo: data
            .get("solo")
            .and_then(|v| v.as_bool())
            .unwrap_or(fallback.solo),
        coefficients: BiquadCoefficients::identity(),
    }
}

pub fn preset_payload(bands: &[EqBand], preamp_db: f64) -> serde_json::Value {
    let bands_array: Vec<serde_json::Value> = bands.iter().map(eq_band_to_dict).collect();
    serde_json::json!({
        "version": PRESET_VERSION,
        "preamp_db": preamp_db,
        "bands": bands_array,
    })
}

pub fn preset_payload_bands(payload: &serde_json::Value) -> anyhow::Result<Vec<EqBand>> {
    let bands_data = payload
        .get("bands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    // Base on the full MAX_BANDS set: `bands_data` may carry up to MAX_BANDS
    // entries, so seeding from `default_bands()` (DEFAULT_ACTIVE_BANDS long)
    // would index out of bounds.
    let fallback = inactive_eq_bands();
    let mut bands = fallback.clone();
    for (i, band_data) in bands_data.iter().take(MAX_BANDS).enumerate() {
        bands[i] = eq_band_from_dict(band_data, &fallback[i]);
    }
    Ok(bands)
}

/// Canonical signature of a preset payload, used to detect "modified" state.
///
/// Mirrors upstream `preset_payload_state_signature`: bands are normalised
/// through the same from_dict/to_dict round trip, values are clamped, and keys
/// are sorted with compact separators so two equivalent presets compare equal.
pub fn preset_payload_state_signature(payload: &serde_json::Value) -> String {
    let bands_data = payload
        .get("bands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    let mut bands = inactive_eq_bands();
    for (index, band_data) in bands_data.iter().take(MAX_BANDS).enumerate() {
        if !band_data.is_object() {
            continue;
        }
        bands[index] = eq_band_from_dict(band_data, &bands[index]);
    }

    let preamp_db = payload
        .get("preamp_db")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .clamp(EQ_PREAMP_MIN_DB, EQ_PREAMP_MAX_DB);
    let signature_payload = serde_json::json!({
        "version": PRESET_VERSION,
        "preamp_db": preamp_db,
        "bands": bands.iter().map(eq_band_to_dict).collect::<Vec<_>>(),
    });
    canonical_json(&signature_payload)
}

/// Serialize JSON with sorted object keys and no insignificant whitespace,
/// matching Python's `json.dumps(..., sort_keys=True, separators=(",", ":"))`.
fn canonical_json(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let inner = keys
                .iter()
                .map(|k| {
                    format!(
                        "{}:{}",
                        serde_json::Value::String((*k).clone()),
                        canonical_json(&map[*k])
                    )
                })
                .collect::<Vec<_>>()
                .join(",");
            format!("{{{}}}", inner)
        }
        serde_json::Value::Array(items) => format!(
            "[{}]",
            items
                .iter()
                .map(canonical_json)
                .collect::<Vec<_>>()
                .join(",")
        ),
        other => other.to_string(),
    }
}

pub fn save_preset_to_file(path: &Path, bands: &[EqBand], preamp_db: f64) -> anyhow::Result<()> {
    let payload = preset_payload(bands, preamp_db);
    let data = serde_json::to_string_pretty(&payload)?;
    std::fs::write(path, format!("{}\n", data))?;
    Ok(())
}

pub fn load_preset_from_file(path: &Path) -> anyhow::Result<(f64, Vec<EqBand>)> {
    let data = std::fs::read_to_string(path)?;
    let payload: serde_json::Value = serde_json::from_str(&data)?;

    // Upstream `json_document_version` treats a missing version as 0 and only
    // rejects presets newer than this build.
    let version = payload.get("version").and_then(|v| v.as_i64()).unwrap_or(0);
    if version < 0 {
        anyhow::bail!("preset version must be a non-negative integer");
    }
    if version > PRESET_VERSION as i64 {
        anyhow::bail!(
            "preset version {} is newer than this Mini EQ build",
            version
        );
    }

    let preamp_db = payload
        .get("preamp_db")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);
    let bands_data = payload
        .get("bands")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();

    let fallback = inactive_eq_bands();
    let mut bands = fallback.clone();
    for (i, band_data) in bands_data.iter().take(MAX_BANDS).enumerate() {
        bands[i] = eq_band_from_dict(band_data, &fallback[i]);
    }

    Ok((preamp_db, bands))
}

/// Upstream stores presets under `$XDG_CONFIG_HOME/mini-eq/output`
/// (`default_preset_storage_dir`), so keep the same location to stay
/// interoperable with the Python original.
pub fn preset_storage_dir() -> std::path::PathBuf {
    app_config_dir().join("output")
}

pub fn preset_path_for_name(name: &str) -> std::path::PathBuf {
    preset_path_for_name_at(&preset_storage_dir(), name)
}

/// Path-injectable variant of [`preset_path_for_name`].
pub fn preset_path_for_name_at(dir: &std::path::Path, name: &str) -> std::path::PathBuf {
    let sanitized = sanitize_preset_name(name);
    dir.join(format!("{}{}", sanitized, PRESET_FILE_SUFFIX))
}

/// Sanitize a preset name exactly as upstream `sanitize_preset_name`:
/// invalid characters (``<>:"/\|?*`` and control chars) collapse to a single
/// space, whitespace runs collapse, and leading/trailing spaces and dots are
/// stripped. Preserving this mapping keeps preset file names interchangeable
/// with the Python original.
pub fn sanitize_preset_name(name: &str) -> String {
    let mut cleaned = String::with_capacity(name.len());
    let mut last_was_space = false;
    for c in name.chars() {
        let invalid =
            matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (c as u32) < 0x20;
        let c = if invalid { ' ' } else { c };
        if c.is_whitespace() {
            if last_was_space {
                continue;
            }
            last_was_space = true;
            cleaned.push(' ');
        } else {
            last_was_space = false;
            cleaned.push(c);
        }
    }
    let cleaned = cleaned.trim();
    cleaned.trim_matches([' ', '.']).chars().take(100).collect()
}

pub fn ensure_preset_storage_dir() -> std::path::PathBuf {
    let dir = preset_storage_dir();
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// List stored preset names, de-duplicated and case-insensitively sorted
/// (upstream `list_preset_names`).
pub fn list_preset_names() -> Vec<String> {
    let dir = ensure_preset_storage_dir();
    let mut names: Vec<String> = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            let is_preset = path.is_file()
                && path.extension().and_then(|e| e.to_str()).is_some_and(|e| {
                    e.eq_ignore_ascii_case(PRESET_FILE_SUFFIX.trim_start_matches('.'))
                });
            if is_preset && let Some(stem) = path.file_stem() {
                let name = stem.to_string_lossy().to_string();
                if !names.contains(&name) {
                    names.push(name);
                }
            }
        }
    }
    names.sort_by_key(|n| n.to_lowercase());
    names
}

pub fn delete_preset_file(name: &str) -> anyhow::Result<()> {
    let preset_name = sanitize_preset_name(name);
    if preset_name.is_empty() {
        anyhow::bail!("preset name is empty");
    }
    let path = preset_path_for_name(&preset_name);
    if path.exists() {
        std::fs::remove_file(&path)?;
    }
    Ok(())
}

pub fn output_preset_links_path() -> std::path::PathBuf {
    app_config_file_path(OUTPUT_PRESET_LINKS_FILE)
}

/// Identity a link is stored under: the sink's PipeWire node name.
///
/// Upstream derives a richer identity from the route table
/// (`output_preset_target_identity`: route key, else an encoded route-device
/// identity, else the target's first key). The node name is the part that
/// actually distinguishes one output from another on this machine, and it is
/// stable across restarts -- which is all a per-output preset needs.
pub fn output_preset_key_for_sink(sink_name: &str) -> String {
    sink_name.trim().to_string()
}

/// The preset that belongs to an output sink: its own link if there is one, else
/// the fallback preset.
///
/// This is the reader the port never had. `set_output_preset_link` and
/// `set_output_preset_fallback_name` wrote the config, and nothing ever looked
/// it up, so per-output settings could be recorded but never applied.
pub fn output_preset_for_sink(sink_name: &str) -> Option<String> {
    output_preset_for_sink_at(&output_preset_links_path(), sink_name)
}

/// Path-injectable variant of [`output_preset_for_sink`].
pub fn output_preset_for_sink_at(path: &Path, sink_name: &str) -> Option<String> {
    let cfg = load_output_preset_config_at(path).ok()?;
    let key = output_preset_key_for_sink(sink_name);
    if let Some(name) = cfg.links.get(&key) {
        return Some(name.clone());
    }
    cfg.default_preset
}

/// Every output sink that has its own preset linked, as `(sink, preset)`.
///
/// For the D-Bus `output-presets` capability, which is advertised but has
/// nothing behind it.
pub fn output_preset_links() -> Vec<(String, String)> {
    let cfg = load_output_preset_config().unwrap_or_default();
    let mut out: Vec<(String, String)> = cfg.links.into_iter().collect();
    out.sort();
    out
}

/// The fallback preset name, if one is set.
pub fn output_preset_fallback() -> Option<String> {
    load_output_preset_config().ok()?.default_preset
}

/// The payload signature of the preset linked to `sink_name`, if any.
///
/// Used by auto-write-back to decide whether the live curve has moved away
/// from the linked preset. Comparing signatures rather than re-deriving the
/// "modified" state from the preset panel keeps the write-back independent of
/// which preset is selected in the panel.
pub fn output_preset_saved_signature_for_sink(sink_name: &str) -> Option<String> {
    output_preset_saved_signature_for_sink_at(&output_preset_links_path(), sink_name)
}

/// Path-injectable variant of [`output_preset_saved_signature_for_sink`].
pub fn output_preset_saved_signature_for_sink_at(path: &Path, sink_name: &str) -> Option<String> {
    output_preset_saved_signature_for_sink_at_at(path, &preset_storage_dir(), sink_name)
}

/// Path-injectable for both the links file and the preset storage dir, so
/// the saved-signature lookup can be exercised in a test directory
/// without touching the real config.
pub fn output_preset_saved_signature_for_sink_at_at(
    links_path: &Path,
    preset_dir: &std::path::Path,
    sink_name: &str,
) -> Option<String> {
    let preset_name = output_preset_for_sink_at(links_path, sink_name)?;
    let preset_path = preset_path_for_name_at(preset_dir, &preset_name);
    if !preset_path.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&preset_path).ok()?;
    let payload: serde_json::Value = serde_json::from_str(&data).ok()?;
    Some(preset_payload_state_signature(&payload))
}

/// How many sinks link to `preset_name`. Auto-write-back only writes into a
/// preset linked from exactly one device: writing into one linked from two
/// would silently change the curve for a device the user did not touch.
pub fn output_preset_link_count_for_preset(preset_name: &str) -> usize {
    load_output_preset_config()
        .ok()
        .map(|c| c.links.values().filter(|n| **n == *preset_name).count())
        .unwrap_or(0)
}

/// The sinks linked to `preset_name`, sorted. Used to report to the user
/// what a write-back would affect.
pub fn output_preset_linked_sinks_for_preset(preset_name: &str) -> Vec<String> {
    let cfg = load_output_preset_config().unwrap_or_default();
    let mut out: Vec<String> = cfg
        .links
        .iter()
        .filter(|(_, n)| n == &preset_name)
        .map(|(k, _)| k.clone())
        .collect();
    out.sort();
    out
}

/// The configured output routing mode. Missing or version-1 config reads as
/// [`OutputRoutingMode::Selected`], which is what the app did before the mode
/// existed.
pub fn output_routing_mode() -> OutputRoutingMode {
    output_routing_mode_at(&output_preset_links_path())
}

/// Path-injectable variant of [`output_routing_mode`].
pub fn output_routing_mode_at(path: &Path) -> OutputRoutingMode {
    load_output_preset_config_at(path)
        .ok()
        .map(|c| c.mode)
        .unwrap_or(OutputRoutingMode::Selected)
}

pub fn set_output_routing_mode(mode: OutputRoutingMode) -> anyhow::Result<()> {
    set_output_routing_mode_at(mode, &output_preset_links_path())
}

/// Path-injectable variant of [`set_output_routing_mode`].
pub fn set_output_routing_mode_at(mode: OutputRoutingMode, path: &Path) -> anyhow::Result<()> {
    let mut cfg = load_output_preset_config_at(path)?;
    cfg.mode = mode;
    write_output_preset_config_at(
        path,
        &cfg.links,
        cfg.default_preset.as_deref(),
        cfg.mode,
        cfg.monitor.as_deref(),
    )
}

/// The sink the monitor is pinned to, if any. `None` means "follow the EQ
/// output". Stored as the sink's node name.
pub fn output_monitor_sink() -> Option<String> {
    load_output_preset_config().ok()?.monitor
}

pub fn set_output_monitor_sink(sink: Option<&str>) -> anyhow::Result<()> {
    let mut cfg = load_output_preset_config()?;
    cfg.monitor = sink.map(|s| s.to_string());
    write_output_preset_config(&cfg)
}

pub fn load_output_preset_config() -> anyhow::Result<OutputPresetConfig> {
    load_output_preset_config_at(&output_preset_links_path())
}

/// Path-injectable variant of [`load_output_preset_config`].
pub fn load_output_preset_config_at(path: &Path) -> anyhow::Result<OutputPresetConfig> {
    if !path.exists() {
        return Ok(OutputPresetConfig::default());
    }
    let data = std::fs::read_to_string(path)?;
    let payload: serde_json::Value = serde_json::from_str(&data)?;
    let version = payload.get("version").and_then(|v| v.as_i64()).unwrap_or(0) as i32;
    if version > OUTPUT_PRESET_LINKS_VERSION {
        anyhow::bail!("unsupported output preset links version: {}", version);
    }

    let mut links = std::collections::HashMap::new();
    if let Some(links_obj) = payload.get("links").and_then(|v| v.as_object()) {
        for (key, value) in links_obj {
            // The old Link button keyed its entry "default" -- that is the
            // fallback, not a device. Reading it back as a sink would make
            // `output_preset_for_sink("default")` return a preset, which is
            // wrong: "default" is never a sink name. Drop it here and let the
            // explicit `default` field carry the fallback.
            if key == "default" {
                continue;
            }
            if let Some(preset_name) = value.as_str() {
                links.insert(key.clone(), preset_name.to_string());
            }
        }
    }
    let default_preset = payload
        .get("default")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    // Version 1 has no mode: default to Selected, which is what the app did
    // before the mode existed. Version 2 carries it explicitly.
    let mode = payload
        .get("mode")
        .and_then(|v| v.as_str())
        .map(OutputRoutingMode::from_mode_str)
        .unwrap_or(OutputRoutingMode::Selected);

    // The monitor was never persisted before; `None` means "follow the EQ
    // output", which is the existing behaviour.
    let monitor = payload
        .get("monitor")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string());

    Ok(OutputPresetConfig {
        links,
        default_preset,
        mode,
        monitor,
    })
}

/// Path-injectable variant of [`write_output_preset_config`].
pub fn write_output_preset_config_at(
    path: &Path,
    links: &std::collections::HashMap<String, String>,
    default_preset: Option<&str>,
    mode: OutputRoutingMode,
    monitor: Option<&str>,
) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut obj = serde_json::json!({
        "version": OUTPUT_PRESET_LINKS_VERSION,
        "links": links,
        "mode": mode.as_str(),
    });
    if let Some(default) = default_preset {
        obj.as_object_mut()
            .unwrap()
            .insert("default".to_string(), serde_json::json!(default));
    }
    if let Some(monitor) = monitor {
        obj.as_object_mut()
            .unwrap()
            .insert("monitor".to_string(), serde_json::json!(monitor));
    }
    let data = serde_json::to_string_pretty(&obj)?;
    std::fs::write(path, format!("{}\n", data))?;
    Ok(())
}

/// Write the whole config back from its parsed parts. Convenience for the
/// callers that already hold an `OutputPresetConfig`.
pub fn write_output_preset_config(cfg: &OutputPresetConfig) -> anyhow::Result<()> {
    write_output_preset_config_at(
        &output_preset_links_path(),
        &cfg.links,
        cfg.default_preset.as_deref(),
        cfg.mode,
        cfg.monitor.as_deref(),
    )
}

pub fn get_output_preset_fallback_name() -> anyhow::Result<Option<String>> {
    let cfg = load_output_preset_config()?;
    Ok(cfg.default_preset)
}

pub fn set_output_preset_fallback_name(name: &str) -> anyhow::Result<()> {
    let mut cfg = load_output_preset_config()?;
    let sanitized = sanitize_preset_name(name);
    if sanitized.is_empty() {
        anyhow::bail!("preset name is empty");
    }
    cfg.default_preset = Some(sanitized);
    write_output_preset_config(&cfg)
}

pub fn clear_output_preset_fallback_name() -> anyhow::Result<bool> {
    let mut cfg = load_output_preset_config()?;
    let had = cfg.default_preset.is_some();
    cfg.default_preset = None;
    write_output_preset_config(&cfg)?;
    Ok(had)
}

pub fn get_output_preset_link_match(output_keys: &[String]) -> Option<(String, String)> {
    let Ok(cfg) = load_output_preset_config() else {
        return None;
    };
    for key in output_keys {
        if let Some(preset) = cfg.links.get(key) {
            return Some((key.clone(), preset.clone()));
        }
    }
    None
}

pub fn set_output_preset_link(key: &str, preset_name: &str) -> anyhow::Result<()> {
    let mut cfg = load_output_preset_config()?;
    cfg.links.insert(key.to_string(), preset_name.to_string());
    write_output_preset_config(&cfg)
}

pub fn clear_output_preset_link(keys: &[String]) -> anyhow::Result<bool> {
    let mut cfg = load_output_preset_config()?;
    let mut removed = false;
    for key in keys {
        if cfg.links.remove(key).is_some() {
            removed = true;
        }
    }
    write_output_preset_config(&cfg)?;
    Ok(removed)
}

/// Write the current curve into the preset linked to `sink_name`, if exactly
/// one device links to it.
///
/// Auto-write-back is the reason the link exists: editing the EQ while a
/// device is linked should not require the user to remember to press Update.
/// It only writes when the linked preset is linked from exactly one device —
/// writing into one linked from two would silently change the curve for a
/// device the user did not touch, and that is worse than leaving it Modified.
///
/// Returns `Ok(true)` when it wrote, `Ok(false)` when the sink is not linked,
/// when the preset is a built-in, or when it is linked from more than one
/// device (so the caller can leave the state chip alone).
/// Path-injectable variant of [`auto_write_output_preset_for_sink`].
///
/// Takes both the links file and the preset storage directory, so the
/// write-back can be exercised in a test directory without touching the
/// real config.
pub fn auto_write_output_preset_for_sink_at(
    links_path: &Path,
    preset_dir: &std::path::Path,
    sink_name: &str,
    bands: &[EqBand],
    preamp_db: f64,
) -> anyhow::Result<bool> {
    let Some(preset_name) = output_preset_for_sink_at(links_path, sink_name) else {
        return Ok(false);
    };
    if is_builtin_preset(&preset_name) {
        return Ok(false);
    }
    let count = load_output_preset_config_at(links_path)
        .ok()
        .map(|c| c.links.values().filter(|n| **n == *preset_name).count())
        .unwrap_or(0);
    if count != 1 {
        return Ok(false);
    }
    let dest = preset_path_for_name_at(preset_dir, &preset_name);
    save_preset_to_file(&dest, bands, preamp_db)?;
    log::info!(
        "Auto write-back: wrote the current curve into preset '{preset_name}' (linked to {sink_name})"
    );
    Ok(true)
}

/// Write the current curve into the preset linked to `sink_name`, if exactly
/// one device links to it.
///
/// Auto-write-back is the reason the link exists: editing the EQ while a
/// device is linked should not require the user to remember to press Update.
/// It only writes when the linked preset is linked from exactly one device —
/// writing into one linked from two would silently change the curve for a
/// device the user did not touch, and that is worse than leaving it Modified.
///
/// Returns `Ok(true)` when it wrote, `Ok(false)` when the sink is not linked,
/// when the preset is a built-in, or when it is linked from more than one
/// device (so the caller can leave the state chip alone).
pub fn auto_write_output_preset_for_sink(
    sink_name: &str,
    bands: &[EqBand],
    preamp_db: f64,
) -> anyhow::Result<bool> {
    auto_write_output_preset_for_sink_at(
        &output_preset_links_path(),
        &preset_storage_dir(),
        sink_name,
        bands,
        preamp_db,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Count local extrema in a response — the direct measure of "waves".
    fn count_extrema(vals: &[f64]) -> usize {
        let mut n = 0usize;
        let mut prev = 0.0f64;
        for i in 1..vals.len() {
            let d = vals[i] - vals[i - 1];
            if d.abs() < 1e-4 {
                continue;
            }
            if prev != 0.0 && d * prev < 0.0 {
                n += 1;
            }
            prev = d;
        }
        n
    }

    #[test]
    fn test_band_spacing_and_width_bounds() {
        let bands = default_bands();
        let sp = band_spacing_oct(&bands);
        // Stock layout is 10 log-spaced bands ~= 1 octave apart.
        assert!(
            (sp - 1.0).abs() < 0.05,
            "stock spacing should be ~1.0 oct, got {sp:.3}"
        );

        let (lo, hi, def) = smooth_spread_bounds_bands();
        assert!(lo > 0.4 && lo < 0.5, "lower bound {lo:.3}");
        assert!(hi > 2.9 && hi < 3.1, "upper bound {hi:.3}");
        assert!(def > 0.75 && def < 0.85, "default {def:.3}");
        assert!(lo < def && def < hi);
    }

    #[test]
    fn test_smooth_kernel_weights_gaussian_falloff() {
        // Narrow sigma stays local; wide sigma reaches far.
        let narrow = smooth_kernel_weights(4, 6.0, 1.0, 10);
        let wide = smooth_kernel_weights(4, 6.0, 3.0, 10);
        assert!(
            wide.len() > narrow.len(),
            "wide sigma must involve more bands: {} vs {}",
            wide.len(),
            narrow.len()
        );
        // The dragged band itself is never in the kernel.
        assert!(!narrow.iter().any(|(i, _)| *i == 4));
        // Symmetric about the centre, and monotone with distance.
        let center = 4usize;
        for (i, w) in &wide {
            let mirror = center as i64 - (*i as i64 - center as i64);
            if mirror >= 0 && (mirror as usize) < 10 {
                if let Some((_, mw)) = wide.iter().find(|(j, _)| *j as i64 == mirror) {
                    assert!((w - mw).abs() < 1e-12, "kernel must be symmetric at {i}");
                }
            }
            let k = *i as i64 - center as i64;
            let expected = 6.0 * (-(k * k) as f64 / (2.0 * 3.0 * 3.0)).exp();
            assert!(
                (w - expected).abs() < 1e-9,
                "kernel value mismatch at k={k}"
            );
        }
        // Degenerate inputs produce no coupling rather than NaN/panic.
        assert!(smooth_kernel_weights(0, 6.0, 0.0, 10).is_empty());
        assert!(smooth_kernel_weights(0, f64::NAN, 1.0, 10).is_empty());
        assert!(smooth_kernel_weights(0, 6.0, 1.0, 0).is_empty());
    }

    #[test]
    fn test_min_spread_moves_only_the_dragged_band() {
        // The user's explicit requirement: at minimum spread, dragging one
        // band must move NOTHING else.
        let k = smooth_kernel_weights(4, 6.0, SMOOTH_SPREAD_MIN_BANDS, 10);
        assert!(
            k.is_empty(),
            "min spread must couple to zero neighbours, got {:?}",
            k
        );
    }

    #[test]
    fn test_spread_scales_participating_bands_without_waves() {
        // The user-visible contract: turning the knob changes HOW MANY bands
        // move, and never introduces U-shaped troughs.
        let bands = default_bands();
        let spacing = band_spacing_oct(&bands);
        let freqs = stepped_response_frequencies(SAMPLE_RATE, 1501);

        let simulate = |sigma: f64| -> (usize, usize, f64) {
            let q = smooth_bell_q(sigma, spacing);
            let center = 2usize;
            let mut bs = default_bands();
            bs[center].filter_type = FilterType::Sin;
            bs[center].gain_db = 6.0;
            bs[center].q = q;
            let mut involved = 1usize;
            for (i, w) in smooth_kernel_weights(center, 6.0, sigma, bs.len()) {
                bs[i].filter_type = FilterType::Sin;
                bs[i].gain_db = w;
                bs[i].q = q;
                involved += 1;
            }
            let resp = total_response_db_at_frequencies(&bs, 0.0, SAMPLE_RATE, &freqs);
            let peak_hz = freqs[resp
                .iter()
                .enumerate()
                .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
                .unwrap()
                .0];
            (involved, count_extrema(&resp), peak_hz)
        };

        let (inv_lo, ex_lo, peak_lo) = simulate(SMOOTH_SPREAD_MIN_BANDS);
        let (inv_def, ex_def, _) = simulate(SMOOTH_SPREAD_DEFAULT_BANDS);
        let (inv_hi, ex_hi, _) = simulate(SMOOTH_SPREAD_MAX_BANDS);

        // Minimum must be exactly one band.
        assert_eq!(inv_lo, 1, "min spread must involve only the dragged band");
        // And the knob must still scale upward.
        assert!(
            inv_lo < inv_def && inv_def < inv_hi,
            "participating bands must grow with spread: {inv_lo} < {inv_def} < {inv_hi}"
        );

        // No setting may produce waves.
        for (label, e, sig) in [
            ("min", ex_lo, SMOOTH_SPREAD_MIN_BANDS),
            ("default", ex_def, SMOOTH_SPREAD_DEFAULT_BANDS),
            ("max", ex_hi, SMOOTH_SPREAD_MAX_BANDS),
        ] {
            assert!(
                e <= 2,
                "{label} spread {sig:.2} gave {e} extrema (want <= 2)"
            );
        }

        // At minimum spread the peak must sit on the dragged band's own
        // frequency (single bell => peak at f0), within grid resolution.
        let f0 = default_bands()[2].frequency;
        assert!(
            (peak_lo - f0).abs() / f0 < 0.01,
            "peak should be at the band frequency {f0:.1} Hz, got {peak_lo:.1} Hz"
        );
    }

    #[test]
    fn test_q_for_smooth_width() {
        // The bridging width is the floor for the bell Q.
        let q = q_for_smooth_width(SMOOTH_BRIDGE_WIDTH_OCT);
        assert!(
            q > 1.0 && q < 1.15,
            "bridge width should give Q ~1.07, got {q:.4}"
        );

        // Wider curve => lower Q, strictly monotonic across the range.
        let mut prev = f64::INFINITY;
        let mut w = SMOOTH_WIDTH_MIN_OCT;
        while w <= SMOOTH_WIDTH_MAX_OCT + 1e-9 {
            let q = q_for_smooth_width(w);
            assert!(q < prev, "width {w} must give a lower Q than the previous");
            prev = q;
            w += 0.1;
        }

        // Out-of-range widths clamp instead of extrapolating.
        assert_eq!(
            q_for_smooth_width(0.0),
            q_for_smooth_width(SMOOTH_WIDTH_MIN_OCT)
        );
        assert_eq!(
            q_for_smooth_width(99.0),
            q_for_smooth_width(SMOOTH_WIDTH_MAX_OCT)
        );

        // Non-finite input falls back to the default, never NaN.
        let d = q_for_smooth_width(f64::NAN);
        assert!(d.is_finite());
        assert!((d - q_for_smooth_width(SMOOTH_BRIDGE_WIDTH_OCT)).abs() < 1e-12);
    }

    #[test]
    fn test_smooth_effective_band_override() {
        let mut b = default_bands();
        b[3].filter_type = FilterType::HiShelf;
        b[3].q = 4.0;

        let e = smooth_effective_band(&b[3], SMOOTH_BELL_Q);
        assert_eq!(e.filter_type, FilterType::Sin);
        // The override must produce a Q in the blended region for the
        // stock layout, and it must come from the width mapping.
        assert!(
            (e.q - SMOOTH_BELL_Q).abs() < 1e-12,
            "Q must pass through unchanged"
        );
        assert!((e.q - SMOOTH_BELL_Q).abs() < 1e-12);
        // The original is untouched, so switching Smooth off restores it.
        assert_eq!(b[3].filter_type, FilterType::HiShelf);
        assert!((b[3].q - 4.0).abs() < 1e-12);

        // An `Off` band must stay `Off` — Smooth cannot switch a bypassed
        // band on.
        b[5].filter_type = FilterType::Off;
        assert_eq!(
            smooth_effective_band(&b[5], SMOOTH_BELL_Q).filter_type,
            FilterType::Off
        );

        // `Sin` is the same peaking math as `Bell`: at equal Q the
        // coefficients are identical. The smoothness is purely the wide Q.
        let mut bell = b[3].clone();
        bell.filter_type = FilterType::Bell;
        bell.q = SMOOTH_BELL_Q;
        let mut sin = b[3].clone();
        sin.filter_type = FilterType::Sin;
        sin.q = SMOOTH_BELL_Q;
        let bc = band_biquad_coefficients(&bell, SAMPLE_RATE, false);
        let sc = band_biquad_coefficients(&sin, SAMPLE_RATE, false);
        for (x, y) in bc.as_array().iter().zip(sc.as_array().iter()) {
            assert!((x - y).abs() < 1e-12, "Sin must match Bell at equal Q");
        }
    }

    #[test]
    fn test_sin_is_wider_and_smoother_than_default_bell() {
        // Coupled gain profile: band 5 +6, +-1 +3, +-2 +1.5.
        let prof: [(usize, f64); 5] = [(5, 6.0), (4, 3.0), (6, 3.0), (3, 1.5), (7, 1.5)];
        let freqs = stepped_response_frequencies(SAMPLE_RATE, 801);

        fn waviness(vals: &[f64]) -> f64 {
            let w = 80usize;
            let dev: Vec<f64> = (0..vals.len())
                .map(|i| {
                    let a = i.saturating_sub(w);
                    let b = (i + w + 1).min(vals.len());
                    let mean = vals[a..b].iter().sum::<f64>() / (b - a) as f64;
                    vals[i] - mean
                })
                .collect();
            let mx = dev.iter().fold(f64::NEG_INFINITY, |a, v| a.max(*v));
            let mn = dev.iter().fold(f64::INFINITY, |a, v| a.min(*v));
            mx - mn
        }

        let sum_with = |q: f64| -> Vec<f64> {
            let mut bands = default_bands();
            for (i, g) in prof {
                bands[i].filter_type = FilterType::Bell;
                bands[i].gain_db = g;
                bands[i].q = q;
            }
            total_response_db_at_frequencies(&bands, 0.0, SAMPLE_RATE, &freqs)
        };

        let default_q = 1.5;
        let w_default = waviness(&sum_with(default_q));
        let w_sin = waviness(&sum_with(SMOOTH_BELL_Q));

        assert!(
            w_sin < w_default,
            "Sin must be smoother: sin {w_sin:.2} vs default {w_default:.2}"
        );
        // And it should land near the ideal-Gaussian regime (~1.1 dB).
        assert!(w_sin < 1.5, "Sin waviness {w_sin:.2} should be ~1.1 dB");
    }

    #[test]
    fn test_smooth_neighbor_deltas() {
        // Middle band: both sides coupled, symmetric, geometric decay.
        let d = smooth_neighbor_deltas(5, 8.0, 10);
        assert_eq!(d.len(), 4);
        assert_eq!(d, vec![(3, 2.0), (4, 4.0), (6, 4.0), (7, 2.0)]);

        // Symmetry: a +delta and a -delta couple the same neighbours.
        let up = smooth_neighbor_deltas(5, 6.0, 10);
        let down = smooth_neighbor_deltas(5, -6.0, 10);
        assert_eq!(up.len(), down.len());
        for ((iu, wu), (id, wd)) in up.iter().zip(down.iter()) {
            assert_eq!(iu, id);
            assert!((wu + wd).abs() < 1e-12);
        }

        // Edges drop out-of-range neighbours, never wrap around.
        let first = smooth_neighbor_deltas(0, 8.0, 10);
        assert_eq!(first, vec![(1, 4.0), (2, 2.0)]);
        let last = smooth_neighbor_deltas(9, 8.0, 10);
        assert_eq!(last, vec![(7, 2.0), (8, 4.0)]);

        // A single-band row has nothing to couple.
        assert!(smooth_neighbor_deltas(0, 8.0, 1).is_empty());

        // Zero delta couples nothing.
        assert!(
            smooth_neighbor_deltas(5, 0.0, 10)
                .iter()
                .all(|(_, w)| *w == 0.0)
        );
    }

    #[test]
    fn test_smooth_coupling_shape_and_cap() {
        // Mirrors the coupling loop in `window::gain_request`: apply the
        // dragged band's post-cap delta to neighbours, re-clamping each
        // neighbour against a fresh snapshot.
        fn coupled_drag(bands: &mut [EqBand], dragged: usize, want: f64) {
            let delta_cap =
                clamp_gain_for_peak(bands, dragged, want, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
            bands[dragged].gain_db = delta_cap;
            let delta = delta_cap;
            for (other, weighted) in smooth_neighbor_deltas(dragged, delta, bands.len()) {
                let cur = bands.to_vec();
                let w = cur[other].gain_db + weighted;
                bands[other].gain_db =
                    clamp_gain_for_peak(&cur, other, w, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
            }
        }

        let mut b = default_bands();
        for band in b.iter_mut() {
            band.filter_type = FilterType::Bell;
        }
        coupled_drag(&mut b, 5, 6.0);

        // Neighbours moved by the expected decaying fractions.
        assert!((b[5].gain_db - 6.0).abs() < 0.11, "self: {}", b[5].gain_db);
        assert!((b[4].gain_db - 3.0).abs() < 0.11, "n-1: {}", b[4].gain_db);
        assert!((b[6].gain_db - 3.0).abs() < 0.11, "n+1: {}", b[6].gain_db);
        assert!((b[3].gain_db - 1.5).abs() < 0.11, "n-2: {}", b[3].gain_db);
        assert!((b[7].gain_db - 1.5).abs() < 0.11, "n+2: {}", b[7].gain_db);

        // The gain profile is smooth: it decays monotonically away from the
        // dragged band on both sides (no isolated hump, no rebound).
        for i in [3usize, 4] {
            assert!(
                b[i].gain_db <= b[i + 1].gain_db + 1e-9,
                "left side not monotonic"
            );
        }
        for i in [6usize, 7] {
            assert!(
                b[i].gain_db <= b[i - 1].gain_db + 1e-9,
                "right side not monotonic"
            );
        }

        // And the whole thing still fits the headroom budget.
        let raw = estimate_response_peak_db(&b, 0.0, SAMPLE_RATE);
        assert!(raw <= MAX_SAFE_RAW_PEAK_DB + 0.1, "coupled stack: {raw:.2}");
        assert!(
            -1.0 - raw >= EQ_PREAMP_MIN_DB,
            "Set Safe must stay reachable: {:.1}",
            -1.0 - raw
        );
    }

    #[test]
    fn test_preamp_budget_invariant() {
        // Auto-Safe's only lever is the preamp floor, so the floor must be
        // able to pull a capped curve down to the target. If this fails, a
        // capped stack saturates the preamp and Set Safe stops working.
        let required = crate::window_headroom::AUTO_SAFE_TARGET_DBFS - MAX_SAFE_RAW_PEAK_DB;
        assert!(
            EQ_PREAMP_MIN_DB <= required,
            "preamp floor {EQ_PREAMP_MIN_DB:.1} must reach target {:.1} minus cap {MAX_SAFE_RAW_PEAK_DB:.1} = {required:.1}",
            crate::window_headroom::AUTO_SAFE_TARGET_DBFS,
        );
    }

    #[test]
    fn test_clamp_gain_for_peak() {
        // A single band is essentially unrestricted; a max-gain shelf
        // overshoots to ~23.5 dB so the last ~0.5 dB is trimmed to fit the
        // 23 dB cap (Auto-Safe can then always reach -1 dBFS).
        let mut bands = default_bands();
        bands[3].filter_type = FilterType::HiShelf;
        let out = clamp_gain_for_peak(&bands, 3, 20.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!(
            out > 19.0 && out <= 20.0,
            "single shelf may lose at most ~1 dB: {out}"
        );
        bands[3].gain_db = out;
        let peak = estimate_response_peak_db(&bands, 0.0, SAMPLE_RATE);
        assert!(peak <= MAX_SAFE_RAW_PEAK_DB + 0.1, "{peak:.2}");
        bands[3].gain_db = 0.0;

        // Stacking: three shelves already near the cap; a fourth proposed at
        // +20 must be pulled down so the raw peak stays <= 23 dB.
        for i in 4..=6 {
            bands[i].filter_type = FilterType::HiShelf;
            bands[i].gain_db = 6.0;
        }
        let out = clamp_gain_for_peak(&bands, 7, 20.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!(
            out < 20.0 && out > 0.0,
            "stacked shelves must clamp the fourth: {out}"
        );
        bands[7].gain_db = out;
        let peak = estimate_response_peak_db(&bands, 0.0, SAMPLE_RATE);
        assert!(
            peak <= MAX_SAFE_RAW_PEAK_DB + 0.1,
            "peak after clamp must fit the cap, got {peak:.2}"
        );

        // A proposal that already fits is returned unchanged; cuts always pass.
        let out = clamp_gain_for_peak(&bands, 7, 1.5, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!(
            (out - 1.5).abs() < 1e-9,
            "fits-through must be a no-op: {out}"
        );
        let out = clamp_gain_for_peak(&bands, 7, -20.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!((out + 20.0).abs() < 1e-9, "cuts must pass: {out}");

        // Muted bands contribute nothing, so they don't restrict others.
        let mut m = default_bands();
        for i in 0..6 {
            m[i].filter_type = FilterType::HiShelf;
            m[i].gain_db = 20.0;
            m[i].mute = true;
        }
        m[7].filter_type = FilterType::HiShelf;
        let out = clamp_gain_for_peak(&m, 7, 10.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!(
            (out - 10.0).abs() < 1e-9,
            "muted stacks must not clamp: {out}"
        );
    }

    #[test]
    fn test_clamp_gain_for_peak_observes_current_line() {
        // Hot preset: current curve peak well above the safe cap. The cap
        // observes the current line, so an edit may not raise the peak above
        // the status quo, but is not clamped to the 23 dB safe cap.
        let mut hot = default_bands();
        for i in 4..=7 {
            hot[i].filter_type = FilterType::HiShelf;
            hot[i].gain_db = 12.0;
        }
        let base = estimate_response_peak_db(&hot, 0.0, SAMPLE_RATE);
        assert!(
            base > MAX_SAFE_RAW_PEAK_DB + 20.0,
            "preset should run hot: {base:.1}"
        );

        // Band 7 sits at 12 dB; proposing +20 would raise the peak, so it
        // clamps back to ~its current value (never above the status quo).
        let out = clamp_gain_for_peak(&hot, 7, 20.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!(
            out > 11.0 && out <= 12.0,
            "must not raise peak above status quo: {out}"
        );

        // A cut still passes untouched.
        let cut = clamp_gain_for_peak(&hot, 7, -5.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
        assert!((cut + 5.0).abs() < 1e-9, "cuts must pass: {cut}");
    }

    #[test]
    fn test_set_safe_can_fix_two_shelf_stack() {
        // Regression: two Hi-Shelves driven to max stack their plateaus in the
        // overlap region. Measured live this reached ~+59 dB raw, which needs a
        // ~-60 dB preamp to be safe -- far below the -24 dB floor, so both
        // Set Safe and Auto-Safe were unable to fix it.
        let mut hot = default_bands();
        hot[6].filter_type = FilterType::HiShelf;
        hot[6].gain_db = 20.0;
        hot[7].filter_type = FilterType::HiShelf;
        hot[7].gain_db = 20.0;
        let raw = estimate_response_peak_db(&hot, 0.0, SAMPLE_RATE);
        assert!(raw > 30.0, "two maxed shelves must stack hot: {raw:.1}");

        // Uncapped, the preamp Set Safe asks for is unreachable.
        let needed = 0.0 - raw - 1.0;
        assert!(
            needed < EQ_PREAMP_MIN_DB,
            "uncapped stack must exceed the preamp floor: {needed:.1}"
        );

        // With the cap applied the way the gain path applies it, the raw peak
        // stays inside what the preamp floor can always absorb, so Set Safe
        // lands on target instead of saturating.
        let mut capped = default_bands();
        capped[6].filter_type = FilterType::HiShelf;
        capped[7].filter_type = FilterType::HiShelf;
        for i in [6usize, 7] {
            let g = clamp_gain_for_peak(&capped, i, 20.0, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
            capped[i].gain_db = g;
        }
        let capped_raw = estimate_response_peak_db(&capped, 0.0, SAMPLE_RATE);
        assert!(
            capped_raw <= MAX_SAFE_RAW_PEAK_DB + 0.1,
            "capped stack must fit the cap: {capped_raw:.2}"
        );
        let needed = 0.0 - capped_raw - 1.0;
        assert!(
            needed >= EQ_PREAMP_MIN_DB,
            "Set Safe must be reachable for a capped stack: {needed:.1}"
        );
        let after = estimate_response_peak_db(&capped, needed, SAMPLE_RATE);
        assert!(
            (after - (-1.0)).abs() < 0.2,
            "Set Safe must land near the -1 dBFS target: {after:.2}"
        );
    }

    #[test]
    fn test_user_shelf_stack_is_not_a_dead_fader() {
        // Acceptance test for the reported scenario: bands 7 and 8 set to
        // Hi-shelf, then band 9 (bell) +12, then band 8 +12, then band 7 +20.
        // With the old 23 dB cap the last step resolved to +0.0 dB — the fader
        // refused to move at all. With the widened budget every step must yield
        // a usable, monotonically increasing raw peak that Set Safe can absorb.
        let mut b = default_bands();
        b[6].filter_type = FilterType::HiShelf;
        b[7].filter_type = FilterType::HiShelf;

        let apply = |b: &mut Vec<EqBand>, i: usize, want: f64| -> f64 {
            let got = clamp_gain_for_peak(b, i, want, SAMPLE_RATE, MAX_SAFE_RAW_PEAK_DB);
            b[i].gain_db = got;
            got
        };

        let g9 = apply(&mut b, 8, 12.0);
        let raw9 = estimate_response_peak_db(&b, 0.0, SAMPLE_RATE);
        let g8 = apply(&mut b, 7, 12.0);
        let raw8 = estimate_response_peak_db(&b, 0.0, SAMPLE_RATE);
        let g7 = apply(&mut b, 6, 20.0);
        let raw7 = estimate_response_peak_db(&b, 0.0, SAMPLE_RATE);

        // No step may be refused outright.
        assert!(g9 > 0.0, "band 9 must accept its boost: {g9}");
        assert!(g8 > 0.0, "band 8 must accept its boost: {g8}");
        assert!(
            g7 > 1.0,
            "band 7 must not be a dead fader (old cap gave 0.0): {g7}"
        );

        // Each step must actually add response, not stall.
        assert!(raw8 > raw9, "raw peak must grow: {raw9} -> {raw8}");
        assert!(raw7 > raw8, "raw peak must grow: {raw8} -> {raw7}");

        // And the result must stay inside the cap with a reachable preamp.
        assert!(
            raw7 <= MAX_SAFE_RAW_PEAK_DB + 0.1,
            "stack must fit the cap: {raw7:.2}"
        );
        let needed = -1.0 - raw7;
        assert!(
            needed >= EQ_PREAMP_MIN_DB,
            "Set Safe must be reachable: {needed:.1} vs floor {EQ_PREAMP_MIN_DB:.1}"
        );
    }

    #[test]
    fn test_estimate_response_peak_db_is_unclamped() {
        // Stacked Hi-shelf boosts sum well past the graph-display clamp
        // (GRAPH_DB_MAX + 12 = +36 dB). The estimate must report the true
        // peak so Auto-Safe / headroom see reality (upstream clamp_output=False).
        let mut m = default_bands();
        for i in 6..=9 {
            m[i].filter_type = FilterType::HiShelf;
            m[i].gain_db = 20.0;
        }
        let raw = estimate_response_peak_db(&m, 0.0, SAMPLE_RATE);
        assert!(
            raw > GRAPH_DB_MAX + 12.0,
            "expected true peak above the +36 dB display clamp, got {raw:.1}"
        );

        // A single +20 dB shelf stays near its true peak (23.5 dB measured).
        let mut b = default_bands();
        b[5].filter_type = FilterType::HiShelf;
        b[5].frequency = 1000.0;
        b[5].gain_db = 20.0;
        let single = estimate_response_peak_db(&b, 0.0, SAMPLE_RATE);
        assert!(
            single > 20.0 && single < 30.0,
            "single shelf peak {single:.1}"
        );

        // Auto-Safe with the true peak clamps at the preamp floor; the residual
        // over-target is expected (preamp range cannot absorb +84 dB of boost),
        // but the caller now knows the real number.
        let desired = crate::window_headroom::auto_safe_preamp_db(
            raw,
            crate::window_headroom::AUTO_SAFE_TARGET_DBFS,
        );
        assert_eq!(desired, EQ_PREAMP_MIN_DB);
        let after = estimate_response_peak_db(&m, desired, SAMPLE_RATE);
        assert!(
            after > GRAPH_DB_MAX + 12.0,
            "peak after floor-clamped preamp: {after:.1}"
        );
    }

    #[test]
    fn test_identity_coefficients() {
        let coeffs = BiquadCoefficients::identity();
        assert!((coeffs.b0 - 1.0).abs() < 1e-12);
        assert!((coeffs.b1).abs() < 1e-12);
        assert!((coeffs.b2).abs() < 1e-12);
        assert!((coeffs.a0 - 1.0).abs() < 1e-12);
        assert!((coeffs.a1).abs() < 1e-12);
        assert!((coeffs.a2).abs() < 1e-12);
    }

    #[test]
    fn test_shelf_biquad_matches_upstream_reference_values() {
        // Reference values produced by the upstream Python
        // `band_biquad_coefficients` (1 kHz, +6 dB, Q=1, 48 kHz), a0-normalised.
        let cases = [
            (
                FilterType::LoShelf,
                [
                    1.0243599982,
                    -1.8785520980,
                    0.8771300926,
                    1.0,
                    -1.8842729798,
                    0.8957692090,
                ],
            ),
            (
                FilterType::HiShelf,
                [
                    1.9478135796,
                    -3.6702124978,
                    1.7447914294,
                    1.0,
                    -1.8338788134,
                    0.8562713246,
                ],
            ),
        ];

        for (filter_type, expected) in cases {
            let mut band = EqBand::new(0);
            band.filter_type = filter_type;
            band.frequency = 1000.0;
            band.gain_db = 6.0;
            band.q = 1.0;
            let c = band_biquad_coefficients(&band, 48000.0, false);
            let actual = [c.b0, c.b1, c.b2, c.a0, c.a1, c.a2];

            for (i, (got, want)) in actual.iter().zip(expected.iter()).enumerate() {
                assert!(
                    (got - want).abs() < 1e-8,
                    "{:?} coeff[{}]: got {} want {}",
                    filter_type,
                    i,
                    got,
                    want
                );
            }
        }
    }

    #[test]
    fn test_biquad_matches_upstream_reference_values() {
        // Bell @ 1 kHz, +6 dB, Q=1, 48 kHz — computed from the upstream Python
        // `band_biquad_coefficients` formula; values are a0-normalised.
        let mut band = EqBand::new(0);
        band.filter_type = FilterType::Bell;
        band.frequency = 1000.0;
        band.gain_db = 6.0;
        band.q = 1.0;
        let c = band_biquad_coefficients(&band, 48000.0, false);

        let a = 10.0_f64.powf(6.0 / 40.0);
        let omega = 2.0 * PI * 1000.0 / 48000.0;
        let cos_w0 = omega.cos();
        let alpha = omega.sin() / 2.0;
        let b0 = 1.0 + alpha * a;
        let b1 = -2.0 * cos_w0;
        let b2 = 1.0 - alpha * a;
        let a0 = 1.0 + alpha / a;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha / a;

        assert!((c.b0 - b0 / a0).abs() < 1e-12);
        assert!((c.b1 - b1 / a0).abs() < 1e-12);
        assert!((c.b2 - b2 / a0).abs() < 1e-12);
        assert!((c.a0 - 1.0).abs() < 1e-12);
        assert!((c.a1 - a1 / a0).abs() < 1e-12);
        assert!((c.a2 - a2 / a0).abs() < 1e-12);
    }

    #[test]
    fn test_biquad_clamps_frequency_below_nyquist() {
        // Upstream clamps centre to `min(20000, sample_rate/2 - 1)`, so at an
        // 8 kHz sample rate a 20 kHz request must land at 3999 Hz, not 20000.
        let mut band = EqBand::new(0);
        band.filter_type = FilterType::Bell;
        band.frequency = 20000.0;
        band.gain_db = 6.0;
        band.q = 1.0;

        let at_nyquist = band_biquad_coefficients(&band, 8000.0, false);
        band.frequency = 3999.0;
        let at_limit = band_biquad_coefficients(&band, 8000.0, false);
        assert!((at_nyquist.b0 - at_limit.b0).abs() < 1e-12);
        assert!((at_nyquist.a1 - at_limit.a1).abs() < 1e-12);

        // The clamped result must differ from an unclamped 20 kHz evaluation.
        band.frequency = 20000.0;
        let high_rate = band_biquad_coefficients(&band, 48000.0, false);
        assert!((at_nyquist.b0 - high_rate.b0).abs() > 1e-6);
    }

    #[test]
    fn test_biquad_q_floor_matches_upstream() {
        // Upstream uses `max(band.q, 0.0001)`; a Q below the UI minimum must not
        // be raised to EQ_Q_MIN.
        let mut band = EqBand::new(0);
        band.filter_type = FilterType::Bell;
        band.frequency = 1000.0;
        band.gain_db = 6.0;
        band.q = 0.0;

        let floored = band_biquad_coefficients(&band, 48000.0, false);
        band.q = 0.0001;
        let explicit = band_biquad_coefficients(&band, 48000.0, false);
        assert!((floored.b0 - explicit.b0).abs() < 1e-12);

        band.q = EQ_Q_MIN;
        let ui_min = band_biquad_coefficients(&band, 48000.0, false);
        assert!((floored.b0 - ui_min.b0).abs() > 1e-6);
    }

    #[test]
    fn test_default_bands_count() {
        // Upstream `default_eq_bands()` returns all MAX_BANDS bands, with the
        // first DEFAULT_ACTIVE_BANDS enabled.
        let bands = default_bands();
        assert_eq!(bands.len(), MAX_BANDS);
        assert_eq!(
            bands
                .iter()
                .filter(|b| b.filter_type == FilterType::Bell)
                .count(),
            DEFAULT_ACTIVE_BANDS
        );
        assert!(
            bands[..DEFAULT_ACTIVE_BANDS]
                .iter()
                .all(|b| b.filter_type == FilterType::Bell && !b.mute)
        );
        assert!(
            bands[DEFAULT_ACTIVE_BANDS..]
                .iter()
                .all(|b| b.filter_type == FilterType::Off)
        );
    }

    #[test]
    fn test_band_coefficients_bell() {
        let mut band = EqBand::new(0);
        band.frequency = 1000.0;
        band.gain_db = 6.0;
        band.q = 1.0;
        band.filter_type = FilterType::Bell;
        band.mute = false;
        let coeffs = band_biquad_coefficients(&band, SAMPLE_RATE, false);
        assert!(!coeffs.is_identity());
    }

    #[test]
    fn test_filter_type_names() {
        assert_eq!(FilterType::Bell.name(), "Bell");
        assert_eq!(FilterType::HiPass.name(), "Hi-pass");
        assert_eq!(FilterType::LoPass.name(), "Lo-pass");
    }

    /// Pins the combo index mapping to upstream `FILTER_TYPE_INDEX_BY_VALUE`,
    /// which is keyed by filter-type *value* rather than by enum position:
    /// `Resonance` (7) is not selectable, so `Allpass` and `Bandpass` shift down.
    #[test]
    fn test_filter_type_combo_index_matches_upstream() {
        let expected = [
            (FilterType::Off, 0),
            (FilterType::Bell, 1),
            (FilterType::HiPass, 2),
            (FilterType::HiShelf, 3),
            (FilterType::LoPass, 4),
            (FilterType::LoShelf, 5),
            (FilterType::Notch, 6),
            (FilterType::Allpass, 7),
            (FilterType::Bandpass, 8),
        ];
        for (filter_type, index) in expected {
            assert_eq!(filter_type_combo_index(filter_type), index);
            assert_eq!(filter_type_from_combo_index(index), filter_type);
        }
    }

    /// `Resonance` is not selectable upstream, so it must not resolve to a combo
    /// index rather than silently landing on a neighbouring type.
    #[test]
    fn test_unselectable_filter_type_falls_back_to_off() {
        assert_eq!(filter_type_combo_index(FilterType::Resonance), 0);
        assert_eq!(filter_type_from_combo_index(99), FilterType::Off);
    }

    #[test]
    fn test_format_frequency_above_khz() {
        assert_eq!(format_frequency(1000.0), "1.0k");
        assert_eq!(format_frequency(2000.0), "2.0k");
    }

    #[test]
    fn test_format_frequency_below_khz() {
        assert_eq!(format_frequency(500.0), "500");
        assert_eq!(format_frequency(999.9), "1000");
    }

    #[test]
    fn test_total_response_db_neutral() {
        let bands = default_bands();
        let db = total_response_db(&bands, 0.0, SAMPLE_RATE, 1000.0);
        assert!((db - 0.0).abs() < 1.0);
    }

    #[test]
    fn test_total_response_db_with_bell() {
        let mut bands = default_bands();
        bands[0].mute = false;
        bands[0].filter_type = FilterType::Bell;
        bands[0].frequency = 1000.0;
        bands[0].gain_db = 6.0;
        let db = total_response_db(&bands, 0.0, SAMPLE_RATE, 1000.0);
        assert!(db > 3.0);
    }

    #[test]
    fn test_estimate_response_peak_db() {
        let bands = default_bands();
        let peak = estimate_response_peak_db(&bands, 0.0, SAMPLE_RATE);
        assert!((peak - 0.0).abs() < 1.0);
    }

    #[test]
    fn test_estimate_response_peak_db_hits_narrow_band_centre() {
        // A Q=6 bell at 1 kHz is narrower than the 1.02 log sweep, so the peak
        // is only exact when the band centre is unioned in. Upstream returns
        // 12.0 for this input; the log sweep alone yields 11.818651.
        let mut bands = default_bands();
        for band in bands.iter_mut() {
            band.filter_type = FilterType::Off;
        }
        bands[0].filter_type = FilterType::Bell;
        bands[0].frequency = 1000.0;
        bands[0].gain_db = 12.0;
        bands[0].q = 6.0;

        let peak = estimate_response_peak_db(&bands, 0.0, SAMPLE_RATE);
        assert!(
            (peak - 12.0).abs() < 1e-9,
            "expected exact 12.0 dB peak, got {peak}"
        );

        let frequencies = response_peak_frequencies(&bands, SAMPLE_RATE);
        assert!(frequencies.contains(&1000.0));
    }

    #[test]
    fn test_log_response_frequencies() {
        let freqs = log_response_frequencies(SAMPLE_RATE, 1.02);
        assert!(!freqs.is_empty());
        assert_eq!(freqs[0], GRAPH_FREQ_MIN);
    }

    #[test]
    fn test_stepped_response_frequencies() {
        let freqs = stepped_response_frequencies(SAMPLE_RATE, 192);
        assert_eq!(freqs.len(), 192);
        assert_eq!(freqs[0], GRAPH_FREQ_MIN);
    }

    #[test]
    fn test_stepped_response_frequencies_degenerate_falls_back_to_max_frequency() {
        // Upstream returns `max(1.0, max_frequency)`, not `GRAPH_FREQ_MIN`.
        // With a sample rate low enough that Nyquist-1 <= GRAPH_FREQ_MIN, the
        // single returned frequency must track `max_frequency` (here 15.0).
        let freqs = stepped_response_frequencies(32.0, 192);
        assert_eq!(freqs, vec![15.0]);
    }

    #[test]
    fn test_eq_mode_apo_matches_upstream_value() {
        assert_eq!(EQ_MODE_APO, 6);
    }

    #[test]
    fn test_preset_storage_dir_uses_upstream_output_subdir() {
        // Upstream `default_preset_storage_dir` is `app_config_dir() / "output"`,
        // so presets remain interchangeable with the Python original.
        assert_eq!(preset_storage_dir(), app_config_dir().join("output"));
    }

    #[test]
    fn test_load_preset_accepts_missing_and_legacy_version() {
        let dir = std::env::temp_dir().join("mini_eq_preset_version_test");
        let _ = std::fs::create_dir_all(&dir);

        // Version-less payload (implicitly version 0) is accepted, as upstream.
        let path = dir.join("noversion.json");
        std::fs::write(
            &path,
            r#"{"preamp_db": -2.0, "bands": [{"filter_type": 1, "frequency": 1000.0, "gain_db": 3.0, "q": 1.0, "mute": false}]}"#,
        )
        .unwrap();
        let (preamp, bands) = load_preset_from_file(&path).unwrap();
        assert_eq!(preamp, -2.0);
        assert_eq!(bands[0].filter_type, FilterType::Bell);
        assert!(!bands[0].mute);

        // A preset from a newer build is rejected.
        let newer = dir.join("newer.json");
        std::fs::write(&newer, r#"{"version": 99, "bands": []}"#).unwrap();
        assert!(load_preset_from_file(&newer).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The reader the port never had: a sink's own link wins, the fallback
    /// covers everything else, and the legacy `"default"` entry is not mistaken
    /// for a sink name.
    #[test]
    fn per_sink_preset_lookup_prefers_the_sink_then_the_fallback() {
        let dir = std::env::temp_dir().join(format!(
            "mini-eq-output-preset-lookup-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("output-presets.json");

        std::fs::write(
            &path,
            r#"{"links":{"alsa_output.pci-0000_04_00.6.analog-stereo":"desk"},
                 "default":"flat","version":1}"#,
        )
        .unwrap();
        assert_eq!(
            output_preset_for_sink_at(&path, "alsa_output.pci-0000_04_00.6.analog-stereo")
                .as_deref(),
            Some("desk")
        );
        assert_eq!(
            output_preset_for_sink_at(&path, "alsa_output.pci-0000_04_00.1.hdmi-stereo").as_deref(),
            Some("flat"),
            "a sink with no link of its own falls back"
        );

        std::fs::write(&path, r#"{"links":{},"version":1}"#).unwrap();
        assert_eq!(
            output_preset_for_sink_at(&path, "alsa_output.pci-0000_04_00.1.hdmi-stereo"),
            None,
            "no link and no fallback means nothing to apply"
        );

        // The entry the old Link button wrote: keyed "default", not by sink.
        std::fs::write(
            &path,
            r#"{"links":{"default":"preset_1"},"default":"preset_1","version":1}"#,
        )
        .unwrap();
        assert_eq!(
            output_preset_for_sink_at(&path, "default").as_deref(),
            Some("preset_1")
        );
        assert_eq!(
            output_preset_for_sink_at(&path, "alsa_output.pci-0000_04_00.6.analog-stereo")
                .as_deref(),
            Some("preset_1"),
            "with no per-sink link the fallback still applies"
        );
    }

    #[test]
    fn output_preset_key_is_the_sink_name() {
        assert_eq!(
            output_preset_key_for_sink("  alsa_output.usb-Generic_USB_Audio-00.analog-stereo  "),
            "alsa_output.usb-Generic_USB_Audio-00.analog-stereo"
        );
    }

    #[test]
    fn test_sanitize_preset_name_matches_upstream() {
        // Reference values from upstream `sanitize_preset_name`.
        let cases = [
            ("My Preset", "My Preset"),
            ("a/b:c*d?e", "a b c d e"),
            ("  ..Name..  ", "Name"),
            ("foo<bar>|baz", "foo bar baz"),
            ("a\tb\nc", "a b c"),
            ("", ""),
            ("....", ""),
            ("Flat  Bass", "Flat Bass"),
        ];
        for (input, expected) in cases {
            assert_eq!(sanitize_preset_name(input), expected, "input {input:?}");
        }

        let long = "x".repeat(150);
        assert_eq!(sanitize_preset_name(&long).len(), 100);
    }

    #[test]
    fn test_preset_payload_roundtrip() {
        let bands = default_bands();
        let payload = preset_payload(&bands, -3.0);
        let parsed = preset_payload_bands(&payload).unwrap();
        assert_eq!(parsed.len(), bands.len());
        assert!(
            (payload
                .get("preamp_db")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.0)
                - (-3.0))
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn test_band_dict_uses_upstream_mute_key() {
        let mut band = EqBand::new(0);
        band.mute = true;
        let dict = eq_band_to_dict(&band);
        assert_eq!(dict.get("mute").and_then(|v| v.as_bool()), Some(true));
        assert!(dict.get("enabled").is_none());

        band.mute = false;
        let dict = eq_band_to_dict(&band);
        assert_eq!(dict.get("mute").and_then(|v| v.as_bool()), Some(false));
    }

    /// A muted band must survive a save/load cycle. Modelling `mute` as the
    /// inverse of a single "enabled" flag lost it, because `Off` bands and
    /// muted bands both serialised to the same value.
    #[test]
    fn test_muted_band_roundtrips_through_preset() {
        let mut bands = default_bands();
        bands[0].filter_type = FilterType::Bell;
        bands[0].mute = true;

        let payload = preset_payload(&bands, 0.0);
        let parsed = preset_payload_bands(&payload).unwrap();
        assert!(parsed[0].mute);
        assert_eq!(parsed[0].filter_type, FilterType::Bell);
    }

    #[test]
    fn test_band_from_dict_reads_upstream_mute_and_rejects_unsupported_types() {
        let fallback = EqBand::new(0);

        // Upstream JSON with `mute: true` must load as disabled.
        let upstream = serde_json::json!({
            "filter_type": 1, "frequency": 1000.0, "gain_db": 3.0,
            "q": 1.0, "mute": true, "solo": false,
        });
        let band = eq_band_from_dict(&upstream, &fallback);
        assert!(band.mute);
        assert_eq!(band.filter_type, FilterType::Bell);

        // Resonance (7) is not a selectable type upstream -> coerced to Off.
        let unsupported = serde_json::json!({
            "filter_type": 7, "frequency": 1000.0, "gain_db": 3.0, "q": 1.0,
        });
        let band = eq_band_from_dict(&unsupported, &fallback);
        assert_eq!(band.filter_type, FilterType::Off);

        // `enabled` from presets written by this port still loads, inverted.
        let legacy = serde_json::json!({ "enabled": true, "filter_type": 1 });
        assert!(!eq_band_from_dict(&legacy, &fallback).mute);
        let legacy = serde_json::json!({ "enabled": false, "filter_type": 1 });
        assert!(eq_band_from_dict(&legacy, &fallback).mute);
    }

    #[test]
    fn test_preset_payload_bands_with_more_than_default_bands() {
        // Upstream presets serialize all MAX_BANDS bands; loading one must not
        // panic even though only DEFAULT_ACTIVE_BANDS are active by default.
        let bands: Vec<EqBand> = (0..MAX_BANDS)
            .map(|index| {
                let mut band = EqBand::new(index);
                band.filter_type = FilterType::Bell;
                band.gain_db = 1.5;
                band
            })
            .collect();
        assert_eq!(bands.len(), MAX_BANDS);

        let payload = preset_payload(&bands, -2.0);
        let parsed = preset_payload_bands(&payload).expect("must parse 32 bands");
        assert_eq!(parsed.len(), MAX_BANDS);
        assert!(parsed.iter().all(|b| b.filter_type == FilterType::Bell));
        assert!(parsed.iter().all(|b| (b.gain_db - 1.5).abs() < 1e-9));
    }

    #[test]
    fn test_default_bands_match_upstream_log_spacing() {
        // Mirrors upstream `compute_log_spaced_band_defaults(DEFAULT_ACTIVE_BANDS)`.
        let bands = default_bands();
        let expected = [
            (29.9526, 1.5048),
            (59.7633, 1.5048),
            (119.2435, 1.5048),
            (237.9221, 1.5048),
            (474.7171, 1.5048),
            (947.1851, 1.5048),
            (1889.8828, 1.5048),
            (3770.8118, 1.5048),
            (7523.7588, 1.5048),
            (15011.8723, 1.5048),
        ];
        for (band, (freq, q)) in bands.iter().zip(expected.iter()) {
            assert!(
                (band.frequency - freq).abs() < 0.01,
                "band {} frequency {} != {}",
                band.index,
                band.frequency,
                freq
            );
            assert!(
                (band.q - q).abs() < 0.001,
                "band {} q {} != {}",
                band.index,
                band.q,
                q
            );
            assert_eq!(band.filter_type, FilterType::Bell);
            assert!(!band.mute);
        }
    }

    #[test]
    fn test_output_preset_config_roundtrip() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("output-presets-roundtrip.json");
        let _ = std::fs::remove_file(&path);

        let mut links = std::collections::HashMap::new();
        links.insert("sink1".to_string(), "preset1".to_string());
        write_output_preset_config_at(
            &path,
            &links,
            Some("fallback"),
            OutputRoutingMode::Selected,
            None,
        )
        .unwrap();

        let cfg = load_output_preset_config_at(&path).unwrap();
        assert_eq!(cfg.links.get("sink1"), Some(&"preset1".to_string()));
        assert_eq!(cfg.default_preset, Some("fallback".to_string()));
        assert_eq!(cfg.mode, OutputRoutingMode::Selected);
        assert_eq!(cfg.monitor, None);

        // Clearing a link preserves the fallback preset. Write back to the same
        // path the test is exercising -- `write_output_preset_config` writes
        // to the real config dir, which is not what this test wants.
        let mut cfg = load_output_preset_config_at(&path).unwrap();
        assert!(cfg.links.remove("sink1").is_some());
        write_output_preset_config_at(
            &path,
            &cfg.links,
            cfg.default_preset.as_deref(),
            cfg.mode,
            cfg.monitor.as_deref(),
        )
        .unwrap();

        let cfg = load_output_preset_config_at(&path).unwrap();
        assert!(!cfg.links.contains_key("sink1"));
        assert_eq!(cfg.default_preset, Some("fallback".to_string()));

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_output_preset_config_missing_file_is_empty() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("does-not-exist-output-presets.json");
        let _ = std::fs::remove_file(&path);
        let cfg = load_output_preset_config_at(&path).unwrap();
        assert!(cfg.links.is_empty());
        assert!(cfg.default_preset.is_none());
        assert_eq!(cfg.mode, OutputRoutingMode::Selected);
        assert_eq!(cfg.monitor, None);
    }

    /// Version 1 has no `mode` or `monitor` field: it must read as Selected /
    /// follow, which is what the app did before the mode existed, and the
    /// legacy `"default"` link key must not be mistaken for a sink.
    #[test]
    fn test_load_output_preset_config_v1_is_selected_and_drops_default_link() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("output-presets-v1.json");
        std::fs::write(
            &path,
            r#"{"links":{"default":"preset_1"},"default":"preset_1","version":1}"#,
        )
        .unwrap();
        let cfg = load_output_preset_config_at(&path).unwrap();
        assert!(
            !cfg.links.contains_key("default"),
            "the legacy 'default' link key is the fallback, not a device"
        );
        assert_eq!(cfg.default_preset, Some("preset_1".to_string()));
        assert_eq!(cfg.mode, OutputRoutingMode::Selected);
        assert_eq!(cfg.monitor, None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_load_output_preset_config_v2_carries_mode_and_monitor() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("output-presets-v2.json");
        std::fs::write(
            &path,
            r#"{"links":{"sink1":"preset1"},"default":"fallback","mode":"reroute",
               "monitor":"sink2","version":2}"#,
        )
        .unwrap();
        let cfg = load_output_preset_config_at(&path).unwrap();
        assert_eq!(cfg.links.get("sink1"), Some(&"preset1".to_string()));
        assert_eq!(cfg.default_preset, Some("fallback".to_string()));
        assert_eq!(cfg.mode, OutputRoutingMode::Reroute);
        assert_eq!(cfg.monitor, Some("sink2".to_string()));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_output_preset_config_writes_version_2() {
        let path = std::env::temp_dir()
            .join("mini-eq-test")
            .join("output-presets-v2-write.json");
        let _ = std::fs::remove_file(&path);
        let mut links = std::collections::HashMap::new();
        links.insert("sink1".to_string(), "preset1".to_string());
        write_output_preset_config_at(
            &path,
            &links,
            Some("fallback"),
            OutputRoutingMode::Reroute,
            Some("sink2"),
        )
        .unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        let payload: serde_json::Value = serde_json::from_str(&data).unwrap();
        assert_eq!(payload.get("version").and_then(|v| v.as_i64()), Some(2));
        assert_eq!(
            payload.get("mode").and_then(|v| v.as_str()),
            Some("reroute")
        );
        assert_eq!(
            payload.get("monitor").and_then(|v| v.as_str()),
            Some("sink2")
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_write_output_preset_config_creates_parent_dir() {
        let dir = std::env::temp_dir()
            .join("mini-eq-test")
            .join("nested")
            .join("deeper");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("output-presets.json");

        write_output_preset_config_at(
            &path,
            &std::collections::HashMap::new(),
            None,
            OutputRoutingMode::Selected,
            None,
        )
        .unwrap();
        assert!(path.is_file());

        let _ = std::fs::remove_dir_all(std::env::temp_dir().join("mini-eq-test").join("nested"));
    }

    /// Auto-write-back writes the live curve into the preset linked to a sink,
    /// and only when exactly one device links to it.
    #[test]
    fn auto_write_output_preset_for_sink_writes_only_for_singly_linked_preset() {
        let dir = std::env::temp_dir()
            .join("mini-eq-test")
            .join("auto-write-back");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let links_path = dir.join("output-presets.json");
        let preset_path = crate::core::preset_path_for_name_at(&dir, "linked_preset");

        // Create the linked preset with a known curve.
        let bands = crate::core::default_bands();
        save_preset_to_file(&preset_path, &bands, 0.0).unwrap();

        // Link it from one sink.
        let mut links = std::collections::HashMap::new();
        links.insert("sinkA".to_string(), "linked_preset".to_string());
        write_output_preset_config_at(&links_path, &links, None, OutputRoutingMode::Selected, None)
            .unwrap();

        // Move the curve away from the saved one.
        let mut moved = bands.clone();
        moved[0].gain_db = 3.0;

        // Writes: exactly one sink links to it.
        let wrote =
            auto_write_output_preset_for_sink_at(&links_path, &dir, "sinkA", &moved, 0.0).unwrap();
        assert!(wrote, "a singly-linked preset must be written back");
        // The write-back writes to the preset storage directory passed in, so
        // read it back from there rather than from the real config dir.
        let after_path = crate::core::preset_path_for_name_at(&dir, "linked_preset");
        let after_data = std::fs::read_to_string(&after_path).unwrap();
        let after_payload: serde_json::Value = serde_json::from_str(&after_data).unwrap();
        let after_bands = preset_payload_bands(&after_payload).unwrap();
        assert!((after_bands[0].gain_db - 3.0).abs() < 1e-9);

        // Link it from a second sink: now the write-back must refuse.
        links.insert("sinkB".to_string(), "linked_preset".to_string());
        write_output_preset_config_at(&links_path, &links, None, OutputRoutingMode::Selected, None)
            .unwrap();
        let refused =
            auto_write_output_preset_for_sink_at(&links_path, &dir, "sinkA", &moved, 0.0).unwrap();
        assert!(
            !refused,
            "a preset linked from two devices must not be written back"
        );

        // A sink with no link: nothing to write.
        let no_link =
            auto_write_output_preset_for_sink_at(&links_path, &dir, "sinkC", &moved, 0.0).unwrap();
        assert!(!no_link, "an unlinked sink has nothing to write back");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The saved signature for a linked sink is the preset's own signature, so
    /// the write-back can compare the live curve against it.
    #[test]
    fn saved_signature_for_linked_sink_matches_the_preset() {
        let dir = std::env::temp_dir()
            .join("mini-eq-test")
            .join("saved-signature");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let links_path = dir.join("output-presets.json");
        let preset_path = crate::core::preset_path_for_name_at(&dir, "preset_a");

        let bands = crate::core::default_bands();
        save_preset_to_file(&preset_path, &bands, 0.0).unwrap();

        let mut links = std::collections::HashMap::new();
        links.insert("sinkA".to_string(), "preset_a".to_string());
        write_output_preset_config_at(&links_path, &links, None, OutputRoutingMode::Selected, None)
            .unwrap();

        let sig = output_preset_saved_signature_for_sink_at_at(&links_path, &dir, "sinkA").unwrap();
        // The claim is that the saved signature IS the preset file's own
        // signature, so compare against the file rather than against a
        // freshly-built payload: the file's floats have been through a JSON
        // round-trip and differ from `default_bands()` in the last digits.
        let file_data = std::fs::read_to_string(&preset_path).unwrap();
        let file_payload: serde_json::Value = serde_json::from_str(&file_data).unwrap();
        assert_eq!(sig, preset_payload_state_signature(&file_payload));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Per-device virtual sinks: prefixed, deterministic, sanitized and
    /// pairwise distinct (multi-chain EQ).
    #[test]
    fn eq_virtual_sink_naming() {
        let a = eq_virtual_sink_for("alsa_output.pci-0000_04_00.6.analog-stereo");
        let b =
            eq_virtual_sink_for("alsa_output.usb-Logitech_Logitech_H570e_Stereo-00.analog-stereo");
        assert!(a.starts_with(VIRTUAL_SINK_BASE));
        assert!(b.starts_with(VIRTUAL_SINK_BASE));
        assert_ne!(a, b);
        // Deterministic across calls (stable identity across restarts).
        assert_eq!(
            a,
            eq_virtual_sink_for("alsa_output.pci-0000_04_00.6.analog-stereo")
        );
        // Only node-name-safe characters survive.
        assert!(
            a.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.')
        );
        assert!(!a.contains(' ') && !a.contains('/'));
        // Degenerate input still yields a usable, non-empty name.
        let weird = eq_virtual_sink_for("  // ");
        assert!(weird.starts_with(VIRTUAL_SINK_BASE));
        assert!(weird.len() > VIRTUAL_SINK_BASE.len());
        // Filter-output name derives from the virtual sink, not the device.
        assert_eq!(
            eq_filter_output_for("alsa_output.pci-0000_04_00.6.analog-stereo"),
            format!("{a}{FILTER_OUTPUT_SUFFIX}")
        );
    }
}
