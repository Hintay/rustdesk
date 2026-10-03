// Pointer input for a gamescope session (Steam Deck Game Mode) through libei.
//
// gamescope consumes only RELATIVE motion from evdev pointers (it listens to wlr_pointer `motion`,
// never `motion_absolute`), so the absolute uinput mouse the Wayland path installs moves nothing
// there: clicks land wherever the cursor already was. gamescope also runs an EIS server at
// `$XDG_RUNTIME_DIR/gamescope-<N>-ei` that does take absolute motion (it warps the cursor), buttons
// and scroll, so in a gamescope session the pointer is sent there instead.
//
// libei is dlopen'd, like libdrmtap, so the binary carries no hard dependency. Anything missing
// (not gamescope, no EIS socket, no libei, no device offered in time) leaves the uinput mouse in
// place, so every other session keeps exactly the path it had.

use enigo::{MouseButton, MouseControllable};
use hbb_common::{libc, libloading::Library, log};
use std::ffi::CString;
use std::os::raw::{c_char, c_double, c_int, c_uint, c_void};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

// libei 1.x `enum ei_device_capability`.
const EI_DEVICE_CAP_POINTER: c_uint = 1 << 0;
const EI_DEVICE_CAP_POINTER_ABSOLUTE: c_uint = 1 << 1;
const EI_DEVICE_CAP_SCROLL: c_uint = 1 << 4;
const EI_DEVICE_CAP_BUTTON: c_uint = 1 << 5;

// libei 1.x `enum ei_event_type`.
const EI_EVENT_CONNECT: c_int = 1;
const EI_EVENT_DISCONNECT: c_int = 2;
const EI_EVENT_SEAT_ADDED: c_int = 3;
const EI_EVENT_DEVICE_ADDED: c_int = 5;
const EI_EVENT_DEVICE_REMOVED: c_int = 6;
const EI_EVENT_DEVICE_PAUSED: c_int = 7;
const EI_EVENT_DEVICE_RESUMED: c_int = 8;

// linux/input-event-codes.h
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// One wheel notch in libei's discrete scroll units.
const SCROLL_NOTCH: i32 = 120;
/// How long the handshake (connect, seat, device, resume) may take before falling back to uinput.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(2);
const DISPATCH_POLL_MS: c_int = 200;

type Ei = c_void;
type EiEvent = c_void;
type EiSeat = c_void;
type EiDevice = c_void;

struct LibEi {
    _lib: Library,
    new_sender: unsafe extern "C" fn(*mut c_void) -> *mut Ei,
    configure_name: unsafe extern "C" fn(*mut Ei, *const c_char),
    setup_backend_socket: unsafe extern "C" fn(*mut Ei, *const c_char) -> c_int,
    get_fd: unsafe extern "C" fn(*mut Ei) -> c_int,
    dispatch: unsafe extern "C" fn(*mut Ei),
    get_event: unsafe extern "C" fn(*mut Ei) -> *mut EiEvent,
    unref: unsafe extern "C" fn(*mut Ei) -> *mut Ei,
    now: unsafe extern "C" fn(*mut Ei) -> u64,
    event_get_type: unsafe extern "C" fn(*mut EiEvent) -> c_int,
    event_get_seat: unsafe extern "C" fn(*mut EiEvent) -> *mut EiSeat,
    event_get_device: unsafe extern "C" fn(*mut EiEvent) -> *mut EiDevice,
    event_unref: unsafe extern "C" fn(*mut EiEvent) -> *mut EiEvent,
    // NULL-terminated variadic list of `enum ei_device_capability`.
    seat_bind_capabilities: unsafe extern "C" fn(*mut EiSeat, ...),
    device_ref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    device_unref: unsafe extern "C" fn(*mut EiDevice) -> *mut EiDevice,
    device_has_capability: unsafe extern "C" fn(*mut EiDevice, c_uint) -> bool,
    device_start_emulating: unsafe extern "C" fn(*mut EiDevice, u32),
    pointer_motion: unsafe extern "C" fn(*mut EiDevice, c_double, c_double),
    pointer_motion_absolute: unsafe extern "C" fn(*mut EiDevice, c_double, c_double),
    button_button: unsafe extern "C" fn(*mut EiDevice, u32, bool),
    scroll_discrete: unsafe extern "C" fn(*mut EiDevice, i32, i32),
    frame: unsafe extern "C" fn(*mut EiDevice, u64),
}

