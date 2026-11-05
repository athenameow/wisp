//! wisp: the history popup. Runs only while open; launching it again
//! while it is open closes it, so one keybind toggles it.

mod settings;

use gtk::glib::BoxedAnyObject;
use gtk::{gdk, gio, glib, pango, prelude::*};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use wisp::config::Config;
use wisp::ipc;
use wisp::store::{Entry, Kind, Store};

const APP_ID: &str = "io.athena.wisp";
const LIST_WIDTH: i32 = 320;
const PREVIEW_WIDTH: i32 = 340;
const HEIGHT: i32 = 420;
/// Characters of text shown in the preview pane.
const PREVIEW_CHARS: usize = 100_000;

pub struct Ui {
    window: gtk::ApplicationWindow,
    store: Store,
    config: RefCell<Config>,
    model: gio::ListStore,
    filter: gtk::CustomFilter,
    selection: gtk::SingleSelection,
    list: gtk::ListView,
    list_stack: gtk::Stack,
    empty_label: gtk::Label,
    search: gtk::SearchEntry,
    status: gtk::Label,
    preview_button: gtk::ToggleButton,
    preview: gtk::Box,
    preview_stack: gtk::Stack,
    preview_text: gtk::TextView,
    preview_image: gtk::Picture,
    preview_meta: gtk::Label,
    views: gtk::Stack,
}

fn main() -> glib::ExitCode {
    // Startup tuning, each overridable by setting the variable yourself:
    // - Software rendering opens faster and uses far less memory than GL/Vulkan
    //   for a popup this small (on NVIDIA ~95 MB vs ~250 MB).
    // - Skipping xdg-desktop-portal saves ~170 ms of GTK startup; the popup uses
    //   no portal features (styling comes from its own CSS and GSettings).
    for (var, value) in [("GSK_RENDERER", "cairo"), ("GDK_DEBUG", "no-portals")] {
        if std::env::var_os(var).is_none() {
            // SAFETY: no other threads exist yet.
            unsafe { std::env::set_var(var, value) };
        }
    }
    let app = gtk::Application::builder().application_id(APP_ID).build();
    app.connect_startup(|_| load_css());
    app.connect_activate(|app| match app.windows().first() {
        Some(window) => window.close(),
        None => build(app),
    });
    // Arguments are ignored: every launch is a toggle.
    app.run_with_args::<&str>(&[])
}

fn load_css() {
    let display = gdk::Display::default().expect("no display");
    // Portals are skipped, so GTK can't learn the desktop's dark preference; the
    // popup is always dark.
    gtk::Settings::for_display(&display).set_gtk_interface_color_scheme(gtk::InterfaceColorScheme::Dark);
    let builtin = gtk::CssProvider::new();
    builtin.load_from_string(include_str!("style.css"));
    gtk::style_context_add_provider_for_display(&display, &builtin, gtk::STYLE_PROVIDER_PRIORITY_USER + 1);

    let custom = wisp::config_dir().join("style.css");
    if custom.exists() {
        let provider = gtk::CssProvider::new();
        provider.load_from_path(&custom);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_USER + 2,
        );
    }
}

