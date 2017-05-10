#![cfg(any(target_os = "linux", target_os = "dragonfly", target_os = "freebsd", target_os = "openbsd"))]

pub use self::monitor::{MonitorId, get_available_monitors, get_primary_monitor};
pub use self::window::{Window, XWindow, WindowProxy};
pub use self::xdisplay::{XConnection, XNotSupported, XError};

pub mod ffi;

use platform::PlatformSpecificWindowBuilderAttributes;
use {CreationError, Event, WindowEvent, DeviceEvent, AxisId, ButtonId, KeyboardInput};

use std::{mem, ptr, slice};
use std::sync::{Arc, Mutex, Weak};
use std::collections::HashMap;
use std::ffi::CStr;

use libc::{self, c_uchar, c_char, c_int};

mod events;
mod monitor;
mod window;
mod xdisplay;

// API TRANSITION
//
// We don't use the gen_api_transistion!() macro but rather do the expansion manually:
//
// As this module is nested into platform/linux, its code is not _exactly_ the same as
// the one generated by the macro.

pub struct EventsLoop {
    interrupted: ::std::sync::atomic::AtomicBool,
    display: Arc<XConnection>,
    wm_delete_window: ffi::Atom,
    windows: Mutex<HashMap<WindowId, WindowData>>,
    devices: Mutex<HashMap<DeviceId, Device>>,
    xi2ext: XExtension,
    root: ffi::Window,
}

impl EventsLoop {
    pub fn new(display: Arc<XConnection>) -> EventsLoop {
        let wm_delete_window = unsafe { (display.xlib.XInternAtom)(display.display, b"WM_DELETE_WINDOW\0".as_ptr() as *const c_char, 0) };
        display.check_errors().expect("Failed to call XInternAtom");

        let xi2ext = unsafe {
            let mut result = XExtension {
                opcode: mem::uninitialized(),
                first_event_id: mem::uninitialized(),
                first_error_id: mem::uninitialized(),
            };
            let res = (display.xlib.XQueryExtension)(
                display.display,
                b"XInputExtension\0".as_ptr() as *const c_char,
                &mut result.opcode as *mut c_int,
                &mut result.first_event_id as *mut c_int,
                &mut result.first_error_id as *mut c_int);
            if res == ffi::False {
                panic!("X server missing XInput extension");
            }
            result
        };

        unsafe {
            let mut xinput_major_ver = ffi::XI_2_Major;
            let mut xinput_minor_ver = ffi::XI_2_Minor;

            if (display.xinput2.XIQueryVersion)(display.display, &mut xinput_major_ver, &mut xinput_minor_ver) != ffi::Success as libc::c_int {
                panic!("X server has XInput extension {}.{} but does not support XInput2", xinput_major_ver, xinput_minor_ver);
            }
        }

        let root = unsafe { (display.xlib.XDefaultRootWindow)(display.display) };

        let result = EventsLoop {
            interrupted: ::std::sync::atomic::AtomicBool::new(false),
            display: display,
            wm_delete_window: wm_delete_window,
            windows: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
            xi2ext: xi2ext,
            root: root,
        };

        {
            // Register for device hotplug events
            let mask = ffi::XI_HierarchyChangedMask;
            unsafe {
                let mut event_mask = ffi::XIEventMask{
                    deviceid: ffi::XIAllDevices,
                    mask: &mask as *const _ as *mut c_uchar,
                    mask_len: mem::size_of_val(&mask) as c_int,
                };
                (result.display.xinput2.XISelectEvents)(result.display.display, root,
                                                 &mut event_mask as *mut ffi::XIEventMask, 1);
            }

            result.init_device(ffi::XIAllDevices);
        }

        result
    }

    pub fn interrupt(&self) {
        self.interrupted.store(true, ::std::sync::atomic::Ordering::Relaxed);

        // Push an event on the X event queue so that methods like run_forever will advance.
        let mut xev = ffi::XClientMessageEvent {
            type_: ffi::ClientMessage,
            window: self.root,
            format: 32,
            message_type: 0,
            serial: 0,
            send_event: 0,
            display: self.display.display,
            data: unsafe { mem::zeroed() },
        };

        unsafe {
            (self.display.xlib.XSendEvent)(self.display.display, self.root, 0, 0, mem::transmute(&mut xev));
            (self.display.xlib.XFlush)(self.display.display);
            self.display.check_errors().expect("Failed to call XSendEvent after wakeup");
        }
    }

