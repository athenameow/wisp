//! wispd: watches the Wayland clipboard through wlr-data-control, records
//! it into the history and serves entries picked in the popup.
//!
//! A single thread sleeps in poll() on the Wayland socket and the IPC socket,
//! so the daemon uses no CPU while nothing is being copied. Reading clipboard
//! data and writing it to paste requests happens on short-lived threads.

use anyhow::{Context, bail};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;
use wayland_client::backend::{ObjectId, WaylandError};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat::WlSeat};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, event_created_child};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1::{self as device, ZwlrDataControlDeviceV1},
    zwlr_data_control_manager_v1::ZwlrDataControlManagerV1,
    zwlr_data_control_offer_v1::{self as offer, ZwlrDataControlOfferV1},
    zwlr_data_control_source_v1::{self as source, ZwlrDataControlSourceV1},
};
use wisp::config::Config;
use wisp::store::{Kind, Store};
use wisp::{ipc, now_ms};

/// Offered alongside our own selections so we don't record them again.
const MARKER_MIME: &str = "application/x-wisp";
/// Password managers set this to keep secrets out of clipboard history.
const SECRET_MIME: &str = "x-kde-passwordManagerHint";
/// Text types, in order of preference. All are offered when serving text.
const TEXT_MIMES: [&str; 5] = [
    "text/plain;charset=utf-8",
    "UTF8_STRING",
    "text/plain",
    "STRING",
    "TEXT",
];
/// Image types we can thumbnail, in order of preference.
const IMAGE_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];
/// Give up on a clipboard source that stops sending data for this long.
const READ_TIMEOUT: Timespec = Timespec {
    tv_sec: 5,
    tv_nsec: 0,
};