fn build(app: &gtk::Application) {
    let store = match Store::open() {
        Ok(store) => store,
        Err(e) => {
            eprintln!("wisp: {e:#}");
            return;
        }
    };
    let config = Config::load();

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Clipboard")
        .build();
    window.add_css_class("wisp");
    window.init_layer_shell();
    window.set_layer(Layer::Overlay);
    window.set_namespace(Some("wisp"));
    // Like a shell panel: the surface covers the monitor (transparent outside
    // the card) so a click outside closes it, and it holds the keyboard while
    // open so Esc and typing always reach it. Picking an entry closes it.
    for edge in [Edge::Top, Edge::Bottom, Edge::Left, Edge::Right] {
        window.set_anchor(edge, true);
    }
    window.set_keyboard_mode(KeyboardMode::Exclusive);

    // Header: search, status, preview toggle, settings.
    let search = gtk::SearchEntry::builder()
        .placeholder_text("Search")
        .hexpand(true)
        .build();
    let status = gtk::Label::builder()
        .css_classes(["status"])
        .visible(false)
        .build();
    let preview_button = icon_toggle("view-dual-symbolic", "Preview (Ctrl+P)");
    preview_button.set_active(config.show_preview);
    let settings_button = icon_button("emblem-system-symbolic", "Settings (Ctrl+,)");
    let header = hbox(
        6,
        &[
            search.upcast_ref(),
            status.upcast_ref(),
            preview_button.upcast_ref(),
            settings_button.upcast_ref(),
        ],
    );
    header.add_css_class("header");

    // History list.
    let model = gio::ListStore::new::<BoxedAnyObject>();
    let filter = gtk::CustomFilter::new(|_| true);
    let filtered = gtk::FilterListModel::new(Some(model.clone()), Some(filter.clone()));
    let selection = gtk::SingleSelection::new(Some(filtered));
    // Hovering a row selects it (updating the preview); a single click copies it.
    let list = gtk::ListView::builder()
        .model(&selection)
        .single_click_activate(true)
        .css_classes(["history"])
        .build();
    let scroller = gtk::ScrolledWindow::builder()
        .child(&list)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .build();
    let empty_label = gtk::Label::builder().css_classes(["dim"]).build();
    let list_stack = gtk::Stack::new();
    list_stack.add_named(&scroller, Some("list"));
    list_stack.add_named(&empty_label, Some("empty"));
    list_stack.set_size_request(LIST_WIDTH, -1);

    // Preview pane.
    let preview_text = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();
    let preview_image = gtk::Picture::builder()
        .content_fit(gtk::ContentFit::Contain)
        .build();
    let preview_stack = gtk::Stack::builder().vexpand(true).build();
    preview_stack.add_named(
        &gtk::ScrolledWindow::builder().child(&preview_text).build(),
        Some("text"),
    );
    // Never-scrolling wrapper so large images shrink to fit instead of growing the window.
    let image_box = gtk::ScrolledWindow::builder()
        .child(&preview_image)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Never)
        .build();
    preview_stack.add_named(&image_box, Some("image"));
    let preview_meta = gtk::Label::builder()
        .xalign(0.0)
        .wrap(true)
        .css_classes(["dim", "meta"])
        .build();
    let preview = vbox(8, &[preview_stack.upcast_ref(), preview_meta.upcast_ref()]);
    preview.add_css_class("preview");
    preview.set_size_request(PREVIEW_WIDTH, -1);
    preview.set_visible(config.show_preview);

    let body = hbox(10, &[list_stack.upcast_ref(), preview.upcast_ref()]);
    body.set_vexpand(true);
    let history = vbox(10, &[header.upcast_ref(), body.upcast_ref()]);

    let views = gtk::Stack::builder()
        .transition_type(gtk::StackTransitionType::Crossfade)
        .build();
    views.add_named(&history, Some("history"));

    let card = gtk::Box::builder()
        .css_classes(["card"])
        .height_request(HEIGHT)
        .halign(gtk::Align::Center)
        .valign(gtk::Align::Center)
        .build();
    card.append(&views);
    window.set_child(Some(&card));

    let outside = gtk::GestureClick::new();
    let (weak_window, weak_card) = (window.downgrade(), card.downgrade());
    outside.connect_pressed(move |_, _, x, y| {
        let (Some(window), Some(card)) = (weak_window.upgrade(), weak_card.upgrade()) else {
            return;
        };
        let point = gtk::graphene::Point::new(x as f32, y as f32);
        if card
            .compute_bounds(&window)
            .is_some_and(|b| !b.contains_point(&point))
        {
            window.close();
        }
    });
    window.add_controller(outside);

    let ui = Rc::new(Ui {
        window,
        store,
        config: RefCell::new(config),
        model,
        filter,
        selection,
        list,
        list_stack,
        empty_label,
        search,
        status,
        preview_button,
        preview,
        preview_stack,
        preview_text,
        preview_image,
        preview_meta,
        views,
    });

    ui.views.add_named(&settings::build(&ui), Some("settings"));
    ui.list.set_factory(Some(&row_factory(Rc::downgrade(&ui))));
    connect_signals(&ui, &settings_button);
    ui.reload();
    ui.update_status(ipc::send("ping").err().map(|_| "Daemon not running"));
    // Handlers hold weak references; the window owns the state until it is destroyed.
    let owner = ui.clone();
    ui.window.connect_destroy(move |_| {
        let _ = &owner;
    });
    // wisp holds the keyboard while open, so a paste meant for another app would
    // land here instead. If something new is copied meanwhile (a screenshot, say),
    // close so the keyboard goes back to that app. GTK also reports the existing
    // clipboard right after opening; that is not a new copy, hence the grace period.
    let opened = std::time::Instant::now();
    let weak = ui.window.downgrade();
    WidgetExt::display(&ui.window)
        .clipboard()
        .connect_changed(move |_| {
            if opened.elapsed() > std::time::Duration::from_millis(400)
                && let Some(window) = weak.upgrade()
            {
                window.close();
            }
        });
    ui.window.present();
    ui.search.grab_focus();
}

