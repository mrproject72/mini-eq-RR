//! Preset management panel.
//!
//! Clean layout: a header (title + saved/modified state chip), a compact
//! toolbar (Add / Remove / Import / Export), and a single ListBox that is
//! the sole preset selector. Built-in factory presets (Neutral, Bass Boost,
//! Treble Boost) always appear at the top and cannot be removed; user
//! presets (added or imported) appear below and can be removed. Clicking a
//! row loads that preset.
//!
//! The per-output-device auto-preset features (Fallback / Link to Output)
//! live in the Headroom panel's "Output Controls" section, not here.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;

use adw::prelude::AdwDialogExt as _;
use gtk4::prelude::*;

use crate::autoeq::parse_apo_file;
use crate::core::{
    BUILTIN_PRESET_NAMES, default_bands, is_builtin_preset, load_preset_from_file,
    preset_path_for_name, sanitize_preset_name, save_preset_to_file,
};

/// Preset management widget.
pub struct PresetPanel {
    pub container: gtk4::Box,
    pub list_box: gtk4::ListBox,
    pub add_button: gtk4::Button,
    /// Rename the selected custom preset.
    pub rename_button: gtk4::Button,
    pub remove_button: gtk4::Button,
    pub import_button: gtk4::Button,
    /// Open the AutoEq headphone-correction browser.
    pub autoeq_button: gtk4::Button,
    pub export_button: gtk4::Button,
    pub state_chip: gtk4::Label,
    current_bands: Vec<crate::core::EqBand>,
    current_preamp_db: f64,
    apply_bands_callback: Option<Box<dyn Fn(Vec<crate::core::EqBand>, f64)>>,
    /// Opens the AutoEq import dialog. Set by the window.
    autoeq_callback: Option<Box<dyn Fn()>>,
    reset_callback: Option<Box<dyn Fn()>>,
    get_signature_callback: Option<Box<dyn Fn() -> String>>,
    /// Reports the EQ as it stands right now: `(bands, preamp_db)`.
    ///
    /// `current_bands` is only updated when a preset is LOADED, so it describes
    /// the last loaded preset, not what the user has since edited. Saving used
    /// it, which meant "Save preset" wrote the default flat curve whenever no
    /// preset had been loaded yet -- the preset saved, and it was empty.
    current_state_callback: Option<Box<dyn Fn() -> (Vec<crate::core::EqBand>, f64)>>,
    current_preset_name: Option<String>,
    saved_signature: Option<String>,
    revert_baseline_label: Option<String>,
    revert_baseline_signature: Option<String>,
    revert_baseline_payload: Option<serde_json::Value>,
    default_signature: Option<String>,
    file_monitor: Option<glib::SignalHandlerId>,
}

/// Pick a preset name that does not collide with an existing preset file.
///
/// AutoEq profile names frequently repeat across sources (the same headphone is
/// measured by several rigs), so importing twice must not silently overwrite
/// the first import. Returns `base` when free, otherwise `base (2)`, `base (3)`
/// and so on.
pub fn unique_preset_name(base: &str) -> String {
    let sanitized = sanitize_preset_name(base);
    if !preset_path_for_name(&sanitized).exists() {
        return sanitized;
    }
    let mut suffix = 2;
    loop {
        let candidate = format!("{sanitized} ({suffix})");
        if !preset_path_for_name(&candidate).exists() {
            return candidate;
        }
        suffix += 1;
    }
}