struct State {
    conn: Connection,
    qh: QueueHandle<State>,
    manager: ZwlrDataControlManagerV1,
    device: ZwlrDataControlDeviceV1,
    /// MIME types announced for each live offer.
    offers: HashMap<ObjectId, Vec<String>>,
    /// Data behind each selection we currently serve.
    sources: HashMap<ObjectId, Arc<Vec<u8>>>,
    store: Store,
    config: Config,
}

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("wispd: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> anyhow::Result<()> {
    let socket = ipc::socket_path();
    if UnixStream::connect(&socket).is_ok() {
        bail!("already running");
    }
    let _ = std::fs::remove_file(&socket);
    let listener = UnixListener::bind(&socket).with_context(|| format!("binding {}", socket.display()))?;
    listener.set_nonblocking(true)?;

    let conn = Connection::connect_to_env().context("connecting to Wayland")?;
    let (globals, mut queue) = registry_queue_init::<State>(&conn)?;
    let qh = queue.handle();
    let seat: WlSeat = globals.bind(&qh, 1..=1, ()).context("no wl_seat")?;
    let manager: ZwlrDataControlManagerV1 = globals
        .bind(&qh, 1..=2, ())
        .context("compositor does not support wlr-data-control")?;
    let device = manager.get_data_device(&seat, &qh, ());

    let mut state = State {
        conn: conn.clone(),
        qh,
        manager,
        device,
        offers: HashMap::new(),
        sources: HashMap::new(),
        store: Store::open()?,
        config: Config::load(),
    };
    state.store.prune(state.config.max_entries)?;

    loop {
        queue.dispatch_pending(&mut state)?;
        match conn.flush() {
            Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            other => other?,
        }
        let Some(guard) = queue.prepare_read() else {
            continue;
        };
        let (wayland_ready, socket_ready) = {
            let mut fds = [
                PollFd::from_borrowed_fd(guard.connection_fd(), PollFlags::IN),
                PollFd::new(&listener, PollFlags::IN),
            ];
            match poll(&mut fds, None) {
                Err(Errno::INTR) => continue,
                other => other?,
            };
            (!fds[0].revents().is_empty(), !fds[1].revents().is_empty())
        };
        if wayland_ready {
            match guard.read() {
                Err(WaylandError::Io(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {}
                other => {
                    other?;
                }
            }
        } else {
            drop(guard);
        }
        queue.dispatch_pending(&mut state)?;
        if socket_ready {
            state.accept(&listener);
        }
    }
}

impl State {
    fn on_selection(&mut self, offer: Option<ZwlrDataControlOfferV1>) {
        let Some(offer) = offer else {
            // The app that owned the clipboard is gone: hand back the newest entry.
            if self.config.keep_clipboard
                && self.config.history_enabled
                && let Err(e) = self.restore_latest()
            {
                eprintln!("wispd: could not restore clipboard: {e:#}");
            }
            return;
        };
        let mimes = self.offers.remove(&offer.id()).unwrap_or_default();
        let ours_or_secret = mimes.iter().any(|m| m == MARKER_MIME || m == SECRET_MIME);
        if self.config.history_enabled
            && !ours_or_secret
            && let Some((kind, mime)) = pick_mime(&mimes)
            && let Err(e) = self.record(&offer, kind, mime)
        {
            eprintln!("wispd: could not read clipboard: {e:#}");
        }
        offer.destroy();
    }

    /// Asks the clipboard owner for its data and stores it on a worker thread.
    fn record(&self, offer: &ZwlrDataControlOfferV1, kind: Kind, mime: &str) -> anyhow::Result<()> {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
        offer.receive(mime.to_owned(), write.as_fd());
        self.conn.flush()?;
        drop(write);

        let (mime, at) = (mime.to_owned(), now_ms());
        let (max_bytes, max_entries) = (self.config.max_item_bytes(), self.config.max_entries);
        std::thread::spawn(move || {
            let result = read_all(read, max_bytes)
                .and_then(|data| Store::open()?.add(kind, &mime, &data, at, max_entries));
            if let Err(e) = result {
                eprintln!("wispd: not recorded: {e:#}");
            }
        });
        Ok(())
    }

    fn restore_latest(&mut self) -> anyhow::Result<()> {
        if let Some(id) = self.store.latest()? {
            let (kind, mime, data) = self.store.content(id)?;
            self.serve(kind, mime, data);
        }
        Ok(())
    }

    /// Makes `data` the current clipboard selection.
    fn serve(&mut self, kind: Kind, mime: String, data: Vec<u8>) {
        let source = self.manager.create_data_source(&self.qh, ());
        match kind {
            Kind::Text => TEXT_MIMES.iter().for_each(|m| source.offer((*m).to_owned())),
            Kind::Image => source.offer(mime),
        }
        source.offer(MARKER_MIME.to_owned());
        self.sources.insert(source.id(), Arc::new(data));
        self.device.set_selection(Some(&source));
    }

    fn accept(&mut self, listener: &UnixListener) {
        while let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
            let mut line = String::new();
            let reply = match BufReader::new(&stream).read_line(&mut line) {
                Ok(_) => match self.command(line.trim()) {
                    Ok(()) => "ok".to_owned(),
                    Err(e) => format!("err {e:#}"),
                },
                Err(e) => format!("err {e}"),
            };
            let _ = stream.write_all(format!("{reply}\n").as_bytes());
        }
    }

    fn command(&mut self, line: &str) -> anyhow::Result<()> {
        match line.split_once(' ').unwrap_or((line, "")) {
            ("copy", id) => {
                let id: i64 = id.parse().context("invalid id")?;
                let (kind, mime, data) = self.store.content(id).context("no such entry")?;
                self.store.touch(id, now_ms())?;
                self.serve(kind, mime, data);
            }
            ("reload", "") => {
                self.config = Config::load();
                self.store.prune(self.config.max_entries)?;
            }
            ("ping", "") => {}
            _ => bail!("unknown command: {line}"),
        }
        Ok(())
    }
}

/// Chooses which representation of the clipboard to store.
fn pick_mime(mimes: &[String]) -> Option<(Kind, &'static str)> {
    let has = |m: &str| mimes.iter().any(|x| x == m);
    IMAGE_MIMES
        .into_iter()
        .find(|m| has(m))
        .map(|m| (Kind::Image, m))
        .or_else(|| TEXT_MIMES.into_iter().find(|m| has(m)).map(|m| (Kind::Text, m)))
}

fn read_all(fd: OwnedFd, max_bytes: usize) -> anyhow::Result<Vec<u8>> {
    let mut file = File::from(fd);
    let mut data = Vec::new();
    let mut buf = vec![0; 64 * 1024];
    loop {
        if poll(&mut [PollFd::new(&file, PollFlags::IN)], Some(&READ_TIMEOUT))? == 0 {
            bail!("timed out waiting for clipboard data");
        }
        match file.read(&mut buf)? {
            0 => return Ok(data),
            n if data.len() + n > max_bytes => bail!("larger than max_item_mb"),
            n => data.extend_from_slice(&buf[..n]),
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlSeat,
        _: <WlSeat as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZwlrDataControlManagerV1,
        _: <ZwlrDataControlManagerV1 as Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrDataControlDeviceV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwlrDataControlDeviceV1,
        event: device::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            device::Event::DataOffer { id } => {
                state.offers.insert(id.id(), Vec::new());
            }
            device::Event::Selection { id } => state.on_selection(id),
            // Middle-click selection is not recorded.
            device::Event::PrimarySelection { id: Some(offer) } => {
                state.offers.remove(&offer.id());
                offer.destroy();
            }
            device::Event::Finished => {
                eprintln!("wispd: data-control device was destroyed by the compositor");
                std::process::exit(1);
            }
            _ => {}
        }
    }

    event_created_child!(State, ZwlrDataControlDeviceV1, [
        device::EVT_DATA_OFFER_OPCODE => (ZwlrDataControlOfferV1, ()),
    ]);
}

impl Dispatch<ZwlrDataControlOfferV1, ()> for State {
    fn event(
        state: &mut Self,
        offer: &ZwlrDataControlOfferV1,
        event: offer::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let offer::Event::Offer { mime_type } = event
            && let Some(mimes) = state.offers.get_mut(&offer.id())
        {
            mimes.push(mime_type);
        }
    }
}

impl Dispatch<ZwlrDataControlSourceV1, ()> for State {
    fn event(
        state: &mut Self,
        source: &ZwlrDataControlSourceV1,
        event: source::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            source::Event::Send { fd, .. } => {
                if let Some(data) = state.sources.get(&source.id()).cloned() {
                    // A paste target that never reads must not block the event loop.
                    std::thread::spawn(move || {
                        let _ = File::from(fd).write_all(&data);
                    });
                }
            }
            source::Event::Cancelled => {
                state.sources.remove(&source.id());
                source.destroy();
            }
            _ => {}
        }
    }
}