fn connect_signals(ui: &Rc<Ui>, settings_button: &gtk::Button) {
    let filter_ui = Rc::downgrade(ui);
    ui.filter.set_filter_func(move |obj| {
        let Some(ui) = filter_ui.upgrade() else {
            return true;
        };
        let query = ui.search.text().to_lowercase();
        let entry = obj.downcast_ref::<BoxedAnyObject>().unwrap().borrow::<Entry>();
        query.is_empty()
            || match entry.kind {
                Kind::Text => entry.snippet.to_lowercase().contains(&query),
                Kind::Image => "image".contains(&query) || entry.mime.contains(&query),
            }
    });

    let weak = Rc::downgrade(ui);
    ui.search.connect_search_changed(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.filter.changed(gtk::FilterChange::Different);
            ui.select(0);
        }
    });

    let weak = Rc::downgrade(ui);
    ui.selection.connect_selected_item_notify(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.show_preview();
        }
    });

    let weak = Rc::downgrade(ui);
    ui.selection.connect_items_changed(move |selection, _, _, _| {
        if let Some(ui) = weak.upgrade() {
            let empty = selection.n_items() == 0;
            ui.list_stack
                .set_visible_child_name(if empty { "empty" } else { "list" });
            ui.empty_label.set_label(if ui.search.text().is_empty() {
                "Nothing copied yet"
            } else {
                "No matches"
            });
        }
    });

    let weak = Rc::downgrade(ui);
    ui.list.connect_activate(move |_, position| {
        if let Some(ui) = weak.upgrade() {
            ui.copy(position);
        }
    });

    let weak = Rc::downgrade(ui);
    ui.preview_button.connect_toggled(move |button| {
        if let Some(ui) = weak.upgrade() {
            ui.set_preview(button.is_active());
        }
    });

    let weak = Rc::downgrade(ui);
    settings_button.connect_clicked(move |_| {
        if let Some(ui) = weak.upgrade() {
            ui.toggle_settings();
        }
    });

    // Keys are handled before the focused widget sees them, so the search
    // entry keeps focus while arrows, Enter and Delete drive the list.
    let keys = gtk::EventControllerKey::new();
    keys.set_propagation_phase(gtk::PropagationPhase::Capture);
    let weak = Rc::downgrade(ui);
    keys.connect_key_pressed(move |_, key, _, mods| {
        let Some(ui) = weak.upgrade() else {
            return glib::Propagation::Proceed;
        };
        ui.on_key(key, mods.contains(gdk::ModifierType::CONTROL_MASK))
    });
    ui.window.add_controller(keys);
    // Added after the key handler above so Esc, arrows and Enter are seen first;
    // other typing goes to the search entry.
    ui.search.set_key_capture_widget(Some(&ui.window));
}

impl Ui {
    fn on_key(&self, key: gdk::Key, ctrl: bool) -> glib::Propagation {
        let in_history = self.views.visible_child_name().as_deref() == Some("history");
        match key {
            gdk::Key::Escape => self.window.close(),
            gdk::Key::comma if ctrl => self.toggle_settings(),
            gdk::Key::p if ctrl && in_history => {
                self.preview_button.set_active(!self.preview_button.is_active())
            }
            gdk::Key::Down if in_history => self.move_selection(1),
            gdk::Key::Up if in_history => self.move_selection(-1),
            gdk::Key::Page_Down if in_history => self.move_selection(8),
            gdk::Key::Page_Up if in_history => self.move_selection(-8),
            gdk::Key::Return | gdk::Key::KP_Enter if in_history => self.copy(self.selection.selected()),
            gdk::Key::Delete | gdk::Key::KP_Delete if in_history => self.delete_selected(),
            // Only text can be pasted into the search box; swallow Ctrl+V otherwise
            // instead of letting GTK ring the error bell.
            gdk::Key::v if ctrl && !self.clipboard_has_text() => {}
            _ => return glib::Propagation::Proceed,
        }
        glib::Propagation::Stop
    }

    fn clipboard_has_text(&self) -> bool {
        WidgetExt::display(&self.window)
            .clipboard()
            .formats()
            .contains_type(glib::Type::STRING)
    }

