use crate::gui::paint::Painter;
use anyhow::{Context as _, Result};
use egui::{
    CursorIcon, Event as InputEvent, Key, Modifiers, MouseWheelUnit, OutputCommand, PointerButton,
    RawInput, Rect, ViewportCommand, ViewportEvent, ViewportId, pos2, vec2,
};
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use x11rb::connection::Connection;
use x11rb::image::{BitsPerPixel, Image, ImageOrder, ScanlinePad};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::*;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        WM_PROTOCOLS, WM_DELETE_WINDOW, WM_NORMAL_HINTS, WM_SIZE_HINTS,
        UTF8_STRING, _NET_WM_NAME, CLIPBOARD, TARGETS, TEXT, INCR,
        RUSTIDE_CLIPBOARD,
    }
}

pub(crate) fn run(title: &str, mut update: impl FnMut(&egui::Context)) -> Result<()> {
    let (connection, screen_number) = x11rb::connect(None)
        .context("Cannot open a desktop window. Linux needs an X11 or XWayland display")?;
    let screen = &connection.setup().roots[screen_number];
    let atoms = Atoms::new(&connection)?.reply()?;
    let geometry = super::fit_window(
        [
            u32::from(screen.width_in_pixels),
            u32::from(screen.height_in_pixels),
        ],
        display_scale(&connection, screen.root),
        [48, 80],
    );
    let native_scale = geometry.scale;
    let mut size = geometry.size.map(|dimension| dimension as u16);
    let window = connection.generate_id()?;
    connection.create_window(
        screen.root_depth,
        window,
        screen.root,
        0,
        0,
        size[0],
        size[1],
        0,
        WindowClass::INPUT_OUTPUT,
        screen.root_visual,
        &CreateWindowAux::new()
            .background_pixel(0x10151d)
            .event_mask(
                EventMask::EXPOSURE
                    | EventMask::STRUCTURE_NOTIFY
                    | EventMask::KEY_PRESS
                    | EventMask::KEY_RELEASE
                    | EventMask::BUTTON_PRESS
                    | EventMask::BUTTON_RELEASE
                    | EventMask::POINTER_MOTION
                    | EventMask::ENTER_WINDOW
                    | EventMask::LEAVE_WINDOW
                    | EventMask::FOCUS_CHANGE
                    | EventMask::PROPERTY_CHANGE,
            ),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_NAME,
        AtomEnum::STRING,
        title.as_bytes(),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        atoms._NET_WM_NAME,
        atoms.UTF8_STRING,
        title.as_bytes(),
    )?;
    connection.change_property8(
        PropMode::REPLACE,
        window,
        AtomEnum::WM_CLASS,
        AtomEnum::STRING,
        b"rustide\0Rustide\0",
    )?;
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.WM_PROTOCOLS,
        AtomEnum::ATOM,
        &[atoms.WM_DELETE_WINDOW],
    )?;
    minimum_size(&connection, &atoms, window, geometry.minimum)?;
    let graphics = connection.generate_id()?;
    connection.create_gc(graphics, window, &CreateGCAux::new().graphics_exposures(0))?;
    connection.map_window(window)?;
    connection.flush()?;

    let context = egui::Context::default();
    let (mut wake_reader, wake_writer) = UnixStream::pair()?;
    wake_reader.set_nonblocking(true)?;
    wake_writer.set_nonblocking(true)?;
    let repaint = Arc::new(Mutex::new(None::<Instant>));
    let callback_repaint = repaint.clone();
    context.set_request_repaint_callback(move |request| {
        if let Some(deadline) = Instant::now().checked_add(request.delay) {
            let mut pending = callback_repaint
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *pending = Some(pending.map_or(deadline, |old| old.min(deadline)));
            let _ = (&wake_writer).write(&[1]);
        }
    });
    let start = Instant::now();
    let mut painter = Painter::default();
    let mut clipboard = Clipboard::default();
    let mut keys = Keyboard::load(&connection)?;
    let mut cursors = Cursors::new(&connection)?;
    let mut raw = RawInput::default();
    let mut focused = true;
    let mut close_requested = false;
    let mut redraw = true;
    let mut next_frame = None;
    let mut time = x11rb::CURRENT_TIME;

    loop {
        let scale = native_scale * context.zoom_factor();
        while let Some(event) = connection.poll_for_event()? {
            match event {
                Event::Expose(_) => redraw = true,
                Event::ConfigureNotify(event) => {
                    size = [event.width.max(1), event.height.max(1)];
                    redraw = true;
                }
                Event::ClientMessage(event)
                    if event.type_ == atoms.WM_PROTOCOLS
                        && event.data.as_data32()[0] == atoms.WM_DELETE_WINDOW =>
                {
                    close_requested = true;
                    redraw = true;
                }
                Event::DestroyNotify(event) if event.window == window => return Ok(()),
                Event::FocusIn(_) => {
                    focused = true;
                    redraw = true;
                }
                Event::FocusOut(_) => {
                    focused = false;
                    raw.modifiers = Modifiers::default();
                    raw.events.push(InputEvent::WindowFocused(false));
                    redraw = true;
                }
                Event::MotionNotify(event) => {
                    time = event.time;
                    raw.modifiers = modifiers(event.state);
                    raw.events.push(InputEvent::PointerMoved(pos2(
                        event.event_x as f32 / scale,
                        event.event_y as f32 / scale,
                    )));
                    redraw = true;
                }
                Event::EnterNotify(event) => {
                    raw.events.push(InputEvent::PointerMoved(pos2(
                        event.event_x as f32 / scale,
                        event.event_y as f32 / scale,
                    )));
                    redraw = true;
                }
                Event::LeaveNotify(_) => {
                    raw.events.push(InputEvent::PointerGone);
                    redraw = true;
                }
                Event::ButtonPress(event) | Event::ButtonRelease(event) => {
                    time = event.time;
                    let pressed = event.response_type & 0x7f == BUTTON_PRESS_EVENT;
                    let mods = modifiers(event.state);
                    raw.modifiers = mods;
                    let pos = pos2(event.event_x as f32 / scale, event.event_y as f32 / scale);
                    match event.detail {
                        1..=3 | 8..=9 => {
                            let button = match event.detail {
                                1 => PointerButton::Primary,
                                2 => PointerButton::Middle,
                                3 => PointerButton::Secondary,
                                8 => PointerButton::Extra1,
                                _ => PointerButton::Extra2,
                            };
                            raw.events.push(InputEvent::PointerButton {
                                pos,
                                button,
                                pressed,
                                modifiers: mods,
                            });
                        }
                        4..=7 if pressed => {
                            let delta = match event.detail {
                                4 => vec2(0.0, 3.0),
                                5 => vec2(0.0, -3.0),
                                6 => vec2(3.0, 0.0),
                                _ => vec2(-3.0, 0.0),
                            };
                            raw.events.push(InputEvent::MouseWheel {
                                unit: MouseWheelUnit::Line,
                                delta,
                                modifiers: mods,
                            });
                        }
                        _ => {}
                    }
                    redraw = true;
                }
                Event::KeyPress(event) | Event::KeyRelease(event) => {
                    time = event.time;
                    let pressed = event.response_type & 0x7f == KEY_PRESS_EVENT;
                    let mods = modifiers(event.state);
                    raw.modifiers = mods;
                    let symbol = keys.symbol(event.detail, event.state);
                    let key = key_from_symbol(symbol).or_else(|| {
                        key_from_symbol(keys.symbol(event.detail, KeyButMask::default()))
                    });
                    if pressed
                        && ((mods.ctrl && key == Some(Key::V))
                            || (mods.shift && key == Some(Key::Insert)))
                    {
                        clipboard.paste(&connection, &atoms, window, time)?;
                    } else if pressed && mods.ctrl && matches!(key, Some(Key::C | Key::X)) {
                        raw.events.push(if key == Some(Key::C) {
                            InputEvent::Copy
                        } else {
                            InputEvent::Cut
                        });
                    } else {
                        if let Some(key) = key {
                            raw.events.push(InputEvent::Key {
                                key,
                                physical_key: None,
                                pressed,
                                repeat: false,
                                modifiers: mods,
                            });
                        }
                        if pressed
                            && ((!mods.ctrl && !mods.alt) || event.state.contains(KeyButMask::MOD5))
                            && let Some(character) =
                                symbol_character(symbol).filter(|character| !character.is_control())
                        {
                            raw.events.push(InputEvent::Text(character.to_string()));
                        }
                    }
                    redraw = true;
                }
                Event::MappingNotify(_) => {
                    keys = Keyboard::load(&connection)?;
                }
                Event::SelectionRequest(event) => clipboard.answer(&connection, &atoms, event)?,
                Event::SelectionNotify(event) => {
                    if let Some(text) = clipboard.receive(&connection, &atoms, window, event)? {
                        raw.events.push(InputEvent::Paste(text));
                        redraw = true;
                    }
                }
                Event::SelectionClear(_) => clipboard.owned = false,
                Event::PropertyNotify(event) => {
                    if let Some(text) = clipboard.property(&connection, &atoms, window, event)? {
                        raw.events.push(InputEvent::Paste(text));
                        redraw = true;
                    }
                }
                _ => {}
            }
        }
        if let Some(deadline) = repaint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            next_frame = Some(next_frame.map_or(deadline, |old: Instant| old.min(deadline)));
        }
        redraw |= next_frame.is_some_and(|deadline| deadline <= Instant::now());
        if redraw {
            raw.screen_rect = Some(Rect::from_min_size(
                pos2(0.0, 0.0),
                vec2(size[0] as f32 / scale, size[1] as f32 / scale),
            ));
            raw.time = Some(start.elapsed().as_secs_f64());
            raw.focused = focused;
            let viewport = raw.viewports.entry(ViewportId::ROOT).or_default();
            viewport.native_pixels_per_point = Some(native_scale);
            viewport.focused = Some(focused);
            viewport.inner_rect = raw.screen_rect;
            if close_requested {
                viewport.events.push(ViewportEvent::Close);
            }
            let output = context.run(raw.take(), |ctx| update(ctx));
            let mut cancel_close = false;
            let mut force_close = false;
            if let Some(viewport) = output.viewport_output.get(&ViewportId::ROOT) {
                next_frame = Instant::now().checked_add(viewport.repaint_delay);
                for command in &viewport.commands {
                    match command {
                        ViewportCommand::CancelClose => cancel_close = true,
                        ViewportCommand::Close => force_close = true,
                        ViewportCommand::Title(title) => {
                            connection.change_property8(
                                PropMode::REPLACE,
                                window,
                                atoms._NET_WM_NAME,
                                atoms.UTF8_STRING,
                                title.as_bytes(),
                            )?;
                            connection.change_property8(
                                PropMode::REPLACE,
                                window,
                                AtomEnum::WM_NAME,
                                AtomEnum::STRING,
                                title.as_bytes(),
                            )?;
                        }
                        ViewportCommand::InnerSize(size) => {
                            connection.configure_window(
                                window,
                                &ConfigureWindowAux::new()
                                    .width((size.x * scale).max(1.0) as u32)
                                    .height((size.y * scale).max(1.0) as u32),
                            )?;
                        }
                        ViewportCommand::RequestPaste => {
                            clipboard.paste(&connection, &atoms, window, time)?
                        }
                        _ => {}
                    }
                }
            }
            if force_close || (close_requested && !cancel_close) {
                break;
            }
            close_requested = false;
            for command in &output.platform_output.commands {
                match command {
                    OutputCommand::CopyText(text) => {
                        clipboard.copy(&connection, &atoms, window, text.clone(), time)?
                    }
                    OutputCommand::OpenUrl(url) => {
                        let _ = std::process::Command::new("xdg-open").arg(&url.url).spawn();
                    }
                    _ => {}
                }
            }
            cursors.set(&connection, window, output.platform_output.cursor_icon)?;
            let primitives = context.tessellate(output.shapes, output.pixels_per_point);
            let pixels = painter.paint(
                [size[0] as usize, size[1] as usize],
                output.pixels_per_point,
                &primitives,
                &output.textures_delta,
                context.style().visuals.panel_fill,
            );
            // RGB words are converted by Image when the server's byte order differs.
            let data = unsafe {
                std::slice::from_raw_parts(pixels.as_ptr().cast::<u8>(), pixels.len() * 4)
            };
            let order = if cfg!(target_endian = "little") {
                ImageOrder::LsbFirst
            } else {
                ImageOrder::MsbFirst
            };
            Image::new(
                size[0],
                size[1],
                ScanlinePad::Pad32,
                screen.root_depth,
                BitsPerPixel::B32,
                order,
                Cow::Borrowed(data),
            )?
            .put(&connection, window, graphics, 0, 0)?;
            connection.flush()?;
            redraw = false;
            continue;
        }
        connection.flush()?;
        let timeout = next_frame.map_or(-1, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1)
                .min(i32::MAX as u128) as i32
        });
        let mut descriptors = [
            libc::pollfd {
                fd: connection.stream().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake_reader.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        unsafe {
            libc::poll(
                descriptors.as_mut_ptr(),
                descriptors.len() as libc::nfds_t,
                timeout,
            );
        }
        let mut bytes = [0; 128];
        while wake_reader.read(&mut bytes).is_ok_and(|count| count > 0) {}
    }
    connection.destroy_window(window)?;
    connection.flush()?;
    Ok(())
}