impl PresetPanel {
    pub fn new() -> Rc<RefCell<Self>> {
        let add_button = gtk4::Button::from_icon_name("list-add-symbolic");
        add_button.set_tooltip_text(Some("Add a new preset from the current EQ"));
        let remove_button = gtk4::Button::from_icon_name("list-remove-symbolic");
        remove_button.set_tooltip_text(Some("Remove the selected custom preset"));
        let rename_button = gtk4::Button::from_icon_name("document-edit-symbolic");
        rename_button.set_tooltip_text(Some("Rename the selected custom preset"));
        let import_button = gtk4::Button::from_icon_name("document-open-symbolic");
        import_button.set_tooltip_text(Some("Import an APO (.apo/.txt) preset..."));
        let autoeq_button = gtk4::Button::from_icon_name("edit-find-symbolic");
        autoeq_button.set_tooltip_text(Some("Import a headphone correction curve from AutoEq"));
        let export_button = gtk4::Button::from_icon_name("document-save-as-symbolic");
        export_button.set_tooltip_text(Some("Export the selected preset to a file..."));

        // --- Header: title + state chip + toolbar.
        let header = gtk4::Box::new(gtk4::Orientation::Horizontal, 6);
        let title = gtk4::Label::new(Some("Presets"));
        title.set_css_classes(&["heading"]);
        header.append(&title);

        let state_chip = gtk4::Label::new(Some("Neutral"));
        state_chip.set_css_classes(&["preset-state-chip", "preset-state-chip-neutral"]);
        state_chip.set_valign(gtk4::Align::Center);
        header.append(&state_chip);

        let spacer = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        header.append(&spacer);

        header.append(&add_button);
        header.append(&rename_button);
        header.append(&remove_button);
        header.append(&import_button);
        header.append(&autoeq_button);
        header.append(&export_button);

        // --- Preset list (the sole selector).
        let list_box = gtk4::ListBox::new();
        list_box.set_selection_mode(gtk4::SelectionMode::Single);
        list_box.set_vexpand(true);
        list_box.set_css_classes(&["preset-list"]);

        let scrolled = gtk4::ScrolledWindow::new();
        scrolled.set_policy(gtk4::PolicyType::Never, gtk4::PolicyType::Automatic);
        scrolled.set_vexpand(true);
        scrolled.set_child(Some(&list_box));

        let container = gtk4::Box::new(gtk4::Orientation::Vertical, 6);
        container.set_css_classes(&["utility-section"]);
        container.set_margin_top(8);
        container.set_margin_bottom(8);
        container.set_margin_start(8);
        container.set_margin_end(8);
        container.append(&header);
        container.append(&scrolled);

        let panel = Rc::new(RefCell::new(Self {
            container,
            list_box,
            add_button,
            rename_button,
            remove_button,
            import_button,
            autoeq_button,
            export_button,
            state_chip,
            current_bands: default_bands(),
            current_preamp_db: 0.0,
            apply_bands_callback: None,
            autoeq_callback: None,
            reset_callback: None,
            get_signature_callback: None,
            current_state_callback: None,
            current_preset_name: None,
            saved_signature: None,
            revert_baseline_label: None,
            revert_baseline_signature: None,
            revert_baseline_payload: None,
            default_signature: None,
            file_monitor: None,
        }));

        refresh_preset_list(&panel.borrow().list_box);

        // --- Add: create a new custom preset from the current EQ state.
        let panel_clone = panel.clone();
        panel.borrow().add_button.connect_clicked(move |_| {
            // Read the curve as it is NOW, not the last loaded preset: the
            // whole point of this button is to capture the current EQ.
            let (bands, preamp, list_box) = {
                let p = panel_clone.borrow();
                let (bands, preamp) = p.live_state();
                (bands, preamp, p.list_box.clone())
            };
            // Suggest the old auto-numbered name as a starting point, but
            // let the user actually name the preset.
            let panel_for_state = panel_clone.clone();
            let suggested = format!("preset_{}", count_custom_children(&list_box) + 1);
            prompt_preset_name("Save Preset", "Save", &suggested, move |name| {
                let sanitized = sanitize_preset_name(&name);
                let path = preset_path_for_name(&sanitized);
                if let Err(e) = save_preset_to_file(&path, &bands, preamp) {
                    log::warn!("Save preset {sanitized}: {e}");
                    return;
                }
                // What was just written is now the current state, so the state
                // chip reads "saved" instead of "modified".
                {
                    let mut p = panel_for_state.borrow_mut();
                    p.current_bands = bands.clone();
                    p.current_preamp_db = preamp;
                    p.current_preset_name = Some(sanitized.clone());
                    p.saved_signature = Some(p.signature_of_current());
                }
                refresh_preset_list(&list_box);
            });
        });

        // --- Rename the selected CUSTOM preset (built-ins are safe).
        let panel_clone = panel.clone();
        panel.borrow().rename_button.connect_clicked(move |_| {
            let (old_name, list_box) = {
                let p = panel_clone.borrow();
                let Some(row) = p.list_box.selected_row() else {
                    return;
                };
                let Some(name) = row_name(&row) else {
                    return;
                };
                if is_builtin_preset(&name) {
                    return;
                }
                (name, p.list_box.clone())
            };
            let old_for_cmp = old_name.clone();
            prompt_preset_name("Rename Preset", "Rename", &old_name, move |new_name| {
                let sanitized = sanitize_preset_name(&new_name);
                let old_path = preset_path_for_name(&old_for_cmp);
                let new_path = preset_path_for_name(&sanitized);
                if old_path == new_path {
                    return;
                }
                // Rename the file itself rather than re-saving the in-memory
                // bands: that preserves the stored preset exactly, including
                // any fields this panel does not model.
                if std::fs::rename(&old_path, &new_path).is_ok() {
                    refresh_preset_list(&list_box);
                }
            });
        });

        // --- Remove: delete the selected CUSTOM preset (built-ins are safe).
        let panel_clone = panel.clone();
        panel.borrow().remove_button.connect_clicked(move |_| {
            let list_box = panel_clone.borrow().list_box.clone();
            let Some(row) = list_box.selected_row() else {
                return;
            };
            let Some(name) = row_name(&row) else { return };
            if is_builtin_preset(&name) {
                // Built-in presets cannot be removed; ignore.
                return;
            }
            let _ = crate::core::delete_preset_file(&name);
            refresh_preset_list(&list_box);
        });

        // --- Import: APO file -> new custom preset.
        let panel_clone = panel.clone();
        panel.borrow().autoeq_button.connect_clicked({
            let panel_clone = panel.clone();
            move |_| {
                // Move the callback out before calling it: the window's handler
                // needs to borrow this same panel (to save the preset and
                // refresh the list), so holding the borrow here would panic.
                let callback = panel_clone.borrow_mut().autoeq_callback.take();
                match callback {
                    Some(cb) => {
                        cb();
                        panel_clone.borrow_mut().autoeq_callback = Some(cb);
                    }
                    None => log::warn!("AutoEq requested but no handler is registered"),
                }
            }
        });

        panel.borrow().import_button.connect_clicked(move |_| {
            let dialog = gtk4::FileChooserDialog::new(
                Some("Import APO Preset"),
                gtk4::Window::NONE,
                gtk4::FileChooserAction::Open,
                &[
                    ("Cancel", gtk4::ResponseType::Cancel),
                    ("Import", gtk4::ResponseType::Accept),
                ],
            );
            dialog.set_modal(true);

            let filter = gtk4::FileFilter::new();
            filter.add_pattern("*.apo");
            filter.add_pattern("*.txt");
            filter.set_name(Some("APO Presets"));
            dialog.add_filter(&filter);

            let list_box_for_import = panel_clone.borrow().list_box.clone();
            dialog.connect_response(move |d, response| {
                if response == gtk4::ResponseType::Accept {
                    if let Some(path) = d.file().and_then(|f| f.path()) {
                        let stem = path
                            .file_stem()
                            .map(|s| s.to_string_lossy().to_string())
                            .unwrap_or_else(|| "imported".to_string());
                        match parse_apo_file(&path) {
                            Ok((preamp, bands)) => {
                                // Name it after the source file rather than
                                // "imported_N", and let the user change it.
                                let list_box = list_box_for_import.clone();
                                prompt_preset_name(
                                    "Import APO Preset",
                                    "Import",
                                    &stem,
                                    move |name| {
                                        let sanitized = sanitize_preset_name(&name);
                                        let dest = preset_path_for_name(&sanitized);
                                        let _ = save_preset_to_file(&dest, &bands, preamp);
                                        refresh_preset_list(&list_box);
                                    },
                                );
                            }
                            // Surfaced rather than swallowed: a silently
                            // ignored bad file is indistinguishable from an
                            // import that never ran.
                            Err(err) => {
                                // adw::Alert needs libadwaita 1.8 and we
                                // are gated at v1_7, so use a plain
                                // labelled dialog.
                                let msg = gtk4::Label::new(Some(&format!(
                                    "Could not import this APO preset:\n\n{err}"
                                )));
                                msg.set_wrap(true);
                                msg.set_xalign(0.0);
                                let close = gtk4::Button::with_label("OK");
                                close.add_css_class("suggested-action");
                                let wrap = gtk4::Box::new(gtk4::Orientation::Vertical, 14);
                                wrap.set_margin_start(18);
                                wrap.set_margin_end(18);
                                wrap.set_margin_top(18);
                                wrap.set_margin_bottom(18);
                                wrap.append(&msg);
                                let btn_row = gtk4::Box::new(gtk4::Orientation::Horizontal, 0);
                                btn_row.set_halign(gtk4::Align::End);
                                btn_row.append(&close);
                                wrap.append(&btn_row);
                                let err_dialog = adw::Dialog::new();
                                err_dialog.set_title("Import failed");
                                err_dialog.set_content_width(420);
                                err_dialog.set_child(Some(&wrap));
                                err_dialog.set_default_widget(Some(&close));
                                let ed = err_dialog.clone();
                                close.connect_clicked(move |_| {
                                    ed.close();
                                });
                                err_dialog.present(None::<&gtk4::Widget>);
                            }
                        }
                    }
                }
                d.close();
            });

            dialog.show();
        });

        // --- Export: selected preset -> file.
        let panel_clone = panel.clone();
        panel.borrow().export_button.connect_clicked(move |_| {
            let list_box = panel_clone.borrow().list_box.clone();
            let Some(row) = list_box.selected_row() else {
                return;
            };
            let Some(name) = row_name(&row) else { return };
            let (preamp, bands) = match load_preset_by_name(&name) {
                Ok(v) => v,
                Err(_) => return,
            };
            let dialog = gtk4::FileChooserDialog::new(
                Some("Export Preset"),
                gtk4::Window::NONE,
                gtk4::FileChooserAction::Save,
                &[
                    ("Cancel", gtk4::ResponseType::Cancel),
                    ("Export", gtk4::ResponseType::Accept),
                ],
            );
            dialog.set_modal(true);
            dialog.set_current_name(&format!("{name}.json"));
            dialog.connect_response(move |d, response| {
                if response == gtk4::ResponseType::Accept {
                    if let Some(file) = d.file() {
                        if let Some(path) = file.path() {
                            let _ = save_preset_to_file(&path, &bands, preamp);
                        }
                    }
                }
                d.close();
            });
            dialog.show();
        });

        // --- Select a row -> load that preset.
        let panel_clone = panel.clone();
        panel.borrow().list_box.connect_row_selected(move |_, row| {
            let Some(row) = row else { return };
            let Some(name) = row_name(row) else { return };
            {
                let mut panel_mut = panel_clone.borrow_mut();
                if let Ok((preamp, bands)) = load_preset_by_name(&name) {
                    if let Some(ref apply) = panel_mut.apply_bands_callback {
                        apply(bands.clone(), preamp);
                    }
                    panel_mut.current_bands = bands;
                    panel_mut.current_preamp_db = preamp;
                    panel_mut.current_preset_name = Some(name.clone());
                    panel_mut.saved_signature = Some(
                        panel_mut
                            .get_signature_callback
                            .as_ref()
                            .map(|f| f())
                            .unwrap_or_default(),
                    );
                    panel_mut.set_curve_revert_baseline(Some(name));
                    panel_mut.update_state_chip();
                }
            }
        });

        panel
    }

