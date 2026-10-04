//! AutoEq preset support.
//!
//! Implements:
//! - Equalizer APO preset file parsing (`parse_apo_file`).
//! - AutoEq entry search/filter scoring (local, no network).
//! - AutoEq.app API download of generated parametric EQ presets.
//!
//! The APO parser mirrors `core.py`'s `parse_apo_config_line`/`parse_apo_file`.
//! The API client mirrors `autoeq.py`'s `load_autoeq_entries`/`download_autoeq_preset`.

use std::path::Path;
use std::time::Duration;

use regex::Regex;
use serde::Deserialize;

use crate::core::{
    EQ_FREQUENCY_MAX_HZ, EQ_FREQUENCY_MIN_HZ, EQ_GAIN_MAX_DB, EQ_GAIN_MIN_DB, EQ_PREAMP_MAX_DB,
    EQ_PREAMP_MIN_DB, EQ_Q_MAX, EQ_Q_MIN, EqBand, FilterType, MAX_BANDS, clamp,
};

// ---------------------------------------------------------------------------
// Equalizer APO preset parsing
// ---------------------------------------------------------------------------

const DEFAULT_BAND_Q: f64 = 1.0 / std::f64::consts::SQRT_2;

/// Regexes matching the Python implementation.
fn re_comment() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)^[ \t]*#").unwrap())
}

fn re_preamp() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)preamp\s*:\s*([+-]?\d+(?:\.\d+)?)\s*db").unwrap())
}

fn re_filter() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"(?i)filter\s*\d*\s*:\s*on\s+([a-z]+(?:\s+(?:6|12)db)?)").unwrap()
    })
}

fn re_freq() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)fc\s+(\d+(?:,\d+)?(?:\.\d+)?)\s*hz").unwrap())
}

fn re_gain() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)gain\s+([+-]?\d+(?:\.\d+)?)\s*db").unwrap())
}

fn re_quality() -> &'static Regex {
    static RE: std::sync::OnceLock<Regex> = std::sync::OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)q\s+(\d+(?:\.\d+)?)").unwrap())
}

/// Map Equalizer APO filter type codes to our `FilterType` enum.
fn apo_filter_type_map(filter_name: &str) -> FilterType {
    match filter_name {
        "PK" | "MODAL" | "PEQ" => FilterType::Bell,
        "LP" | "LPQ" => FilterType::LoPass,
        "HP" | "HPQ" => FilterType::HiPass,
        "LS" | "LSC" | "LS 6DB" | "LS 12DB" => FilterType::LoShelf,
        "HS" | "HSC" | "HS 6DB" | "HS 12DB" => FilterType::HiShelf,
        "NO" => FilterType::Notch,
        "AP" => FilterType::Allpass,
        _ => FilterType::Off,
    }
}

/// Parsed APO filter band before conversion to EqBand.
#[derive(Debug, Clone)]
struct ApoBand {
    filter_type: String,
    filter_name: String,
    frequency: f64,
    gain_db: f64,
    q: f64,
}

fn parse_number(caps: Option<regex::Captures>, group: usize) -> Option<f64> {
    caps.and_then(|c| {
        c.get(group)
            .map(|m| m.as_str().replace(',', "").parse::<f64>().ok())
    })
    .flatten()
}

fn parse_apo_filter(line: &str, band: &mut ApoBand) -> String {
    if let Some(caps) = re_filter().captures(line) {
        let raw = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let filter_name = raw
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_uppercase();

        band.filter_name = filter_name.clone();
        band.filter_type = apo_filter_type_map(&band.filter_name).name().to_string();
        return filter_name;
    }
    String::new()
}

fn parse_apo_frequency(line: &str, band: &mut ApoBand) -> bool {
    if let Some(value) = parse_number(re_freq().captures(line), 1) {
        band.frequency = value;
        true
    } else {
        false
    }
}

fn parse_apo_gain(line: &str, band: &mut ApoBand) -> bool {
    if let Some(value) = parse_number(re_gain().captures(line), 1) {
        band.gain_db = value;
        true
    } else {
        false
    }
}