fn display_scale(connection: &RustConnection, root: Window) -> f32 {
    connection
        .get_property(
            false,
            root,
            AtomEnum::RESOURCE_MANAGER,
            AtomEnum::STRING,
            0,
            16384,
        )
        .ok()
        .and_then(|cookie| cookie.reply().ok())
        .and_then(|property| String::from_utf8(property.value).ok())
        .and_then(|resources| {
            resources.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                (name.trim() == "Xft.dpi")
                    .then(|| value.trim().parse::<f32>().ok())
                    .flatten()
            })
        })
        .map_or(1.0, |dpi| (dpi / 96.0).clamp(0.75, 3.0))
}

fn minimum_size(
    connection: &RustConnection,
    atoms: &Atoms,
    window: Window,
    minimum: [u32; 2],
) -> Result<()> {
    let mut hints = [0_u32; 18];
    hints[0] = 1 << 4;
    hints[5] = minimum[0];
    hints[6] = minimum[1];
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.WM_NORMAL_HINTS,
        atoms.WM_SIZE_HINTS,
        &hints,
    )?;
    Ok(())
}

fn modifiers(state: KeyButMask) -> Modifiers {
    Modifiers {
        alt: state.contains(KeyButMask::MOD1),
        ctrl: state.contains(KeyButMask::CONTROL),
        shift: state.contains(KeyButMask::SHIFT),
        mac_cmd: false,
        command: state.contains(KeyButMask::CONTROL),
    }
}

