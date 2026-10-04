//! Desktop integration: .desktop files, icon installation, GSettings schema.
//!
//! Mirrors `desktop_integration.py` from the original Python project.

use std::fs;
use std::process::Command;

use crate::core::{APP_ID, app_data_file_path};

// ── Application constants ────────────────────────────────────────────────────

pub const APP_DISPLAY_NAME: &str = "Mini EQ";
pub const APP_ICON_NAME: &str = APP_ID;
pub const APP_ICON_SEARCH_PATH: &str = "assets/icons";
pub const APP_SCHEMA_NAME: &str = "io.github.mrproject72.mini_eq_rr.gschema.xml";
pub const APP_SCHEMA_SOURCE: &str = "assets/schemas/io.github.mrproject72.mini_eq_rr.gschema.xml";

/// Escape a single argument for use in a freedesktop `.desktop` Exec line.
pub fn quote_desktop_exec_arg(value: &str) -> String {
    let escaped = value
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('%', "%%");
    format!("\"{}\"", escaped)
}

/// Build the content of the main application `.desktop` file.
pub fn build_desktop_file() -> String {
    let lines = [
        "[Desktop Entry]",
        "GenericName=System-wide Equalizer",
        "Comment=Minimal system-wide parametric equalizer for PipeWire",
        &format!("Name={}", APP_DISPLAY_NAME),
        "Keywords=equalizer;audio;pipewire;",
        "Categories=GTK;AudioVideo;Audio;",
        &format!("Exec={}", "mini-eq-rr"),
        &format!("Icon={}", APP_ICON_NAME),
        "StartupNotify=true",
        "Terminal=false",
        "Type=Application",
        &format!("StartupWMClass={}", APP_ID),
        "",
    ];
    lines.join("\n")
}

/// Install desktop integration: .desktop file, icons, GSettings schema.
pub fn install_desktop_integration() -> anyhow::Result<()> {
    let data_home = gio_data_home();
    let applications_dir = data_home.join("applications");
    fs::create_dir_all(&applications_dir)?;

    let desktop_file = applications_dir.join(format!("{}.desktop", APP_ID));
    fs::write(&desktop_file, build_desktop_file())?;

    let hicolor_dir = data_home.join("icons/hicolor");
    remove_legacy_raster_app_icons(&hicolor_dir);
    // `copy_app_icons` appends the `hicolor` theme component itself, so hand it
    // the parent icons dir. Passing `hicolor_dir` produced
    // `icons/hicolor/hicolor/scalable/apps/...`, where nothing looks for icons.
    // This went unnoticed for as long as the source tree was missing and the
    // function returned early.
    copy_app_icons(&data_home.join("icons"));

    refresh_desktop_database(&applications_dir);
    refresh_icon_cache(&hicolor_dir);
    let _ = install_gsettings_schema(&data_home.join("glib-2.0/schemas"));

    println!("desktop entry installed: {}", desktop_file.display());
    println!("icons installed under: {}", hicolor_dir.display());
    Ok(())
}

fn remove_legacy_raster_app_icons(hicolor_dir: &std::path::Path) {
    if !hicolor_dir.exists() {
        return;
    }
    if let Ok(entries) = fs::read_dir(hicolor_dir) {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type()
                && file_type.is_dir()
            {
                let glob_path = entry.path().join(format!("apps/{}.png", APP_ICON_NAME));
                let _ = fs::remove_file(&glob_path);
            }
        }
    }
}

#[allow(clippy::collapsible_if)]
fn copy_app_icons(target_dir: &std::path::Path) {
    let source_dir = std::path::Path::new(APP_ICON_SEARCH_PATH);
    if !source_dir.exists() {
        return;
    }

    fn walk_icon(dir: &std::path::Path, source: &std::path::Path, target: &std::path::Path) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk_icon(&path, source, target);
                } else if path.extension().is_some_and(|e| e == "svg" || e == "png")
                    && let Ok(rel) = path.strip_prefix(source)
                {
                    let dest = target.join("hicolor").join(rel);
                    if let Some(parent) = dest.parent() {
                        let _ = fs::create_dir_all(parent);
                    }
                    let _ = fs::copy(&path, dest);
                }
            }
        }
    }

    walk_icon(source_dir, source_dir, target_dir);
}

fn refresh_desktop_database(applications_dir: &std::path::Path) {
    if let Some(exe) = find_executable("update-desktop-database") {
        let _ = Command::new(exe).arg(applications_dir).status();
    }
}

fn refresh_icon_cache(hicolor_dir: &std::path::Path) {
    let index_theme = hicolor_dir.join("index.theme");
    if !index_theme.exists() {
        return;
    }
    if let Some(exe) = find_executable("gtk-update-icon-cache") {
        let _ = Command::new(exe)
            .arg("-q")
            .arg("-f")
            .arg("-t")
            .arg(hicolor_dir)
            .status();
    }
}

fn install_gsettings_schema(schemas_dir: &std::path::Path) -> anyhow::Result<()> {
    fs::create_dir_all(schemas_dir)?;

    let source = std::path::Path::new(APP_SCHEMA_SOURCE);
    if !source.exists() {
        return Ok(());
    }

    let target = schemas_dir.join(APP_SCHEMA_NAME);
    fs::copy(source, &target)?;

    compile_gsettings_schemas(schemas_dir)?;
    Ok(())
}