    pub fn set_callbacks(
        &mut self,
        apply_bands: Option<Box<dyn Fn(Vec<crate::core::EqBand>, f64)>>,
        reset: Option<Box<dyn Fn()>>,
        get_signature: Option<Box<dyn Fn() -> String>>,
        current_state: Option<Box<dyn Fn() -> (Vec<crate::core::EqBand>, f64)>>,
    ) {
        self.apply_bands_callback = apply_bands;
        self.reset_callback = reset;
        self.get_signature_callback = get_signature;
        self.current_state_callback = current_state;
    }

    /// The live EQ state, falling back to the last loaded preset when the
    /// window has not wired a reader (headless callers, tests).
    fn live_state(&self) -> (Vec<crate::core::EqBand>, f64) {
        match self.current_state_callback.as_ref() {
            Some(f) => f(),
            None => (self.current_bands.clone(), self.current_preamp_db),
        }
    }

    pub fn set_default_signature(&mut self, signature: String) {
        self.default_signature = Some(signature);
    }

    /// Register the handler that opens the AutoEq import dialog.
    ///
    /// This is what makes the feature reachable: `window_autoeq.rs` builds a
    /// complete dialog (search, results, curve preview) but until something
    /// constructs it, none of it can be used.
    pub fn set_autoeq_callback(&mut self, cb: Box<dyn Fn()>) {
        self.autoeq_callback = Some(cb);
    }