struct Keyboard {
    first: u8,
    width: usize,
    symbols: Vec<u32>,
}

impl Keyboard {
    fn load(connection: &RustConnection) -> Result<Self> {
        let setup = connection.setup();
        let map = connection
            .get_keyboard_mapping(setup.min_keycode, setup.max_keycode - setup.min_keycode + 1)?
            .reply()?;
        Ok(Self {
            first: setup.min_keycode,
            width: map.keysyms_per_keycode as usize,
            symbols: map.keysyms,
        })
    }

    fn symbol(&self, code: u8, state: KeyButMask) -> u32 {
        let start = usize::from(code.saturating_sub(self.first)) * self.width;
        let symbols = self.symbols.get(start..start + self.width).unwrap_or(&[]);
        let group = if state.contains(KeyButMask::MOD5)
            && symbols.get(2).is_some_and(|symbol| *symbol != 0)
        {
            2
        } else {
            0
        };
        let base = symbols.get(group).copied().unwrap_or(0);
        let shifted = symbols
            .get(group + 1)
            .copied()
            .filter(|symbol| *symbol != 0)
            .unwrap_or(base);
        let alphabetic = symbol_character(base).is_some_and(|character| character.is_alphabetic());
        let shifted_state = state.contains(KeyButMask::SHIFT)
            ^ (alphabetic && state.contains(KeyButMask::LOCK))
            ^ ((0xff80..=0xffbd).contains(&base) && state.contains(KeyButMask::MOD2));
        if shifted_state {
            if shifted == base && alphabetic {
                symbol_character(base)
                    .and_then(|character| character.to_uppercase().next())
                    .map_or(shifted, |character| character as u32)
            } else {
                shifted
            }
        } else {
            base
        }
    }
}