fn parse_apo_quality(line: &str, band: &mut ApoBand) -> bool {
    if let Some(value) = parse_number(re_quality().captures(line), 1) {
        band.q = value;
        true
    } else {
        false
    }
}

fn parse_apo_config_line(line: &str) -> Option<ApoBand> {
    let mut band = ApoBand {
        filter_type: "Off".to_string(),
        filter_name: String::new(),
        frequency: 1000.0,
        gain_db: 0.0,
        q: DEFAULT_BAND_Q,
    };

    let filter_name = parse_apo_filter(line, &mut band);

    if filter_name.is_empty() {
        return None;
    }

    parse_apo_frequency(line, &mut band);

    match filter_name.as_str() {
        "PK" | "MODAL" | "PEQ" => {
            parse_apo_gain(line, &mut band);
            parse_apo_quality(line, &mut band);
        }
        "LP" | "LPQ" | "HP" | "HPQ" => {
            parse_apo_quality(line, &mut band);
        }
        "LS" | "LSC" | "HS" | "HSC" => {
            parse_apo_gain(line, &mut band);
            if !parse_apo_quality(line, &mut band) {
                band.q = 2.0 / 3.0;
            }
        }
        "LS 6DB" => {
            band.frequency *= 2.0 / 3.0;
            band.q = std::f64::consts::SQRT_2 / 3.0;
            parse_apo_gain(line, &mut band);
        }
        "LS 12DB" => {
            band.frequency *= 3.0 / 2.0;
            parse_apo_gain(line, &mut band);
        }
        "HS 6DB" => {
            band.frequency *= std::f64::consts::SQRT_2;
            band.q = std::f64::consts::SQRT_2 / 3.0;
            parse_apo_gain(line, &mut band);
        }
        "HS 12DB" => {
            band.frequency /= std::f64::consts::SQRT_2;
            parse_apo_gain(line, &mut band);
        }
        "NO" => {
            if !parse_apo_quality(line, &mut band) {
                band.q = 100.0 / 3.0;
            }
        }
        "AP" => {
            parse_apo_quality(line, &mut band);
        }
        _ => {}
    }

    Some(band)
}

