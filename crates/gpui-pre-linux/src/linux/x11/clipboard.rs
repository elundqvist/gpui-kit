// Changed in elundqvist/gpui-kit from gpui-pre-linux 0.3.3 as published to crates.io: FORK.md at
// the crate's root lists the changes.
/*
 * Copyright 2022 - 2025 Zed Industries, Inc.
 * License: Apache-2.0
 * See LICENSE-APACHE for complete license terms
 *
 * Adapted from the x11 submodule of the arboard project https://github.com/1Password/arboard
 *
 * SPDX-License-Identifier: Apache-2.0 OR MIT
 *
 * Copyright 2022 The Arboard contributors
 *
 * The project to which this file belongs is licensed under either of
 * the Apache 2.0 or the MIT license at the licensee's choice. The terms
 * and conditions of the chosen license apply to this file.
*/

// More info about using the clipboard on X11:
// https://tronche.com/gui/x/icccm/sec-2.html#s-2.6
// https://freedesktop.org/wiki/ClipboardManager/

use std::{
    borrow::Cow,
    cell::RefCell,
    collections::{HashMap, hash_map::Entry},
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    thread::JoinHandle,
    thread_local,
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex, MutexGuard, RwLock};
use x11rb::{
    COPY_DEPTH_FROM_PARENT, COPY_FROM_PARENT, NONE,
    connection::Connection,
    protocol::{
        Event,
        xproto::{
            Atom, AtomEnum, ChangeWindowAttributesAux, ConnectionExt as _, CreateWindowAux,
            EventMask, PropMode, Property, PropertyNotifyEvent, SELECTION_NOTIFY_EVENT,
            SelectionNotifyEvent, SelectionRequestEvent, Time, WindowClass,
        },
    },
    rust_connection::RustConnection,
    wrapper::ConnectionExt as _,
};

use gpui::{ClipboardItem, Image, ImageFormat, hash};
use strum::IntoEnumIterator;

type Result<T, E = Error> = std::result::Result<T, E>;

static CLIPBOARD: Mutex<Option<GlobalClipboard>> = parking_lot::const_mutex(None);

x11rb::atom_manager! {
    pub Atoms: AtomCookies {
        CLIPBOARD,
        PRIMARY,
        SECONDARY,

        CLIPBOARD_MANAGER,
        SAVE_TARGETS,
        TARGETS,
        ATOM,
        INCR,

        UTF8_STRING,
        UTF8_MIME_0: b"text/plain;charset=utf-8",
        UTF8_MIME_1: b"text/plain;charset=UTF-8",
        // Text in ISO Latin-1 encoding
        // See: https://tronche.com/gui/x/icccm/sec-2.html#s-2.6.2
        STRING,
        // Text in unknown encoding
        // See: https://tronche.com/gui/x/icccm/sec-2.html#s-2.6.2
        TEXT,
        TEXT_MIME_UNKNOWN: b"text/plain",

        // HTML: b"text/html",
        // URI_LIST: b"text/uri-list",

        PNG__MIME: ImageFormat::mime_type(ImageFormat::Png ).as_bytes(),
        JPEG_MIME: ImageFormat::mime_type(ImageFormat::Jpeg).as_bytes(),
        WEBP_MIME: ImageFormat::mime_type(ImageFormat::Webp).as_bytes(),
        GIF__MIME: ImageFormat::mime_type(ImageFormat::Gif ).as_bytes(),
        SVG__MIME: ImageFormat::mime_type(ImageFormat::Svg ).as_bytes(),
        BMP__MIME: ImageFormat::mime_type(ImageFormat::Bmp ).as_bytes(),
        TIFF_MIME: ImageFormat::mime_type(ImageFormat::Tiff).as_bytes(),
        ICO__MIME: ImageFormat::mime_type(ImageFormat::Ico ).as_bytes(),
        PNM__MIME: ImageFormat::mime_type(ImageFormat::Pnm ).as_bytes(),
        // This is just some random name for the property on our window, into which
        // the clipboard owner writes the data we requested.
        ARBOARD_CLIPBOARD,
    }
}

thread_local! {
    static ATOM_NAME_CACHE: RefCell<HashMap<Atom, &'static str>> = Default::default();
}

// Some clipboard items, like images, may take a very long time to produce a
// `SelectionNotify`. Multiple seconds long.
const LONG_TIMEOUT_DUR: Duration = Duration::from_millis(4000);
const SHORT_TIMEOUT_DUR: Duration = Duration::from_millis(10);

#[derive(Debug, PartialEq, Eq)]
enum ManagerHandoverState {
    Idle,
    InProgress,
    Finished,
}

struct GlobalClipboard {
    inner: Arc<Inner>,

    /// Join handle to the thread which serves selection requests.
    server_handle: JoinHandle<()>,
}

struct XContext {
    conn: RustConnection,
    win_id: u32,
}

struct Inner {
    /// The display this is the clipboard of, `$DISPLAY`'s where it is
    /// `None`: a read connects to it too.
    display: Option<String>,
    /// The context for the thread which serves clipboard read
    /// requests coming to us.
    server: XContext,
    atoms: Atoms,

    clipboard: Selection,
    primary: Selection,
    secondary: Selection,

    handover_state: Mutex<ManagerHandoverState>,
    handover_cv: Condvar,

    serve_stopped: AtomicBool,

    /// How many reads on other threads hold this (`ClipboardReader`), for
    /// the last `Clipboard` to tell itself from them when it is dropped.
    reads: AtomicUsize,
}

impl XContext {
    /// A connection to `display`, or to `$DISPLAY`'s where it is `None`.
    fn on(display: Option<&str>) -> Result<Self> {
        // create a new connection to an X11 server
        let (conn, screen_num): (RustConnection, _) =
            RustConnection::connect(display).map_err(|_| {
                Error::unknown("X11 server connection timed out because it was unreachable")
            })?;
        let screen = conn
            .setup()
            .roots
            .get(screen_num)
            .ok_or(Error::unknown("no screen found"))?;
        let win_id = conn.generate_id().map_err(into_unknown)?;

        let event_mask =
            // Just in case that some program reports SelectionNotify events
            // with XCB_EVENT_MASK_PROPERTY_CHANGE mask.
            EventMask::PROPERTY_CHANGE |
            // To receive DestroyNotify event and stop the message loop.
            EventMask::STRUCTURE_NOTIFY;
        // create the window
        conn.create_window(
            // copy as much as possible from the parent, because no other specific input is needed
            COPY_DEPTH_FROM_PARENT,
            win_id,
            screen.root,
            0,
            0,
            1,
            1,
            0,
            WindowClass::COPY_FROM_PARENT,
            COPY_FROM_PARENT,
            // don't subscribe to any special events because we are requesting everything we need ourselves
            &CreateWindowAux::new().event_mask(event_mask),
        )
        .map_err(into_unknown)?;
        conn.flush().map_err(into_unknown)?;

        Ok(Self { conn, win_id })
    }
}

#[derive(Default)]
struct Selection {
    data: RwLock<Option<Vec<ClipboardData>>>,
    /// Mutex around nothing to use with the below condvar.
    mutex: Mutex<()>,
    /// A condvar that is notified when the contents of this clipboard are changed.
    ///
    /// This is associated with `Self::mutex`.
    data_changed: Condvar,
}

#[derive(Debug, Clone)]
struct ClipboardData {
    bytes: Vec<u8>,

    /// The atom representing the format in which the data is encoded.
    format: Atom,
}

enum ReadSelNotifyResult {
    GotData(ClipboardData),
    IncrStarted,
    EventNotRecognized,
}

