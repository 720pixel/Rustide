use crate::gui::paint::Painter;
use anyhow::{Context as _, Result};
use egui::{
    CursorIcon, Event, Key, Modifiers, MouseWheelUnit, OutputCommand, PointerButton, RawInput,
    Rect, ViewportCommand, ViewportEvent, ViewportId, pos2, vec2,
};
use std::ptr::{null, null_mut};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const REPAINT: u32 = WM_APP + 1;
const UNICODE_TEXT: u32 = 13;
const MOUSE_LEAVE: u32 = 0x02a3;

pub(crate) fn run(title: &str, mut update: impl FnMut(&egui::Context)) -> Result<()> {
    unsafe {
        SetProcessDPIAware();
    }
    let mut state = Box::new(WindowState::default());
    let instance = unsafe { GetModuleHandleW(null()) };
    let icon_resource = std::ptr::without_provenance::<u16>(1);
    let icon = unsafe { LoadIconW(instance, icon_resource) };
    let class_name = wide("RustideDesktop");
    let class = WNDCLASSW {
        style: CS_HREDRAW | CS_VREDRAW,
        lpfnWndProc: Some(window_procedure),
        hInstance: instance,
        hIcon: icon,
        hCursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        lpszClassName: class_name.as_ptr(),
        ..Default::default()
    };
    if unsafe { RegisterClassW(&class) } == 0 {
        return Err(std::io::Error::last_os_error()).context("Cannot register the desktop window");
    }
    let device = unsafe { GetDC(null_mut()) };
    if !device.is_null() {
        state.native_scale =
            (unsafe { GetDeviceCaps(device, LOGPIXELSX as i32) } as f32 / 96.0).clamp(0.75, 3.0);
        unsafe {
            ReleaseDC(null_mut(), device);
        }
    }
    let geometry = window_geometry(null_mut(), state.native_scale);
    state.native_scale = geometry.scale;
    state.scale = geometry.scale;
    let mut dimensions = RECT {
        left: 0,
        top: 0,
        right: geometry.size[0] as i32,
        bottom: geometry.size[1] as i32,
    };
    unsafe {
        AdjustWindowRectEx(&mut dimensions, WS_OVERLAPPEDWINDOW, 0, 0);
    }
    let work_area = window_work_area(null_mut());
    let width = dimensions.right - dimensions.left;
    let height = dimensions.bottom - dimensions.top;
    let left = work_area.left + (work_area.right - work_area.left - width).max(0) / 2;
    let top = work_area.top + (work_area.bottom - work_area.top - height).max(0) / 2;
    let title = wide(title);
    let window = unsafe {
        CreateWindowExW(
            0,
            class_name.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            left,
            top,
            width,
            height,
            null_mut(),
            null_mut(),
            instance,
            (&mut *state as *mut WindowState).cast(),
        )
    };
    if window.is_null() {
        return Err(std::io::Error::last_os_error()).context("Cannot open the desktop window");
    }
    unsafe {
        let small_icon = LoadImageW(
            instance,
            icon_resource,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            LR_SHARED,
        );
        SendMessageW(window, WM_SETICON, ICON_BIG as usize, icon as isize);
        SendMessageW(window, WM_SETICON, ICON_SMALL as usize, small_icon as isize);
        ShowWindow(window, SW_SHOW);
    }
    let context = egui::Context::default();
    context.data_mut(|data| data.insert_temp(egui::Id::new("native-window"), window as usize));
    let repaint = Arc::new(Mutex::new(None::<Instant>));
    let callback_repaint = repaint.clone();
    let window_address = window as usize;
    context.set_request_repaint_callback(move |request| {
        if let Some(deadline) = Instant::now().checked_add(request.delay) {
            let mut pending = callback_repaint
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            *pending = Some(pending.map_or(deadline, |old| old.min(deadline)));
            unsafe {
                PostMessageW(window_address as HWND, REPAINT, 0, 0);
            }
        }
    });
    let start = Instant::now();
    let mut painter = Painter::default();
    let mut raw = RawInput::default();
    let mut next_frame = None;
    let mut done = false;
    while !done {
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message, null_mut(), 0, 0, PM_REMOVE) } != 0 {
            if message.message == WM_QUIT {
                done = true;
                break;
            }
            unsafe {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
        if done {
            break;
        }
        if let Some(deadline) = repaint
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            next_frame = Some(next_frame.map_or(deadline, |old: Instant| old.min(deadline)));
        }
        state.redraw |= next_frame.is_some_and(|deadline| deadline <= Instant::now());
        if state.redraw {
            state.redraw = false;
            let mut dimensions = RECT::default();
            unsafe {
                GetClientRect(window, &mut dimensions);
            }
            let size = [
                (dimensions.right - dimensions.left).max(1) as usize,
                (dimensions.bottom - dimensions.top).max(1) as usize,
            ];
            state.scale = state.native_scale * context.zoom_factor();
            raw.screen_rect = Some(Rect::from_min_size(
                pos2(0.0, 0.0),
                vec2(size[0] as f32 / state.scale, size[1] as f32 / state.scale),
            ));
            raw.time = Some(start.elapsed().as_secs_f64());
            raw.focused = state.focused;
            raw.modifiers = modifiers();
            raw.events.append(&mut state.events);
            let viewport = raw.viewports.entry(ViewportId::ROOT).or_default();
            viewport.native_pixels_per_point = Some(state.native_scale);
            viewport.focused = Some(state.focused);
            viewport.minimized = Some(state.minimized);
            viewport.inner_rect = raw.screen_rect;
            if state.close_requested {
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
                        ViewportCommand::Title(title) => unsafe {
                            SetWindowTextW(window, wide(title).as_ptr());
                        },
                        ViewportCommand::InnerSize(size) => {
                            let mut rect = RECT {
                                left: 0,
                                top: 0,
                                right: (size.x * state.scale).max(1.0) as i32,
                                bottom: (size.y * state.scale).max(1.0) as i32,
                            };
                            unsafe {
                                AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0);
                                SetWindowPos(
                                    window,
                                    null_mut(),
                                    0,
                                    0,
                                    rect.right - rect.left,
                                    rect.bottom - rect.top,
                                    SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
                                );
                            }
                        }
                        ViewportCommand::RequestPaste => {
                            if let Some(text) = read_clipboard(window) {
                                state.events.push(Event::Paste(text));
                            }
                        }
                        _ => {}
                    }
                }
            }
            if force_close || (state.close_requested && !cancel_close) {
                break;
            }
            state.close_requested = false;
            for command in &output.platform_output.commands {
                if let OutputCommand::CopyText(text) = command {
                    write_clipboard(window, text);
                }
            }
            state.cursor = cursor(output.platform_output.cursor_icon);
            unsafe {
                SetCursor(state.cursor);
            }
            let primitives = context.tessellate(output.shapes, output.pixels_per_point);
            let pixels = painter.paint(
                size,
                output.pixels_per_point,
                &primitives,
                &output.textures_delta,
                context.style().visuals.panel_fill,
            );
            if !state.minimized {
                present(window, size, pixels);
            }
            state.redraw |= !state.events.is_empty();
            continue;
        }
        let timeout = next_frame.map_or(u32::MAX, |deadline| {
            deadline
                .saturating_duration_since(Instant::now())
                .as_millis()
                .saturating_add(1)
                .min((u32::MAX - 1) as u128) as u32
        });
        unsafe {
            MsgWaitForMultipleObjectsEx(0, null(), timeout, QS_ALLINPUT, MWMO_INPUTAVAILABLE);
        }
    }
    // Drop the callback before destroying the native window.
    context.set_request_repaint_callback(|_| {});
    unsafe {
        DestroyWindow(window);
        UnregisterClassW(class_name.as_ptr(), instance);
    }
    Ok(())
}