    pub fn poll_events<F>(&self, mut callback: F)
        where F: FnMut(Event)
    {
        let xlib = &self.display.xlib;

        let mut xev = unsafe { mem::uninitialized() };
        loop {
            // Get next event
            unsafe {
                // Ensure XNextEvent won't block
                let count = (xlib.XPending)(self.display.display);
                if count == 0 {
                    break;
                }

                (xlib.XNextEvent)(self.display.display, &mut xev);
            }
            self.process_event(&mut xev, &mut callback);
            if self.interrupted.load(::std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
    }

    pub fn run_forever<F>(&self, mut callback: F)
        where F: FnMut(Event)
    {
        self.interrupted.store(false, ::std::sync::atomic::Ordering::Relaxed);

        let xlib = &self.display.xlib;

        let mut xev = unsafe { mem::uninitialized() };

        loop {
            unsafe { (xlib.XNextEvent)(self.display.display, &mut xev) }; // Blocks as necessary
            self.process_event(&mut xev, &mut callback);
            if self.interrupted.load(::std::sync::atomic::Ordering::Relaxed) {
                break;
            }
        }
    }

    pub fn device_name(&self, device: DeviceId) -> String {
        let devices = self.devices.lock().unwrap();
        let device = devices.get(&device).unwrap();
        device.name.clone()
    }

    fn process_event<F>(&self, xev: &mut ffi::XEvent, callback: &mut F)
        where F: FnMut(Event)
    {
        let xlib = &self.display.xlib;

        // Handle dead keys and other input method funtimes
        if ffi::True == unsafe { (self.display.xlib.XFilterEvent)(xev, { let xev: &ffi::XAnyEvent = xev.as_ref(); xev.window }) } {
            return;
        }

        let xwindow = { let xev: &ffi::XAnyEvent = xev.as_ref(); xev.window };
        let wid = ::WindowId(::platform::WindowId::X(WindowId(xwindow)));
        match xev.get_type() {
            ffi::MappingNotify => {
                unsafe { (xlib.XRefreshKeyboardMapping)(xev.as_mut()); }
                self.display.check_errors().expect("Failed to call XRefreshKeyboardMapping");
            }

            ffi::ClientMessage => {
                let client_msg: &ffi::XClientMessageEvent = xev.as_ref();

                if client_msg.data.get_long(0) as ffi::Atom == self.wm_delete_window {
                    callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Closed })
                } else {
                    // FIXME: Prone to spurious wakeups
                    callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Awakened })
                }
            }