    /// The currently-loaded preset name, if any (used by the Output Controls
    /// Fallback / Link-to-Output actions).
    pub fn current_preset_name(&self) -> Option<String> {
        self.current_preset_name.clone()
    }

    pub fn start_file_monitoring(&mut self) {
        let dir = crate::core::ensure_preset_storage_dir();
        let file = gio::File::for_path(&dir);
        if let Ok(monitor) =
            file.monitor_directory(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE)
        {
            let list_box = self.list_box.clone();
            let handler = monitor.connect_changed(move |_, _, _, _| {
                refresh_preset_list(&list_box);
            });
            self.file_monitor = Some(handler);
        }
    }

    pub fn refresh_list(&self) {
        refresh_preset_list(&self.list_box);
    }

    /// Signature of the curve as it stands, via the window's reader.
    fn signature_of_current(&self) -> String {
        self.get_signature_callback
            .as_ref()
            .map(|f| f())
            .unwrap_or_default()
    }

    pub fn update_state_chip(&mut self) {
        let signature = self
            .get_signature_callback
            .as_ref()
            .map(|f| f())
            .unwrap_or_default();
        let current_name = self.current_preset_name.as_deref();
        let saved_sig = self.saved_signature.as_deref();

        if current_name.is_some() && saved_sig == Some(signature.as_str()) {
            self.state_chip.set_text("Saved");
            self.state_chip
                .set_css_classes(&["preset-state-chip", "preset-state-chip-saved"]);
        } else if current_name.is_some() {
            self.state_chip.set_text("Modified");
            self.state_chip
                .set_css_classes(&["preset-state-chip", "preset-state-chip-modified"]);
        } else if self.default_signature.as_deref() == Some(signature.as_str()) {
            self.state_chip.set_text("Neutral");
            self.state_chip
                .set_css_classes(&["preset-state-chip", "preset-state-chip-neutral"]);
        } else {
            self.state_chip.set_text("Unsaved");
            self.state_chip
                .set_css_classes(&["preset-state-chip", "preset-state-chip-unsaved"]);
        }
    }

