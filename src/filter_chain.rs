//! PipeWire filter-chain graph construction.
//!
//! Port of upstream `mini_eq/filter_chain.py`. The graph is described as a
//! PipeWire module argument string and loaded with
//! `pw_context_load_module("libpipewire-module-filter-chain", args)`; it is not
//! a `create_object` factory.

use crate::core::{
    BiquadCoefficients, EQ_PREAMP_MAX_DB, EQ_PREAMP_MIN_DB, EqBand, MAX_BANDS, OUTPUT_CLIENT_NAME,
    SAMPLE_RATE, VIRTUAL_SINK_DESCRIPTION, band_biquad_coefficients, band_is_effective,
    bands_have_solo, db_to_linear, identity_biquad_coefficients,
};

pub const FILTER_CHAIN_MODULE_NAME: &str = "libpipewire-module-filter-chain";

/// Sample rates the builtin `bq_raw` nodes carry coefficients for.
pub const BIQUAD_CONFIG_SAMPLE_RATES: [f64; 4] = [44100.0, 48000.0, 96000.0, 192000.0];

/// Upstream `NATIVE_BIQUAD_LABELS`: filter type -> SPA biquad label.
pub fn native_biquad_label(ft: crate::core::FilterType) -> &'static str {
    ft.native_label()
}

/// True when the band maps to a native SPA biquad label (upstream
/// `band.filter_type in NATIVE_BIQUAD_LABELS`).
pub fn filter_type_has_native_biquad(ft: crate::core::FilterType) -> bool {
    ft.has_native_biquad()
}

pub fn biquad_node_name(side: &str, index: usize) -> String {
    format!("band_{}_{}", side, index)
}

pub fn preamp_node_name(side: &str) -> String {
    format!("preamp_{}", side)
}