            ffi::ConfigureNotify => {
                let xev: &ffi::XConfigureEvent = xev.as_ref();
                let size = (xev.width, xev.height);
                let position = (xev.x, xev.y);
                // Gymnastics to ensure self.windows isn't locked when we invoke callback
                let (resized, moved) = {
                    let mut windows = self.windows.lock().unwrap();
                    let window_data = windows.get_mut(&WindowId(xwindow)).unwrap();
                    if window_data.config.is_none() {
                        window_data.config = Some(WindowConfig::new(xev));
                        (true, true)
                    } else {
                        let window = window_data.config.as_mut().unwrap();
                        (if window.size != size {
                            window.size = size;
                            true
                        } else { false },
                        if window.position != position {
                            window.position = position;
                            true
                        } else { false })
                    }
                };
                if resized {
                    callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Resized(xev.width as u32, xev.height as u32) });
                }
                if moved {
                    callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Moved(xev.x as i32, xev.y as i32) });
                }
            }

            ffi::Expose => {
                callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Refresh });
            }

            // FIXME: Use XInput2 + libxkbcommon for keyboard input!
            ffi::KeyPress | ffi::KeyRelease => {
                use events::ModifiersState;
                use events::ElementState::{Pressed, Released};

                let state;
                if xev.get_type() == ffi::KeyPress {
                    state = Pressed;
                } else {
                    state = Released;
                }

                let xkev: &mut ffi::XKeyEvent = xev.as_mut();


                let mut ev_mods = ModifiersState::default();

                let keysym = unsafe {
                    (self.display.xlib.XKeycodeToKeysym)(self.display.display, xkev.keycode as ffi::KeyCode, 0)
                };

                let vkey = events::keysym_to_element(keysym as libc::c_uint);

                callback(Event::WindowEvent { window_id: wid, event: WindowEvent::KeyboardInput {
                     // Typical virtual core keyboard ID. xinput2 needs to be used to get a reliable value.
                    device_id: mkdid(3),
                    input: KeyboardInput {
                        state: state,
                        scancode: xkev.keycode,
                        virtual_keycode: vkey,
                        modifiers: ev_mods,
                    },
                }});

                if state == Pressed {
                    let written = unsafe {
                        use std::str;

                        let mut windows = self.windows.lock().unwrap();
                        let window_data = windows.get_mut(&WindowId(xwindow)).unwrap();
                        let mut buffer: [u8; 16] = [mem::uninitialized(); 16];
                        let mut keysym = 0;
                        let count = (self.display.xlib.Xutf8LookupString)(window_data.ic, xkev,
                                                                          mem::transmute(buffer.as_mut_ptr()),
                                                                          buffer.len() as libc::c_int, &mut keysym, ptr::null_mut());

                        {
                            // Translate x event state to mods
                            let state = xkev.state;
                            if (state & ffi::Mod1Mask) != 0 {
                                ev_mods.alt = true;
                            }

                            if (state & ffi::ShiftMask) != 0 {
                                ev_mods.shift = true;
                            }

                            if (state & ffi::ControlMask) != 0 {
                                ev_mods.ctrl = true;
                            }

                            if (state & ffi::Mod4Mask) != 0 {
                                ev_mods.logo = true;
                            }
                        }

                        str::from_utf8(&buffer[..count as usize]).unwrap_or("").to_string()
                    };

                    for chr in written.chars() {
                        callback(Event::WindowEvent { window_id: wid, event: WindowEvent::ReceivedCharacter(chr) })
                    }
                }
            }

            ffi::GenericEvent => {
                let guard = if let Some(e) = GenericEventCookie::from_event(&self.display, *xev) { e } else { return };
                let xev = &guard.cookie;
                if self.xi2ext.opcode != xev.extension {
                    return;
                }

                use events::WindowEvent::{Focused, MouseEntered, MouseInput, MouseLeft, MouseMoved, MouseWheel, AxisMotion};
                use events::ElementState::{Pressed, Released};
                use events::MouseButton::{Left, Right, Middle, Other};
                use events::MouseScrollDelta::LineDelta;
                use events::{Touch, TouchPhase};

                match xev.evtype {
                    ffi::XI_ButtonPress | ffi::XI_ButtonRelease => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let wid = mkwid(xev.event);
                        let did = mkdid(xev.deviceid);
                        if (xev.flags & ffi::XIPointerEmulated) != 0 && self.windows.lock().unwrap().get(&WindowId(xev.event)).unwrap().multitouch {
                            // Deliver multi-touch events instead of emulated mouse events.
                            return;
                        }
                        let state = if xev.evtype == ffi::XI_ButtonPress {
                            Pressed
                        } else {
                            Released
                        };
                        match xev.detail as u32 {
                            ffi::Button1 => callback(Event::WindowEvent { window_id: wid, event:
                                                                          MouseInput { device_id: did, state: state, button: Left } }),
                            ffi::Button2 => callback(Event::WindowEvent { window_id: wid, event:
                                                                          MouseInput { device_id: did, state: state, button: Middle } }),
                            ffi::Button3 => callback(Event::WindowEvent { window_id: wid, event:
                                                                          MouseInput { device_id: did, state: state, button: Right } }),

                            // Suppress emulated scroll wheel clicks, since we handle the real motion events for those.
                            // In practice, even clicky scroll wheels appear to be reported by evdev (and XInput2 in
                            // turn) as axis motion, so we don't otherwise special-case these button presses.
                            4 | 5 | 6 | 7 if xev.flags & ffi::XIPointerEmulated != 0 => {}

                            x => callback(Event::WindowEvent { window_id: wid, event: MouseInput { device_id: did, state: state, button: Other(x as u8) } })
                        }
                    }
                    ffi::XI_Motion => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let did = mkdid(xev.deviceid);
                        let wid = mkwid(xev.event);
                        let new_cursor_pos = (xev.event_x, xev.event_y);

                        // Gymnastics to ensure self.windows isn't locked when we invoke callback
                        if {
                            let mut windows = self.windows.lock().unwrap();
                            let window_data = windows.get_mut(&WindowId(xev.event)).unwrap();
                            if Some(new_cursor_pos) != window_data.cursor_pos {
                                window_data.cursor_pos = Some(new_cursor_pos);
                                true
                            } else { false }
                        } {
                            callback(Event::WindowEvent { window_id: wid, event: MouseMoved {
                                device_id: did,
                                position: new_cursor_pos
                            }});
                        }

                        // More gymnastics, for self.devices
                        let mut events = Vec::new();
                        {
                            let mask = unsafe { slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize) };
                            let mut devices = self.devices.lock().unwrap();
                            let physical_device = devices.get_mut(&DeviceId(xev.sourceid)).unwrap();

                            let mut value = xev.valuators.values;
                            for i in 0..xev.valuators.mask_len*8 {
                                if ffi::XIMaskIsSet(mask, i) {
                                    if let Some(&mut (_, ref mut info)) = physical_device.scroll_axes.iter_mut().find(|&&mut (axis, _)| axis == i) {
                                        let delta = (unsafe { *value } - info.position) / info.increment;
                                        info.position = unsafe { *value };
                                        events.push(Event::WindowEvent { window_id: wid, event: MouseWheel {
                                            device_id: did,
                                            delta: match info.orientation {
                                                ScrollOrientation::Horizontal => LineDelta(delta as f32, 0.0),
                                                ScrollOrientation::Vertical => LineDelta(0.0, delta as f32),
                                            },
                                            phase: TouchPhase::Moved,
                                        }});
                                    } else {
                                        events.push(Event::WindowEvent { window_id: wid, event: AxisMotion {
                                            device_id: did,
                                            axis: AxisId(i as u32),
                                            value: unsafe { *value },
                                        }});
                                    }
                                    value = unsafe { value.offset(1) };
                                }
                            }
                        }
                        for event in events {
                            callback(event);
                        }
                    }

                    ffi::XI_Enter => {
                        let xev: &ffi::XIEnterEvent = unsafe { &*(xev.data as *const _) };
                        callback(Event::WindowEvent { window_id: mkwid(xev.event), event: MouseEntered { device_id: mkdid(xev.deviceid) } })
                    }
                    ffi::XI_Leave => {
                        let xev: &ffi::XILeaveEvent = unsafe { &*(xev.data as *const _) };
                        callback(Event::WindowEvent { window_id: mkwid(xev.event), event: MouseLeft { device_id: mkdid(xev.deviceid) } })
                    }
                    ffi::XI_FocusIn => {
                        let xev: &ffi::XIFocusInEvent = unsafe { &*(xev.data as *const _) };
                        callback(Event::WindowEvent { window_id: mkwid(xev.event), event: Focused(true) })
                    }
                    ffi::XI_FocusOut => {
                        let xev: &ffi::XIFocusOutEvent = unsafe { &*(xev.data as *const _) };
                        callback(Event::WindowEvent { window_id: mkwid(xev.event), event: Focused(false) })
                    }

                    ffi::XI_TouchBegin | ffi::XI_TouchUpdate | ffi::XI_TouchEnd => {
                        let xev: &ffi::XIDeviceEvent = unsafe { &*(xev.data as *const _) };
                        let wid = mkwid(xev.event);
                        let phase = match xev.evtype {
                            ffi::XI_TouchBegin => TouchPhase::Started,
                            ffi::XI_TouchUpdate => TouchPhase::Moved,
                            ffi::XI_TouchEnd => TouchPhase::Ended,
                            _ => unreachable!()
                        };
                        callback(Event::WindowEvent { window_id: wid, event: WindowEvent::Touch(Touch {
                            device_id: mkdid(xev.deviceid),
                            phase: phase,
                            location: (xev.event_x, xev.event_y),
                            id: xev.detail as u64,
                        })})
                    }

                    ffi::XI_RawButtonPress | ffi::XI_RawButtonRelease => {
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };
                        if xev.flags & ffi::XIPointerEmulated == 0 {
                            callback(Event::DeviceEvent { device_id: mkdid(xev.deviceid), event: DeviceEvent::Button {
                                button: ButtonId(xev.detail as u32),
                                state: match xev.evtype {
                                    ffi::XI_RawButtonPress => Pressed,
                                    ffi::XI_RawButtonRelease => Released,
                                    _ => unreachable!(),
                                },
                            }});
                        }
                    }

                    ffi::XI_RawMotion => {
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };
                        let did = mkdid(xev.deviceid);

                        let mask = unsafe { slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize) };
                        let mut value = xev.valuators.values;
                        for i in 0..xev.valuators.mask_len*8 {
                            if ffi::XIMaskIsSet(mask, i) {
                                callback(Event::DeviceEvent { device_id: did, event: DeviceEvent::Motion {
                                    axis: AxisId(i as u32),
                                    value: unsafe { *value },
                                }});
                                value = unsafe { value.offset(1) };
                            }
                        }
                    }

                    ffi::XI_RawKeyPress | ffi::XI_RawKeyRelease => {
                        // TODO: Use xkbcommon for keysym and text decoding
                        let xev: &ffi::XIRawEvent = unsafe { &*(xev.data as *const _) };
                        let xkeysym = unsafe { (self.display.xlib.XKeycodeToKeysym)(self.display.display, xev.detail as ffi::KeyCode, 0) };
                        callback(Event::DeviceEvent { device_id: mkdid(xev.deviceid), event: DeviceEvent::Key(KeyboardInput {
                            scancode: xev.detail as u32,
                            virtual_keycode: events::keysym_to_element(xkeysym as libc::c_uint),
                            state: match xev.evtype {
                                ffi::XI_RawKeyPress => Pressed,
                                ffi::XI_RawKeyRelease => Released,
                                _ => unreachable!(),
                            },
                            modifiers: ::events::ModifiersState::default(),
                        })});
                    }

                    ffi::XI_HierarchyChanged => {
                        let xev: &ffi::XIHierarchyEvent = unsafe { &*(xev.data as *const _) };
                        for info in unsafe { slice::from_raw_parts(xev.info, xev.num_info as usize) } {
                            if 0 != info.flags & (ffi::XISlaveAdded | ffi::XIMasterAdded) {
                                self.init_device(info.deviceid);
                                callback(Event::DeviceEvent { device_id: mkdid(info.deviceid), event: DeviceEvent::Added });
                            } else if 0 != info.flags & (ffi::XISlaveRemoved | ffi::XIMasterRemoved) {
                                callback(Event::DeviceEvent { device_id: mkdid(info.deviceid), event: DeviceEvent::Removed });
                                let mut devices = self.devices.lock().unwrap();
                                devices.remove(&DeviceId(info.deviceid));
                            }
                        }
                    }

                    _ => {}
                }
            }

            _ => {}
        }
    }

    fn init_device(&self, device: c_int) {
        let mut devices = self.devices.lock().unwrap();
        for info in DeviceInfo::get(&self.display, device).iter() {
            devices.insert(DeviceId(info.deviceid), Device::new(&self, info));
        }
    }
}