fn compile_gsettings_schemas(schemas_dir: &std::path::Path) -> anyhow::Result<()> {
    if let Some(exe) = find_executable("glib-compile-schemas") {
        let result = Command::new(exe)
            .arg("--strict")
            .arg(schemas_dir)
            .output()?;
        if !result.status.success() {
            let details = String::from_utf8_lossy(&result.stderr).trim().to_string();
            let msg = format!(
                "could not compile GSettings schemas in {}",
                schemas_dir.display()
            );
            anyhow::bail!(
                "{}{}",
                msg,
                if details.is_empty() {
                    String::new()
                } else {
                    format!(": {}", details)
                }
            );
        }
    }
    Ok(())
}

#[allow(clippy::collapsible_if)]
fn find_executable(name: &str) -> Option<String> {
    if let Ok(path) = std::env::var("PATH") {
        for dir in path.split(':') {
            let candidate = std::path::Path::new(dir).join(name);
            if candidate.is_file() {
                if let Some(s) = candidate.to_str() {
                    return Some(s.to_string());
                }
            }
        }
    }
    None
}

fn gio_data_home() -> std::path::PathBuf {
    let data_home = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|p| std::path::Path::new(p).is_absolute())
        .unwrap_or_else(|| {
            std::env::var("HOME")
                .map(|h| format!("{}/.local/share", h))
                .unwrap_or_default()
        });
    std::path::PathBuf::from(data_home)
}

/// Build a native autostart desktop file content for background mode.
pub fn build_native_autostart_desktop_file(command: &[String], _auto_route: bool) -> String {
    let exec_line = command
        .iter()
        .map(|s| quote_desktop_exec_arg(s))
        .collect::<Vec<_>>()
        .join(" ");

    let lines = [
        "[Desktop Entry]",
        "Type=Application",
        &format!("Name={}", APP_DISPLAY_NAME),
        "GenericName=System-wide Equalizer",
        "Comment=Keep Mini EQ available in the background",
        &format!("Exec={}", exec_line),
        &format!("Icon={}", APP_ICON_NAME),
        "Terminal=false",
        "NoDisplay=true",
        "X-GNOME-Autostart-enabled=true",
        "",
    ];
    lines.join("\n")
}

#[allow(unused_imports)]
fn _asset_path() -> std::path::PathBuf {
    app_data_file_path("mini-eq-rr")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `install_desktop_integration` used to hand `copy_app_icons` a path that
    /// already ended in `hicolor`, and `copy_app_icons` appends that component
    /// itself. The result was `icons/hicolor/hicolor/scalable/apps/...`, which
    /// no icon lookup finds. The bug was invisible while
    /// `APP_ICON_SEARCH_PATH` pointed at a directory that did not exist.
    #[test]
    fn app_icon_assets_exist_at_every_hicolor_size() {
        let dir = std::path::Path::new(APP_ICON_SEARCH_PATH);
        assert!(
            dir.exists(),
            "{APP_ICON_SEARCH_PATH} must exist or no icon is ever installed"
        );

        // The icon is a raster design (assets/parametric_eq_mixer_icon.png),
        // so it ships as PNGs at the standard hicolor sizes rather than a
        // scalable SVG. `copy_app_icons` only used to copy `.svg`, which would
        // have silently installed nothing again.
        for size in [16, 24, 32, 48, 64, 128, 256, 512] {
            let icon = dir
                .join(format!("{size}x{size}/apps"))
                .join(format!("{APP_ICON_NAME}.png"));
            assert!(icon.exists(), "expected {}", icon.display());
            let body = std::fs::read(&icon).expect("icon readable");
            assert_eq!(
                &body[..8],
                b"\x89PNG\r\n\x1a\n",
                "{} is not a PNG",
                icon.display()
            );
            assert!(
                body.len() > 100,
                "{} looks truncated ({} bytes)",
                icon.display(),
                body.len()
            );

            let symbolic = dir
                .join(format!("{size}x{size}/apps"))
                .join(format!("{APP_ICON_NAME}-symbolic.png"));
            assert!(
                symbolic.exists(),
                "expected a symbolic variant at {}",
                symbolic.display()
            );
        }
    }

    #[test]
    fn icons_reach_the_hicolor_theme_root() {
        // `copy_app_icons` appends `hicolor` itself, so callers must hand it the
        // parent icons dir. Passing the hicolor dir produced
        // `icons/hicolor/hicolor/...`, where nothing looks for icons.
        assert!(
            !APP_ICON_SEARCH_PATH.ends_with("hicolor"),
            "search path must not already end in the hicolor component"
        );
    }
    #[test]
    fn desktop_file_points_at_an_icon_we_ship() {
        let desktop = build_desktop_file();
        assert!(desktop.contains(&format!("Icon={APP_ICON_NAME}")));
        assert!(desktop.contains("Exec=mini-eq-rr"));
        // Icon name and Exec must agree with what we actually package.
        assert!(
            desktop.contains(&format!("StartupWMClass={APP_ID}")),
            "StartupWMClass must match APP_ID"
        );
    }

    #[test]
    fn metainfo_is_well_formed_and_declares_the_binary() {
        let path = std::path::Path::new("assets/metainfo").join(format!("{APP_ID}.metainfo.xml"));
        let body = std::fs::read_to_string(&path).expect("metainfo readable");
        assert!(body.contains("<component type=\"desktop-application\">"));
        assert!(body.contains("<binary>mini-eq-rr</binary>"));
        assert!(
            body.contains(&format!("<id>{APP_ID}</id>")),
            "metainfo id must match APP_ID"
        );
    }
}