/// Waits until the X server has sent something on `conn`, or `until`.
///
/// Changed in elundqvist/gpui-kit (elundqvist/kvist#384): the reader slept
/// a millisecond each time it found no event, so a selection handed over
/// in pieces (INCR) took a millisecond and more a piece, a piece of 4 KB
/// as Tk and others hand them: 0.2 to 0.4 s of the drawing thread for a
/// paste of 1.4 MB from another program. It wakes as the next piece comes.
fn wait_for_server(conn: &RustConnection, until: Instant) {
    use std::os::fd::AsRawFd as _;
    let left = until.saturating_duration_since(Instant::now());
    let mut fd = libc::pollfd {
        fd: conn.stream().as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: `fd` is one valid `pollfd`, for the length of the call.
    unsafe {
        libc::poll(&mut fd, 1, left.as_millis().clamp(1, 100) as libc::c_int);
    }
}

impl Inner {
    fn new() -> Result<Self> {
        Self::on(None)
    }

    /// The clipboard of `display`, or of `$DISPLAY`'s where it is `None`.
    fn on(display: Option<&str>) -> Result<Self> {
        let server = XContext::on(display)?;
        let atoms = Atoms::new(&server.conn)
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        Ok(Self {
            display: display.map(str::to_owned),
            server,
            atoms,
            clipboard: Selection::default(),
            primary: Selection::default(),
            secondary: Selection::default(),
            handover_state: Mutex::new(ManagerHandoverState::Idle),
            handover_cv: Condvar::new(),
            serve_stopped: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        })
    }

    fn write(
        &self,
        data: Vec<ClipboardData>,
        selection: ClipboardKind,
        wait: WaitConfig,
    ) -> Result<()> {
        if self.serve_stopped.load(Ordering::Relaxed) {
            return Err(Error::unknown(
                "The clipboard handler thread seems to have stopped. Logging messages may reveal the cause. (See the `log` crate.)",
            ));
        }

        let server_win = self.server.win_id;

        // ICCCM version 2, section 2.6.1.3 states that we should re-assert ownership whenever data
        // changes.
        self.server
            .conn
            .set_selection_owner(server_win, self.atom_of(selection), Time::CURRENT_TIME)
            .map_err(|_| Error::ClipboardOccupied)?;

        self.server.conn.flush().map_err(into_unknown)?;

        // Just setting the data, and the `serve_requests` will take care of the rest.
        let selection = self.selection_of(selection);
        let mut data_guard = selection.data.write();
        *data_guard = Some(data);

        // Lock the mutex to both ensure that no wakers of `data_changed` can wake us between
        // dropping the `data_guard` and calling `wait[_for]` and that we don't we wake other
        // threads in that position.
        let mut guard = selection.mutex.lock();

        // Notify any existing waiting threads that we have changed the data in the selection.
        // It is important that the mutex is locked to prevent this notification getting lost.
        selection.data_changed.notify_all();

        match wait {
            WaitConfig::None => {}
            WaitConfig::Forever => {
                drop(data_guard);
                selection.data_changed.wait(&mut guard);
            }
            WaitConfig::Until(deadline) => {
                drop(data_guard);
                selection.data_changed.wait_until(&mut guard, deadline);
            }
        }

        Ok(())
    }

    /// `formats` must be a slice of atoms, where each atom represents a target format.
    /// The first format from `formats`, which the clipboard owner supports will be the
    /// format of the return value.
    fn read(&self, formats: &[Atom], selection: ClipboardKind) -> Result<ClipboardData> {
        // if we are the current owner, we can get the current clipboard ourselves
        if self.is_owner(selection)? {
            let data = self.selection_of(selection).data.read();
            if let Some(data_list) = &*data {
                for data in data_list {
                    for format in formats {
                        if *format == data.format {
                            return Ok(data.clone());
                        }
                    }
                }
            }
            return Err(Error::ContentNotAvailable);
        }
        let reader = XContext::on(self.display.as_deref())?;

        let highest_precedence_format =
            match self.read_single(&reader, selection, self.atoms.TARGETS) {
                // Changed in elundqvist/gpui-kit (elundqvist/kvist#401): an
                // owner that does not answer for its targets does not answer
                // for any of the formats either, and asking for each of them
                // in turn, four seconds apiece, held a paste a minute
                Err(Error::TimedOut) => return Err(Error::TimedOut),
                Err(err) => {
                    log::trace!("Clipboard TARGETS query failed with {err:?}");
                    None
                }
                Ok(ClipboardData { bytes, format }) => {
                    if format == self.atoms.ATOM {
                        let available_formats = Self::parse_formats(&bytes);
                        formats
                            .iter()
                            .find(|format| available_formats.contains(format))
                    } else {
                        log::trace!(
                            "Unexpected clipboard TARGETS format {}",
                            self.atom_name(format)
                        );
                        None
                    }
                }
            };

        if let Some(&format) = highest_precedence_format {
            let data = self.read_single(&reader, selection, format)?;
            if !formats.contains(&data.format) {
                // This shouldn't happen since the format is from the TARGETS list.
                log::trace!(
                    "Conversion to {} responded with {} which is not supported",
                    self.atom_name(format),
                    self.atom_name(data.format),
                );
                return Err(Error::ConversionFailure);
            }
            return Ok(data);
        }

        log::trace!("Falling back on attempting to convert clipboard to each format.");
        for format in formats {
            match self.read_single(&reader, selection, *format) {
                Ok(data) => {
                    if formats.contains(&data.format) {
                        return Ok(data);
                    } else {
                        log::trace!(
                            "Conversion to {} responded with {} which is not supported",
                            self.atom_name(*format),
                            self.atom_name(data.format),
                        );
                        continue;
                    }
                }
                Err(Error::ContentNotAvailable) => {
                    continue;
                }
                Err(e) => {
                    // a time-out among them: the owner answers no more
                    log::trace!("Conversion to {} failed: {}", self.atom_name(*format), e);
                    return Err(e);
                }
            }
        }
        log::trace!("All conversions to supported formats failed.");
        Err(Error::ContentNotAvailable)
    }

    fn parse_formats(bytes: &[u8]) -> Vec<Atom> {
        bytes
            .chunks_exact(4)
            .map(|chunk| u32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
            .collect()
    }

    fn read_single(
        &self,
        reader: &XContext,
        selection: ClipboardKind,
        target_format: Atom,
    ) -> Result<ClipboardData> {
        // Delete the property so that we can detect (using property notify)
        // when the selection owner receives our request.
        reader
            .conn
            .delete_property(reader.win_id, self.atoms.ARBOARD_CLIPBOARD)
            .map_err(into_unknown)?;

        // request to convert the clipboard selection to our data type(s)
        reader
            .conn
            .convert_selection(
                reader.win_id,
                self.atom_of(selection),
                target_format,
                self.atoms.ARBOARD_CLIPBOARD,
                Time::CURRENT_TIME,
            )
            .map_err(into_unknown)?;
        reader.conn.sync().map_err(into_unknown)?;

        log::trace!("Finished `convert_selection`");

        let mut incr_data: Vec<u8> = Vec::new();
        let mut using_incr = false;

        let mut timeout_end = Instant::now() + LONG_TIMEOUT_DUR;

        while Instant::now() < timeout_end {
            let event = reader.conn.poll_for_event().map_err(into_unknown)?;
            let event = match event {
                Some(e) => e,
                None => {
                    wait_for_server(&reader.conn, timeout_end);
                    continue;
                }
            };
            match event {
                // The first response after requesting a selection.
                Event::SelectionNotify(event) => {
                    log::trace!("Read SelectionNotify");
                    let result = self.handle_read_selection_notify(
                        reader,
                        target_format,
                        &mut using_incr,
                        &mut incr_data,
                        event,
                    )?;
                    match result {
                        ReadSelNotifyResult::GotData(data) => return Ok(data),
                        ReadSelNotifyResult::IncrStarted => {
                            // This means we received an indication that an the
                            // data is going to be sent INCRementally. Let's
                            // reset our timeout.
                            timeout_end += SHORT_TIMEOUT_DUR;
                        }
                        ReadSelNotifyResult::EventNotRecognized => (),
                    }
                }
                // If the previous SelectionNotify event specified that the data
                // will be sent in INCR segments, each segment is transferred in
                // a PropertyNotify event.
                Event::PropertyNotify(event) => {
                    let result = self.handle_read_property_notify(
                        reader,
                        target_format,
                        using_incr,
                        &mut incr_data,
                        &mut timeout_end,
                        event,
                    )?;
                    if result {
                        return Ok(ClipboardData {
                            bytes: incr_data,
                            format: target_format,
                        });
                    }
                }
                _ => log::trace!(
                    "An unexpected event arrived while reading the clipboard: {:?}",
                    event
                ),
            }
        }
        log::info!("Time-out hit while reading the clipboard.");
        Err(Error::TimedOut)
    }

    fn atom_of(&self, selection: ClipboardKind) -> Atom {
        match selection {
            ClipboardKind::Clipboard => self.atoms.CLIPBOARD,
            ClipboardKind::Primary => self.atoms.PRIMARY,
            ClipboardKind::Secondary => self.atoms.SECONDARY,
        }
    }

    fn selection_of(&self, selection: ClipboardKind) -> &Selection {
        match selection {
            ClipboardKind::Clipboard => &self.clipboard,
            ClipboardKind::Primary => &self.primary,
            ClipboardKind::Secondary => &self.secondary,
        }
    }

    fn kind_of(&self, atom: Atom) -> Option<ClipboardKind> {
        match atom {
            a if a == self.atoms.CLIPBOARD => Some(ClipboardKind::Clipboard),
            a if a == self.atoms.PRIMARY => Some(ClipboardKind::Primary),
            a if a == self.atoms.SECONDARY => Some(ClipboardKind::Secondary),
            _ => None,
        }
    }

    fn is_owner(&self, selection: ClipboardKind) -> Result<bool> {
        let current = self
            .server
            .conn
            .get_selection_owner(self.atom_of(selection))
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?
            .owner;

        Ok(current == self.server.win_id)
    }

    fn query_atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> Result<String> {
        String::from_utf8(
            self.server
                .conn
                .get_atom_name(atom)
                .map_err(into_unknown)?
                .reply()
                .map_err(into_unknown)?
                .name,
        )
        .map_err(into_unknown)
    }

    fn atom_name(&self, atom: x11rb::protocol::xproto::Atom) -> &'static str {
        ATOM_NAME_CACHE.with(|cache| {
            let mut cache = cache.borrow_mut();
            match cache.entry(atom) {
                Entry::Occupied(entry) => *entry.get(),
                Entry::Vacant(entry) => {
                    let s = self
                        .query_atom_name(atom)
                        .map(|s| Box::leak(s.into_boxed_str()) as &str)
                        .unwrap_or("FAILED-TO-GET-THE-ATOM-NAME");
                    entry.insert(s);
                    s
                }
            }
        })
    }

    fn handle_read_selection_notify(
        &self,
        reader: &XContext,
        target_format: u32,
        using_incr: &mut bool,
        incr_data: &mut Vec<u8>,
        event: SelectionNotifyEvent,
    ) -> Result<ReadSelNotifyResult> {
        // The property being set to NONE means that the `convert_selection`
        // failed.

        // According to: https://tronche.com/gui/x/icccm/sec-2.html#s-2.4
        // the target must be set to the same as what we requested.
        if event.property == NONE || event.target != target_format {
            return Err(Error::ContentNotAvailable);
        }
        if self.kind_of(event.selection).is_none() {
            log::info!(
                "Received a SelectionNotify for a selection other than CLIPBOARD, PRIMARY or SECONDARY. This is unexpected."
            );
            return Ok(ReadSelNotifyResult::EventNotRecognized);
        }
        if *using_incr {
            log::warn!("Received a SelectionNotify while already expecting INCR segments.");
            return Ok(ReadSelNotifyResult::EventNotRecognized);
        }
        // Accept any property type. The property type will typically match the format type except
        // when it is `TARGETS` in which case it is `ATOM`. `ANY` is provided to handle the case
        // where the clipboard is not convertible to the requested format. In this case
        // `reply.type_` will have format information, but `bytes` will only be non-empty if `ANY`
        // is provided.
        let property_type = AtomEnum::ANY;
        // request the selection
        let mut reply = reader
            .conn
            .get_property(
                true,
                event.requestor,
                event.property,
                property_type,
                0,
                u32::MAX / 4,
            )
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        // we found something
        if reply.type_ == self.atoms.INCR {
            // Note that we call the get_property again because we are
            // indicating that we are ready to receive the data by deleting the
            // property, however deleting only works if the type matches the
            // property type. But the type didn't match in the previous call.
            reply = reader
                .conn
                .get_property(
                    true,
                    event.requestor,
                    event.property,
                    self.atoms.INCR,
                    0,
                    u32::MAX / 4,
                )
                .map_err(into_unknown)?
                .reply()
                .map_err(into_unknown)?;
            log::trace!("Receiving INCR segments");
            *using_incr = true;
            if reply.value_len == 4 {
                let min_data_len = reply
                    .value32()
                    .and_then(|mut vals| vals.next())
                    .unwrap_or(0);
                incr_data.reserve(min_data_len as usize);
            }
            Ok(ReadSelNotifyResult::IncrStarted)
        } else {
            Ok(ReadSelNotifyResult::GotData(ClipboardData {
                bytes: reply.value,
                format: reply.type_,
            }))
        }
    }

    /// Returns Ok(true) when the incr_data is ready
    fn handle_read_property_notify(
        &self,
        reader: &XContext,
        target_format: u32,
        using_incr: bool,
        incr_data: &mut Vec<u8>,
        timeout_end: &mut Instant,
        event: PropertyNotifyEvent,
    ) -> Result<bool> {
        if event.atom != self.atoms.ARBOARD_CLIPBOARD || event.state != Property::NEW_VALUE {
            return Ok(false);
        }
        if !using_incr {
            // This must mean the selection owner received our request, and is
            // now preparing the data
            return Ok(false);
        }
        let reply = reader
            .conn
            .get_property(
                true,
                event.window,
                event.atom,
                if target_format == self.atoms.TARGETS {
                    self.atoms.ATOM
                } else {
                    target_format
                },
                0,
                u32::MAX / 4,
            )
            .map_err(into_unknown)?
            .reply()
            .map_err(into_unknown)?;

        // log::trace!("Received segment. value_len {}", reply.value_len,);
        if reply.value_len == 0 {
            // This indicates that all the data has been sent.
            return Ok(true);
        }
        incr_data.extend(reply.value);

        // Let's reset our timeout, since we received a valid chunk.
        *timeout_end = Instant::now() + SHORT_TIMEOUT_DUR;

        // Not yet complete
        Ok(false)
    }

    /// The most a property set in one request holds: the server's largest
    /// request without the BIG-REQUESTS extension, less room for the
    /// request's own fields. A selection larger than this is handed over
    /// in pieces of this size (`IncrSend`), as ICCCM has an owner do: a
    /// requestor reads a property with a bound of its own, Tk's 400 KB,
    /// and took 1.4 MB set whole as an error, "selection property too
    /// large" (elundqvist/kvist#424).
    fn largest_property(&self) -> usize {
        largest_property(self.server.conn.setup().maximum_request_length)
    }

    /// Answers `event`; returns whether the data goes in pieces, the
    /// hand-over begun and not yet done.
    fn handle_selection_request(
        &self,
        event: SelectionRequestEvent,
        transfers: &mut Vec<IncrSend>,
    ) -> Result<bool> {
        let selection = match self.kind_of(event.selection) {
            Some(kind) => kind,
            None => {
                log::warn!(
                    "Received a selection request to a selection other than the CLIPBOARD, PRIMARY or SECONDARY. This is unexpected."
                );
                return Ok(false);
            }
        };

        let success;
        let mut in_pieces = false;
        // we are asked for a list of supported conversion targets
        if event.target == self.atoms.TARGETS {
            log::trace!(
                "Handling TARGETS, dst property is {}",
                self.atom_name(event.property)
            );
            let mut targets = Vec::with_capacity(10);
            targets.push(self.atoms.TARGETS);
            targets.push(self.atoms.SAVE_TARGETS);
            let data = self.selection_of(selection).data.read();
            if let Some(data_list) = &*data {
                for data in data_list {
                    targets.push(data.format);
                    if data.format == self.atoms.UTF8_STRING {
                        // When we are storing a UTF8 string,
                        // add all equivalent formats to the supported targets,
                        // and Latin-1, which older programs ask for
                        // (`served_as`)
                        targets.push(self.atoms.UTF8_MIME_0);
                        targets.push(self.atoms.UTF8_MIME_1);
                        targets.push(self.atoms.STRING);
                    }
                }
            }
            self.server
                .conn
                .change_property32(
                    PropMode::REPLACE,
                    event.requestor,
                    event.property,
                    // TODO: change to `AtomEnum::ATOM`
                    self.atoms.ATOM,
                    &targets,
                )
                .map_err(into_unknown)?;
            self.server.conn.flush().map_err(into_unknown)?;
            success = true;
        } else {
            log::trace!("Handling request for (probably) the clipboard contents.");
            let data = self.selection_of(selection).data.read();
            if let Some(data_list) = &*data {
                success = match self.served_as(data_list, event.target) {
                    Some(bytes) if bytes.len() > self.largest_property() => {
                        self.begin_incr(&event, &bytes, transfers)?;
                        in_pieces = true;
                        true
                    }
                    Some(bytes) => {
                        self.server
                            .conn
                            .change_property8(
                                PropMode::REPLACE,
                                event.requestor,
                                event.property,
                                event.target,
                                &bytes,
                            )
                            .map_err(into_unknown)?;
                        self.server.conn.flush().map_err(into_unknown)?;
                        true
                    }
                    None => false,
                };
            } else {
                // This must mean that we lost ownership of the data
                // since the other side requested the selection.
                // Let's respond with the property set to none.
                success = false;
            }
        }
        // on failure we notify the requester of it
        let property = if success {
            event.property
        } else {
            AtomEnum::NONE.into()
        };
        // tell the requestor that we finished sending data
        self.server
            .conn
            .send_event(
                false,
                event.requestor,
                EventMask::NO_EVENT,
                SelectionNotifyEvent {
                    response_type: SELECTION_NOTIFY_EVENT,
                    sequence: event.sequence,
                    time: event.time,
                    requestor: event.requestor,
                    selection: event.selection,
                    target: event.target,
                    property,
                },
            )
            .map_err(into_unknown)?;

        self.server.conn.flush().map_err(into_unknown)?;
        Ok(in_pieces)
    }

    /// What the owner hands over for a request for `target`: the data held
    /// in that format, and for a text held as `UTF8_STRING` the same bytes
    /// as `text/plain;charset=utf-8` and `text/plain;charset=UTF-8`, and
    /// the text in Latin-1 as `STRING`, a character outside it as `?`, as
    /// Qt hands it over.
    ///
    /// Changed in elundqvist/gpui-kit: the two MIME types were among the
    /// targets the owner listed, and a request for either was refused; and
    /// there was no `STRING`, which older programs ask for: Tk asked for it
    /// by name said `form "STRING" not defined`.
    fn served_as<'a>(&self, data_list: &'a [ClipboardData], target: Atom) -> Option<Cow<'a, [u8]>> {
        if let Some(data) = data_list.iter().find(|d| d.format == target) {
            return Some(Cow::Borrowed(&data.bytes));
        }
        let text = data_list
            .iter()
            .find(|d| d.format == self.atoms.UTF8_STRING)?;
        if target == self.atoms.UTF8_MIME_0 || target == self.atoms.UTF8_MIME_1 {
            return Some(Cow::Borrowed(&text.bytes));
        }
        if target == self.atoms.STRING {
            return Some(Cow::Owned(latin1(&text.bytes)));
        }
        None
    }

    /// Begin handing `bytes` over to the requestor of `event` in pieces
    /// (ICCCM 2.7.2, elundqvist/kvist#424): its property is set to the type
    /// `INCR` and the size, and the requestor's property changes are
    /// listened to; each time it deletes the property, the next piece goes
    /// in, and an empty one ends it (`send_incr_piece`). A request for the
    /// same property of the same window ends the one before it.
    fn begin_incr(
        &self,
        event: &SelectionRequestEvent,
        bytes: &[u8],
        transfers: &mut Vec<IncrSend>,
    ) -> Result<()> {
        let conn = &self.server.conn;
        conn.change_window_attributes(
            event.requestor,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(into_unknown)?;
        conn.change_property32(
            PropMode::REPLACE,
            event.requestor,
            event.property,
            self.atoms.INCR,
            &[u32::try_from(bytes.len()).unwrap_or(u32::MAX)],
        )
        .map_err(into_unknown)?;
        conn.flush().map_err(into_unknown)?;
        transfers.retain(|t| !t.is_for(event.requestor, event.property));
        transfers.push(IncrSend::new(
            event.requestor,
            event.property,
            event.target,
            bytes.to_vec(),
            self.largest_property(),
        ));
        log::trace!(
            "Handing {} bytes over in pieces to window {}",
            bytes.len(),
            event.requestor
        );
        Ok(())
    }

    /// The requestor of a selection handed over in pieces deleted its
    /// property: the next piece goes in, or the empty one that ends it.
    /// Returns the hand-over `event` moved on, if it moved one, and
    /// whether that ended it.
    fn send_incr_piece(
        &self,
        event: &PropertyNotifyEvent,
        transfers: &mut Vec<IncrSend>,
    ) -> Result<Option<(u32, Atom, bool)>> {
        if event.state != Property::DELETE {
            return Ok(None);
        }
        let Some(at) = transfers
            .iter()
            .position(|t| t.is_for(event.window, event.atom))
        else {
            return Ok(None);
        };
        let transfer = &mut transfers[at];
        let moved = (transfer.requestor, transfer.property);
        let Some(piece) = transfer.next_piece() else {
            transfers.remove(at);
            return Ok(Some((moved.0, moved.1, true)));
        };
        let conn = &self.server.conn;
        conn.change_property8(
            PropMode::REPLACE,
            transfer.requestor,
            transfer.property,
            transfer.target,
            &transfer.bytes[piece.clone()],
        )
        .map_err(into_unknown)?;
        if piece.is_empty() {
            let window = transfer.requestor;
            transfers.remove(at);
            // the requestor's property changes are listened to no more,
            // unless another piece is on its way to it
            if !transfers.iter().any(|t| t.requestor == window) {
                conn.change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
                )
                .map_err(into_unknown)?;
            }
        }
        conn.flush().map_err(into_unknown)?;
        Ok(Some((moved.0, moved.1, piece.is_empty())))
    }

    fn ask_clipboard_manager_to_request_our_data(&self) -> Result<()> {
        if self.server.win_id == 0 {
            // This shouldn't really ever happen but let's just check.
            log::error!("The server's window id was 0. This is unexpected");
            return Ok(());
        }

        if !self.is_owner(ClipboardKind::Clipboard)? {
            // We are not owning the clipboard, nothing to do.
            return Ok(());
        }
        if self
            .selection_of(ClipboardKind::Clipboard)
            .data
            .read()
            .is_none()
        {
            // If we don't have any data, there's nothing to do.
            return Ok(());
        }

        // It's important that we lock the state before sending the request
        // because we don't want the request server thread to lock the state
        // after the request but before we can lock it here.
        let mut handover_state = self.handover_state.lock();

        log::trace!("Sending the data to the clipboard manager");
        self.server
            .conn
            .convert_selection(
                self.server.win_id,
                self.atoms.CLIPBOARD_MANAGER,
                self.atoms.SAVE_TARGETS,
                self.atoms.ARBOARD_CLIPBOARD,
                Time::CURRENT_TIME,
            )
            .map_err(into_unknown)?;
        self.server.conn.flush().map_err(into_unknown)?;

        *handover_state = ManagerHandoverState::InProgress;
        let max_handover_duration = Duration::from_millis(100);
        // Changed in elundqvist/gpui-kit (elundqvist/kvist#424): contents
        // handed over in pieces take the manager longer than one wait, and
        // the serve thread wakes this one at each piece it takes, for as
        // long again; within a bound, the app being on its way out.
        let give_up = Instant::now() + MAX_HANDOVER_IN_PIECES;

        loop {
            // Note that we are using a parking_lot condvar here, which doesn't wake up
            // spuriously
            let result = self
                .handover_cv
                .wait_for(&mut handover_state, max_handover_duration);

            if *handover_state == ManagerHandoverState::Finished {
                return Ok(());
            }
            if result.timed_out() || Instant::now() >= give_up {
                log::warn!(
                    "Could not hand the clipboard contents over to the clipboard manager. The request timed out."
                );
                return Ok(());
            }
            // woken by a piece the manager took: it is still at it
        }
    }
}