struct DeviceInfo<'a> {
    display: &'a XConnection,
    info: *const ffi::XIDeviceInfo,
    count: usize,
}

impl<'a> DeviceInfo<'a> {
    fn get(display: &'a XConnection, device: c_int) -> Self {
        unsafe {
            let mut count = mem::uninitialized();
            let info = (display.xinput2.XIQueryDevice)(display.display, device, &mut count);
            DeviceInfo {
                display: display,
                info: info,
                count: count as usize,
            }
        }
    }
}

impl<'a> Drop for DeviceInfo<'a> {
    fn drop(&mut self) {
        unsafe { (self.display.xinput2.XIFreeDeviceInfo)(self.info as *mut _) };
    }
}

impl<'a> ::std::ops::Deref for DeviceInfo<'a> {
    type Target = [ffi::XIDeviceInfo];
    fn deref(&self) -> &Self::Target {
        unsafe { slice::from_raw_parts(self.info, self.count) }
    }
}

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WindowId(ffi::Window);

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct DeviceId(c_int);

pub struct Window2 {
    pub window: Arc<Window>,
    events_loop: Weak<::platform::EventsLoop>,
}

impl ::std::ops::Deref for Window2 {
    type Target = Window;
    #[inline]
    fn deref(&self) -> &Window {
        &*self.window
    }
}