struct WindowState {
    events: Vec<Event>,
    native_scale: f32,
    scale: f32,
    focused: bool,
    minimized: bool,
    close_requested: bool,
    redraw: bool,
    tracking_mouse: bool,
    buttons: u8,
    high_surrogate: Option<u16>,
    cursor: HCURSOR,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            native_scale: 1.0,
            scale: 1.0,
            focused: true,
            minimized: false,
            close_requested: false,
            redraw: true,
            tracking_mouse: false,
            buttons: 0,
            high_surrogate: None,
            cursor: unsafe { LoadCursorW(null_mut(), IDC_ARROW) },
        }
    }
}

unsafe extern "system" fn window_procedure(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
        unsafe {
            SetWindowLongPtrW(window, GWLP_USERDATA, create.lpCreateParams as isize);
        }
    }
    let pointer = unsafe { GetWindowLongPtrW(window, GWLP_USERDATA) } as *mut WindowState;
    if pointer.is_null() {
        return unsafe { DefWindowProcW(window, message, wparam, lparam) };
    }
    let state = unsafe { &mut *pointer };
    match message {
        WM_CLOSE => {
            state.close_requested = true;
            state.redraw = true;
            return 0;
        }
        WM_DESTROY => {
            unsafe {
                PostQuitMessage(0);
            }
            return 0;
        }
        WM_ERASEBKGND => return 1,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            unsafe {
                BeginPaint(window, &mut paint);
                EndPaint(window, &paint);
            }
            state.redraw = true;
            return 0;
        }
        WM_SIZE => {
            state.minimized = wparam == SIZE_MINIMIZED as usize;
            state.redraw = true;
            return 0;
        }
        WM_GETMINMAXINFO => {
            let minimum = unsafe { &mut *(lparam as *mut MINMAXINFO) };
            let geometry = window_geometry(window, state.native_scale);
            let mut rect = RECT {
                left: 0,
                top: 0,
                right: geometry.minimum[0] as i32,
                bottom: geometry.minimum[1] as i32,
            };
            unsafe {
                AdjustWindowRectEx(&mut rect, WS_OVERLAPPEDWINDOW, 0, 0);
            }
            minimum.ptMinTrackSize = POINT {
                x: rect.right - rect.left,
                y: rect.bottom - rect.top,
            };
            return 0;
        }
        WM_DPICHANGED => {
            state.native_scale = ((wparam & 0xffff) as f32 / 96.0).clamp(0.75, 3.0);
            state.scale = state.native_scale;
            state.redraw = true;
            let rect = unsafe { &*(lparam as *const RECT) };
            unsafe {
                SetWindowPos(
                    window,
                    null_mut(),
                    rect.left,
                    rect.top,
                    rect.right - rect.left,
                    rect.bottom - rect.top,
                    SWP_NOZORDER | SWP_NOACTIVATE,
                );
            }
            return 0;
        }
        WM_SETFOCUS | WM_KILLFOCUS => {
            state.focused = message == WM_SETFOCUS;
            state.events.push(Event::WindowFocused(state.focused));
            state.redraw = true;
            return 0;
        }
        WM_SETCURSOR if (lparam as u32 & 0xffff) == HTCLIENT => {
            unsafe {
                SetCursor(state.cursor);
            }
            return 1;
        }
        WM_MOUSEMOVE => {
            state
                .events
                .push(Event::PointerMoved(pointer_position(lparam, state.scale)));
            if !state.tracking_mouse {
                let mut track = TRACKMOUSEEVENT {
                    cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                    dwFlags: TME_LEAVE,
                    hwndTrack: window,
                    dwHoverTime: 0,
                };
                unsafe {
                    TrackMouseEvent(&mut track);
                }
                state.tracking_mouse = true;
            }
            state.redraw = true;
            return 0;
        }
        MOUSE_LEAVE => {
            state.events.push(Event::PointerGone);
            state.tracking_mouse = false;
            state.redraw = true;
            return 0;
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP | WM_RBUTTONDOWN | WM_RBUTTONUP | WM_MBUTTONDOWN
        | WM_MBUTTONUP | WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let pressed = matches!(
                message,
                WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN
            );
            let (button, mask) = match message {
                WM_LBUTTONDOWN | WM_LBUTTONUP => (PointerButton::Primary, 1),
                WM_RBUTTONDOWN | WM_RBUTTONUP => (PointerButton::Secondary, 2),
                WM_MBUTTONDOWN | WM_MBUTTONUP => (PointerButton::Middle, 4),
                _ if wparam >> 16 == XBUTTON1 as usize => (PointerButton::Extra1, 8),
                _ => (PointerButton::Extra2, 16),
            };
            if pressed {
                state.buttons |= mask;
            } else {
                state.buttons &= !mask;
            }
            state.events.push(Event::PointerButton {
                pos: pointer_position(lparam, state.scale),
                button,
                pressed,
                modifiers: modifiers(),
            });
            state.redraw = true;
            let release = !pressed && state.buttons == 0;
            if pressed {
                unsafe {
                    SetCapture(window);
                    SetFocus(window);
                }
            } else if release {
                unsafe {
                    ReleaseCapture();
                }
            }
            return 0;
        }
        WM_MOUSEWHEEL | WM_MOUSEHWHEEL => {
            let amount = ((wparam >> 16) as u16 as i16) as f32 / WHEEL_DELTA as f32 * 3.0;
            let delta = if message == WM_MOUSEWHEEL {
                vec2(0.0, amount)
            } else {
                vec2(-amount, 0.0)
            };
            state.events.push(Event::MouseWheel {
                unit: MouseWheelUnit::Line,
                delta,
                modifiers: modifiers(),
            });
            state.redraw = true;
            return 0;
        }
        WM_KEYDOWN | WM_KEYUP | WM_SYSKEYDOWN | WM_SYSKEYUP => {
            let pressed = matches!(message, WM_KEYDOWN | WM_SYSKEYDOWN);
            let mods = modifiers();
            if let Some(key) = key_from_virtual(wparam as u16) {
                if pressed
                    && ((mods.ctrl && !mods.alt && key == Key::V)
                        || (mods.shift && key == Key::Insert))
                {
                    if let Some(text) = read_clipboard(window) {
                        state.events.push(Event::Paste(text));
                    }
                } else if pressed && mods.ctrl && !mods.alt && matches!(key, Key::C | Key::X) {
                    state.events.push(if key == Key::C {
                        Event::Copy
                    } else {
                        Event::Cut
                    });
                } else {
                    state.events.push(Event::Key {
                        key,
                        physical_key: None,
                        pressed,
                        repeat: lparam & (1 << 30) != 0,
                        modifiers: mods,
                    });
                }
            }
            state.redraw = true;
            if message == WM_SYSKEYDOWN || message == WM_SYSKEYUP {
                return unsafe { DefWindowProcW(window, message, wparam, lparam) };
            }
            return 0;
        }
        WM_CHAR | WM_SYSCHAR => {
            let mods = modifiers();
            let alt_graph = key_down(VK_RMENU);
            if (!mods.ctrl && !mods.alt) || alt_graph {
                let unit = wparam as u16;
                if (0xd800..=0xdbff).contains(&unit) {
                    state.high_surrogate = Some(unit);
                } else {
                    let code = if (0xdc00..=0xdfff).contains(&unit) {
                        state.high_surrogate.take().map(|high| {
                            0x10000 + (((high as u32 - 0xd800) << 10) | (unit as u32 - 0xdc00))
                        })
                    } else {
                        state.high_surrogate = None;
                        Some(unit as u32)
                    };
                    if let Some(character) = code
                        .and_then(char::from_u32)
                        .filter(|character| !character.is_control())
                    {
                        state.events.push(Event::Text(character.to_string()));
                    }
                }
            }
            state.redraw = true;
            return 0;
        }
        WM_UNICHAR => {
            if wparam == 0xffff {
                return 1;
            }
            if let Some(character) =
                char::from_u32(wparam as u32).filter(|character| !character.is_control())
            {
                state.events.push(Event::Text(character.to_string()));
            }
            state.redraw = true;
            return 0;
        }
        REPAINT => return 0,
        _ => {}
    }
    unsafe { DefWindowProcW(window, message, wparam, lparam) }
}