fn serve_requests(context: Arc<Inner>) -> Result<(), Box<dyn std::error::Error>> {
    fn handover_finished(clip: &Arc<Inner>, mut handover_state: MutexGuard<ManagerHandoverState>) {
        log::trace!("Finishing clipboard manager handover.");
        *handover_state = ManagerHandoverState::Finished;

        // Not sure if unlocking the mutex is necessary here but better safe than sorry.
        drop(handover_state);

        clip.handover_cv.notify_all();
    }

    log::trace!("Started serve requests thread.");

    let _guard = gpui_util::defer(|| {
        context.serve_stopped.store(true, Ordering::Relaxed);
    });

    let mut written = false;
    let mut notified = false;
    // the selections being handed over in pieces (elundqvist/kvist#424)
    let mut transfers: Vec<IncrSend> = Vec::new();
    // those of them that hand the contents to the clipboard manager as the
    // app quits: the contents are written once the last piece has gone
    let mut to_manager: Vec<(u32, Atom)> = Vec::new();

    loop {
        let event = context.server.conn.wait_for_event().map_err(into_unknown)?;
        // a requestor that went before it took the last piece, its window
        // gone, never deletes its property again
        transfers.retain(|t| !t.abandoned(Instant::now()));
        match event {
            Event::DestroyNotify(_) => {
                // This window is being destroyed.
                log::trace!("Clipboard server window is being destroyed x_x");
                return Ok(());
            }
            Event::SelectionClear(event) => {
                // TODO: check if this works
                // Someone else has new content in the clipboard, so it is
                // notifying us that we should delete our data now.
                log::trace!("Somebody else owns the clipboard now");

                if let Some(selection) = context.kind_of(event.selection) {
                    let selection = context.selection_of(selection);
                    let mut data_guard = selection.data.write();
                    *data_guard = None;

                    // It is important that this mutex is locked at the time of calling
                    // `notify_all` to prevent notifications getting lost in case the sleeping
                    // thread has unlocked its `data_guard` and is just about to sleep.
                    // It is also important that the RwLock is kept write-locked for the same
                    // reason.
                    let _guard = selection.mutex.lock();
                    selection.data_changed.notify_all();
                }
            }
            Event::SelectionRequest(event) => {
                log::trace!(
                    "SelectionRequest - selection is: {}, target is {}",
                    context.atom_name(event.selection),
                    context.atom_name(event.target),
                );
                // Someone is requesting the clipboard content from us.
                let in_pieces = context
                    .handle_selection_request(event, &mut transfers)
                    .map_err(into_unknown)?;

                // if we are in the progress of saving to the clipboard manager
                // make sure we save that we have finished writing
                let handover_state = context.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    // Only set written, when the actual contents were written,
                    // not just a response to what TARGETS we have.
                    if event.target != context.atoms.TARGETS {
                        if in_pieces {
                            // Changed in elundqvist/gpui-kit (elundqvist/kvist#424):
                            // written once the last piece has gone; a manager
                            // that notified first, as KDE's does, got the
                            // window destroyed under its first piece.
                            to_manager.push((event.requestor, event.property));
                        } else {
                            log::trace!("The contents were written to the clipboard manager.");
                            written = true;
                            // if we have written and notified, make sure to notify that we are done
                            if notified {
                                handover_finished(&context, handover_state);
                            }
                        }
                    }
                }
            }
            Event::SelectionNotify(event) => {
                // We've requested the clipboard content and this is the answer.
                // Considering that this thread is not responsible for reading
                // clipboard contents, this must come from the clipboard manager
                // signaling that the data was handed over successfully.
                if event.selection != context.atoms.CLIPBOARD_MANAGER {
                    log::error!(
                        "Received a `SelectionNotify` from a selection other than the CLIPBOARD_MANAGER. This is unexpected in this thread."
                    );
                    continue;
                }
                let handover_state = context.handover_state.lock();
                if *handover_state == ManagerHandoverState::InProgress {
                    // Note that some clipboard managers send a selection notify
                    // before even sending a request for the actual contents.
                    // (That's why we use the "notified" & "written" flags)
                    log::trace!(
                        "The clipboard manager indicated that it's done requesting the contents from us."
                    );
                    notified = true;

                    // One would think that we could also finish if the property
                    // here is set 0, because that indicates failure. However
                    // this is not the case; for example on KDE plasma 5.18, we
                    // immediately get a SelectionNotify with property set to 0,
                    // but following that, we also get a valid SelectionRequest
                    // from the clipboard manager.
                    if written {
                        handover_finished(&context, handover_state);
                    }
                }
            }
            Event::PropertyNotify(event) => {
                let moved = context
                    .send_incr_piece(&event, &mut transfers)
                    .map_err(into_unknown)?;
                let Some((requestor, property, done)) = moved else {
                    continue;
                };
                if !to_manager.contains(&(requestor, property)) {
                    continue;
                }
                let handover_state = context.handover_state.lock();
                if *handover_state != ManagerHandoverState::InProgress {
                    continue;
                }
                if !done {
                    // the manager is taking the pieces: let the app wait on
                    drop(handover_state);
                    context.handover_cv.notify_all();
                    continue;
                }
                to_manager.retain(|&to| to != (requestor, property));
                log::trace!("The contents were written to the clipboard manager in pieces.");
                written = true;
                if notified {
                    handover_finished(&context, handover_state);
                }
            }
            _event => {
                // May be useful for debugging but nothing else really.
                //log::trace!("Received unwanted event: {:?}", event);
            }
        }
    }
}