// XOpenIM doesn't seem to be thread-safe
lazy_static! {      // TODO: use a static mutex when that's possible, and put me back in my function
    static ref GLOBAL_XOPENIM_LOCK: Mutex<()> = Mutex::new(());
}

impl Window2 {
    pub fn new(events_loop: Arc<::platform::EventsLoop>,
               window: &::WindowAttributes, pl_attribs: &PlatformSpecificWindowBuilderAttributes)
               -> Result<Window2, CreationError>
    {
        let x_events_loop = if let ::platform::EventsLoop::X(ref e) = *events_loop { e } else { unreachable!() };
        let win = ::std::sync::Arc::new(try!(Window::new(&x_events_loop, window, pl_attribs)));

        // creating IM
        let im = unsafe {
            let _lock = GLOBAL_XOPENIM_LOCK.lock().unwrap();

            let im = (x_events_loop.display.xlib.XOpenIM)(x_events_loop.display.display, ptr::null_mut(), ptr::null_mut(), ptr::null_mut());
            if im.is_null() {
                panic!("XOpenIM failed");
            }
            im
        };

        // creating input context
        let ic = unsafe {
            let ic = (x_events_loop.display.xlib.XCreateIC)(im,
                                              b"inputStyle\0".as_ptr() as *const _,
                                              ffi::XIMPreeditNothing | ffi::XIMStatusNothing, b"clientWindow\0".as_ptr() as *const _,
                                              win.id().0, ptr::null::<()>());
            if ic.is_null() {
                panic!("XCreateIC failed");
            }
            (x_events_loop.display.xlib.XSetICFocus)(ic);
            x_events_loop.display.check_errors().expect("Failed to call XSetICFocus");
            ic
        };
        
        x_events_loop.windows.lock().unwrap().insert(win.id(), WindowData {
            im: im,
            ic: ic,
            config: None,
            multitouch: window.multitouch,
            cursor_pos: None,
        });

        Ok(Window2 {
            window: win,
            events_loop: Arc::downgrade(&events_loop),
        })
    }