fn symbol_character(symbol: u32) -> Option<char> {
    match symbol {
        0x20..=0xff => char::from_u32(symbol),
        0x0100_0000..=0x0110_ffff => char::from_u32(symbol - 0x0100_0000),
        0xffb0..=0xffb9 => char::from_u32(b'0' as u32 + symbol - 0xffb0),
        0xffaa => Some('*'),
        0xffab => Some('+'),
        0xffad => Some('-'),
        0xffae => Some('.'),
        0xffaf => Some('/'),
        _ => None,
    }
}

fn key_from_symbol(symbol: u32) -> Option<Key> {
    Some(match symbol {
        0xff08 => Key::Backspace,
        0xff09 | 0xfe20 => Key::Tab,
        0xff0d | 0xff8d => Key::Enter,
        0xff1b => Key::Escape,
        0xff50 | 0xff95 => Key::Home,
        0xff51 | 0xff96 => Key::ArrowLeft,
        0xff52 | 0xff97 => Key::ArrowUp,
        0xff53 | 0xff98 => Key::ArrowRight,
        0xff54 | 0xff99 => Key::ArrowDown,
        0xff55 | 0xff9a => Key::PageUp,
        0xff56 | 0xff9b => Key::PageDown,
        0xff57 | 0xff9c => Key::End,
        0xff63 | 0xff9e => Key::Insert,
        0xffff | 0xff9f => Key::Delete,
        0xffbe..=0xffd5 => return Key::from_name(&format!("F{}", symbol - 0xffbe + 1)),
        _ => {
            return symbol_character(symbol)
                .and_then(|character| Key::from_name(&character.to_ascii_uppercase().to_string()));
        }
    })
}