/// UTF-8 `bytes` in Latin-1, the `STRING` of ICCCM 2.7.1: a character
/// outside it is `?`.
fn latin1(bytes: &[u8]) -> Vec<u8> {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
        .collect()
}

/// The most a property set in one request holds, for a server whose
/// largest request is `maximum_request_length` four-byte units without
/// the BIG-REQUESTS extension: a `ChangeProperty` request takes 24 bytes
/// of its own, and some room is left besides, as other owners leave it.
fn largest_property(maximum_request_length: u16) -> usize {
    (usize::from(maximum_request_length) * 4)
        .saturating_sub(100)
        .max(4096)
}

/// A selection handed over in pieces to one requestor's property (ICCCM
/// 2.7.2, elundqvist/kvist#424): each deletion of the property by the
/// requestor takes the next piece, at most `piece` bytes, and the empty
/// piece after the last ends it.
struct IncrSend {
    requestor: u32,
    property: Atom,
    target: Atom,
    bytes: Vec<u8>,
    piece: usize,
    /// How much of `bytes` has gone; `None` once the empty piece has.
    sent: Option<usize>,
    /// When the requestor last took a piece, or the hand-over began.
    last: Instant,
}

/// How long a requestor may leave a piece untaken before the hand-over is
/// given up, its window gone or its reading stopped.
const INCR_ABANDONED_AFTER: Duration = Duration::from_secs(10);