    #[inline]
    pub fn id(&self) -> WindowId {
        self.window.id()
    }
}

impl Drop for Window2 {
    fn drop(&mut self) {
        if let Some(ev) = self.events_loop.upgrade() {
            if let ::platform::EventsLoop::X(ref ev) = *ev {
                let mut windows = ev.windows.lock().unwrap();


                let w = windows.remove(&self.window.id()).unwrap();
                let _lock = GLOBAL_XOPENIM_LOCK.lock().unwrap();
                unsafe {
                    (ev.display.xlib.XDestroyIC)(w.ic);
                    (ev.display.xlib.XCloseIM)(w.im);
                }
            }
        }
    }
}

/// State maintained for translating window-related events
struct WindowData {
    config: Option<WindowConfig>,
    im: ffi::XIM,
    ic: ffi::XIC,
    multitouch: bool,
    cursor_pos: Option<(f64, f64)>,
}

// Required by ffi members
unsafe impl Send for WindowData {}

struct WindowConfig {
    size: (c_int, c_int),
    position: (c_int, c_int),
}

impl WindowConfig {
    fn new(event: &ffi::XConfigureEvent) -> Self {
        WindowConfig {
            size: (event.width, event.height),
            position: (event.x, event.y),
        }
    }
}


/// XEvents of type GenericEvent store their actual data in an XGenericEventCookie data structure. This is a wrapper to
/// extract the cookie from a GenericEvent XEvent and release the cookie data once it has been processed
struct GenericEventCookie<'a> {
    display: &'a XConnection,
    cookie: ffi::XGenericEventCookie
}