struct Cursors {
    font: Font,
    current: Option<CursorIcon>,
    cursors: HashMap<u16, Cursor>,
}

impl Cursors {
    fn new(connection: &RustConnection) -> Result<Self> {
        let font = connection.generate_id()?;
        connection.open_font(font, b"cursor")?;
        Ok(Self {
            font,
            current: None,
            cursors: HashMap::new(),
        })
    }

    fn set(&mut self, connection: &RustConnection, window: Window, icon: CursorIcon) -> Result<()> {
        if self.current == Some(icon) {
            return Ok(());
        }
        let glyph = match icon {
            CursorIcon::None => u16::MAX,
            CursorIcon::Text | CursorIcon::VerticalText => 152,
            CursorIcon::PointingHand => 60,
            CursorIcon::Wait | CursorIcon::Progress => 150,
            CursorIcon::Crosshair | CursorIcon::Cell => 34,
            CursorIcon::ResizeHorizontal
            | CursorIcon::ResizeEast
            | CursorIcon::ResizeWest
            | CursorIcon::ResizeColumn => 108,
            CursorIcon::ResizeVertical
            | CursorIcon::ResizeNorth
            | CursorIcon::ResizeSouth
            | CursorIcon::ResizeRow => 116,
            CursorIcon::ResizeNwSe | CursorIcon::ResizeNorthWest | CursorIcon::ResizeSouthEast => {
                134
            }
            CursorIcon::ResizeNeSw | CursorIcon::ResizeNorthEast | CursorIcon::ResizeSouthWest => {
                136
            }
            CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::Grab | CursorIcon::Grabbing => {
                52
            }
            CursorIcon::NotAllowed | CursorIcon::NoDrop => 24,
            _ => 68,
        };
        let cursor = if let Some(cursor) = self.cursors.get(&glyph) {
            *cursor
        } else {
            let cursor = connection.generate_id()?;
            if glyph == u16::MAX {
                let bitmap = connection.generate_id()?;
                let graphics = connection.generate_id()?;
                connection.create_pixmap(1, bitmap, window, 1, 1)?;
                connection.create_gc(graphics, bitmap, &CreateGCAux::new().foreground(0))?;
                connection.poly_fill_rectangle(
                    bitmap,
                    graphics,
                    &[Rectangle {
                        x: 0,
                        y: 0,
                        width: 1,
                        height: 1,
                    }],
                )?;
                connection.create_cursor(cursor, bitmap, bitmap, 0, 0, 0, 0, 0, 0, 0, 0)?;
                connection.free_gc(graphics)?;
                connection.free_pixmap(bitmap)?;
            } else {
                connection.create_glyph_cursor(
                    cursor,
                    self.font,
                    self.font,
                    glyph,
                    glyph + 1,
                    0,
                    0,
                    0,
                    65535,
                    65535,
                    65535,
                )?;
            }
            self.cursors.insert(glyph, cursor);
            cursor
        };
        connection
            .change_window_attributes(window, &ChangeWindowAttributesAux::new().cursor(cursor))?;
        self.current = Some(icon);
        Ok(())
    }
}