    /// Reloads all entries from the database.
    pub fn reload(&self) {
        let entries = self.store.list().unwrap_or_else(|e| {
            eprintln!("wisp: {e:#}");
            Vec::new()
        });
        let items: Vec<BoxedAnyObject> = entries.into_iter().map(BoxedAnyObject::new).collect();
        self.model.splice(0, self.model.n_items(), &items);
        self.select(0);
    }

    fn entry_at(&self, position: u32) -> Option<Entry> {
        let obj = self.selection.item(position)?.downcast::<BoxedAnyObject>().ok()?;
        let entry = obj.borrow::<Entry>().clone();
        Some(entry)
    }

    fn select(&self, position: u32) {
        if position < self.selection.n_items() {
            self.selection.set_selected(position);
            self.list.scroll_to(position, gtk::ListScrollFlags::NONE, None);
        }
        self.show_preview();
    }

    fn move_selection(&self, delta: i32) {
        let n = self.selection.n_items();
        if n == 0 {
            return;
        }
        let current = match self.selection.selected() {
            gtk::INVALID_LIST_POSITION => 0,
            p => p as i32,
        };
        self.select((current + delta).clamp(0, n as i32 - 1) as u32);
    }

    /// Puts the entry on the clipboard (the daemon serves it) and closes.
    fn copy(&self, position: u32) {
        let Some(entry) = self.entry_at(position) else {
            return;
        };
        match ipc::send(&format!("copy {}", entry.id)) {
            Ok(()) => self.window.close(),
            Err(e) => self.update_status(Some(&e.to_string())),
        }
    }

    fn delete_selected(&self) {
        let Some(entry) = self.entry_at(self.selection.selected()) else {
            return;
        };
        if let Err(e) = self.store.delete(entry.id) {
            self.update_status(Some(&e.to_string()));
            return;
        }
        let position = (0..self.model.n_items()).find(|&i| {
            self.model
                .item(i)
                .and_downcast::<BoxedAnyObject>()
                .is_some_and(|obj| obj.borrow::<Entry>().id == entry.id)
        });
        if let Some(position) = position {
            self.model.remove(position);
        }
        self.show_preview();
    }

    fn set_preview(&self, visible: bool) {
        self.preview.set_visible(visible);
        self.show_preview();
        let mut config = self.config.borrow_mut();
        if config.show_preview != visible {
            config.show_preview = visible;
            let _ = config.save();
        }
    }

    fn show_preview(&self) {
        if !self.preview_button.is_active() {
            return;
        }
        let Some(entry) = self.entry_at(self.selection.selected()) else {
            self.preview_text.buffer().set_text("");
            self.preview_image.set_paintable(None::<&gdk::Paintable>);
            self.preview_meta.set_label("");
            self.preview_stack.set_visible_child_name("text");
            return;
        };
        let detail = match entry.kind {
            Kind::Text => {
                let text = self.store.text(entry.id).unwrap_or_default();
                let chars = text.chars().count();
                let shown: String = text.chars().take(PREVIEW_CHARS).collect();
                self.preview_text.buffer().set_text(&shown);
                self.preview_stack.set_visible_child_name("text");
                format!("{chars} characters, {} lines", text.lines().count().max(1))
            }
            Kind::Image => {
                let texture = gdk::Texture::from_filename(self.store.image_path(&entry.hash)).ok();
                self.preview_image.set_paintable(texture.as_ref());
                self.preview_stack.set_visible_child_name("image");
                format!(
                    "{} × {} {}",
                    entry.width,
                    entry.height,
                    entry.mime.trim_start_matches("image/").to_uppercase()
                )
            }
        };
        self.preview_meta.set_label(&format!(
            "Copied {}\n{detail} · {}",
            format_long_time(entry.copied_at),
            format_size(entry.size)
        ));
    }

    pub fn toggle_settings(&self) {
        let to_settings = self.views.visible_child_name().as_deref() == Some("history");
        self.views
            .set_visible_child_name(if to_settings { "settings" } else { "history" });
        if !to_settings {
            self.search.grab_focus();
        }
    }

    /// Shows an error, or "Paused" while recording is off.
    pub fn update_status(&self, error: Option<&str>) {
        let paused = !self.config.borrow().history_enabled;
        let text = error.unwrap_or(if paused { "Paused" } else { "" });
        self.status.set_label(text);
        self.status.set_visible(!text.is_empty());
        self.status.set_css_classes(if error.is_some() {
            &["status", "error"]
        } else {
            &["status"]
        });
    }
}