fn window_work_area(window: HWND) -> RECT {
    let mut monitor = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    let mut work_area = RECT::default();
    unsafe {
        let handle = MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST);
        if !handle.is_null() && GetMonitorInfoW(handle, &mut monitor) != 0 {
            work_area = monitor.rcWork;
        } else if SystemParametersInfoW(SPI_GETWORKAREA, 0, (&mut work_area as *mut RECT).cast(), 0)
            == 0
        {
            work_area.right = GetSystemMetrics(SM_CXSCREEN);
            work_area.bottom = GetSystemMetrics(SM_CYSCREEN);
        }
    }
    work_area
}

fn window_geometry(window: HWND, scale: f32) -> super::WindowGeometry {
    let work_area = window_work_area(window);
    let mut chrome = RECT::default();
    unsafe {
        AdjustWindowRectEx(&mut chrome, WS_OVERLAPPEDWINDOW, 0, 0);
    }
    let available = [
        (work_area.right - work_area.left).max(1) as u32,
        (work_area.bottom - work_area.top).max(1) as u32,
    ];
    let chrome = [
        (chrome.right - chrome.left).max(0) as u32 + 24,
        (chrome.bottom - chrome.top).max(0) as u32 + 24,
    ];
    super::fit_window(available, scale, chrome)
}