/// Parse an Equalizer APO config file.
///
/// Returns `(preamp_db, bands)`.
pub fn parse_apo_file(path: &Path) -> Result<(f64, Vec<EqBand>), String> {
    let content = std::fs::read_to_string(path)
        .map_err(|e| format!("APO preset file not found or unreadable: {}", e))?;

    let mut preamp = 0.0_f64;
    let mut apo_bands: Vec<ApoBand> = Vec::new();

    for line in content.lines() {
        if re_comment().is_match(line) {
            continue;
        }

        if let Some(band) = parse_apo_config_line(line) {
            apo_bands.push(band);
            continue;
        }

        if let Some(caps) = re_preamp().captures(line)
            && let Some(value) = parse_number(Some(caps), 1)
        {
            preamp = clamp(value, EQ_PREAMP_MIN_DB, EQ_PREAMP_MAX_DB);
        }
    }

    if apo_bands.is_empty() {
        return Err("APO preset did not contain any supported filter".to_string());
    }

    // Sort by frequency (matching Python's apo_bands.sort(key=lambda b: b.frequency))
    apo_bands.sort_by(|a, b| {
        a.frequency
            .partial_cmp(&b.frequency)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let eq_bands: Vec<EqBand> = apo_bands
        .into_iter()
        .take(MAX_BANDS)
        .enumerate()
        .map(|(index, band)| EqBand {
            index,
            frequency: clamp(band.frequency, EQ_FREQUENCY_MIN_HZ, EQ_FREQUENCY_MAX_HZ),
            gain_db: clamp(band.gain_db, EQ_GAIN_MIN_DB, EQ_GAIN_MAX_DB),
            q: clamp(band.q, EQ_Q_MIN, EQ_Q_MAX),
            filter_type: FilterType::from_name(&band.filter_type).unwrap_or(FilterType::Off),
            mute: false,
            solo: false,
            coefficients: crate::core::BiquadCoefficients::identity(),
        })
        .collect();

    Ok((preamp, eq_bands))
}

// ---------------------------------------------------------------------------
// AutoEq entry search
// ---------------------------------------------------------------------------

/// A headphone/measurement entry from autoeq.app.
#[derive(Debug, Clone, Deserialize)]
pub struct AutoEqEntry {
    pub name: String,
    pub source: String,
    #[allow(dead_code)]
    pub form: String,
    #[allow(dead_code)]
    pub rig: String,
}

impl AutoEqEntry {
    pub fn cache_key(&self) -> String {
        format!(
            "autoeq.app/v1/{}/{}/{}/{}",
            self.source, self.form, self.rig, self.name
        )
    }

    pub fn detail(&self) -> String {
        let parts: Vec<String> = [&self.source, &self.rig]
            .iter()
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect();
        parts.join(" - ")
    }
}

fn normalize_search_query(query: &str) -> Vec<String> {
    query.split_whitespace().map(|t| t.to_lowercase()).collect()
}

fn autoeq_search_score(entry: &AutoEqEntry, tokens: &[String]) -> (u32, usize, usize, String) {
    let name = entry.name.to_lowercase();
    let detail = format!("{} {} {}", entry.source, entry.rig, entry.form).to_lowercase();
    let haystack = format!("{} {}", name, detail);

    if tokens.iter().any(|token| !haystack.contains(token)) {
        return (1_000_000, 1_000_000, entry.name.len(), name);
    }

    let first_token = tokens.first().cloned().unwrap_or_default();
    let prefix_penalty = if name.starts_with(&first_token) {
        0
    } else {
        40
    };
    let token_distance: usize = tokens
        .iter()
        .map(|token| haystack.find(token).unwrap_or(0))
        .sum();
    let source_bonus =
        if ["oratory1990", "crinacle", "rtings"].contains(&entry.source.to_lowercase().as_str()) {
            0
        } else {
            8
        };
    (
        prefix_penalty + token_distance as u32 + source_bonus,
        entry.name.len(),
        entry.cache_key().len(),
        name,
    )
}

/// Search AutoEq entries by query, returning up to `limit` results sorted by score.
pub fn search_autoeq_entries(
    entries: &[AutoEqEntry],
    query: &str,
    limit: usize,
) -> Vec<AutoEqEntry> {
    let tokens = normalize_search_query(query);
    if tokens.is_empty() {
        return Vec::new();
    }

    let mut matched: Vec<(AutoEqEntry, (u32, usize, usize, String))> = entries
        .iter()
        .map(|e| (e.clone(), autoeq_search_score(e, &tokens)))
        .filter(|(_, score)| score.0 < 1_000_000)
        .collect();

    matched.sort_by(|a, b| a.1.cmp(&b.1));
    matched.into_iter().take(limit).map(|(e, _)| e).collect()
}

// ---------------------------------------------------------------------------
// AutoEq.app API client
// ---------------------------------------------------------------------------

const AUTOEQ_APP_ENTRIES_URL: &str = "https://autoeq.app/entries";
const AUTOEQ_APP_EQUALIZE_URL: &str = "https://autoeq.app/equalize";
const AUTOEQ_REQUEST_TIMEOUT_SECONDS: u64 = 20;
const AUTOEQ_PARAMETRIC_EQ_CONFIG: &str = "8_PEAKING_WITH_SHELVES";
const AUTOEQ_UNKNOWN_TARGET_LABEL: &str = "Unknown";

/// Download and cache the AutoEq entries JSON from autoeq.app.
///
/// Synchronous on purpose. This is called from a GTK app whose main loop is
/// GLib, not Tokio, and `reqwest`'s async client needs a Tokio reactor: a
/// `glib::MainContext::spawn_local` around it aborts the process with
/// "there is no reactor running, must be called from the context of a Tokio 1.x
/// runtime". Callers must therefore run this on a worker thread and hop back to
/// the main context to touch widgets.
pub fn load_autoeq_entries(cache_dir: &Path) -> Result<Vec<AutoEqEntry>, String> {
    let cache_path = cache_dir.join("autoeq").join("entries.json");

    if let Ok(cached) = std::fs::read_to_string(&cache_path) {
        return parse_autoeq_app_entries(&cached);
    }

    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(AUTOEQ_REQUEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    let text = client
        .get(AUTOEQ_APP_ENTRIES_URL)
        .header("User-Agent", "Mini EQ")
        .send()
        .map_err(|e| format!("could not download AutoEq entries: {}", e))?
        .text()
        .map_err(|e| format!("could not read AutoEq entries response: {}", e))?;

    let entries = parse_autoeq_app_entries(&text)?;

    if let Some(parent) = cache_path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create cache dir: {}", e))?;
        std::fs::write(&cache_path, &text).map_err(|e| format!("Failed to write cache: {}", e))?;
    }

    Ok(entries)
}

/// Parse the autoeq.app entries JSON into a list of `AutoEqEntry`.
pub fn parse_autoeq_app_entries(text: &str) -> Result<Vec<AutoEqEntry>, String> {
    let data: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("AutoEq entries JSON parse error: {}", e))?;

    let mut entries = Vec::new();
    let mut seen = std::collections::HashSet::new();

    if let Some(obj) = data.as_object() {
        for (name, measurements) in obj {
            if let Some(arr) = measurements.as_array() {
                for measurement in arr {
                    if let Some(m) = measurement.as_object() {
                        let source = m
                            .get("source")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let form = m
                            .get("form")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();
                        let rig = m
                            .get("rig")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .trim()
                            .to_string();

                        if source.is_empty() || form.is_empty() {
                            continue;
                        }

                        let key = (
                            name.to_lowercase(),
                            source.to_lowercase(),
                            form.to_lowercase(),
                            rig.to_lowercase(),
                        );
                        if seen.contains(&key) {
                            continue;
                        }
                        seen.insert(key);

                        entries.push(AutoEqEntry {
                            name: name.clone(),
                            source,
                            form,
                            rig,
                        });
                    }
                }
            }
        }
    }

    Ok(entries)
}

/// A generated parametric EQ preset downloaded from autoeq.app.
#[derive(Debug)]
pub struct AutoEqGeneratedPreset {
    pub text: String,
    pub target_label: String,
}

/// A downloaded AutoEq preset saved to disk.
#[derive(Debug, Clone)]
pub struct AutoEqDownloadedPreset {
    pub path: std::path::PathBuf,
    pub target_label: String,
    pub sample_rate: u32,
}

/// Build the body for the autoeq.app equalize POST request.
fn autoeq_equalize_body(entry: &AutoEqEntry, sample_rate: f64) -> serde_json::Value {
    serde_json::json!({
        "target": "Flat",
        "sound_signature": null,
        "sound_signature_smoothing_window_size": 1.0,
        "bass_boost_gain": 0.0,
        "bass_boost_fc": 105.0,
        "bass_boost_q": 0.7,
        "treble_boost_gain": 0.0,
        "treble_boost_fc": 10000.0,
        "treble_boost_q": 0.7,
        "tilt": 0.0,
        "fs": sample_rate as u32,
        "bit_depth": 16,
        "phase": "minimum",
        "f_res": 16.0,
        "preamp": 0.0,
        "max_gain": 12.0,
        "max_slope": 18,
        "window_size": 0.08,
        "treble_window_size": 2.0,
        "treble_f_lower": 6000.0,
        "treble_f_upper": 8000.0,
        "treble_gain_k": 1.0,
        "graphic_eq": false,
        "parametric_eq": true,
        "fixed_band_eq": false,
        "convolution_eq": false,
        "response": {
            "fr_f_step": 1.02,
            "fr_fields": ["frequency", "smoothed", "error_smoothed", "target", "equalization", "equalized_smoothed"],
            "base64fp16": true,
        },
        "name": entry.name,
        "source": entry.source,
        "rig": entry.rig,
        "parametric_eq_config": AUTOEQ_PARAMETRIC_EQ_CONFIG,
    })
}

fn format_autoeq_parametric_eq(parametric_eq: &serde_json::Value) -> Result<String, String> {
    let preamp = parametric_eq
        .get("preamp")
        .and_then(|v| v.as_f64())
        .ok_or("AutoEq response did not include preamp")?;
    let filters = parametric_eq
        .get("filters")
        .and_then(|v| v.as_array())
        .ok_or("AutoEq response did not include filters")?;

    let filter_type_map = |ft: &str| match ft {
        "LOW_SHELF" => "LSC",
        "PEAKING" => "PK",
        "HIGH_SHELF" => "HSC",
        _ => "PK",
    };

    let mut lines = vec![format!("Preamp: {:.2} dB", preamp)];
    for (index, filter_data) in filters.iter().enumerate() {
        let filter_type = filter_data
            .get("type")
            .and_then(|v| v.as_str())
            .map(filter_type_map)
            .ok_or("filter missing type")?;
        let fc = filter_data
            .get("fc")
            .and_then(|v| v.as_f64())
            .ok_or("filter missing fc")?;
        let gain = filter_data
            .get("gain")
            .and_then(|v| v.as_f64())
            .ok_or("filter missing gain")?;
        let q = filter_data
            .get("q")
            .and_then(|v| v.as_f64())
            .ok_or("filter missing q")?;

        lines.push(format!(
            "Filter {}: ON {} Fc {:.1} Hz Gain {:.1} dB Q {:.2}",
            index + 1,
            filter_type,
            fc,
            gain,
            q
        ));
    }

    Ok(lines.join("\n") + "\n")
}

/// Download and cache an AutoEq preset for the given entry.
///
/// Synchronous for the same reason as [`load_autoeq_entries`]: run on a worker
/// thread, not on the GLib main loop.
pub fn download_autoeq_preset(
    entry: &AutoEqEntry,
    cache_dir: &Path,
    sample_rate: f64,
) -> Result<AutoEqDownloadedPreset, String> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(AUTOEQ_REQUEST_TIMEOUT_SECONDS))
        .build()
        .map_err(|e| format!("Failed to create HTTP client: {}", e))?;

    // Step 1: get the equalize response from autoeq.app
    let body = autoeq_equalize_body(entry, sample_rate);
    let resp = client
        .post(AUTOEQ_APP_EQUALIZE_URL)
        .json(&body)
        .send()
        .map_err(|e| format!("could not download AutoEq data: {}", e))?;

    let data: serde_json::Value = resp
        .json()
        .map_err(|e| format!("AutoEq response is not valid JSON: {}", e))?;

    let parametric_eq = &data["parametric_eq"];
    let text = format_autoeq_parametric_eq(parametric_eq)?;

    if !text.contains("Filter ") && !text.contains("Preamp:") {
        return Err(
            "downloaded AutoEq preset does not look like an Equalizer APO preset".to_string(),
        );
    }

    // Determine target label
    let target_label = data
        .get("target")
        .and_then(|v| v.as_str())
        .unwrap_or(AUTOEQ_UNKNOWN_TARGET_LABEL)
        .to_string();

    // Save to cache
    let preset_dir = cache_dir.join("autoeq").join("presets");
    std::fs::create_dir_all(&preset_dir)
        .map_err(|e| format!("Failed to create cache dir: {}", e))?;

    let digest = {
        let key = entry.cache_key();
        let bytes = key.as_bytes();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        use std::hash::{Hash, Hasher};
        bytes.hash(&mut hasher);
        format!("{:012x}", hasher.finish())
    };
    let path = preset_dir.join(format!("AutoEq-{}.txt", digest));

    let header = format!(
        "# AutoEq target: {}\n# AutoEq sample rate: {}\n",
        target_label, sample_rate as u32
    );
    std::fs::write(&path, format!("{}{}", header, text))
        .map_err(|e| format!("Failed to write preset: {}", e))?;

    Ok(AutoEqDownloadedPreset {
        path,
        target_label,
        sample_rate: sample_rate as u32,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_apo_filter_types() {
        let band = parse_apo_config_line("Filter 1: ON PK Fc 1000 Hz Gain 6.0 dB Q 1.0").unwrap();
        assert_eq!(band.filter_name, "PK");
        assert_eq!(band.filter_type, "Bell");
    }

    #[test]
    fn test_parse_apo_preamp() {
        let preamp_line = "Preamp: -3.5 dB";
        let caps = re_preamp().captures(preamp_line).unwrap();
        let value = parse_number(Some(caps), 1).unwrap();
        assert!((value - (-3.5)).abs() < 1e-9);
    }

    #[test]
    fn test_parse_apo_file_shelves() {
        let content = "Preamp: -1.0 dB\nFilter 1: ON LS Fc 100 Hz Gain -3.0 dB Q 0.7\nFilter 2: ON HS Fc 10000 Hz Gain 2.0 dB Q 0.7\n";
        let tmp = tempfile_path("test_apo.txt");
        std::fs::write(&tmp, content).unwrap();
        let (preamp, bands) = parse_apo_file(&tmp).unwrap();
        assert!((preamp - (-1.0)).abs() < 1e-9);
        assert_eq!(bands.len(), 2);
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn test_parse_apo_file_rejects_no_filters() {
        let tmp = tempfile_path("test_apo_empty.txt");
        std::fs::write(&tmp, "# just a comment\n").unwrap();
        let result = parse_apo_file(&tmp);
        assert!(result.is_err());
        std::fs::remove_file(&tmp).ok();
    }

    /// Uses the real shape returned by `https://autoeq.app/entries`, which is a
    /// flat object mapping headphone name -> list of measurement records:
    /// `{"HD650": [{"form": "...", "rig": "...", "source": "..."}], ...}`
    /// (6033 names as of 2026-10-04). A parser that only handled a list, or a
    /// flat record, would silently yield zero profiles.
    #[test]
    fn test_parse_entries_real_api_shape() {
        let payload = r#"{
            "HD650": [{"form": "over-ear", "rig": "HD650", "source": "oratory1990"}],
            "1Custom SA02": [{"form": "in-ear", "rig": "711", "source": "crinacle"}],
            "M50x": [
                {"form": "over-ear", "rig": "", "source": "rtings"},
                {"form": "over-ear", "rig": "ANC", "source": "crinacle"}
            ]
        }"#;
        let entries = parse_autoeq_app_entries(payload).expect("payload should parse");
        assert_eq!(entries.len(), 4, "one entry per name/measurement pair");

        let hd650 = entries.iter().find(|e| e.name == "HD650").expect("HD650");
        assert_eq!(hd650.source, "oratory1990");
        assert_eq!(hd650.form, "over-ear");
        assert_eq!(hd650.rig, "HD650");

        // Multiple measurements of one headphone must all be offered.
        let m50x: Vec<&AutoEqEntry> = entries.iter().filter(|e| e.name == "M50x").collect();
        assert_eq!(m50x.len(), 2, "both M50x measurements should be listed");

        // And they must be searchable.
        let hits = search_autoeq_entries(&entries, "hd650", 10);
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn test_parse_entries_rejects_malformed_json() {
        assert!(parse_autoeq_app_entries("not json").is_err());
    }

    #[test]
    fn test_search_autoeq_entries() {
        let entries = vec![
            AutoEqEntry {
                name: "HD650".to_string(),
                source: "oratory1990".to_string(),
                form: "over-ear".to_string(),
                rig: "HD650".to_string(),
            },
            AutoEqEntry {
                name: "SR-325".to_string(),
                source: "rtings".to_string(),
                form: "over-ear".to_string(),
                rig: "".to_string(),
            },
        ];
        let results = search_autoeq_entries(&entries, "HD650", 10);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].name, "HD650");
    }

    #[test]
    fn test_search_autoeq_entries_empty_query() {
        let entries: Vec<AutoEqEntry> = vec![];
        let results = search_autoeq_entries(&entries, "", 10);
        assert!(results.is_empty());
    }

    fn tempfile_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("mini-eq-test");
        std::fs::create_dir_all(&dir).ok();
        dir.join(name)
    }
}