    pub fn load_library_preset(&mut self, name: &str) -> anyhow::Result<()> {
        let preset_name = sanitize_preset_name(name);
        if preset_name.is_empty() {
            anyhow::bail!("preset name is empty");
        }
        let (preamp, bands) = load_preset_by_name(&preset_name)?;
        if let Some(ref apply) = self.apply_bands_callback {
            apply(bands.clone(), preamp);
        }
        self.current_bands = bands;
        self.current_preamp_db = preamp;
        self.current_preset_name = Some(preset_name.clone());
        self.saved_signature = Some(
            self.get_signature_callback
                .as_ref()
                .map(|f| f())
                .unwrap_or_default(),
        );
        self.set_curve_revert_baseline(Some(preset_name));
        self.update_state_chip();
        Ok(())
    }

    pub fn reset_to_neutral(&mut self) {
        if let Some(ref reset) = self.reset_callback {
            reset();
        }
        self.current_preset_name = None;
        self.saved_signature = None;
        self.set_curve_revert_baseline(None);
        self.update_state_chip();
    }

    pub fn revert_to_baseline(&mut self) {
        if let Some(ref payload) = self.revert_baseline_payload {
            if let (Some(preamp), Ok(bands)) = (
                payload.get("preamp_db").and_then(|v| v.as_f64()),
                crate::core::preset_payload_bands(payload),
            ) {
                if let Some(ref apply) = self.apply_bands_callback {
                    apply(bands.clone(), preamp);
                }
                self.current_bands = bands;
                self.current_preamp_db = preamp;
                self.current_preset_name = self.revert_baseline_label.clone();
                self.saved_signature = self.revert_baseline_signature.clone();
                self.update_state_chip();
            }
        }
    }

    fn set_curve_revert_baseline(&mut self, label: Option<String>) {
        self.revert_baseline_label = label;
        self.revert_baseline_signature = self.get_signature_callback.as_ref().map(|f| f());
        let bands = self.current_bands.clone();
        let preamp = self.current_preamp_db;
        self.revert_baseline_payload = Some(crate::core::preset_payload(&bands, preamp));
    }

    pub fn widget(&self) -> &gtk4::Box {
        &self.container
    }
}

/// Load a preset by name: built-in presets come from code, custom presets
/// from the preset directory.
fn load_preset_by_name(name: &str) -> anyhow::Result<(f64, Vec<crate::core::EqBand>)> {
    if is_builtin_preset(name) {
        if let Some((bands, preamp)) = crate::core::builtin_preset_bands(name) {
            return Ok((preamp, bands));
        }
    }
    let path = preset_path_for_name(name);
    load_preset_from_file(&path)
}

