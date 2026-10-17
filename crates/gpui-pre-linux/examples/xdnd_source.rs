//! An XDND drag source to try a drag over a window with, as `xdotool`
//! cannot: it offers the types it is given, sends `XdndEnter` and an
//! `XdndPosition` at the window's centre (where it puts the pointer),
//! answers the selection request with the data it is given, prints the
//! window's `XdndStatus`, and then leaves or drops. Added in
//! elundqvist/gpui-kit for elundqvist/kvist#218.
//!
//! ```text
//! cargo run --example xdnd_source -- <window id> <type>... [--data <text>]
//!     [--drop] [--no-enter] [--type-list]
//! ```
//!
//! - `--data <text>`: what a request for the drag's data in one of its
//!   types is answered with, `file:///tmp/x.txt` for `text/uri-list`;
//!   without it, every request is refused.
//! - `--drop`: drop after the status rather than leave, even when the
//!   window refused the drag, as a source should not.
//! - `--no-enter`: send the `XdndPosition` with no `XdndEnter` before it.
//! - `--type-list`: offer the types in `XdndTypeList` rather than in the
//!   `XdndEnter` message, as a source with more than three must.
//!
//! It prints one line for each thing that happens, among them `status:
//! accepted` or `status: not accepted`, and exits 0 once it has left or
//! dropped.

use std::error::Error;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, PropMode,
    SELECTION_NOTIFY_EVENT, SelectionNotifyEvent, SelectionRequestEvent, Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as _;
use x11rb::{COPY_DEPTH_FROM_PARENT, CURRENT_TIME, NONE};

struct Atoms {
    aware: u32,
    enter: u32,
    position: u32,
    status: u32,
    leave: u32,
    drop: u32,
    finished: u32,
    selection: u32,
    type_list: u32,
    action_copy: u32,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let usage = "usage: xdnd_source <window id> <type>... [--data <text>] [--drop] [--no-enter] [--type-list]";
    let target = args.next().ok_or(usage)?;
    let target: Window = match target.strip_prefix("0x") {
        Some(hex) => u32::from_str_radix(hex, 16)?,
        None => target.parse()?,
    };
    let (mut type_names, mut data, mut drops, mut no_enter, mut type_list) =
        (Vec::new(), None, false, false, false);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--data" => data = Some(args.next().ok_or(usage)?),
            "--drop" => drops = true,
            "--no-enter" => no_enter = true,
            "--type-list" => type_list = true,
            _ => type_names.push(arg),
        }
    }
    if type_names.is_empty() {
        return Err(usage.into());
    }

    let (conn, screen_num) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen_num].root;
    let intern = |name: &str| -> Result<u32, Box<dyn Error>> {
        Ok(conn.intern_atom(false, name.as_bytes())?.reply()?.atom)
    };
    let atoms = Atoms {
        aware: intern("XdndAware")?,
        enter: intern("XdndEnter")?,
        position: intern("XdndPosition")?,
        status: intern("XdndStatus")?,
        leave: intern("XdndLeave")?,
        drop: intern("XdndDrop")?,
        finished: intern("XdndFinished")?,
        selection: intern("XdndSelection")?,
        type_list: intern("XdndTypeList")?,
        action_copy: intern("XdndActionCopy")?,
    };
    let types: Vec<u32> = type_names
        .iter()
        .map(|name| intern(name))
        .collect::<Result<_, _>>()?;

    // of any type: gpui gives it the type of an atom it names "XA_ATOM"
    let aware = conn
        .get_property(false, target, atoms.aware, AtomEnum::ANY, 0, 1)?
        .reply()?;
    let Some(version) = aware.value32().and_then(|mut value| value.next()) else {
        return Err(format!("window {target:#x} is not XdndAware").into());
    };
    let version = version.min(5);
    println!("window {target:#x} is XdndAware, version {version}");

    let source = conn.generate_id()?;
    conn.create_window(
        COPY_DEPTH_FROM_PARENT,
        source,
        root,
        0,
        0,
        1,
        1,
        0,
        WindowClass::INPUT_ONLY,
        0,
        &CreateWindowAux::new(),
    )?;
    conn.set_selection_owner(source, atoms.selection, CURRENT_TIME)?;
    let in_list = type_list || types.len() > 3;
    if in_list {
        conn.change_property32(
            PropMode::REPLACE,
            source,
            atoms.type_list,
            AtomEnum::ATOM,
            &types,
        )?;
    }

    let geometry = conn.get_geometry(target)?.reply()?;
    let (x, y) = ((geometry.width / 2) as i16, (geometry.height / 2) as i16);
    let at = conn.translate_coordinates(target, root, x, y)?.reply()?;
    conn.warp_pointer(NONE, target, 0, 0, 0, 0, x, y)?;

    let send = |type_: u32, data: [u32; 5]| -> Result<(), Box<dyn Error>> {
        let message = ClientMessageEvent::new(32, target, type_, data);
        conn.send_event(false, target, EventMask::NO_EVENT, message)?;
        conn.flush()?;
        Ok(())
    };
    if !no_enter {
        let offered = |i: usize| {
            if in_list {
                0
            } else {
                types.get(i).copied().unwrap_or(0)
            }
        };
        send(
            atoms.enter,
            [
                source,
                (version << 24) | in_list as u32,
                offered(0),
                offered(1),
                offered(2),
            ],
        )?;
        println!("sent XdndEnter offering {}", type_names.join(", "));
    }
    let point = ((at.dst_x as u16 as u32) << 16) | at.dst_y as u16 as u32;
    send(
        atoms.position,
        [source, 0, point, CURRENT_TIME, atoms.action_copy],
    )?;
    println!("sent XdndPosition at {}, {}", at.dst_x, at.dst_y);

    let accepted = match wait(&conn, &atoms, source, &types, data.as_deref(), atoms.status)? {
        Some(status) => {
            let accepted = status.data.as_data32()[1] & 1 == 1;
            println!(
                "status: {}",
                if accepted { "accepted" } else { "not accepted" }
            );
            accepted
        }
        None => {
            println!("status: none came");
            false
        }
    };

    if drops {
        send(atoms.drop, [source, 0, CURRENT_TIME, 0, 0])?;
        println!(
            "sent XdndDrop{}",
            if accepted {
                ""
            } else {
                " to a window that refused the drag"
            }
        );
        match wait(
            &conn,
            &atoms,
            source,
            &types,
            data.as_deref(),
            atoms.finished,
        )? {
            Some(finished) => {
                let done = finished.data.as_data32()[1] & 1 == 1;
                println!(
                    "finished: {}",
                    if done { "accepted" } else { "not accepted" }
                );
            }
            None => println!("finished: none came"),
        }
    } else {
        send(atoms.leave, [source, 0, 0, 0, 0])?;
        println!("sent XdndLeave");
        // answer what is still on its way, then go
        wait(&conn, &atoms, source, &types, data.as_deref(), NONE)?;
    }
    Ok(())
}