/// How long the app, quitting, waits at most for the clipboard manager to
/// take contents handed over in pieces, as long as it keeps taking them.
const MAX_HANDOVER_IN_PIECES: Duration = Duration::from_secs(2);

impl IncrSend {
    fn new(requestor: u32, property: Atom, target: Atom, bytes: Vec<u8>, piece: usize) -> Self {
        Self {
            requestor,
            property,
            target,
            bytes,
            piece: piece.max(1),
            sent: Some(0),
            last: Instant::now(),
        }
    }

    fn is_for(&self, window: u32, property: Atom) -> bool {
        self.requestor == window && self.property == property
    }

    /// The range of `bytes` the next piece is: the empty range after the
    /// last, which ends the hand-over, and `None` after that.
    fn next_piece(&mut self) -> Option<std::ops::Range<usize>> {
        let sent = self.sent?;
        let end = (sent + self.piece).min(self.bytes.len());
        self.sent = (sent < end).then_some(end);
        self.last = Instant::now();
        Some(sent..end)
    }

    fn abandoned(&self, now: Instant) -> bool {
        now.saturating_duration_since(self.last) > INCR_ABANDONED_AFTER
    }
}

pub(crate) struct Clipboard {
    inner: Arc<Inner>,
}

impl Clipboard {
    pub(crate) fn new() -> Result<Self> {
        let mut global_cb = CLIPBOARD.lock();
        if let Some(global_cb) = &*global_cb {
            return Ok(Self {
                inner: Arc::clone(&global_cb.inner),
            });
        }
        // At this point we know that the clipboard does not exist.
        let ctx = Arc::new(Inner::new()?);
        let join_handle = std::thread::Builder::new()
            .name("Clipboard".to_owned())
            .spawn({
                let ctx = Arc::clone(&ctx);
                move || {
                    if let Err(error) = serve_requests(ctx) {
                        log::error!("Worker thread errored with: {}", error);
                    }
                }
            })
            .unwrap();
        *global_cb = Some(GlobalClipboard {
            inner: Arc::clone(&ctx),
            server_handle: join_handle,
        });
        Ok(Self { inner: ctx })
    }

    pub(crate) fn set_text(
        &self,
        message: Cow<'_, str>,
        selection: ClipboardKind,
        wait: WaitConfig,
    ) -> Result<()> {
        let data = vec![ClipboardData {
            bytes: message.into_owned().into_bytes(),
            format: self.inner.atoms.UTF8_STRING,
        }];
        self.inner.write(data, selection, wait)
    }

    #[allow(unused)]
    pub(crate) fn set_image(
        &self,
        image: Image,
        selection: ClipboardKind,
        wait: WaitConfig,
    ) -> Result<()> {
        let format = self.inner.image_format_atom(image.format);
        let data = vec![ClipboardData {
            bytes: image.bytes,
            format: self.inner.atoms.PNG__MIME,
        }];
        self.inner.write(data, selection, wait)
    }

    pub(crate) fn get_any(&self, selection: ClipboardKind) -> Result<ClipboardItem> {
        self.inner.get_any(selection)
    }

    /// A reader of the clipboard for another thread
    /// (elundqvist/kvist#401).
    pub(crate) fn reader(&self) -> ClipboardReader {
        ClipboardReader {
            inner: Arc::downgrade(&self.inner),
        }
    }

    pub fn is_owner(&self, selection: ClipboardKind) -> bool {
        self.inner.is_owner(selection).unwrap_or(false)
    }
}