/// Copy one function pointer out of `lib`, typed by the field it is assigned to.
unsafe fn sym<T: Copy>(lib: &Library, name: &str) -> Option<T> {
    match lib.get::<T>(name.as_bytes()) {
        Ok(s) => Some(*s),
        Err(e) => {
            log::warn!("gamescope: libei has no {name}: {e}");
            None
        }
    }
}

impl LibEi {
    fn load() -> Option<Self> {
        unsafe {
            let lib = match Library::new("libei.so.1") {
                Ok(lib) => lib,
                Err(e) => {
                    log::info!("gamescope: libei not available ({e}); keeping the uinput mouse");
                    return None;
                }
            };
            Some(Self {
                new_sender: sym(&lib, "ei_new_sender")?,
                configure_name: sym(&lib, "ei_configure_name")?,
                setup_backend_socket: sym(&lib, "ei_setup_backend_socket")?,
                get_fd: sym(&lib, "ei_get_fd")?,
                dispatch: sym(&lib, "ei_dispatch")?,
                get_event: sym(&lib, "ei_get_event")?,
                unref: sym(&lib, "ei_unref")?,
                now: sym(&lib, "ei_now")?,
                event_get_type: sym(&lib, "ei_event_get_type")?,
                event_get_seat: sym(&lib, "ei_event_get_seat")?,
                event_get_device: sym(&lib, "ei_event_get_device")?,
                event_unref: sym(&lib, "ei_event_unref")?,
                seat_bind_capabilities: sym(&lib, "ei_seat_bind_capabilities")?,
                device_ref: sym(&lib, "ei_device_ref")?,
                device_unref: sym(&lib, "ei_device_unref")?,
                device_has_capability: sym(&lib, "ei_device_has_capability")?,
                device_start_emulating: sym(&lib, "ei_device_start_emulating")?,
                pointer_motion: sym(&lib, "ei_device_pointer_motion")?,
                pointer_motion_absolute: sym(&lib, "ei_device_pointer_motion_absolute")?,
                button_button: sym(&lib, "ei_device_button_button")?,
                scroll_discrete: sym(&lib, "ei_device_scroll_discrete")?,
                frame: sym(&lib, "ei_device_frame")?,
                _lib: lib,
            })
        }
    }

    fn get() -> Option<&'static LibEi> {
        static LIB: OnceLock<Option<LibEi>> = OnceLock::new();
        LIB.get_or_init(Self::load).as_ref()
    }
}

/// One libei sender context. libei is not thread-safe, so every call goes through the mutex that
/// owns this, from the input thread and the dispatch thread alike.
struct Context {
    lib: &'static LibEi,
    ei: *mut Ei,
    fd: c_int,
    device: *mut EiDevice,
    emulating: bool,
    sequence: u32,
    disconnected: bool,
}

// The raw pointers are only ever touched under the owning mutex.
unsafe impl Send for Context {}

impl Context {
    /// Drain libei's queue; returns false once gamescope dropped the connection.
    fn dispatch(&mut self) -> bool {
        let lib = self.lib;
        unsafe {
            (lib.dispatch)(self.ei);
            loop {
                let ev = (lib.get_event)(self.ei);
                if ev.is_null() {
                    break;
                }
                match (lib.event_get_type)(ev) {
                    EI_EVENT_CONNECT => log::debug!("gamescope: ei connected"),
                    EI_EVENT_DISCONNECT => {
                        log::warn!("gamescope: ei disconnected");
                        self.emulating = false;
                        self.disconnected = true;
                    }
                    EI_EVENT_SEAT_ADDED => {
                        (lib.seat_bind_capabilities)(
                            (lib.event_get_seat)(ev),
                            EI_DEVICE_CAP_POINTER,
                            EI_DEVICE_CAP_POINTER_ABSOLUTE,
                            EI_DEVICE_CAP_BUTTON,
                            EI_DEVICE_CAP_SCROLL,
                            std::ptr::null::<c_void>(),
                        );
                    }
                    EI_EVENT_DEVICE_ADDED => {
                        let device = (lib.event_get_device)(ev);
                        if self.device.is_null()
                            && (lib.device_has_capability)(device, EI_DEVICE_CAP_POINTER_ABSOLUTE)
                        {
                            self.device = (lib.device_ref)(device);
                        }
                    }
                    EI_EVENT_DEVICE_RESUMED => {
                        if (lib.event_get_device)(ev) == self.device && !self.device.is_null() {
                            self.sequence = self.sequence.wrapping_add(1);
                            (lib.device_start_emulating)(self.device, self.sequence);
                            self.emulating = true;
                        }
                    }
                    EI_EVENT_DEVICE_PAUSED => {
                        if (lib.event_get_device)(ev) == self.device {
                            self.emulating = false;
                        }
                    }
                    EI_EVENT_DEVICE_REMOVED => {
                        if (lib.event_get_device)(ev) == self.device && !self.device.is_null() {
                            (lib.device_unref)(self.device);
                            self.device = std::ptr::null_mut();
                            self.emulating = false;
                        }
                    }
                    _ => {}
                }
                (lib.event_unref)(ev);
            }
        }
        !self.disconnected
    }