/// Quote a value for a PipeWire config string (upstream `pipewire_quote`).
pub fn pipewire_quote(value: &str) -> String {
    format!("\"{}\"", value.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Format a float the way upstream `spa_float` does (C's `%.8g`).
///
/// `%g` picks scientific notation when the decimal exponent is below -4 or at
/// least the precision (8), and trims trailing zeros either way.
pub fn spa_float(value: f64) -> String {
    const PRECISION: i32 = 8;

    if value == 0.0 {
        return "0".to_string();
    }

    let exponent = value.abs().log10().floor() as i32;
    if !(-4..PRECISION).contains(&exponent) {
        let formatted = format!("{:.*e}", (PRECISION - 1) as usize, value);
        let (mantissa, exp) = formatted
            .split_once('e')
            .unwrap_or((formatted.as_str(), "0"));
        let mantissa = trim_trailing_zeros(mantissa);
        let exp: i32 = exp.parse().unwrap_or(0);
        format!(
            "{}e{}{:02}",
            mantissa,
            if exp < 0 { '-' } else { '+' },
            exp.abs()
        )
    } else {
        let decimals = (PRECISION - 1 - exponent).max(0) as usize;
        trim_trailing_zeros(&format!("{:.*}", decimals, value))
    }
}

fn trim_trailing_zeros(s: &str) -> String {
    if !s.contains('.') {
        return s.to_string();
    }
    let trimmed = s.trim_end_matches('0').trim_end_matches('.');
    trimmed.to_string()
}

/// Format a control weight the way Python's `f"{value}"` does for floats, which
/// is what upstream interpolates into the mixer `control = { ... }` block.
pub fn py_float(value: f64) -> String {
    format!("{:?}", value)
}

/// Preamp biquad: a plain gain, identity coefficients otherwise.
pub fn preamp_biquad_coefficients(preamp_db: f64, eq_enabled: bool) -> BiquadCoefficients {
    if !eq_enabled {
        return identity_biquad_coefficients(1.0);
    }

    let gain = db_to_linear(preamp_db.clamp(EQ_PREAMP_MIN_DB, EQ_PREAMP_MAX_DB));
    identity_biquad_coefficients(gain).scaled_for_control_range(10.0)
}

/// Band coefficients for the graph, scaled for the control range.
pub fn active_band_biquad_coefficients(
    band: &EqBand,
    sample_rate: f64,
    eq_enabled: bool,
    solo_active: bool,
) -> BiquadCoefficients {
    if !eq_enabled {
        return identity_biquad_coefficients(1.0);
    }

    band_biquad_coefficients(band, sample_rate, solo_active).scaled_for_control_range(10.0)
}

/// Control values for a native (builtin) biquad band.
///
/// The native filter computes its own coefficients at the DSP clock rate, so we
/// send Freq/Q/Gain plus mixer weights rather than raw coefficients.
pub fn native_biquad_band_control_values(
    index: usize,
    band: &EqBand,
    eq_enabled: bool,
    solo_active: bool,
) -> Vec<(String, f64)> {
    let wet = if eq_enabled
        && band_is_effective(band, solo_active)
        && filter_type_has_native_biquad(band.filter_type)
    {
        1.0
    } else {
        0.0
    };

    let mut controls = Vec::new();
    for side in ["l", "r"] {
        let name = biquad_node_name(side, index);
        controls.push((format!("{}_filter:Freq", name), band.frequency));
        controls.push((format!("{}_filter:Q", name), band.q));
        controls.push((format!("{}_filter:Gain", name), band.gain_db));
        controls.push((format!("{}:Gain 1", name), wet));
        controls.push((format!("{}:Gain 2", name), 1.0 - wet));
    }
    controls
}

/// Map a biquad's raw coefficients to the `bq_raw` node's control names
/// (`<node>:b0` … `<node>:a2`). Mirrors upstream
/// `biquad_coefficients_to_controls`. The preamp node is a `bq_raw` node,
/// so its live control is the coefficient set — NOT mixer `Gain 1/2`.
fn biquad_coefficients_to_controls(
    node_name: &str,
    coefficients: &crate::core::BiquadCoefficients,
) -> Vec<(String, f64)> {
    let (b0, b1, b2, a0, a1, a2) = coefficients.as_tuple();
    vec![
        (format!("{node_name}:b0"), b0),
        (format!("{node_name}:b1"), b1),
        (format!("{node_name}:b2"), b2),
        (format!("{node_name}:a0"), a0),
        (format!("{node_name}:a1"), a1),
        (format!("{node_name}:a2"), a2),
    ]
}

/// Control values for the preamp node. The preamp is a `bq_raw` node whose
/// gain is carried in the biquad coefficients, so the live control pushes
/// the raw `b0..a2` set (scaled for the control range), matching upstream
/// `builtin_biquad_preamp_control_values`.
pub fn native_biquad_preamp_control_values(preamp_db: f64, eq_enabled: bool) -> Vec<(String, f64)> {
    let coefficients = preamp_biquad_coefficients(preamp_db, eq_enabled);
    let mut controls = Vec::new();
    for side in ["l", "r"] {
        controls.extend(biquad_coefficients_to_controls(
            &preamp_node_name(side),
            &coefficients,
        ));
    }
    controls
}

/// All native biquad control values, mirroring upstream
/// `native_biquad_control_values`.
pub fn native_biquad_control_values(
    bands: &[EqBand],
    preamp_db: f64,
    eq_enabled: bool,
) -> Vec<(String, f64)> {
    let mut controls = native_biquad_preamp_control_values(preamp_db, eq_enabled);
    let solo_active = bands_have_solo(bands);

    for (index, band) in bands.iter().take(MAX_BANDS).enumerate() {
        controls.extend(native_biquad_band_control_values(
            index,
            band,
            eq_enabled,
            solo_active,
        ));
    }
    controls
}

/// Control values for a `bq_raw` band node: the raw biquad coefficients
/// (`b0..a2`), which fully encode the filter TYPE along with Freq/Q/Gain.
///
/// This is the upstream DEFAULT strategy (`native_biquads=False`). Because
/// the type lives in the coefficients and the node `label` is always
/// `bq_raw`, the graph topology never changes: a filter-type edit is just a
/// live coefficient push, so the engine never needs to be torn down and
/// reloaded (and the sink node id / app routing stays stable). Mirrors
/// upstream `builtin_biquad_band_control_values`.
pub fn bq_raw_band_control_values(
    index: usize,
    band: &EqBand,
    eq_enabled: bool,
    sample_rate: f64,
    solo_active: bool,
) -> Vec<(String, f64)> {
    let coefficients = active_band_biquad_coefficients(band, sample_rate, eq_enabled, solo_active);
    let mut controls = Vec::new();
    for side in ["l", "r"] {
        controls.extend(biquad_coefficients_to_controls(
            &biquad_node_name(side, index),
            &coefficients,
        ));
    }
    controls
}

/// All `bq_raw` control values (preamp + every band), mirroring upstream
/// `builtin_biquad_control_values`.
pub fn bq_raw_control_values(
    bands: &[EqBand],
    preamp_db: f64,
    eq_enabled: bool,
    sample_rate: f64,
) -> Vec<(String, f64)> {
    let mut controls = native_biquad_preamp_control_values(preamp_db, eq_enabled);
    let solo_active = bands_have_solo(bands);
    for (index, band) in bands.iter().take(MAX_BANDS).enumerate() {
        controls.extend(bq_raw_band_control_values(
            index,
            band,
            eq_enabled,
            sample_rate,
            solo_active,
        ));
    }
    controls
}

fn format_coefficients(coefficients: &BiquadCoefficients) -> String {
    let (b0, b1, b2, a0, a1, a2) = coefficients.as_tuple();
    format!(
        "b0 = {} b1 = {} b2 = {} a0 = {} a1 = {} a2 = {}",
        spa_float(b0),
        spa_float(b1),
        spa_float(b2),
        spa_float(a0),
        spa_float(a1),
        spa_float(a2)
    )
}

fn build_biquad_raw_config(coefficients_by_rate: &[(f64, BiquadCoefficients)]) -> String {
    let lines: Vec<String> = coefficients_by_rate
        .iter()
        .map(|(rate, coeffs)| {
            format!(
                "            {{ rate = {} {} }}",
                *rate as i64,
                format_coefficients(coeffs)
            )
        })
        .collect();
    format!(
        "        config = {{\n          coefficients = [\n{}\n          ]\n        }}",
        lines.join("\n")
    )
}

fn build_biquad_node(
    node_name: &str,
    coefficients_by_rate: &[(f64, BiquadCoefficients)],
) -> String {
    let config = build_biquad_raw_config(coefficients_by_rate);
    format!(
        "      {{\n        type = builtin\n        name = {}\n        label = bq_raw\n{}\n      }}",
        node_name, config
    )
}

/// Nodes for the graph.
///
/// `native_biquads` selects the upstream native variant, where each band is a
/// SPA biquad node (`bq_peaking`, `bq_highpass`, ...) paired with a `mixer`
/// node that performs the wet/dry crossfade.
pub fn build_biquad_nodes(
    bands: &[EqBand],
    preamp_db: f64,
    eq_enabled: bool,
    native_biquads: bool,
) -> String {
    let graph_bands = &bands[..bands.len().min(MAX_BANDS)];
    let solo_active = bands_have_solo(graph_bands);
    let mut nodes: Vec<String> = Vec::new();

    for side in ["l", "r"] {
        if native_biquads {
            // Preamp is a `bq_raw` gain node, matching upstream.
            nodes.push(build_biquad_node(
                &preamp_node_name(side),
                &preamp_coefficients_by_rate(preamp_db, eq_enabled),
            ));

            for (index, band) in graph_bands.iter().enumerate() {
                let name = biquad_node_name(side, index);
                let label = native_biquad_label(band.filter_type);
                let controls =
                    native_biquad_band_control_values(index, band, eq_enabled, solo_active);
                let wet = controls
                    .iter()
                    .find(|(k, _)| k == &format!("{}:Gain 1", name))
                    .map(|(_, v)| *v)
                    .unwrap_or(0.0);
                nodes.push(format!(
                    "      {{ type = builtin name = {}_filter label = {}\n        control = {{ Freq = {} Q = {} Gain = {} }}\n      }}\n      {{ type = builtin name = {} label = mixer\n        control = {{ \"Gain 1\" = {} \"Gain 2\" = {} }}\n      }}",
                    name,
                    label,
                    spa_float(band.frequency),
                    spa_float(band.q),
                    spa_float(band.gain_db),
                    name,
                    py_float(wet),
                    py_float(1.0 - wet)
                ));
            }
        } else {
            nodes.push(build_biquad_node(
                &preamp_node_name(side),
                &preamp_coefficients_by_rate(preamp_db, eq_enabled),
            ));
            for (index, band) in graph_bands.iter().enumerate() {
                nodes.push(build_biquad_node(
                    &biquad_node_name(side, index),
                    &band_coefficients_by_rate(band, eq_enabled, solo_active),
                ));
            }
        }
    }

    nodes.join("\n")
}

pub fn build_biquad_links(band_count: usize, native_biquads: bool) -> String {
    let mut links: Vec<String> = Vec::new();

    for side in ["l", "r"] {
        let mut previous = preamp_node_name(side);
        for index in 0..band_count {
            let current = biquad_node_name(side, index);
            if native_biquads {
                links.push(format!(
                    "      {{ output = \"{}:Out\" input = \"{}_filter:In\" }}",
                    previous, current
                ));
                links.push(format!(
                    "      {{ output = \"{}:Out\" input = \"{}:In 2\" }}",
                    previous, current
                ));
                links.push(format!(
                    "      {{ output = \"{}_filter:Out\" input = \"{}:In 1\" }}",
                    current, current
                ));
            } else {
                links.push(format!(
                    "      {{ output = \"{}:Out\" input = \"{}:In\" }}",
                    previous, current
                ));
            }
            previous = current;
        }
    }

    links.join("\n")
}

pub fn preamp_coefficients_by_rate(
    preamp_db: f64,
    eq_enabled: bool,
) -> Vec<(f64, BiquadCoefficients)> {
    let coefficients = preamp_biquad_coefficients(preamp_db, eq_enabled);
    BIQUAD_CONFIG_SAMPLE_RATES
        .iter()
        .map(|rate| (*rate, coefficients.clone()))
        .collect()
}

pub fn band_coefficients_by_rate(
    band: &EqBand,
    eq_enabled: bool,
    solo_active: bool,
) -> Vec<(f64, BiquadCoefficients)> {
    BIQUAD_CONFIG_SAMPLE_RATES
        .iter()
        .map(|rate| {
            (
                *rate,
                active_band_biquad_coefficients(band, *rate, eq_enabled, solo_active),
            )
        })
        .collect()
}

/// Build the full `libpipewire-module-filter-chain` argument string.
///
/// This is the Rust port of upstream `build_builtin_biquad_filter_chain_module_args`.
pub fn build_filter_chain_module_args(
    bands: &[EqBand],
    preamp_db: f64,
    eq_enabled: bool,
    virtual_sink_name: &str,
    filter_output_name: &str,
    output_sink: &str,
    native_biquads: bool,
) -> String {
    let graph_bands = &bands[..bands.len().min(MAX_BANDS)];
    let band_count = graph_bands.len();
    let nodes = build_biquad_nodes(graph_bands, preamp_db, eq_enabled, native_biquads);
    let links = build_biquad_links(band_count, native_biquads);
    let rate_property = if native_biquads {
        String::new()
    } else {
        format!("  audio.rate = {}\n", SAMPLE_RATE as i64)
    };
    let output_l = if band_count > 0 {
        biquad_node_name("l", band_count - 1)
    } else {
        preamp_node_name("l")
    };
    let output_r = if band_count > 0 {
        biquad_node_name("r", band_count - 1)
    } else {
        preamp_node_name("r")
    };

    format!(
        "{{\n  node.description = {}\n  media.name = {}\n  filter.graph = {{\n    nodes = [\n{}\n    ]\n    links = [\n{}\n    ]\n    inputs = [ \"{}:In\" \"{}:In\" ]\n    outputs = [ \"{}:Out\" \"{}:Out\" ]\n  }}\n  audio.channels = 2\n{}  audio.position = [ FL FR ]\n  capture.props = {{\n    node.name = {}\n    node.description = {}\n    media.class = Audio/Sink\n    state.restore-props = false\n    state.restore-target = false\n    audio.channels = 2\n    audio.position = [ FL FR ]\n  }}\n  playback.props = {{\n    node.name = {}\n    node.description = {}\n    node.passive = true\n    target.object = {}\n    state.restore-props = false\n    state.restore-target = false\n    audio.channels = 2\n    audio.position = [ FL FR ]\n  }}\n}}\n",
        pipewire_quote(VIRTUAL_SINK_DESCRIPTION),
        pipewire_quote(VIRTUAL_SINK_DESCRIPTION),
        nodes,
        links,
        preamp_node_name("l"),
        preamp_node_name("r"),
        output_l,
        output_r,
        rate_property,
        pipewire_quote(virtual_sink_name),
        pipewire_quote(VIRTUAL_SINK_DESCRIPTION),
        pipewire_quote(filter_output_name),
        pipewire_quote(OUTPUT_CLIENT_NAME),
        pipewire_quote(output_sink),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{FilterType, default_bands};

    #[test]
    fn test_module_args_use_native_biquad_labels() {
        let mut bands = default_bands();
        bands[0].filter_type = FilterType::HiPass;
        bands[1].filter_type = FilterType::HiShelf;

        let args = build_filter_chain_module_args(
            &bands,
            0.0,
            true,
            "mini_eq_sink",
            "mini_eq_sink_output",
            "alsa_output.test",
            true,
        );

        assert!(args.contains("label = bq_highpass"), "HiPass label missing");
        assert!(
            args.contains("label = bq_highshelf"),
            "HiShelf label missing"
        );
        assert!(args.contains("name = band_l_0_filter"));
        assert!(args.contains("node.name = \"mini_eq_sink\""));
        assert!(args.contains("target.object = \"alsa_output.test\""));
        // Native biquads must not pin the graph to 48 kHz.
        assert!(!args.contains("audio.rate"));
    }

    #[test]
    fn test_module_args_structure_matches_upstream() {
        let bands = default_bands();
        let args = build_filter_chain_module_args(
            &bands,
            0.0,
            true,
            "mini_eq_sink",
            "mini_eq_sink_output",
            "alsa_output.test",
            true,
        );

        assert!(args.contains("filter.graph = {"));
        assert!(args.contains("inputs = [ \"preamp_l:In\" \"preamp_r:In\" ]"));
        assert!(args.contains("outputs = [ \"band_l_31:Out\" \"band_r_31:Out\" ]"));
        assert!(args.contains("media.class = Audio/Sink"));
        assert!(args.contains("node.passive = true"));
        assert!(args.contains("state.restore-props = false"));
        // Balanced braces — the argument string is a SPA config object.
        assert_eq!(
            args.matches('{').count(),
            args.matches('}').count(),
            "unbalanced braces in module args"
        );
    }

    #[test]
    fn test_native_biquad_controls_wet_dry() {
        let mut bands = default_bands();
        // Active band -> fully wet.
        let active = native_biquad_band_control_values(0, &bands[0], true, false);
        assert_eq!(
            active
                .iter()
                .find(|(k, _)| k == "band_l_0:Gain 1")
                .unwrap()
                .1,
            1.0
        );
        assert_eq!(
            active
                .iter()
                .find(|(k, _)| k == "band_l_0:Gain 2")
                .unwrap()
                .1,
            0.0
        );

        // Muted band -> fully dry.
        bands[0].mute = true;
        let inactive = native_biquad_band_control_values(0, &bands[0], true, false);
        assert_eq!(
            inactive
                .iter()
                .find(|(k, _)| k == "band_l_0:Gain 1")
                .unwrap()
                .1,
            0.0
        );
        assert_eq!(
            inactive
                .iter()
                .find(|(k, _)| k == "band_l_0:Gain 2")
                .unwrap()
                .1,
            1.0
        );

        // EQ bypassed -> fully dry.
        bands[0].mute = false;
        let bypassed = native_biquad_band_control_values(0, &bands[0], false, false);
        assert_eq!(
            bypassed
                .iter()
                .find(|(k, _)| k == "band_l_0:Gain 1")
                .unwrap()
                .1,
            0.0
        );
    }

    #[test]
    fn test_solo_mutes_non_solo_bands() {
        let mut bands = default_bands();
        bands[0].solo = true;
        let controls = native_biquad_control_values(&bands, 0.0, true);
        let gain_for = |name: &str| {
            controls
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| *v)
                .unwrap()
        };
        assert_eq!(gain_for("band_l_0:Gain 1"), 1.0);
        assert_eq!(gain_for("band_l_1:Gain 1"), 0.0);
    }

    #[test]
    fn test_coefficients_scaled_for_control_range() {
        // A high-Q bell can exceed the +/-10 control limit.
        let mut band = crate::core::EqBand::new(0);
        band.filter_type = FilterType::Bell;
        band.frequency = 1000.0;
        band.gain_db = 20.0;
        band.q = 6.0;

        let raw = band_biquad_coefficients(&band, 48000.0, false);
        let scaled = raw.scaled_for_control_range(10.0);
        let max_abs = scaled
            .as_array()
            .iter()
            .fold(0.0_f64, |acc, v| acc.max(v.abs()));
        assert!(max_abs <= 10.0 + 1e-9, "max coefficient {} > 10", max_abs);
    }

    #[test]
    fn test_spa_float_formatting() {
        // Reference values from C's printf("%.8g"), matching upstream `spa_float`.
        let cases: &[(f64, &str)] = &[
            (0.0, "0"),
            (1.0, "1"),
            (-2.5, "-2.5"),
            (1000.0, "1000"),
            (29.9526, "29.9526"),
            (1.5048, "1.5048"),
            (0.0001, "0.0001"),
            (1e-5, "1e-05"),
            (0.5, "0.5"),
            (15011.8723, "15011.872"),
            (1.9952623, "1.9952623"),
            (0.12345678, "0.12345678"),
            (123456789.0, "1.2345679e+08"),
            (1234.5678, "1234.5678"),
            (0.00012345, "0.00012345"),
            (-0.000012345, "-1.2345e-05"),
            (1e9, "1e+09"),
            (6.28e-7, "6.28e-07"),
            (0.000000123456, "1.23456e-07"),
        ];

        for (input, expected) in cases {
            assert_eq!(spa_float(*input), *expected, "spa_float({})", input);
        }
    }

    #[test]
    fn test_preamp_control_gain() {
        // The preamp is a `bq_raw` node: its gain is carried in the raw
        // biquad coefficients (`preamp_l:b0`), not a mixer `Gain 1`.
        // +6 dB preamp -> linear gain ~1.9953 in b0.
        let controls = native_biquad_preamp_control_values(6.0, true);
        let b0 = controls.iter().find(|(k, _)| k == "preamp_l:b0").unwrap().1;
        assert!((b0 - 1.995_262_3).abs() < 1e-5, "b0 was {}", b0);
        // a0 stays 1 (pure gain, no scaling needed under the +/-10 limit).
        let a0 = controls.iter().find(|(k, _)| k == "preamp_l:a0").unwrap().1;
        assert!((a0 - 1.0).abs() < 1e-9, "a0 was {}", a0);
        // The old mixer control must NOT be present.
        assert!(
            !controls.iter().any(|(k, _)| k == "preamp_l:Gain 1"),
            "stale mixer control present"
        );

        // Bypassed EQ -> unity (b0 == 1).
        let bypassed = native_biquad_preamp_control_values(6.0, false);
        assert_eq!(
            bypassed.iter().find(|(k, _)| k == "preamp_l:b0").unwrap().1,
            1.0
        );
    }

    /// The A/B bypass must produce a genuinely different control set from the
    /// active EQ, otherwise toggling the switch is inaudible.
    ///
    /// This was reported as "A/B switch does nothing". The mechanism turned out
    /// to be correct (and matches upstream formula-for-formula) but the toggle
    /// also went through the 400 ms fader-drag debounce in `window.rs`, so the
    /// change could be missed entirely when toggling back quickly. That is
    /// fixed there; this test pins the DSP side so a future refactor cannot
    /// silently make bypass a no-op.
    #[test]
    fn test_eq_bypass_pushes_flat_response_not_the_active_curve() {
        let bands = vec![
            crate::core::EqBand {
                index: 0,
                frequency: 125.0,
                gain_db: 18.0,
                q: 1.0,
                filter_type: crate::core::FilterType::Bell,
                mute: false,
                solo: false,
                coefficients: crate::core::BiquadCoefficients::identity(),
            },
            crate::core::EqBand {
                index: 1,
                frequency: 1000.0,
                gain_db: -12.0,
                q: 2.0,
                filter_type: crate::core::FilterType::Bell,
                mute: false,
                solo: false,
                coefficients: crate::core::BiquadCoefficients::identity(),
            },
        ];

        let active = bq_raw_control_values(&bands, 0.0, true, crate::core::SAMPLE_RATE);
        let bypassed = bq_raw_control_values(&bands, 0.0, false, crate::core::SAMPLE_RATE);

        assert_eq!(
            active.len(),
            bypassed.len(),
            "bypass must keep the same control set, only different values"
        );
        assert_ne!(
            active, bypassed,
            "bypassing produced an identical control set — A/B would be inaudible"
        );

        // Every band node must collapse to unity gain (b0 == a0, no b1/b2/a1/a2),
        // i.e. a true pass-through, not merely "some other curve".
        for (key, value) in &bypassed {
            if key.ends_with(":b0") {
                let node = key.trim_end_matches(":b0");
                let a0 = bypassed
                    .iter()
                    .find(|(k, _)| *k == format!("{node}:a0"))
                    .unwrap()
                    .1;
                assert!(
                    (value - a0).abs() < 1e-9,
                    "{node} is not unity gain: b0={value} a0={a0}"
                );
            }
            if key.ends_with(":b1")
                || key.ends_with(":b2")
                || key.ends_with(":a1")
                || key.ends_with(":a2")
            {
                assert_eq!(*value, 0.0, "{key} should be 0 in bypass, was {value}");
            }
        }

        // And the active set must genuinely be doing something at band 0.
        let active_b0 = active.iter().find(|(k, _)| k == "band_l_0:b0").unwrap().1;
        let active_a0 = active.iter().find(|(k, _)| k == "band_l_0:a0").unwrap().1;
        assert!(
            (active_b0 - active_a0).abs() > 1e-6,
            "active band 0 is already unity — the test fixture is not testing anything"
        );
    }

    #[test]
    fn test_links_chain_preamp_through_all_bands() {
        let links = build_biquad_links(2, true);
        assert!(links.contains("\"preamp_l:Out\" input = \"band_l_0_filter:In\""));
        assert!(links.contains("\"band_l_0:Out\" input = \"band_l_1_filter:In\""));
        assert!(links.contains("\"preamp_r:Out\" input = \"band_r_0_filter:In\""));

        let raw_links = build_biquad_links(2, false);
        assert!(raw_links.contains("\"preamp_l:Out\" input = \"band_l_0:In\""));
        assert!(!raw_links.contains("_filter:In"));
    }

    #[test]
    fn bq_raw_band_control_values_are_coefficients_for_both_sides() {
        let mut bands = default_bands();
        bands[0].filter_type = crate::core::FilterType::Bell;
        bands[0].frequency = 1000.0;
        bands[0].gain_db = 6.0;
        bands[0].q = 1.0;

        let controls = bq_raw_band_control_values(0, &bands[0], true, 48000.0, false);
        let names: Vec<&str> = controls.iter().map(|(k, _)| k.as_str()).collect();

        // bq_raw pushes the coefficient set for BOTH channels, NOT the
        // native Freq/Q/Gain or mixer Gain 1/2 controls.
        for side in ["l", "r"] {
            for c in ["b0", "b1", "b2", "a0", "a1", "a2"] {
                assert!(
                    names.contains(&format!("band_{side}_0:{c}").as_str()),
                    "missing band_{side}_0:{c} in {names:?}"
                );
            }
        }
        assert!(
            !names
                .iter()
                .any(|n| n.contains(":Freq") || n.contains(":Q"))
        );
        assert!(
            !names
                .iter()
                .any(|n| n.contains("Gain 1") || n.contains("Gain 2"))
        );
    }

    #[test]
    fn bq_raw_encodes_filter_type_in_coefficients() {
        // Same Freq/Q/Gain, different filter TYPE -> different coefficients.
        // This is what makes a type change a live push (no topology reload).
        let mk = |ft: crate::core::FilterType| {
            let mut b = default_bands();
            b[0].filter_type = ft;
            b[0].frequency = 1000.0;
            b[0].gain_db = 6.0;
            b[0].q = 1.0;
            bq_raw_band_control_values(0, &b[0], true, 48000.0, false)
        };
        let bell = mk(crate::core::FilterType::Bell);
        let lowshelf = mk(crate::core::FilterType::LoShelf);
        let get = |c: &[(String, f64)], k: &str| c.iter().find(|(n, _)| n == k).unwrap().1;
        // At least one coefficient must differ between the two types.
        let differs = ["b0", "b1", "b2", "a1", "a2"].iter().any(|c| {
            (get(&bell, &format!("band_l_0:{c}")) - get(&lowshelf, &format!("band_l_0:{c}"))).abs()
                > 1e-6
        });
        assert!(differs, "filter type must change the bq_raw coefficients");
    }

    #[test]
    fn bq_raw_control_values_cover_preamp_and_all_bands() {
        let bands = default_bands();
        let controls = bq_raw_control_values(&bands, 0.0, true, 48000.0);
        // Preamp (both sides) + MAX_BANDS bands (both sides), 6 coeffs each.
        let has = |n: &str| controls.iter().any(|(k, _)| k == n);
        assert!(has("preamp_l:b0"));
        assert!(has("preamp_r:a2"));
        assert!(has(&format!("band_l_{}:b0", MAX_BANDS - 1)));
        assert!(has(&format!("band_r_{}:a2", MAX_BANDS - 1)));
        assert_eq!(controls.len(), (1 + MAX_BANDS) * 2 * 6);
    }
}