/// Reads the clipboard on a thread other than the one that draws.
///
/// Changed in elundqvist/gpui-kit (elundqvist/kvist#401): the clipboard
/// another program owns was read on the drawing thread, which a paste of
/// 1.4 MB held for about 35 ms while the owner handed it over. The reader
/// asks the owner on a connection of its own, as before, from a
/// background thread; a `Clipboard`'s handover to the clipboard manager
/// when the last is dropped is not this one's to make, so it is a handle
/// of its own rather than a `Clipboard`, and one the last `Clipboard`
/// does not take for another while it reads: an app that quit while a
/// paste was read handed the clipboard manager nothing.
pub(crate) struct ClipboardReader {
    inner: Weak<Inner>,
}

impl ClipboardReader {
    pub(crate) fn get_any(&self, selection: ClipboardKind) -> Result<ClipboardItem> {
        let inner = self.inner.upgrade().ok_or(Error::ContentNotAvailable)?;
        inner.reads.fetch_add(1, Ordering::SeqCst);
        let _read = gpui_util::defer(|| {
            inner.reads.fetch_sub(1, Ordering::SeqCst);
        });
        inner.get_any(selection)
    }
}

impl Inner {
    fn image_format_atom(&self, format: ImageFormat) -> Atom {
        match format {
            ImageFormat::Png => self.atoms.PNG__MIME,
            ImageFormat::Jpeg => self.atoms.JPEG_MIME,
            ImageFormat::Webp => self.atoms.WEBP_MIME,
            ImageFormat::Gif => self.atoms.GIF__MIME,
            ImageFormat::Svg => self.atoms.SVG__MIME,
            ImageFormat::Bmp => self.atoms.BMP__MIME,
            ImageFormat::Tiff => self.atoms.TIFF_MIME,
            ImageFormat::Ico => self.atoms.ICO__MIME,
            ImageFormat::Pnm => self.atoms.PNM__MIME,
        }
    }

    fn get_any(&self, selection: ClipboardKind) -> Result<ClipboardItem> {
        let image_entries = ImageFormat::iter()
            .map(|format| (self.image_format_atom(format), format))
            .collect::<Vec<_>>();

        let text_format_atoms: &[Atom] = &[
            self.atoms.UTF8_STRING,
            self.atoms.UTF8_MIME_0,
            self.atoms.UTF8_MIME_1,
            self.atoms.STRING,
            self.atoms.TEXT,
            self.atoms.TEXT_MIME_UNKNOWN,
        ];

        // image formats first, as they are more specific, and read will return the first
        // format that the contents can be converted to
        let mut format_atoms = Vec::with_capacity(image_entries.len() + text_format_atoms.len());
        format_atoms.extend(image_entries.iter().map(|(atom, _)| *atom));
        format_atoms.extend_from_slice(text_format_atoms);

        let result = self.read(&format_atoms, selection)?;

        log::trace!(
            "read clipboard as format {:?}",
            self.atom_name(result.format)
        );

        for (format_atom, image_format) in image_entries {
            if result.format == format_atom {
                let bytes = result.bytes;
                let id = hash(&bytes);
                return Ok(ClipboardItem::new_image(&Image {
                    id,
                    format: image_format,
                    bytes,
                }));
            }
        }

        let text = if result.format == self.atoms.STRING {
            // ISO Latin-1
            // See: https://stackoverflow.com/questions/28169745/what-are-the-options-to-convert-iso-8859-1-latin-1-to-a-string-utf-8
            result.bytes.into_iter().map(|c| c as char).collect()
        } else {
            String::from_utf8(result.bytes).map_err(|_| Error::ConversionFailure)?
        };
        Ok(ClipboardItem::new_string(text))
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // There are always at least 3 owners:
        // the global, the server thread, and one `Clipboard::inner`
        const MIN_OWNERS: usize = 3;

        // We start with locking the global guard to prevent race
        // conditions below.
        let mut global_cb = CLIPBOARD.lock();
        // a read on another thread holds it too while it reads (elundqvist/kvist#401)
        let reads = self.inner.reads.load(Ordering::SeqCst);
        if Arc::strong_count(&self.inner).saturating_sub(reads) == MIN_OWNERS {
            // If the are the only owners of the clipboard are ourselves and
            // the global object, then we should destroy the global object,
            // and send the data to the clipboard manager

            if let Err(e) = self.inner.ask_clipboard_manager_to_request_our_data() {
                log::error!(
                    "Could not hand the clipboard data over to the clipboard manager: {}",
                    e
                );
            }
            let global_cb = global_cb.take();
            if let Err(e) = self
                .inner
                .server
                .conn
                .destroy_window(self.inner.server.win_id)
            {
                log::error!("Failed to destroy the clipboard window. Error: {}", e);
                return;
            }
            if let Err(e) = self.inner.server.conn.flush() {
                log::error!("Failed to flush the clipboard window. Error: {}", e);
                return;
            }
            if let Some(global_cb) = global_cb
                && let Err(e) = global_cb.server_handle.join()
            {
                // Let's try extracting the error message
                let message;
                if let Some(msg) = e.downcast_ref::<&'static str>() {
                    message = Some((*msg).to_string());
                } else if let Some(msg) = e.downcast_ref::<String>() {
                    message = Some(msg.clone());
                } else {
                    message = None;
                }
                if let Some(message) = message {
                    log::error!(
                        "The clipboard server thread panicked. Panic message: '{}'",
                        message,
                    );
                } else {
                    log::error!("The clipboard server thread panicked.");
                }
            }
        }
    }
}

fn into_unknown<E: std::fmt::Display>(error: E) -> Error {
    Error::Unknown {
        description: error.to_string(),
    }
}

/// Clipboard selection
///
/// Linux has a concept of clipboard "selections" which tend to be used in different contexts. This
/// enum provides a way to get/set to a specific clipboard
///
/// See <https://specifications.freedesktop.org/clipboards-spec/clipboards-0.1.txt> for a better
/// description of the different clipboards.
#[derive(Copy, Clone, Debug)]
pub enum ClipboardKind {
    /// Typically used selection for explicit cut/copy/paste actions (ie. windows/macos like
    /// clipboard behavior)
    Clipboard,

    /// Typically used for mouse selections and/or currently selected text. Accessible via middle
    /// mouse click.
    Primary,

    /// The secondary clipboard is rarely used but theoretically available on X11.
    Secondary,
}

/// Configuration on how long to wait for a new X11 copy event is emitted.
#[derive(Default)]
pub(crate) enum WaitConfig {
    /// Waits until the given [`Instant`] has reached.
    #[allow(
        unused,
        reason = "Right now we don't wait for clipboard contents to sync on app close, but we may in the future"
    )]
    Until(Instant),

    /// Waits forever until a new event is reached.
    #[allow(unused)]
    #[allow(
        unused,
        reason = "Right now we don't wait for clipboard contents to sync on app close, but we may in the future"
    )]
    Forever,

    /// It shouldn't wait.
    #[default]
    None,
}

#[non_exhaustive]
pub enum Error {
    /// The clipboard contents were not available in the requested format.
    /// This could either be due to the clipboard being empty or the clipboard contents having
    /// an incompatible format to the requested one (eg when calling `get_image` on text)
    ContentNotAvailable,

    /// The native clipboard is not accessible due to being held by an other party.
    ///
    /// This "other party" could be a different process or it could be within
    /// the same program. So for example you may get this error when trying
    /// to interact with the clipboard from multiple threads at once.
    ///
    /// Note that it's OK to have multiple `Clipboard` instances. The underlying
    /// implementation will make sure that the native clipboard is only
    /// opened for transferring data and then closed as soon as possible.
    ClipboardOccupied,

    /// The image or the text that was about the be transferred to/from the clipboard could not be
    /// converted to the appropriate format.
    ConversionFailure,

    /// The owner of the clipboard did not answer in time.
    ///
    /// Changed in elundqvist/gpui-kit (elundqvist/kvist#401): a time-out
    /// was taken for content not available, and the reader went on to ask
    /// for every other format, waiting as long for each.
    TimedOut,

    /// Any error that doesn't fit the other error types.
    ///
    /// The `description` field is only meant to help the developer and should not be relied on as a
    /// means to identify an error case during runtime.
    Unknown { description: String },
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
			Error::ContentNotAvailable => f.write_str("The clipboard contents were not available in the requested format or the clipboard is empty."),
			Error::ClipboardOccupied => f.write_str("The native clipboard is not accessible due to being held by an other party."),
			Error::ConversionFailure => f.write_str("The image or the text that was about the be transferred to/from the clipboard could not be converted to the appropriate format."),
			Error::TimedOut => f.write_str("The owner of the clipboard did not answer in time."),
			Error::Unknown { description } => f.write_fmt(format_args!("Unknown error while interacting with the clipboard: {description}")),
		}
    }
}