impl<'a> GenericEventCookie<'a> {
    fn from_event<'b>(display: &'b XConnection, event: ffi::XEvent) -> Option<GenericEventCookie<'b>> {
        unsafe {
            let mut cookie: ffi::XGenericEventCookie = From::from(event);
            if (display.xlib.XGetEventData)(display.display, &mut cookie) == ffi::True {
                Some(GenericEventCookie{display: display, cookie: cookie})
            } else {
                None
            }
        }
    }
}

impl<'a> Drop for GenericEventCookie<'a> {
    fn drop(&mut self) {
        unsafe {
            let xlib = &self.display.xlib;
            (xlib.XFreeEventData)(self.display.display, &mut self.cookie);
        }
    }
}

#[derive(Debug, Copy, Clone)]
struct XExtension {
    opcode: c_int,
    first_event_id: c_int,
    first_error_id: c_int,
}

fn mkwid(w: ffi::Window) -> ::WindowId { ::WindowId(::platform::WindowId::X(WindowId(w))) }
fn mkdid(w: c_int) -> ::DeviceId { ::DeviceId(::platform::DeviceId::X(DeviceId(w))) }

#[derive(Debug)]
struct Device {
    name: String,
    scroll_axes: Vec<(i32, ScrollAxis)>,
}

#[derive(Debug, Copy, Clone)]
struct ScrollAxis {
    increment: f64,
    orientation: ScrollOrientation,
    position: f64,
}

#[derive(Debug, Copy, Clone)]
enum ScrollOrientation {
    Vertical,
    Horizontal,
}

impl Device {
    fn new(el: &EventsLoop, info: &ffi::XIDeviceInfo) -> Self
    {
        let name = unsafe { CStr::from_ptr(info.name).to_string_lossy() };

        let physical_device = info._use == ffi::XISlaveKeyboard || info._use == ffi::XISlavePointer || info._use == ffi::XIFloatingSlave;
        if physical_device {
            // Register for global raw events
            let mask = ffi::XI_RawMotionMask
                | ffi::XI_RawButtonPressMask | ffi::XI_RawButtonReleaseMask
                | ffi::XI_RawKeyPressMask | ffi::XI_RawKeyReleaseMask;
            unsafe {
                let mut event_mask = ffi::XIEventMask{
                    deviceid: info.deviceid,
                    mask: &mask as *const _ as *mut c_uchar,
                    mask_len: mem::size_of_val(&mask) as c_int,
                };
                (el.display.xinput2.XISelectEvents)(el.display.display, el.root, &mut event_mask as *mut ffi::XIEventMask, 1);
            }
        }

        let mut scroll_axes = Vec::new();

        if physical_device {
            let classes : &[*const ffi::XIAnyClassInfo] =
                unsafe { slice::from_raw_parts(info.classes as *const *const ffi::XIAnyClassInfo, info.num_classes as usize) };
            // Identify scroll axes
            for class_ptr in classes {
                let class = unsafe { &**class_ptr };
                match class._type {
                    ffi::XIScrollClass => {
                        let info = unsafe { mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIScrollClassInfo>(class) };
                        scroll_axes.push((info.number, ScrollAxis {
                            increment: info.increment,
                            orientation: match info.scroll_type {
                                ffi::XIScrollTypeHorizontal => ScrollOrientation::Horizontal,
                                ffi::XIScrollTypeVertical => ScrollOrientation::Vertical,
                                _ => { unreachable!() }
                            },
                            position: 0.0,
                        }));
                    }
                    _ => {}
                }
            }
            // Fix up initial scroll positions
            for class_ptr in classes {
                let class = unsafe { &**class_ptr };
                match class._type {
                    ffi::XIValuatorClass => {
                        let info = unsafe { mem::transmute::<&ffi::XIAnyClassInfo, &ffi::XIValuatorClassInfo>(class) };
                        if let Some(&mut (_, ref mut axis)) = scroll_axes.iter_mut().find(|&&mut (axis, _)| axis == info.number) {
                            axis.position = info.value;
                        }
                    }
                    _ => {}
                }
            }
        }

        Device {
            name: name.into_owned(),
            scroll_axes: scroll_axes,
        }
    }
}