fn row_factory(ui: Weak<Ui>) -> gtk::SignalListItemFactory {
    let factory = gtk::SignalListItemFactory::new();
    factory.connect_setup(|_, item| {
        let thumb = gtk::Image::builder()
            .pixel_size(32)
            .css_classes(["thumb"])
            .build();
        // max_width_chars(1) keeps long text from widening the window; it ellipsizes instead.
        let title = gtk::Label::builder()
            .xalign(0.0)
            .single_line_mode(true)
            .max_width_chars(1)
            .hexpand(true)
            .ellipsize(pango::EllipsizeMode::End)
            .build();
        let time = gtk::Label::builder()
            .xalign(0.0)
            .css_classes(["dim", "time"])
            .build();
        let text = vbox(2, &[title.upcast_ref(), time.upcast_ref()]);
        text.set_valign(gtk::Align::Center);
        text.set_hexpand(true);
        let row = hbox(10, &[thumb.upcast_ref(), text.upcast_ref()]);
        item.downcast_ref::<gtk::ListItem>()
            .unwrap()
            .set_child(Some(&row));
    });
    factory.connect_bind(move |_, item| {
        let Some(ui) = ui.upgrade() else { return };
        let item = item.downcast_ref::<gtk::ListItem>().unwrap();
        let obj = item.item().and_downcast::<BoxedAnyObject>().unwrap();
        let entry = obj.borrow::<Entry>();
        let row = item.child().unwrap();
        let thumb = row.first_child().and_downcast::<gtk::Image>().unwrap();
        let text = row.last_child().unwrap();
        let title = text.first_child().and_downcast::<gtk::Label>().unwrap();
        let time = text.last_child().and_downcast::<gtk::Label>().unwrap();

        time.set_label(&format_time(entry.copied_at));
        match entry.kind {
            Kind::Text => {
                thumb.set_visible(false);
                thumb.clear();
                let line: String = entry
                    .snippet
                    .split_whitespace()
                    .take(60)
                    .collect::<Vec<_>>()
                    .join(" ");
                title.set_label(&line);
            }
            Kind::Image => {
                let texture = gdk::Texture::from_filename(ui.store.thumb_path(&entry.hash)).ok();
                thumb.set_paintable(texture.as_ref());
                thumb.set_visible(true);
                title.set_label(&format!("Image  {} × {}", entry.width, entry.height));
            }
        }
    });
    factory
}

/// "Today 14:32", "Yesterday 09:10", "3 Nov 18:22", "3 Nov 2025 18:22".
fn format_time(ms: i64) -> String {
    let (Ok(time), Ok(now)) = (
        glib::DateTime::from_unix_local(ms / 1000),
        glib::DateTime::now_local(),
    ) else {
        return String::new();
    };
    let same_day =
        |a: &glib::DateTime, b: &glib::DateTime| a.year() == b.year() && a.day_of_year() == b.day_of_year();
    let format = if same_day(&time, &now) {
        "Today %H:%M"
    } else if now
        .add_days(-1)
        .is_ok_and(|yesterday| same_day(&time, &yesterday))
    {
        "Yesterday %H:%M"
    } else if time.year() == now.year() {
        "%-d %b %H:%M"
    } else {
        "%-d %b %Y %H:%M"
    };
    time.format(format).map(String::from).unwrap_or_default()
}

/// "Tuesday 5 November 2026, 14:32:07"
fn format_long_time(ms: i64) -> String {
    glib::DateTime::from_unix_local(ms / 1000)
        .and_then(|t| t.format("%A %-d %B %Y, %H:%M:%S"))
        .map(String::from)
        .unwrap_or_default()
}

fn format_size(bytes: i64) -> String {
    match bytes {
        b if b < 1024 => format!("{b} B"),
        b if b < 1024 * 1024 => format!("{:.1} KB", b as f64 / 1024.0),
        b => format!("{:.1} MB", b as f64 / (1024.0 * 1024.0)),
    }
}

pub fn hbox(spacing: i32, children: &[&gtk::Widget]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Horizontal, spacing);
    children.iter().for_each(|c| b.append(*c));
    b
}

pub fn vbox(spacing: i32, children: &[&gtk::Widget]) -> gtk::Box {
    let b = gtk::Box::new(gtk::Orientation::Vertical, spacing);
    children.iter().for_each(|c| b.append(*c));
    b
}

pub fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    gtk::Button::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .css_classes(["flat", "icon"])
        .build()
}

fn icon_toggle(icon: &str, tooltip: &str) -> gtk::ToggleButton {
    gtk::ToggleButton::builder()
        .icon_name(icon)
        .tooltip_text(tooltip)
        .css_classes(["flat", "icon"])
        .build()
}