fn present(window: HWND, size: [usize; 2], pixels: &[u32]) {
    let bitmap = BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: size[0] as i32,
            biHeight: -(size[1] as i32),
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        let device = GetDC(window);
        if !device.is_null() {
            StretchDIBits(
                device,
                0,
                0,
                size[0] as i32,
                size[1] as i32,
                0,
                0,
                size[0] as i32,
                size[1] as i32,
                pixels.as_ptr().cast(),
                &bitmap,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
            ReleaseDC(window, device);
        }
    }
}

fn pointer_position(lparam: LPARAM, scale: f32) -> egui::Pos2 {
    pos2(
        (lparam as u16 as i16) as f32 / scale,
        ((lparam >> 16) as u16 as i16) as f32 / scale,
    )
}

fn key_down(key: VIRTUAL_KEY) -> bool {
    (unsafe { GetKeyState(key as i32) }) < 0
}

fn modifiers() -> Modifiers {
    let ctrl = key_down(VK_CONTROL);
    Modifiers {
        alt: key_down(VK_MENU),
        ctrl,
        shift: key_down(VK_SHIFT),
        mac_cmd: false,
        command: ctrl,
    }
}

fn key_from_virtual(key: u16) -> Option<Key> {
    Some(match key {
        VK_BACK => Key::Backspace,
        VK_TAB => Key::Tab,
        VK_RETURN => Key::Enter,
        VK_ESCAPE => Key::Escape,
        VK_HOME => Key::Home,
        VK_END => Key::End,
        VK_LEFT => Key::ArrowLeft,
        VK_RIGHT => Key::ArrowRight,
        VK_UP => Key::ArrowUp,
        VK_DOWN => Key::ArrowDown,
        VK_PRIOR => Key::PageUp,
        VK_NEXT => Key::PageDown,
        VK_INSERT => Key::Insert,
        VK_DELETE => Key::Delete,
        VK_SPACE => Key::Space,
        VK_OEM_MINUS | VK_SUBTRACT => Key::Minus,
        VK_OEM_PLUS | VK_ADD => Key::Plus,
        VK_OEM_PERIOD | VK_DECIMAL => Key::Period,
        VK_OEM_COMMA => Key::Comma,
        VK_OEM_1 => Key::Semicolon,
        VK_OEM_2 | VK_DIVIDE => Key::Slash,
        VK_OEM_3 => Key::Backtick,
        VK_OEM_4 => Key::OpenBracket,
        VK_OEM_5 => Key::Backslash,
        VK_OEM_6 => Key::CloseBracket,
        VK_OEM_7 => Key::Quote,
        0x30..=0x39 | 0x41..=0x5a => {
            return char::from_u32(key as u32)
                .and_then(|character| Key::from_name(&character.to_string()));
        }
        0x60..=0x69 => return Key::from_name(&(key - 0x60).to_string()),
        VK_F1..=VK_F24 => return Key::from_name(&format!("F{}", key - VK_F1 + 1)),
        _ => return None,
    })
}