impl std::error::Error for Error {}

impl std::fmt::Debug for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        use Error::*;
        macro_rules! kind_to_str {
			($( $e: pat ),*) => {
				match self {
					$(
						$e => stringify!($e),
					)*
				}
			}
		}
        let name = kind_to_str!(
            ContentNotAvailable,
            ClipboardOccupied,
            ConversionFailure,
            TimedOut,
            Unknown { .. }
        );
        f.write_fmt(format_args!("{name} - \"{self}\""))
    }
}

impl Error {
    pub(crate) fn unknown<M: Into<String>>(message: M) -> Self {
        Error::Unknown {
            description: message.into(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 1.4 MB of text, the paste of elundqvist/kvist#424.
    fn large_text() -> Vec<u8> {
        (0..40_000)
            .map(|i| format!("fn f{i}(x: u64) -> u64 {{ x * {i} }}\n"))
            .collect::<String>()
            .into_bytes()
    }

    #[test]
    fn the_largest_property_is_the_servers_largest_request_less_its_fields() {
        // a server's usual largest request, 65,535 units of four bytes
        assert_eq!(largest_property(u16::MAX), 262_040);
        // Tk reads a property 100,000 units of four bytes at a time
        assert!(largest_property(u16::MAX) < 400_000);
        assert_eq!(largest_property(0), 4096);
    }

    #[test]
    fn a_selection_in_pieces_is_handed_over_whole_and_ended_by_an_empty_piece() {
        let text = large_text();
        let piece = largest_property(u16::MAX);
        let mut transfer = IncrSend::new(7, 8, 9, text.clone(), piece);
        assert!(transfer.is_for(7, 8));
        assert!(!transfer.is_for(7, 9) && !transfer.is_for(6, 8));
        let mut got = Vec::new();
        let mut pieces = 0;
        loop {
            let range = transfer.next_piece().expect("a piece until the empty one");
            assert!(range.len() <= piece);
            if range.is_empty() {
                break;
            }
            pieces += 1;
            got.extend_from_slice(&text[range]);
        }
        assert_eq!(got, text);
        assert_eq!(pieces, text.len().div_ceil(piece));
        assert_eq!(transfer.next_piece(), None, "nothing after the empty piece");
    }

    #[test]
    fn a_hand_over_whose_requestor_takes_nothing_is_given_up() {
        let transfer = IncrSend::new(7, 8, 9, vec![1; 10], 4);
        let now = Instant::now();
        assert!(!transfer.abandoned(now));
        assert!(transfer.abandoned(now + INCR_ABANDONED_AFTER + Duration::from_secs(1)));
    }

    /// An X server of the test's own, never the desktop's, whose clipboard
    /// a test would take: an Xvfb on a display it picks (`-displayfd`),
    /// stopped when this is dropped, or the server `GPUI_X11_TEST_DISPLAY`
    /// names. `None` where there is neither, and the test is skipped, as
    /// tests that need a program the machine may not have are.
    struct TestDisplay {
        name: String,
        xvfb: Option<std::process::Child>,
    }

    impl TestDisplay {
        fn start() -> Option<Self> {
            use std::io::BufRead as _;
            use std::os::fd::{FromRawFd as _, OwnedFd};
            if let Ok(name) = std::env::var("GPUI_X11_TEST_DISPLAY") {
                return Some(Self { name, xvfb: None });
            }
            let mut ends = [0; 2];
            // SAFETY: `ends` holds the two descriptors `pipe` writes.
            if unsafe { libc::pipe(ends.as_mut_ptr()) } != 0 {
                return None;
            }
            // SAFETY: both are open, and owned here alone.
            let (read, write) =
                unsafe { (OwnedFd::from_raw_fd(ends[0]), OwnedFd::from_raw_fd(ends[1])) };
            // SAFETY: `read` is open; the server is not to hold it.
            unsafe { libc::fcntl(ends[0], libc::F_SETFD, libc::FD_CLOEXEC) };
            let xvfb = std::process::Command::new("Xvfb")
                .args(["-displayfd", &ends[1].to_string()])
                .args(["-nolisten", "tcp", "-screen", "0", "64x64x24"])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
            drop(write);
            let Ok(xvfb) = xvfb else {
                eprintln!("no Xvfb to run an X server on: skipped");
                return None;
            };
            let mut display = Self {
                name: String::new(),
                xvfb: Some(xvfb),
            };
            // the server writes its display's number once it takes
            // connections, and the pipe ends if it ends first
            let mut number = String::new();
            std::io::BufReader::new(std::fs::File::from(read))
                .read_line(&mut number)
                .ok()?;
            let number = number.trim();
            assert!(!number.is_empty(), "Xvfb ended before it took connections");
            display.name = format!(":{number}");
            Some(display)
        }
    }

    impl Drop for TestDisplay {
        /// Ends the server as it ends itself, so that it takes its socket
        /// in `/tmp/.X11-unix` with it, which one killed leaves behind;
        /// killed only where it has not ended in five seconds.
        fn drop(&mut self) {
            let Some(xvfb) = &mut self.xvfb else {
                return;
            };
            if let Ok(pid) = libc::pid_t::try_from(xvfb.id()) {
                // SAFETY: `pid` is the server's, which has not been waited
                // for, so it is still this process's child.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until {
                if let Ok(Some(_)) = xvfb.try_wait() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            xvfb.kill().ok();
            xvfb.wait().ok();
        }
    }

    /// Reads the selection as ICCCM has a requestor read it, with Tk's
    /// bound on a property, from a client of its own on `display`: the
    /// data, and whether it came in pieces.
    fn read_as_tk(display: &str, atoms: &Atoms) -> (Vec<u8>, bool) {
        let (data, in_pieces, type_) = read_target(display, atoms, atoms.UTF8_STRING);
        assert_eq!(type_, atoms.UTF8_STRING);
        (data, in_pieces)
    }

    /// Reads the selection converted to `target` as `read_as_tk` does: the
    /// data, whether it came in pieces, and its type.
    fn read_target(display: &str, atoms: &Atoms, target: Atom) -> (Vec<u8>, bool, Atom) {
        const TK_PROPERTY_WORDS: u32 = 100_000;
        let reader = XContext::on(Some(display)).expect("a connection");
        let conn = &reader.conn;
        conn.change_window_attributes(
            reader.win_id,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .unwrap();
        conn.convert_selection(
            reader.win_id,
            atoms.CLIPBOARD,
            target,
            atoms.ARBOARD_CLIPBOARD,
            Time::CURRENT_TIME,
        )
        .unwrap();
        conn.flush().unwrap();
        let until = Instant::now() + Duration::from_secs(10);
        let next_event = || loop {
            assert!(Instant::now() < until, "the owner stopped answering");
            match conn.poll_for_event().unwrap() {
                Some(event) => return event,
                None => wait_for_server(conn, until),
            }
        };
        loop {
            if let Event::SelectionNotify(notify) = next_event() {
                assert_ne!(notify.property, NONE, "the owner refused the request");
                break;
            }
        }
        let get = |delete: bool| {
            conn.get_property(
                delete,
                reader.win_id,
                atoms.ARBOARD_CLIPBOARD,
                AtomEnum::ANY,
                0,
                TK_PROPERTY_WORDS,
            )
            .unwrap()
            .reply()
            .unwrap()
        };
        let first = get(false);
        if first.type_ != atoms.INCR {
            assert_eq!(
                first.bytes_after, 0,
                "the selection was set whole, larger than Tk reads a property: \"selection property too large\""
            );
            return (first.value, false, first.type_);
        }
        // deleting the INCR property asks for the first piece
        get(true);
        conn.flush().unwrap();
        let mut data = Vec::new();
        loop {
            match next_event() {
                Event::PropertyNotify(event)
                    if event.atom == atoms.ARBOARD_CLIPBOARD
                        && event.state == Property::NEW_VALUE =>
                {
                    let piece = get(true);
                    conn.flush().unwrap();
                    assert_eq!(piece.bytes_after, 0, "a piece larger than Tk reads");
                    assert_eq!(piece.type_, target);
                    if piece.value.is_empty() {
                        return (data, true, target);
                    }
                    data.extend_from_slice(&piece.value);
                }
                _ => {}
            }
        }
    }

    /// The owner hands a selection larger than a request holds over in
    /// pieces, as Tk and xclip read one, where it set it whole and Tk took
    /// it for an error (elundqvist/kvist#424); a small one goes whole.
    /// On an X server of its own (`TestDisplay`).
    #[test]
    fn a_large_selection_is_served_in_pieces_to_a_requestor_that_reads_as_tk_does() {
        let Some(server) = TestDisplay::start() else {
            return;
        };
        let display = server.name.clone();
        let owner = Arc::new(Inner::on(Some(&display)).expect("a connection"));
        let serving = std::thread::spawn({
            let owner = owner.clone();
            move || serve_requests(owner).is_ok()
        });
        let atoms = owner.atoms;
        let write = |bytes: Vec<u8>| {
            owner
                .write(
                    vec![ClipboardData {
                        bytes,
                        format: atoms.UTF8_STRING,
                    }],
                    ClipboardKind::Clipboard,
                    WaitConfig::None,
                )
                .unwrap()
        };

        let text = large_text();
        write(text.clone());
        let (got, in_pieces) = read_as_tk(&display, &atoms);
        assert!(in_pieces, "1.4 MB set whole");
        assert_eq!(got.len(), text.len());
        assert!(got == text, "the pieces are the text");

        // twice, the first hand-over over and done with
        let (again, _) = read_as_tk(&display, &atoms);
        assert!(again == text);

        write(b"small".to_vec());
        assert_eq!(read_as_tk(&display, &atoms), (b"small".to_vec(), false));

        owner
            .server
            .conn
            .destroy_window(owner.server.win_id)
            .unwrap();
        owner.server.conn.flush().unwrap();
        assert!(serving.join().unwrap());
    }

    /// A clipboard manager that says it is done before it asks for the
    /// contents, as KDE's does, still gets a large selection whole as the
    /// app quits: the hand-over was taken for done once its pieces had
    /// begun, the app went, and the manager got none of them
    /// (elundqvist/kvist#424). On an X server of its own (`TestDisplay`).
    #[test]
    fn a_manager_that_answers_first_gets_a_large_selection_whole_at_quit() {
        let Some(server) = TestDisplay::start() else {
            return;
        };
        let display = server.name.clone();
        let owner = Arc::new(Inner::on(Some(&display)).expect("a connection"));
        let serving = std::thread::spawn({
            let owner = owner.clone();
            move || serve_requests(owner).is_ok()
        });
        let atoms = owner.atoms;
        let text = large_text();
        owner
            .write(
                vec![ClipboardData {
                    bytes: text.clone(),
                    format: atoms.UTF8_STRING,
                }],
                ClipboardKind::Clipboard,
                WaitConfig::None,
            )
            .unwrap();

        // the manager: it owns CLIPBOARD_MANAGER, answers the app's
        // SAVE_TARGETS at once, and only then reads the clipboard
        let (owns, owned) = std::sync::mpsc::channel();
        let manager = std::thread::spawn({
            let display = display.clone();
            move || {
                let manager = XContext::on(Some(&display)).expect("a connection");
                let conn = &manager.conn;
                conn.set_selection_owner(
                    manager.win_id,
                    atoms.CLIPBOARD_MANAGER,
                    Time::CURRENT_TIME,
                )
                .unwrap();
                conn.sync().unwrap();
                owns.send(()).unwrap();
                let until = Instant::now() + Duration::from_secs(10);
                let request = loop {
                    assert!(Instant::now() < until, "the app never asked the manager");
                    match conn.poll_for_event().unwrap() {
                        Some(Event::SelectionRequest(request))
                            if request.target == atoms.SAVE_TARGETS =>
                        {
                            break request;
                        }
                        Some(_) => {}
                        None => wait_for_server(conn, until),
                    }
                };
                conn.send_event(
                    false,
                    request.requestor,
                    EventMask::NO_EVENT,
                    SelectionNotifyEvent {
                        response_type: SELECTION_NOTIFY_EVENT,
                        sequence: 0,
                        time: request.time,
                        requestor: request.requestor,
                        selection: request.selection,
                        target: request.target,
                        property: NONE,
                    },
                )
                .unwrap();
                conn.flush().unwrap();
                read_as_tk(&display, &atoms)
            }
        });
        owned.recv().unwrap();

        // the app quits: it asks the manager to take the clipboard, and
        // its window goes once that is done, as `Clipboard`'s drop does
        owner.ask_clipboard_manager_to_request_our_data().unwrap();
        owner
            .server
            .conn
            .destroy_window(owner.server.win_id)
            .unwrap();
        owner.server.conn.flush().unwrap();

        let (got, in_pieces) = manager.join().expect("the manager read it all");
        assert!(in_pieces, "1.4 MB set whole");
        assert!(got == text, "the manager got the text");
        assert!(serving.join().unwrap());
    }

    /// A read of a clipboard whose owner does not answer gives up after
    /// one wait, where it went on to ask for each of the formats in turn,
    /// as long for each, and a paste waited a minute for nothing
    /// (elundqvist/kvist#401). On an X server of its own (`TestDisplay`).
    #[test]
    fn a_read_from_an_owner_that_does_not_answer_gives_up_once() {
        let Some(server) = TestDisplay::start() else {
            return;
        };
        let display = server.name.clone();
        let reader = Inner::on(Some(&display)).expect("a connection");
        // an owner that never reads its events, as one stopped or hung
        let hung = XContext::on(Some(&display)).expect("a connection");
        hung.conn
            .set_selection_owner(hung.win_id, reader.atoms.CLIPBOARD, Time::CURRENT_TIME)
            .unwrap();
        hung.conn.sync().unwrap();

        let began = Instant::now();
        let read = reader.get_any(ClipboardKind::Clipboard);
        assert!(matches!(read, Err(Error::TimedOut)), "{:?}", read.err());
        assert!(
            began.elapsed() < LONG_TIMEOUT_DUR * 2,
            "gave up after {:?}",
            began.elapsed()
        );
    }

    /// Text the app owns is handed over as every target it lists: the two
    /// MIME types of UTF-8 as `UTF8_STRING` is, and `STRING` in Latin-1,
    /// which older programs ask for: the first two were refused, and Tk
    /// asked for the third said `form "STRING" not defined`. In pieces too
    /// where it is large. On an X server of its own (`TestDisplay`).
    #[test]
    fn text_is_served_as_every_target_the_owner_lists() {
        let Some(server) = TestDisplay::start() else {
            return;
        };
        let display = server.name.clone();
        let owner = Arc::new(Inner::on(Some(&display)).expect("a connection"));
        let serving = std::thread::spawn({
            let owner = owner.clone();
            move || serve_requests(owner).is_ok()
        });
        let atoms = owner.atoms;
        let write = |text: &str| {
            owner
                .write(
                    vec![ClipboardData {
                        bytes: text.as_bytes().to_vec(),
                        format: atoms.UTF8_STRING,
                    }],
                    ClipboardKind::Clipboard,
                    WaitConfig::None,
                )
                .unwrap()
        };

        write("héllo ✓");
        let (targets, _, type_) = read_target(&display, &atoms, atoms.TARGETS);
        assert_eq!(type_, atoms.ATOM);
        let targets = Inner::parse_formats(&targets);
        for target in [
            atoms.UTF8_STRING,
            atoms.UTF8_MIME_0,
            atoms.UTF8_MIME_1,
            atoms.STRING,
        ] {
            assert!(targets.contains(&target), "{target} listed");
        }
        for mime in [atoms.UTF8_MIME_0, atoms.UTF8_MIME_1] {
            let (got, in_pieces, type_) = read_target(&display, &atoms, mime);
            assert_eq!(
                (got, in_pieces, type_),
                ("héllo ✓".as_bytes().to_vec(), false, mime)
            );
        }
        let (got, _, type_) = read_target(&display, &atoms, atoms.STRING);
        assert_eq!((got, type_), (b"h\xe9llo ?".to_vec(), atoms.STRING));

        let text = String::from_utf8(large_text()).unwrap();
        write(&text);
        let (got, in_pieces, _) = read_target(&display, &atoms, atoms.STRING);
        assert!(in_pieces, "1.4 MB of Latin-1 set whole");
        assert!(got == text.as_bytes(), "the pieces are the text");

        owner
            .server
            .conn
            .destroy_window(owner.server.win_id)
            .unwrap();
        owner.server.conn.flush().unwrap();
        assert!(serving.join().unwrap());
    }

    #[test]
    fn latin1_is_the_text_with_a_character_outside_it_as_a_question_mark() {
        assert_eq!(latin1("héllo ✓ ÿ".as_bytes()), b"h\xe9llo ? \xff");
    }
}