/// Ask the user for a preset name.
///
/// GTK4 has no synchronous modal, so the confirmed name is delivered
/// through `on_ok`. An empty/whitespace name is rejected in place rather
/// than silently saving as something the user did not type.
///
/// `AdwDialog` has no `add_action` (unlike `GtkDialog`), so the buttons
/// live inside the content box, and `set_title` takes a plain `&str`
/// rather than an `Option`.
fn prompt_preset_name(
    title: &str,
    ok_label: &str,
    initial: &str,
    on_ok: impl Fn(String) + 'static,
) {
    let dialog = adw::Dialog::new();
    dialog.set_title(title);
    dialog.set_content_width(380);

    let entry = gtk4::Entry::new();
    entry.set_text(initial);
    entry.set_placeholder_text(Some("Preset name"));
    entry.set_activates_default(true);

    let cancel = gtk4::Button::with_label("Cancel");
    let ok = gtk4::Button::with_label(ok_label);
    ok.add_css_class("suggested-action");

    let buttons = gtk4::Box::new(gtk4::Orientation::Horizontal, 8);
    buttons.set_halign(gtk4::Align::End);
    buttons.append(&cancel);
    buttons.append(&ok);

    let content = gtk4::Box::new(gtk4::Orientation::Vertical, 12);
    content.set_margin_start(18);
    content.set_margin_end(18);
    content.set_margin_top(18);
    content.set_margin_bottom(18);
    content.append(&entry);
    content.append(&buttons);
    dialog.set_child(Some(&content));
    dialog.set_default_widget(Some(&ok));

    {
        let dialog_cancel = dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog_cancel.close();
        });
    }
    {
        let entry_ok = entry.clone();
        let dialog_ok = dialog.clone();
        ok.connect_clicked(move |_| {
            let name = entry_ok.text().trim().to_string();
            if name.is_empty() {
                entry_ok.grab_focus();
                return;
            }
            on_ok(name);
            dialog_ok.close();
        });
    }

    // AdwDialog has no `presented` signal; point its focus at the entry so
    // typing starts immediately.
    dialog.set_focus(Some(&entry));
    dialog.present(None::<&gtk4::Widget>);
}

/// Extract the preset name stored on a list row (set by `refresh_preset_list`).
fn row_name(row: &gtk4::ListBoxRow) -> Option<String> {
    row.child()
        .and_downcast::<gtk4::Label>()
        .map(|l| l.label().to_string())
}

/// Count only the CUSTOM (non-built-in) preset rows, so new custom presets
/// get sequential names that don't collide with built-ins.
fn count_custom_children(list_box: &gtk4::ListBox) -> usize {
    let mut n = 0;
    let mut child = list_box.first_child();
    while let Some(c) = child {
        if let Some(row) = c.downcast_ref::<gtk4::ListBoxRow>()
            && let Some(name) = row_name(row)
            && !is_builtin_preset(&name)
        {
            n += 1;
        }
        child = c.next_sibling();
    }
    n
}

/// Rebuild the preset ListBox: built-in presets first (marked), then custom
/// presets. The row's label text IS the preset name (used by `row_name`).
/// Rebuild the preset rows. Public so the window can refresh the list after an
/// AutoEq import adds a new preset.
pub fn refresh_preset_list(list_box: &gtk4::ListBox) {
    let selected = list_box.selected_row().and_then(|r| row_name(&r));
    while let Some(child) = list_box.first_child() {
        list_box.remove(&child);
    }

    let mut rows: Vec<(String, bool)> = Vec::new();
    for name in BUILTIN_PRESET_NAMES {
        rows.push(((*name).to_string(), true));
    }
    for name in list_preset_names() {
        if !is_builtin_preset(&name) {
            rows.push((name, false));
        }
    }

    for (name, builtin) in rows {
        let row = gtk4::ListBoxRow::new();
        let label = gtk4::Label::new(Some(&name));
        label.set_halign(gtk4::Align::Start);
        if builtin {
            label.set_css_classes(&["preset-builtin-label"]);
            label.set_tooltip_text(Some("Built-in preset (cannot be removed)"));
        }
        row.set_child(Some(&label));
        list_box.append(&row);
        if selected.as_deref() == Some(name.as_str()) {
            list_box.select_row(Some(&row));
        }
    }
}

/// Get the preset storage directory.
pub fn preset_storage_dir() -> PathBuf {
    crate::core::preset_storage_dir()
}

/// List all custom preset names (de-duplicated, case-insensitively sorted).
pub fn list_preset_names() -> Vec<String> {
    crate::core::list_preset_names()
}
