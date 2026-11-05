//! The settings view, shown in place of the history list.

use super::{Ui, hbox, icon_button, vbox};
use gtk::{glib, prelude::*};
use std::cell::Cell;
use std::rc::Rc;
use wisp::config::Config;
use wisp::ipc;

pub fn build(ui: &Rc<Ui>) -> gtk::Box {
    let config = ui.config.borrow().clone();

    let back = icon_button("go-previous-symbolic", "Back (Ctrl+,)");
    let title = gtk::Label::builder()
        .label("Settings")
        .css_classes(["title"])
        .build();
    let header = hbox(6, &[back.upcast_ref(), title.upcast_ref()]);
    header.add_css_class("header");

    let record = gtk::Switch::builder()
        .active(config.history_enabled)
        .valign(gtk::Align::Center)
        .build();
    let size = gtk::SpinButton::with_range(10.0, 10_000.0, 10.0);
    size.set_value(config.max_entries as f64);
    size.set_valign(gtk::Align::Center);
    let preview = gtk::Switch::builder().valign(gtk::Align::Center).build();
    let keep = gtk::Switch::builder()
        .active(config.keep_clipboard)
        .valign(gtk::Align::Center)
        .build();
    let clear = gtk::Button::builder()
        .label("Clear")
        .valign(gtk::Align::Center)
        .css_classes(["destructive"])
        .build();

    let rows = vbox(
        2,
        &[
            row("Record history", "Save new clipboard contents", &record).upcast_ref(),
            row("History size", "Older entries beyond this are deleted", &size).upcast_ref(),
            row("Preview pane", "Show the preview when the popup opens", &preview).upcast_ref(),
            row(
                "Keep clipboard",
                "Restore the newest entry when the app you copied from closes",
                &keep,
            )
            .upcast_ref(),
            row("Clear history", "Delete every saved entry", &clear).upcast_ref(),
        ],
    );
    let location = gtk::Label::builder()
        .label(tilde(&wisp::data_dir()))
        .xalign(0.0)
        .vexpand(true)
        .valign(gtk::Align::End)
        .selectable(true)
        .css_classes(["dim", "meta"])
        .build();

    let weak = Rc::downgrade(ui);
    back.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.toggle_settings();
        }
    });

    let weak = Rc::downgrade(ui);
    record.connect_active_notify(move |switch| {
        if let Some(ui) = weak.upgrade() {
            update(&ui, |c| c.history_enabled = switch.is_active());
        }
    });

    let weak = Rc::downgrade(ui);
    size.connect_value_changed(move |spin| {
        if let Some(ui) = weak.upgrade() {
            let max = spin.value() as u32;
            update(&ui, |c| c.max_entries = max);
            if ui.store.prune(max).is_ok() {
                ui.reload();
            }
        }
    });

    // Same setting as the preview button in the header (Ctrl+P), which saves it.
    ui.preview_button
        .bind_property("active", &preview, "active")
        .bidirectional()
        .sync_create()
        .build();

    let weak = Rc::downgrade(ui);
    keep.connect_active_notify(move |switch| {
        if let Some(ui) = weak.upgrade() {
            update(&ui, |c| c.keep_clipboard = switch.is_active());
        }
    });

    // First click arms the button, a second click within 3 s clears.
    let armed = Rc::new(Cell::new(false));
    let weak = Rc::downgrade(ui);
    clear.connect_clicked(move |button| {
        let Some(ui) = weak.upgrade() else { return };
        if !armed.replace(true) {
            button.set_label("Confirm");
            let (button, armed) = (button.clone(), armed.clone());
            glib::timeout_add_seconds_local_once(3, move || {
                armed.set(false);
                button.set_label("Clear");
            });
            return;
        }
        armed.set(false);
        button.set_label("Clear");
        match ui.store.clear() {
            Ok(()) => ui.reload(),
            Err(e) => ui.update_status(Some(&e.to_string())),
        }
    });

    let view = vbox(
        10,
        &[header.upcast_ref(), rows.upcast_ref(), location.upcast_ref()],
    );
    view.add_css_class("settings");
    view
}

/// Title and description on the left, control on the right.
fn row(title: &str, description: &str, control: &impl IsA<gtk::Widget>) -> gtk::Box {
    let title = gtk::Label::builder().label(title).xalign(0.0).build();
    let description = gtk::Label::builder()
        .label(description)
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim", "meta"])
        .build();
    let text = vbox(2, &[title.upcast_ref(), description.upcast_ref()]);
    text.set_hexpand(true);
    let row = hbox(12, &[text.upcast_ref(), control.upcast_ref()]);
    row.add_css_class("setting");
    row
}

/// Applies a config change, saves it and tells the daemon to reload.
fn update(ui: &Ui, change: impl FnOnce(&mut Config)) {
    let saved = {
        let mut config = ui.config.borrow_mut();
        change(&mut config);
        config.save()
    };
    let error = saved
        .and_then(|()| ipc::send("reload"))
        .err()
        .map(|e| e.to_string());
    ui.update_status(error.as_deref());
}

/// Shows paths under the home directory as `~/...`.
fn tilde(path: &std::path::Path) -> String {
    match std::env::var_os("HOME").and_then(|home| path.strip_prefix(home).ok().map(|p| p.to_owned())) {
        Some(rest) => format!("~/{}", rest.display()),
        None => path.display().to_string(),
    }
}