/// Waits up to two seconds for a client message of type `wanted`,
/// answering requests for the drag's data meanwhile; with `wanted` NONE
/// it only answers them for half a second.
fn wait(
    conn: &impl Connection,
    atoms: &Atoms,
    source: Window,
    types: &[u32],
    data: Option<&str>,
    wanted: u32,
) -> Result<Option<ClientMessageEvent>, Box<dyn Error>> {
    let limit = if wanted == NONE { 500 } else { 2000 };
    let deadline = Instant::now() + Duration::from_millis(limit);
    while Instant::now() < deadline {
        let Some(event) = conn.poll_for_event()? else {
            std::thread::sleep(Duration::from_millis(10));
            continue;
        };
        match event {
            Event::ClientMessage(message) if wanted != NONE && message.type_ == wanted => {
                return Ok(Some(message));
            }
            Event::SelectionRequest(request) if request.selection == atoms.selection => {
                answer(conn, source, types, data, &request)?;
            }
            _ => {}
        }
    }
    Ok(None)
}

fn answer(
    conn: &impl Connection,
    source: Window,
    types: &[u32],
    data: Option<&str>,
    request: &SelectionRequestEvent,
) -> Result<(), Box<dyn Error>> {
    let name = if request.target == NONE {
        "atom 0".to_string()
    } else {
        String::from_utf8_lossy(&conn.get_atom_name(request.target)?.reply()?.name).into_owned()
    };
    let property = match data {
        Some(data) if request.owner == source && types.contains(&request.target) => {
            conn.change_property8(
                PropMode::REPLACE,
                request.requestor,
                request.property,
                request.target,
                data.as_bytes(),
            )?;
            println!("selection asked for in {name}: answered");
            request.property
        }
        _ => {
            println!("selection asked for in {name}: refused");
            NONE
        }
    };
    let notify = SelectionNotifyEvent {
        response_type: SELECTION_NOTIFY_EVENT,
        sequence: 0,
        time: request.time,
        requestor: request.requestor,
        selection: request.selection,
        target: request.target,
        property,
    };
    conn.send_event(false, request.requestor, EventMask::NO_EVENT, notify)?;
    conn.flush()?;
    Ok(())
}