    /// Run `f` on the device and close the frame, if gamescope currently lets us emulate.
    fn emit(&mut self, f: impl FnOnce(&LibEi, *mut EiDevice)) {
        if !self.emulating || self.device.is_null() {
            return;
        }
        let lib = self.lib;
        f(lib, self.device);
        unsafe { (lib.frame)(self.device, (lib.now)(self.ei)) };
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe {
            if !self.device.is_null() {
                (self.lib.device_unref)(self.device);
            }
            (self.lib.unref)(self.ei);
        }
    }
}

fn poll_readable(fd: c_int, timeout_ms: c_int) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    unsafe { libc::poll(&mut pfd, 1, timeout_ms) > 0 }
}

/// Keeps libei's protocol moving (pings, device pause/resume) for as long as the mouse lives.
fn dispatch_loop(ctx: Weak<Mutex<Context>>, fd: c_int) {
    loop {
        let readable = poll_readable(fd, DISPATCH_POLL_MS);
        let Some(ctx) = ctx.upgrade() else {
            return;
        };
        if readable && !ctx.lock().unwrap().dispatch() {
            return;
        }
    }
}

/// The evdev code libei expects for a button, or None for the scroll pseudo-buttons.
fn evdev_button(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(BTN_LEFT),
        MouseButton::Right => Some(BTN_RIGHT),
        MouseButton::Middle => Some(BTN_MIDDLE),
        MouseButton::Back => Some(BTN_SIDE),
        MouseButton::Forward => Some(BTN_EXTRA),
        MouseButton::ScrollUp
        | MouseButton::ScrollDown
        | MouseButton::ScrollLeft
        | MouseButton::ScrollRight => None,
    }
}

/// Discrete scroll (dx, dy) for a scroll pseudo-button: libei's positive y scrolls down.
fn scroll_button_delta(button: MouseButton) -> Option<(i32, i32)> {
    match button {
        MouseButton::ScrollUp => Some((0, -SCROLL_NOTCH)),
        MouseButton::ScrollDown => Some((0, SCROLL_NOTCH)),
        MouseButton::ScrollLeft => Some((-SCROLL_NOTCH, 0)),
        MouseButton::ScrollRight => Some((SCROLL_NOTCH, 0)),
        _ => None,
    }
}

pub struct EiMouse {
    ctx: Arc<Mutex<Context>>,
}