fn cursor(icon: CursorIcon) -> HCURSOR {
    let identifier = match icon {
        CursorIcon::None => return null_mut(),
        CursorIcon::Text | CursorIcon::VerticalText => IDC_IBEAM,
        CursorIcon::PointingHand => IDC_HAND,
        CursorIcon::Wait => IDC_WAIT,
        CursorIcon::Progress => IDC_APPSTARTING,
        CursorIcon::Crosshair | CursorIcon::Cell => IDC_CROSS,
        CursorIcon::ResizeHorizontal
        | CursorIcon::ResizeEast
        | CursorIcon::ResizeWest
        | CursorIcon::ResizeColumn => IDC_SIZEWE,
        CursorIcon::ResizeVertical
        | CursorIcon::ResizeNorth
        | CursorIcon::ResizeSouth
        | CursorIcon::ResizeRow => IDC_SIZENS,
        CursorIcon::ResizeNwSe | CursorIcon::ResizeNorthWest | CursorIcon::ResizeSouthEast => {
            IDC_SIZENWSE
        }
        CursorIcon::ResizeNeSw | CursorIcon::ResizeNorthEast | CursorIcon::ResizeSouthWest => {
            IDC_SIZENESW
        }
        CursorIcon::Move | CursorIcon::AllScroll | CursorIcon::Grab | CursorIcon::Grabbing => {
            IDC_SIZEALL
        }
        CursorIcon::NotAllowed | CursorIcon::NoDrop => IDC_NO,
        CursorIcon::Help => IDC_HELP,
        _ => IDC_ARROW,
    };
    unsafe { LoadCursorW(null_mut(), identifier) }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

fn read_clipboard(window: HWND) -> Option<String> {
    unsafe {
        if OpenClipboard(window) == 0 {
            return None;
        }
        let handle = GetClipboardData(UNICODE_TEXT);
        let text = if handle.is_null() {
            None
        } else {
            let length = GlobalSize(handle) / 2;
            let pointer = GlobalLock(handle).cast::<u16>();
            if pointer.is_null() {
                None
            } else {
                let units = std::slice::from_raw_parts(pointer, length);
                let end = units.iter().position(|unit| *unit == 0).unwrap_or(length);
                let value = String::from_utf16_lossy(&units[..end]).replace("\r\n", "\n");
                GlobalUnlock(handle);
                Some(value)
            }
        };
        CloseClipboard();
        text
    }
}

fn write_clipboard(window: HWND, value: &str) {
    let text = wide(&value.replace("\r\n", "\n").replace('\n', "\r\n"));
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, text.len() * 2);
        if handle.is_null() {
            return;
        }
        let pointer = GlobalLock(handle).cast::<u16>();
        if pointer.is_null() {
            GlobalFree(handle);
            return;
        }
        std::ptr::copy_nonoverlapping(text.as_ptr(), pointer, text.len());
        GlobalUnlock(handle);
        if OpenClipboard(window) == 0 {
            GlobalFree(handle);
            return;
        }
        EmptyClipboard();
        if SetClipboardData(UNICODE_TEXT, handle).is_null() {
            GlobalFree(handle);
        }
        CloseClipboard();
    }
}