#[derive(Default)]
struct Clipboard {
    text: String,
    owned: bool,
    owner: Window,
    incoming: Option<Vec<u8>>,
    incoming_type: u32,
    fallback: bool,
    outgoing: HashMap<(Window, Atom), (Vec<u8>, usize, Atom)>,
}

impl Clipboard {
    fn copy(
        &mut self,
        connection: &RustConnection,
        atoms: &Atoms,
        window: Window,
        text: String,
        time: Timestamp,
    ) -> Result<()> {
        self.text = text;
        self.owned = true;
        self.owner = window;
        connection.set_selection_owner(window, atoms.CLIPBOARD, time)?;
        Ok(())
    }

    fn paste(
        &mut self,
        connection: &RustConnection,
        atoms: &Atoms,
        window: Window,
        time: Timestamp,
    ) -> Result<()> {
        self.fallback = false;
        self.incoming = None;
        connection.convert_selection(
            window,
            atoms.CLIPBOARD,
            atoms.UTF8_STRING,
            atoms.RUSTIDE_CLIPBOARD,
            time,
        )?;
        Ok(())
    }

    fn answer(
        &mut self,
        connection: &RustConnection,
        atoms: &Atoms,
        request: SelectionRequestEvent,
    ) -> Result<()> {
        let property = if request.property == x11rb::NONE {
            request.target
        } else {
            request.property
        };
        let supported = request.target == atoms.TARGETS
            || request.target == atoms.UTF8_STRING
            || request.target == atoms.TEXT
            || request.target == u32::from(AtomEnum::STRING);
        let mut reply_property = x11rb::NONE;
        if self.owned && supported {
            if request.target == atoms.TARGETS {
                connection.change_property32(
                    PropMode::REPLACE,
                    request.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[
                        atoms.TARGETS,
                        atoms.UTF8_STRING,
                        atoms.TEXT,
                        u32::from(AtomEnum::STRING),
                    ],
                )?;
            } else {
                let (data, encoding) = if request.target == u32::from(AtomEnum::STRING) {
                    (
                        self.text
                            .chars()
                            .map(|character| {
                                if u32::from(character) <= 255 {
                                    character as u8
                                } else {
                                    b'?'
                                }
                            })
                            .collect::<Vec<_>>(),
                        u32::from(AtomEnum::STRING),
                    )
                } else {
                    (self.text.as_bytes().to_vec(), atoms.UTF8_STRING)
                };
                if data.len() <= 128 * 1024 {
                    connection.change_property8(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        encoding,
                        &data,
                    )?;
                } else {
                    if request.requestor != self.owner {
                        connection.change_window_attributes(
                            request.requestor,
                            &ChangeWindowAttributesAux::new()
                                .event_mask(EventMask::PROPERTY_CHANGE),
                        )?;
                    }
                    connection.change_property32(
                        PropMode::REPLACE,
                        request.requestor,
                        property,
                        atoms.INCR,
                        &[data.len() as u32],
                    )?;
                    self.outgoing
                        .insert((request.requestor, property), (data, 0, encoding));
                }
            }
            reply_property = property;
        }
        connection.send_event(
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
                property: reply_property,
            },
        )?;
        Ok(())
    }

    fn receive(
        &mut self,
        connection: &RustConnection,
        atoms: &Atoms,
        window: Window,
        event: SelectionNotifyEvent,
    ) -> Result<Option<String>> {
        if event.selection != atoms.CLIPBOARD {
            return Ok(None);
        }
        if event.property == x11rb::NONE {
            if !self.fallback {
                self.fallback = true;
                connection.convert_selection(
                    window,
                    atoms.CLIPBOARD,
                    AtomEnum::STRING.into(),
                    atoms.RUSTIDE_CLIPBOARD,
                    event.time,
                )?;
            }
            return Ok(None);
        }
        let property = connection
            .get_property(
                true,
                window,
                event.property,
                AtomEnum::ANY,
                0,
                16 * 1024 * 1024,
            )?
            .reply()?;
        if property.type_ == atoms.INCR {
            self.incoming = Some(Vec::new());
            return Ok(None);
        }
        Ok(Some(decode_clipboard(property.type_, &property.value)))
    }

    fn property(
        &mut self,
        connection: &RustConnection,
        atoms: &Atoms,
        window: Window,
        event: PropertyNotifyEvent,
    ) -> Result<Option<String>> {
        if event.state == Property::DELETE {
            let transfer = (event.window, event.atom);
            if let Some((data, offset, encoding)) = self.outgoing.get_mut(&transfer) {
                let end = (*offset + 128 * 1024).min(data.len());
                connection.change_property8(
                    PropMode::REPLACE,
                    event.window,
                    event.atom,
                    *encoding,
                    &data[*offset..end],
                )?;
                let finished = end == *offset;
                *offset = end;
                if finished {
                    self.outgoing.remove(&transfer);
                }
            }
        } else if event.window == window
            && event.atom == atoms.RUSTIDE_CLIPBOARD
            && self.incoming.is_some()
        {
            let property = connection
                .get_property(true, window, event.atom, AtomEnum::ANY, 0, 16 * 1024 * 1024)?
                .reply()?;
            if property.type_ == atoms.INCR {
                return Ok(None);
            }
            self.incoming_type = property.type_;
            if property.value.is_empty() {
                return Ok(self
                    .incoming
                    .take()
                    .map(|bytes| decode_clipboard(self.incoming_type, &bytes)));
            }
            if let Some(bytes) = self.incoming.as_mut() {
                bytes.extend_from_slice(&property.value);
            }
        }
        Ok(None)
    }
}

fn decode_clipboard(encoding: Atom, bytes: &[u8]) -> String {
    if encoding == u32::from(AtomEnum::STRING) {
        bytes.iter().map(|byte| char::from(*byte)).collect()
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyboard_handles_shift_caps_punctuation_and_unicode() {
        let keyboard = Keyboard {
            first: 8,
            width: 2,
            symbols: vec![b'a' as u32, b'A' as u32, b'1' as u32, b'!' as u32],
        };
        assert_eq!(keyboard.symbol(8, KeyButMask::SHIFT), b'A' as u32);
        assert_eq!(keyboard.symbol(8, KeyButMask::LOCK), b'A' as u32);
        assert_eq!(
            keyboard.symbol(8, KeyButMask::SHIFT | KeyButMask::LOCK),
            b'a' as u32
        );
        assert_eq!(
            keyboard.symbol(9, KeyButMask::SHIFT | KeyButMask::LOCK),
            b'!' as u32
        );
        assert_eq!(symbol_character(0x0100_09ac), Some('ব'));
        assert_eq!(key_from_symbol(0xff53), Some(Key::ArrowRight));
    }
}