impl EiMouse {
    /// Connect to gamescope's EIS socket and wait until it hands over an absolute pointer.
    /// None means "keep the uinput mouse"; it blocks for up to `HANDSHAKE_TIMEOUT`.
    pub fn new() -> Option<Self> {
        let socket = crate::platform::linux::gamescope_ei_socket()?;
        let lib = LibEi::get()?;
        let path = CString::new(socket.to_string_lossy().as_bytes()).ok()?;
        let ei = unsafe { (lib.new_sender)(std::ptr::null_mut()) };
        if ei.is_null() {
            return None;
        }
        let mut ctx = Context {
            lib,
            ei,
            fd: -1,
            device: std::ptr::null_mut(),
            emulating: false,
            sequence: 0,
            disconnected: false,
        };
        unsafe {
            (lib.configure_name)(ei, b"rustdesk\0".as_ptr() as *const c_char);
            let rc = (lib.setup_backend_socket)(ei, path.as_ptr());
            if rc != 0 {
                log::warn!(
                    "gamescope: cannot connect to {}: {}",
                    socket.display(),
                    std::io::Error::from_raw_os_error(-rc)
                );
                return None;
            }
            ctx.fd = (lib.get_fd)(ei);
        }
        let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
        while !ctx.emulating {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                log::warn!("gamescope: no ei pointer offered in time; keeping the uinput mouse");
                return None;
            }
            if poll_readable(ctx.fd, left.as_millis() as c_int) && !ctx.dispatch() {
                return None;
            }
        }
        log::info!(
            "gamescope: pointer input goes through ei at {}",
            socket.display()
        );
        let fd = ctx.fd;
        let ctx = Arc::new(Mutex::new(ctx));
        let weak = Arc::downgrade(&ctx);
        std::thread::Builder::new()
            .name("gamescope-ei".to_owned())
            .spawn(move || dispatch_loop(weak, fd))
            .ok()?;
        Some(Self { ctx })
    }

    fn button(&mut self, button: MouseButton, press: bool) {
        if let Some(code) = evdev_button(button) {
            self.ctx
                .lock()
                .unwrap()
                .emit(|lib, dev| unsafe { (lib.button_button)(dev, code, press) });
        } else if press {
            if let Some((dx, dy)) = scroll_button_delta(button) {
                self.scroll(dx, dy);
            }
        }
    }

    fn scroll(&mut self, dx: i32, dy: i32) {
        self.ctx
            .lock()
            .unwrap()
            .emit(|lib, dev| unsafe { (lib.scroll_discrete)(dev, dx, dy) });
    }
}

impl MouseControllable for EiMouse {
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_mut_any(&mut self) -> &mut dyn std::any::Any {
        self
    }

    fn mouse_move_to(&mut self, x: i32, y: i32) {
        self.ctx.lock().unwrap().emit(|lib, dev| unsafe {
            (lib.pointer_motion_absolute)(dev, x as c_double, y as c_double)
        });
    }

    fn mouse_move_relative(&mut self, x: i32, y: i32) {
        self.ctx
            .lock()
            .unwrap()
            .emit(|lib, dev| unsafe { (lib.pointer_motion)(dev, x as c_double, y as c_double) });
    }

    fn mouse_down(&mut self, button: MouseButton) -> enigo::ResultType {
        self.button(button, true);
        Ok(())
    }

    fn mouse_up(&mut self, button: MouseButton) {
        self.button(button, false);
    }

    fn mouse_click(&mut self, button: MouseButton) {
        self.button(button, true);
        self.button(button, false);
    }

    // Same sign convention as the uinput mouse: a negative length scrolls up / left.
    fn mouse_scroll_x(&mut self, length: i32) {
        self.scroll(length.saturating_mul(SCROLL_NOTCH), 0);
    }

    fn mouse_scroll_y(&mut self, length: i32) {
        self.scroll(0, length.saturating_mul(SCROLL_NOTCH));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buttons_map_to_evdev_codes() {
        assert_eq!(evdev_button(MouseButton::Left), Some(BTN_LEFT));
        assert_eq!(evdev_button(MouseButton::Right), Some(BTN_RIGHT));
        assert_eq!(evdev_button(MouseButton::Middle), Some(BTN_MIDDLE));
        assert_eq!(evdev_button(MouseButton::Back), Some(BTN_SIDE));
        assert_eq!(evdev_button(MouseButton::Forward), Some(BTN_EXTRA));
        assert_eq!(evdev_button(MouseButton::ScrollUp), None);
    }

    #[test]
    fn scroll_buttons_follow_libei_signs() {
        assert_eq!(
            scroll_button_delta(MouseButton::ScrollUp),
            Some((0, -SCROLL_NOTCH))
        );
        assert_eq!(
            scroll_button_delta(MouseButton::ScrollDown),
            Some((0, SCROLL_NOTCH))
        );
        assert_eq!(
            scroll_button_delta(MouseButton::ScrollLeft),
            Some((-SCROLL_NOTCH, 0))
        );
        assert_eq!(
            scroll_button_delta(MouseButton::ScrollRight),
            Some((SCROLL_NOTCH, 0))
        );
        assert_eq!(scroll_button_delta(MouseButton::Left), None);
    }
}
