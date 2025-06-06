use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::os::raw::{c_char, c_int, c_long, c_ulong};
use std::slice;
use std::sync::{Arc, Mutex};

use dpi::{PhysicalPosition, PhysicalSize};
use winit_core::application::ApplicationHandler;
use winit_core::event::{
    ButtonSource, DeviceEvent, DeviceId, ElementState, FingerId, Ime, MouseButton,
    MouseScrollDelta, PointerKind, PointerSource, RawKeyEvent, SurfaceSizeWriter, TouchPhase,
    WindowEvent,
};
use winit_core::keyboard::ModifiersState;
use x11_dl::xinput2::{
    self, XIDeviceEvent, XIEnterEvent, XIFocusInEvent, XIFocusOutEvent, XIHierarchyEvent,
    XILeaveEvent, XIModifierState, XIRawEvent,
};
use x11_dl::xlib::{
    self, Display as XDisplay, Window as XWindow, XAnyEvent, XClientMessageEvent, XConfigureEvent,
    XDestroyWindowEvent, XEvent, XExposeEvent, XKeyEvent, XMapEvent, XPropertyEvent,
    XReparentEvent, XSelectionEvent, XVisibilityEvent, XkbAnyEvent, XkbStateRec,
};
use x11rb::protocol::sync::{ConnectionExt, Int64};
use x11rb::protocol::xinput;
use x11rb::protocol::xkb::ID as XkbId;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ModMask};
use x11rb::x11_utils::{ExtensionInformation, Serialize};
use xkbcommon_dl::xkb_mod_mask_t;

use crate::platform_impl::common::xkb::{self, XkbState};
use crate::platform_impl::platform::common::xkb::Context;
use crate::platform_impl::platform::x11::ime::{ImeEvent, ImeEventReceiver, ImeRequest};
use crate::platform_impl::platform::x11::ActiveEventLoop;
use crate::platform_impl::x11::atoms::*;
use crate::platform_impl::x11::util::cookie::GenericEventCookie;
use crate::platform_impl::x11::{
    mkdid, mkwid, util, CookieResultExt, Device, DeviceInfo, Dnd, DndState, ImeReceiver,
    ScrollOrientation, UnownedWindow, WindowId,
};

/// The maximum amount of X modifiers to replay.
pub const MAX_MOD_REPLAY_LEN: usize = 32;

/// The X11 documentation states: "Keycodes lie in the inclusive range `[8, 255]`".
const KEYCODE_OFFSET: u8 = 8;

#[derive(Debug)]
pub struct EventProcessor {
    pub dnd: Dnd,
    pub ime_receiver: ImeReceiver,
    pub ime_event_receiver: ImeEventReceiver,
    pub randr_event_offset: u8,
    pub devices: RefCell<HashMap<DeviceId, Device>>,
    pub xi2ext: ExtensionInformation,
    pub xkbext: ExtensionInformation,
    pub target: ActiveEventLoop,
    pub xkb_context: Context,
    // Number of touch events currently in progress
    pub num_touch: u32,
    // This is the last pressed key that is repeatable (if it hasn't been
    // released).
    //
    // Used to detect key repeats.
    pub held_key_press: Option<u32>,
    pub first_touch: Option<u32>,
    // Currently focused window belonging to this process
    pub active_window: Option<xproto::Window>,
    /// Latest modifiers we've sent for the user to trigger change in event.
    pub modifiers: Cell<ModifiersState>,
    // Track modifiers based on keycodes. NOTE: that serials generally don't work for tracking
    // since they are not unique and could be duplicated in case of sequence of key events is
    // delivered at near the same time.
    pub xfiltered_modifiers: VecDeque<u8>,
    pub xmodmap: util::ModifierKeymap,
    pub is_composing: bool,
}

impl EventProcessor {
    pub(crate) fn process_event(&mut self, xev: &mut XEvent, app: &mut dyn ApplicationHandler) {
        self.process_xevent(xev, app);

        // Handle IME requests.
        while let Ok(request) = self.ime_receiver.try_recv() {
            let ime = match self.target.ime.as_mut() {
                Some(ime) => ime,
                None => continue,
            };

            let ime = ime.get_mut();
            match request {
                ImeRequest::Area(window_id, x, y, w, h) => {
                    ime.send_xim_area(window_id, x, y, w, h);
                },
                ImeRequest::Allow(window_id, allowed) => {
                    ime.set_ime_allowed(window_id, allowed);
                },
            }
        }

        // Drain IME events.
        while let Ok((window, event)) = self.ime_event_receiver.try_recv() {
            let window_id = mkwid(window as xproto::Window);
            let event = match event {
                ImeEvent::Enabled => WindowEvent::Ime(Ime::Enabled),
                ImeEvent::Start => {
                    self.is_composing = true;
                    WindowEvent::Ime(Ime::Preedit("".to_owned(), None))
                },
                ImeEvent::Update(text, position) if self.is_composing => {
                    WindowEvent::Ime(Ime::Preedit(text, Some((position, position))))
                },
                ImeEvent::End => {
                    self.is_composing = false;
                    // Issue empty preedit on `Done`.
                    WindowEvent::Ime(Ime::Preedit(String::new(), None))
                },
                ImeEvent::Disabled => {
                    self.is_composing = false;
                    WindowEvent::Ime(Ime::Disabled)
                },
                _ => continue,
            };

            app.window_event(&self.target, window_id, event);
        }
    }

    /// XFilterEvent tells us when an event has been discarded by the input method.
    /// Specifically, this involves all of the KeyPress events in compose/pre-edit sequences,
    /// along with an extra copy of the KeyRelease events. This also prevents backspace and
    /// arrow keys from being detected twice.
    #[must_use]
    fn filter_event(&mut self, xev: &mut XEvent) -> bool {
        unsafe {
            (self.target.xconn.xlib.XFilterEvent)(xev, {
                let xev: &XAnyEvent = xev.as_ref();
                xev.window
            }) == xlib::True
        }
    }

    fn process_xevent(&mut self, xev: &mut XEvent, app: &mut dyn ApplicationHandler) {
        let event_type = xev.get_type();

        // If we have IME disabled, don't try to `filter_event`, since only IME can consume them
        // and forward back. This is not desired for e.g. games since some IMEs may delay the input
        // and game can toggle IME back when e.g. typing into some field where latency won't really
        // matter.
        let filtered = if event_type == xlib::KeyPress || event_type == xlib::KeyRelease {
            let ime = self.target.ime.as_ref();
            let window = self.active_window.map(|window| window as XWindow);
            let forward_to_ime = ime
                .and_then(|ime| window.map(|window| ime.borrow().is_ime_allowed(window)))
                .unwrap_or(false);

            let filtered = forward_to_ime && self.filter_event(xev);
            if filtered {
                let xev: &XKeyEvent = xev.as_ref();
                if self.xmodmap.is_modifier(xev.keycode as u8) {
                    // Don't grow the buffer past the `MAX_MOD_REPLAY_LEN`. This could happen
                    // when the modifiers are consumed entirely.
                    if self.xfiltered_modifiers.len() == MAX_MOD_REPLAY_LEN {
                        self.xfiltered_modifiers.pop_back();
                    }
                    self.xfiltered_modifiers.push_front(xev.keycode as u8);
                }
            }

            filtered
        } else {
            self.filter_event(xev)
        };

        // Don't process event if it was filtered.
        if filtered {
            return;
        }

        match event_type {
            xlib::ClientMessage => self.client_message(xev.as_ref(), app),
            xlib::SelectionNotify => self.selection_notify(xev.as_ref(), app),
            xlib::ConfigureNotify => self.configure_notify(xev.as_ref(), app),
            xlib::ReparentNotify => self.reparent_notify(xev.as_ref()),
            xlib::MapNotify => self.map_notify(xev.as_ref(), app),
            xlib::DestroyNotify => self.destroy_notify(xev.as_ref(), app),
            xlib::PropertyNotify => self.property_notify(xev.as_ref(), app),
            xlib::VisibilityNotify => self.visibility_notify(xev.as_ref(), app),
            xlib::Expose => self.expose(xev.as_ref()),
            // Note that in compose/pre-edit sequences, we'll always receive KeyRelease events.
            ty @ xlib::KeyPress | ty @ xlib::KeyRelease => {
                let state = if ty == xlib::KeyPress {
                    ElementState::Pressed
                } else {
                    ElementState::Released
                };

                self.xinput_key_input(xev.as_mut(), state, app);
            },
            xlib::GenericEvent => {
                let xev: GenericEventCookie =
                    match GenericEventCookie::from_event(self.target.xconn.clone(), *xev) {
                        Some(xev) if xev.extension() == self.xi2ext.major_opcode => xev,
                        _ => return,
                    };

                let evtype = xev.evtype();

                match evtype {
                    ty @ xinput2::XI_ButtonPress | ty @ xinput2::XI_ButtonRelease => {
                        let state = if ty == xinput2::XI_ButtonPress {
                            ElementState::Pressed
                        } else {
                            ElementState::Released
                        };

                        let xev: &XIDeviceEvent = unsafe { xev.as_event() };
                        self.update_mods_from_xinput2_event(&xev.mods, &xev.group, false, app);
                        self.xinput2_button_input(xev, state, app);
                    },
                    xinput2::XI_Motion => {
                        let xev: &XIDeviceEvent = unsafe { xev.as_event() };
                        self.update_mods_from_xinput2_event(&xev.mods, &xev.group, false, app);
                        self.xinput2_mouse_motion(xev, app);
                    },
                    xinput2::XI_Enter => {
                        let xev: &XIEnterEvent = unsafe { xev.as_event() };
                        self.xinput2_mouse_enter(xev, app);
                    },
                    xinput2::XI_Leave => {
                        let xev: &XILeaveEvent = unsafe { xev.as_event() };
                        self.update_mods_from_xinput2_event(&xev.mods, &xev.group, false, app);
                        self.xinput2_mouse_left(xev, app);
                    },
                    xinput2::XI_FocusIn => {
                        let xev: &XIFocusInEvent = unsafe { xev.as_event() };
                        self.xinput2_focused(xev, app);
                    },
                    xinput2::XI_FocusOut => {
                        let xev: &XIFocusOutEvent = unsafe { xev.as_event() };
                        self.xinput2_unfocused(xev, app);
                    },
                    xinput2::XI_TouchBegin | xinput2::XI_TouchUpdate | xinput2::XI_TouchEnd => {
                        let xev: &XIDeviceEvent = unsafe { xev.as_event() };
                        self.xinput2_touch(xev, evtype, app);
                    },
                    xinput2::XI_RawButtonPress | xinput2::XI_RawButtonRelease => {
                        let state = match evtype {
                            xinput2::XI_RawButtonPress => ElementState::Pressed,
                            xinput2::XI_RawButtonRelease => ElementState::Released,
                            _ => unreachable!(),
                        };

                        let xev: &XIRawEvent = unsafe { xev.as_event() };
                        self.xinput2_raw_button_input(xev, state, app);
                    },
                    xinput2::XI_RawMotion => {
                        let xev: &XIRawEvent = unsafe { xev.as_event() };
                        self.xinput2_raw_mouse_motion(xev, app);
                    },
                    xinput2::XI_RawKeyPress | xinput2::XI_RawKeyRelease => {
                        let state = match evtype {
                            xinput2::XI_RawKeyPress => ElementState::Pressed,
                            xinput2::XI_RawKeyRelease => ElementState::Released,
                            _ => unreachable!(),
                        };

                        let xev: &xinput2::XIRawEvent = unsafe { xev.as_event() };
                        self.xinput2_raw_key_input(xev, state, app);
                    },

                    xinput2::XI_HierarchyChanged => {
                        let xev: &XIHierarchyEvent = unsafe { xev.as_event() };
                        self.xinput2_hierarchy_changed(xev);
                    },
                    _ => {},
                }
            },
            _ => {
                if event_type == self.xkbext.first_event as _ {
                    let xev: &XkbAnyEvent = unsafe { &*(xev as *const _ as *const XkbAnyEvent) };
                    self.xkb_event(xev, app);
                }
                if event_type == self.randr_event_offset as c_int {
                    self.process_dpi_change(app);
                }
            },
        }
    }

    pub fn poll(&self) -> bool {
        unsafe { (self.target.xconn.xlib.XPending)(self.target.xconn.display) != 0 }
    }

    pub unsafe fn poll_one_event(&mut self, event_ptr: *mut XEvent) -> bool {
        // This function is used to poll and remove a single event
        // from the Xlib event queue in a non-blocking, atomic way.
        // XCheckIfEvent is non-blocking and removes events from queue.
        // XNextEvent can't be used because it blocks while holding the
        // global Xlib mutex.
        // XPeekEvent does not remove events from the queue.
        unsafe extern "C" fn predicate(
            _display: *mut XDisplay,
            _event: *mut XEvent,
            _arg: *mut c_char,
        ) -> c_int {
            // This predicate always returns "true" (1) to accept all events
            1
        }

        unsafe {
            (self.target.xconn.xlib.XCheckIfEvent)(
                self.target.xconn.display,
                event_ptr,
                Some(predicate),
                std::ptr::null_mut(),
            ) != 0
        }
    }

    pub fn init_device(&self, device: xinput::DeviceId) {
        let mut devices = self.devices.borrow_mut();
        if let Some(info) = DeviceInfo::get(&self.target.xconn, device as _) {
            for info in info.iter() {
                devices.insert(mkdid(info.deviceid as xinput::DeviceId), Device::new(info));
            }
        }
    }

    pub fn with_window<F, Ret>(&self, window_id: xproto::Window, callback: F) -> Option<Ret>
    where
        F: Fn(&Arc<UnownedWindow>) -> Ret,
    {
        let mut deleted = false;
        let window_id = WindowId::from_raw(window_id as _);
        let result = self
            .target
            .windows
            .borrow()
            .get(&window_id)
            .and_then(|window| {
                let arc = window.upgrade();
                deleted = arc.is_none();
                arc
            })
            .map(|window| callback(&window));

        if deleted {
            // Garbage collection
            self.target.windows.borrow_mut().remove(&window_id);
        }

        result
    }

    fn client_message(&mut self, xev: &XClientMessageEvent, app: &mut dyn ApplicationHandler) {
        let atoms = self.target.xconn.atoms();

        let window = xev.window as xproto::Window;
        let window_id = mkwid(window);

        if xev.data.get_long(0) as xproto::Atom == self.target.wm_delete_window {
            app.window_event(&self.target, window_id, WindowEvent::CloseRequested);
            return;
        }

        if xev.data.get_long(0) as xproto::Atom == self.target.net_wm_ping {
            let client_msg = xproto::ClientMessageEvent {
                response_type: xproto::CLIENT_MESSAGE_EVENT,
                format: xev.format as _,
                sequence: xev.serial as _,
                window: self.target.root,
                type_: xev.message_type as _,
                data: xproto::ClientMessageData::from({
                    let [a, b, c, d, e]: [c_long; 5] = xev.data.as_longs().try_into().unwrap();
                    [a as u32, b as u32, c as u32, d as u32, e as u32]
                }),
            };

            self.target
                .xconn
                .xcb_connection()
                .send_event(
                    false,
                    self.target.root,
                    xproto::EventMask::SUBSTRUCTURE_NOTIFY
                        | xproto::EventMask::SUBSTRUCTURE_REDIRECT,
                    client_msg.serialize(),
                )
                .expect_then_ignore_error("Failed to send `ClientMessage` event.");
            return;
        }

        if xev.data.get_long(0) as xproto::Atom == self.target.net_wm_sync_request {
            let sync_counter_id = match self
                .with_window(xev.window as xproto::Window, |window| window.sync_counter_id())
            {
                Some(Some(sync_counter_id)) => sync_counter_id.get(),
                _ => return,
            };

            #[cfg(target_pointer_width = "32")]
            let (lo, hi) =
                (bytemuck::cast::<c_long, u32>(xev.data.get_long(2)), xev.data.get_long(3));

            #[cfg(not(target_pointer_width = "32"))]
            let (lo, hi) = (
                (xev.data.get_long(2) & 0xffffffff) as u32,
                bytemuck::cast::<u32, i32>((xev.data.get_long(3) & 0xffffffff) as u32),
            );

            self.target
                .xconn
                .xcb_connection()
                .sync_set_counter(sync_counter_id, Int64 { lo, hi })
                .expect_then_ignore_error("Failed to set XSync counter.");

            return;
        }

        if xev.message_type == atoms[XdndEnter] as c_ulong {
            let source_window = xev.data.get_long(0) as xproto::Window;
            let flags = xev.data.get_long(1);
            let version = flags >> 24;
            self.dnd.version = Some(version);
            let has_more_types = flags - (flags & (c_long::MAX - 1)) == 1;
            if !has_more_types {
                let type_list = vec![
                    xev.data.get_long(2) as xproto::Atom,
                    xev.data.get_long(3) as xproto::Atom,
                    xev.data.get_long(4) as xproto::Atom,
                ];
                self.dnd.type_list = Some(type_list);
            } else if let Ok(more_types) = unsafe { self.dnd.get_type_list(source_window) } {
                self.dnd.type_list = Some(more_types);
            }
            return;
        }

        if xev.message_type == atoms[XdndPosition] as c_ulong {
            // This event occurs every time the mouse moves while a file's being dragged
            // over our window. We emit HoveredFile in response; while the macOS backend
            // does that upon a drag entering, XDND doesn't have access to the actual drop
            // data until this event. For parity with other platforms, we only emit
            // `HoveredFile` the first time, though if winit's API is later extended to
            // supply position updates with `HoveredFile` or another event, implementing
            // that here would be trivial.

            let source_window = xev.data.get_long(0) as xproto::Window;

            // https://www.freedesktop.org/wiki/Specifications/XDND/#xdndposition
            // Note that coordinates are in "desktop space", not "window space"
            // (in X11 parlance, they're root window coordinates)
            let packed_coordinates = xev.data.get_long(2);
            let x = (packed_coordinates >> 16) as i16;
            let y = (packed_coordinates & 0xffff) as i16;

            let coords = self
                .target
                .xconn
                .translate_coords(self.target.root, window, x, y)
                .expect("Failed to translate window coordinates");
            self.dnd.position = PhysicalPosition::new(coords.dst_x as f64, coords.dst_y as f64);

            // By our own state flow, `version` should never be `None` at this point.
            let version = self.dnd.version.unwrap_or(5);

            // Action is specified in versions 2 and up, though we don't need it anyway.
            // let action = xev.data.get_long(4);

            let accepted = if let Some(ref type_list) = self.dnd.type_list {
                type_list.contains(&atoms[TextUriList])
            } else {
                false
            };

            if !accepted {
                unsafe {
                    self.dnd
                        .send_status(window, source_window, DndState::Rejected)
                        .expect("Failed to send `XdndStatus` message.");
                }
                self.dnd.reset();
                return;
            }

            self.dnd.source_window = Some(source_window);
            let time = if version == 0 {
                // In version 0, time isn't specified
                x11rb::CURRENT_TIME
            } else {
                xev.data.get_long(3) as xproto::Timestamp
            };

            // Log this timestamp.
            self.target.xconn.set_timestamp(time);

            // This results in the `SelectionNotify` event below
            unsafe {
                self.dnd.convert_selection(window, time);
            }

            unsafe {
                self.dnd
                    .send_status(window, source_window, DndState::Accepted)
                    .expect("Failed to send `XdndStatus` message.");
            }
            return;
        }

        if xev.message_type == atoms[XdndDrop] as c_ulong {
            let (source_window, state) = if let Some(source_window) = self.dnd.source_window {
                if let Some(Ok(ref path_list)) = self.dnd.result {
                    let event = WindowEvent::DragDropped {
                        paths: path_list.iter().map(Into::into).collect(),
                        position: self.dnd.position,
                    };
                    app.window_event(&self.target, window_id, event);
                }
                (source_window, DndState::Accepted)
            } else {
                // `source_window` won't be part of our DND state if we already rejected the drop in
                // our `XdndPosition` handler.
                let source_window = xev.data.get_long(0) as xproto::Window;
                (source_window, DndState::Rejected)
            };

            unsafe {
                self.dnd
                    .send_finished(window, source_window, state)
                    .expect("Failed to send `XdndFinished` message.");
            }

            self.dnd.reset();
            return;
        }

        if xev.message_type == atoms[XdndLeave] as c_ulong {
            if self.dnd.dragging {
                let event = WindowEvent::DragLeft { position: Some(self.dnd.position) };
                app.window_event(&self.target, window_id, event);
            }
            self.dnd.reset();
        }
    }

    fn selection_notify(&mut self, xev: &XSelectionEvent, app: &mut dyn ApplicationHandler) {
        let atoms = self.target.xconn.atoms();

        let window = xev.requestor as xproto::Window;
        let window_id = mkwid(window);

        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        if xev.property != atoms[XdndSelection] as c_ulong {
            return;
        }

        // This is where we receive data from drag and drop
        self.dnd.result = None;
        if let Ok(mut data) = unsafe { self.dnd.read_data(window) } {
            let parse_result = self.dnd.parse_data(&mut data);

            if let Ok(ref path_list) = parse_result {
                let event = if self.dnd.dragging {
                    WindowEvent::DragMoved { position: self.dnd.position }
                } else {
                    let paths = path_list.iter().map(Into::into).collect();
                    self.dnd.dragging = true;
                    WindowEvent::DragEntered { paths, position: self.dnd.position }
                };

                app.window_event(&self.target, window_id, event);
            }

            self.dnd.result = Some(parse_result);
        }
    }

    fn configure_notify(&self, xev: &XConfigureEvent, app: &mut dyn ApplicationHandler) {
        let xwindow = xev.window as xproto::Window;
        let window_id = mkwid(xwindow);

        let window = match self.with_window(xwindow, Arc::clone) {
            Some(window) => window,
            None => return,
        };

        // So apparently...
        // `XSendEvent` (synthetic `ConfigureNotify`) -> position relative to root
        // `XConfigureNotify` (real `ConfigureNotify`) -> position relative to parent
        // https://tronche.com/gui/x/icccm/sec-4.html#s-4.1.5
        // We don't want to send `Moved` when this is false, since then every `SurfaceResized`
        // (whether the window moved or not) is accompanied by an extraneous `Moved` event
        // that has a position relative to the parent window.
        let is_synthetic = xev.send_event == xlib::True;

        // These are both in physical space.
        let new_surface_size = (xev.width as u32, xev.height as u32);
        let new_inner_position = (xev.x, xev.y);

        let (mut resized, moved) = {
            let mut shared_state_lock = window.shared_state_lock();

            let resized = util::maybe_change(&mut shared_state_lock.size, new_surface_size);
            let moved = if is_synthetic {
                util::maybe_change(&mut shared_state_lock.inner_position, new_inner_position)
            } else {
                // Detect when frame extents change.
                // Since this isn't synthetic, as per the notes above, this position is relative to
                // the parent window.
                let rel_parent = new_inner_position;
                if util::maybe_change(&mut shared_state_lock.inner_position_rel_parent, rel_parent)
                {
                    // This ensures we process the next `Moved`.
                    shared_state_lock.inner_position = None;
                    // Extra insurance against stale frame extents.
                    shared_state_lock.frame_extents = None;
                }
                false
            };
            (resized, moved)
        };

        let position = window.shared_state_lock().position;

        let new_outer_position = if let (Some(position), false) = (position, moved) {
            position
        } else {
            let mut shared_state_lock = window.shared_state_lock();

            // We need to convert client area position to window position.
            let frame_extents =
                shared_state_lock.frame_extents.as_ref().cloned().unwrap_or_else(|| {
                    let frame_extents =
                        self.target.xconn.get_frame_extents_heuristic(xwindow, self.target.root);
                    shared_state_lock.frame_extents = Some(frame_extents.clone());
                    frame_extents
                });
            let outer =
                frame_extents.inner_pos_to_outer(new_inner_position.0, new_inner_position.1);
            shared_state_lock.position = Some(outer);

            // Unlock shared state to prevent deadlock in callback below
            drop(shared_state_lock);

            if moved {
                app.window_event(&self.target, window_id, WindowEvent::Moved(outer.into()));
            }
            outer
        };

        if is_synthetic {
            let mut shared_state_lock = window.shared_state_lock();
            // If we don't use the existing adjusted value when available, then the user can screw
            // up the resizing by dragging across monitors *without* dropping the
            // window.
            let (width, height) =
                shared_state_lock.dpi_adjusted.unwrap_or((xev.width as u32, xev.height as u32));

            let last_scale_factor = shared_state_lock.last_monitor.scale_factor;
            let new_scale_factor = {
                let window_rect = util::AaRect::new(new_outer_position, new_surface_size);
                let monitor = self
                    .target
                    .xconn
                    .get_monitor_for_window(Some(window_rect))
                    .expect("Failed to find monitor for window");

                if monitor.is_dummy() {
                    // Avoid updating monitor using a dummy monitor handle
                    last_scale_factor
                } else {
                    shared_state_lock.last_monitor = monitor.clone();
                    monitor.scale_factor
                }
            };
            if last_scale_factor != new_scale_factor {
                let (new_width, new_height) = window.adjust_for_dpi(
                    last_scale_factor,
                    new_scale_factor,
                    width,
                    height,
                    &shared_state_lock,
                );

                let old_surface_size = PhysicalSize::new(width, height);
                let new_surface_size = PhysicalSize::new(new_width, new_height);

                // Unlock shared state to prevent deadlock in callback below
                drop(shared_state_lock);

                let surface_size = Arc::new(Mutex::new(new_surface_size));
                app.window_event(&self.target, window_id, WindowEvent::ScaleFactorChanged {
                    scale_factor: new_scale_factor,
                    surface_size_writer: SurfaceSizeWriter::new(Arc::downgrade(&surface_size)),
                });

                let new_surface_size = *surface_size.lock().unwrap();
                drop(surface_size);

                if new_surface_size != old_surface_size {
                    window.request_surface_size_physical(
                        new_surface_size.width,
                        new_surface_size.height,
                    );
                    window.shared_state_lock().dpi_adjusted = Some(new_surface_size.into());
                    // if the DPI factor changed, force a resize event to ensure the logical
                    // size is computed with the right DPI factor
                    resized = true;
                }
            }
        }

        // NOTE: Ensure that the lock is dropped before handling the resized and
        // sending the event back to user.
        let hittest = {
            let mut shared_state_lock = window.shared_state_lock();
            let hittest = shared_state_lock.cursor_hittest;

            // This is a hack to ensure that the DPI adjusted resize is actually
            // applied on all WMs. KWin doesn't need this, but Xfwm does. The hack
            // should not be run on other WMs, since tiling WMs constrain the window
            // size, making the resize fail. This would cause an endless stream of
            // XResizeWindow requests, making Xorg, the winit client, and the WM
            // consume 100% of CPU.
            if let Some(adjusted_size) = shared_state_lock.dpi_adjusted {
                if new_surface_size == adjusted_size || !util::wm_name_is_one_of(&["Xfwm4"]) {
                    // When this finally happens, the event will not be synthetic.
                    shared_state_lock.dpi_adjusted = None;
                } else {
                    // Unlock shared state to prevent deadlock in callback below
                    drop(shared_state_lock);
                    window.request_surface_size_physical(adjusted_size.0, adjusted_size.1);
                }
            }

            hittest
        };

        // Reload hittest.
        if hittest.unwrap_or(false) {
            let _ = window.set_cursor_hittest(true);
        }

        if resized {
            let event = WindowEvent::SurfaceResized(new_surface_size.into());
            app.window_event(&self.target, window_id, event);
        }
    }

    /// This is generally a reliable way to detect when the window manager's been
    /// replaced, though this event is only fired by reparenting window managers
    /// (which is almost all of them). Failing to correctly update WM info doesn't
    /// really have much impact, since on the WMs affected (xmonad, dwm, etc.) the only
    /// effect is that we waste some time trying to query unsupported properties.
    fn reparent_notify(&self, xev: &XReparentEvent) {
        self.target.xconn.update_cached_wm_info(self.target.root);

        self.with_window(xev.window as xproto::Window, |window| {
            window.invalidate_cached_frame_extents();
        });
    }

    fn map_notify(&self, xev: &XMapEvent, app: &mut dyn ApplicationHandler) {
        let window = xev.window as xproto::Window;
        let window_id = mkwid(window);

        // NOTE: Re-issue the focus state when mapping the window.
        //
        // The purpose of it is to deliver initial focused state of the newly created
        // window, given that we can't rely on `CreateNotify`, due to it being not
        // sent.
        let focus = self.with_window(window, |window| window.has_focus()).unwrap_or_default();
        app.window_event(&self.target, window_id, WindowEvent::Focused(focus));
    }

    fn destroy_notify(&self, xev: &XDestroyWindowEvent, app: &mut dyn ApplicationHandler) {
        let window = xev.window as xproto::Window;
        let window_id = mkwid(window);

        // In the event that the window's been destroyed without being dropped first, we
        // cleanup again here.
        self.target.windows.borrow_mut().remove(&WindowId::from_raw(window as _));

        // Since all XIM stuff needs to happen from the same thread, we destroy the input
        // context here instead of when dropping the window.
        if let Some(ime) = self.target.ime.as_ref() {
            ime.borrow_mut()
                .remove_context(window as XWindow)
                .expect("Failed to destroy input context");
        }

        app.window_event(&self.target, window_id, WindowEvent::Destroyed);
    }

    fn property_notify(&mut self, xev: &XPropertyEvent, app: &mut dyn ApplicationHandler) {
        let atoms = self.target.x_connection().atoms();
        let atom = xev.atom as xproto::Atom;

        if atom == xproto::Atom::from(xproto::AtomEnum::RESOURCE_MANAGER)
            || atom == atoms[_XSETTINGS_SETTINGS]
        {
            self.process_dpi_change(app);
        }
    }

    fn visibility_notify(&self, xev: &XVisibilityEvent, app: &mut dyn ApplicationHandler) {
        let xwindow = xev.window as xproto::Window;

        let window_id = mkwid(xwindow);
        let event = WindowEvent::Occluded(xev.state == xlib::VisibilityFullyObscured);
        app.window_event(&self.target, window_id, event);

        self.with_window(xwindow, |window| {
            window.visibility_notify();
        });
    }

    fn expose(&self, xev: &XExposeEvent) {
        // Multiple Expose events may be received for subareas of a window.
        // We issue `RedrawRequested` only for the last event of such a series.
        if xev.count == 0 {
            let window = xev.window as xproto::Window;
            let window_id = mkwid(window);
            self.target.redraw_sender.send(window_id);
        }
    }

    fn xinput_key_input(
        &mut self,
        xev: &mut XKeyEvent,
        state: ElementState,
        app: &mut dyn ApplicationHandler,
    ) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        let window = match self.active_window {
            Some(window) => window,
            None => return,
        };

        let window_id = mkwid(window);

        let keycode = xev.keycode as _;

        // Update state to track key repeats and determine whether this key was a repeat.
        //
        // Note, when a key is held before focusing on this window the first
        // (non-synthetic) event will not be flagged as a repeat (also note that the
        // synthetic press event that is generated before this when the window gains focus
        // will also not be flagged as a repeat).
        //
        // Only keys that can repeat should change the held_key_press state since a
        // continuously held repeatable key may continue repeating after the press of a
        // non-repeatable key.
        let key_repeats =
            self.xkb_context.keymap_mut().map(|k| k.key_repeats(keycode)).unwrap_or(false);
        let repeat = if key_repeats {
            let is_latest_held = self.held_key_press == Some(keycode);

            if state == ElementState::Pressed {
                self.held_key_press = Some(keycode);
                is_latest_held
            } else {
                // Check that the released key is the latest repeatable key that has been
                // pressed, since repeats will continue for the latest key press if a
                // different previously pressed key is released.
                if is_latest_held {
                    self.held_key_press = None;
                }
                false
            }
        } else {
            false
        };

        // NOTE: When the modifier was captured by the XFilterEvents the modifiers for the modifier
        // itself are out of sync due to XkbState being delivered before XKeyEvent, since it's
        // being replayed by the XIM, thus we should replay ourselves.
        let replay = if let Some(position) =
            self.xfiltered_modifiers.iter().rev().position(|&s| s == xev.keycode as u8)
        {
            // We don't have to replay modifiers pressed before the current event if some events
            // were not forwarded to us, since their state is irrelevant.
            self.xfiltered_modifiers.resize(self.xfiltered_modifiers.len() - 1 - position, 0);
            true
        } else {
            false
        };

        // Always update the modifiers when we're not replaying.
        if !replay {
            self.update_mods_from_core_event(window_id, xev.state as u16, app);
        }

        if keycode != 0 && !self.is_composing {
            // Don't alter the modifiers state from replaying.
            if replay {
                self.send_synthic_modifier_from_core(window_id, xev.state as u16, app);
            }

            if let Some(mut key_processor) = self.xkb_context.key_context() {
                let event = key_processor.process_key_event(keycode, state, repeat);
                let event =
                    WindowEvent::KeyboardInput { device_id: None, event, is_synthetic: false };
                app.window_event(&self.target, window_id, event);
            }

            // Restore the client's modifiers state after replay.
            if replay {
                self.send_modifiers(window_id, self.modifiers.get(), true, app);
            }

            return;
        }

        if let Some(ic) =
            self.target.ime.as_ref().and_then(|ime| ime.borrow().get_context(window as XWindow))
        {
            let written = self.target.xconn.lookup_utf8(ic, xev);
            if !written.is_empty() {
                let event = WindowEvent::Ime(Ime::Preedit(String::new(), None));
                app.window_event(&self.target, window_id, event);

                let event = WindowEvent::Ime(Ime::Commit(written));
                self.is_composing = false;
                app.window_event(&self.target, window_id, event);
            }
        }
    }

    fn send_synthic_modifier_from_core(
        &mut self,
        window_id: winit_core::window::WindowId,
        state: u16,
        app: &mut dyn ApplicationHandler,
    ) {
        let keymap = match self.xkb_context.keymap_mut() {
            Some(keymap) => keymap,
            None => return,
        };

        let xcb = self.target.xconn.xcb_connection().get_raw_xcb_connection();

        // Use synthetic state since we're replaying the modifier. The user modifier state
        // will be restored later.
        let mut xkb_state = match XkbState::new_x11(xcb, keymap) {
            Some(xkb_state) => xkb_state,
            None => return,
        };

        let mask = self.xkb_mod_mask_from_core(state);
        xkb_state.update_modifiers(mask, 0, 0, 0, 0, Self::core_keyboard_group(state));
        let mods: ModifiersState = xkb_state.modifiers().into();

        let event = WindowEvent::ModifiersChanged(mods.into());
        app.window_event(&self.target, window_id, event);
    }

    fn xinput2_button_input(
        &self,
        event: &XIDeviceEvent,
        state: ElementState,
        app: &mut dyn ApplicationHandler,
    ) {
        let window_id = mkwid(event.event as xproto::Window);
        let device_id = Some(mkdid(event.deviceid as xinput::DeviceId));

        // Set the timestamp.
        self.target.xconn.set_timestamp(event.time as xproto::Timestamp);

        // Deliver multi-touch events instead of emulated mouse events.
        if (event.flags & xinput2::XIPointerEmulated) != 0 {
            return;
        }

        let position = PhysicalPosition::new(event.event_x, event.event_y);

        let event = match event.detail as u32 {
            xlib::Button1 => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Left.into(),
            },
            xlib::Button2 => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Middle.into(),
            },

            xlib::Button3 => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Right.into(),
            },

            // Suppress emulated scroll wheel clicks, since we handle the real motion events for
            // those. In practice, even clicky scroll wheels appear to be reported by
            // evdev (and XInput2 in turn) as axis motion, so we don't otherwise
            // special-case these button presses.
            4..=7 => WindowEvent::MouseWheel {
                device_id,
                delta: match event.detail {
                    4 => MouseScrollDelta::LineDelta(0.0, 1.0),
                    5 => MouseScrollDelta::LineDelta(0.0, -1.0),
                    6 => MouseScrollDelta::LineDelta(1.0, 0.0),
                    7 => MouseScrollDelta::LineDelta(-1.0, 0.0),
                    _ => unreachable!(),
                },
                phase: TouchPhase::Moved,
            },
            8 => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Back.into(),
            },

            9 => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Forward.into(),
            },
            x => WindowEvent::PointerButton {
                device_id,
                primary: true,
                state,
                position,
                button: MouseButton::Other(x as u16).into(),
            },
        };

        app.window_event(&self.target, window_id, event);
    }

    fn xinput2_mouse_motion(&self, event: &XIDeviceEvent, app: &mut dyn ApplicationHandler) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(event.time as xproto::Timestamp);

        let device_id = Some(mkdid(event.deviceid as xinput::DeviceId));
        let window = event.event as xproto::Window;
        let window_id = mkwid(window);
        let new_cursor_pos = (event.event_x, event.event_y);

        let cursor_moved = self.with_window(window, |window| {
            let mut shared_state_lock = window.shared_state_lock();
            util::maybe_change(&mut shared_state_lock.cursor_pos, new_cursor_pos)
        });

        if cursor_moved == Some(true) {
            let position = PhysicalPosition::new(event.event_x, event.event_y);

            let event = WindowEvent::PointerMoved {
                device_id,
                primary: true,
                position,
                source: PointerSource::Mouse,
            };
            app.window_event(&self.target, window_id, event);
        } else if cursor_moved.is_none() {
            return;
        }

        // More gymnastics, for self.devices
        let mask = unsafe {
            slice::from_raw_parts(event.valuators.mask, event.valuators.mask_len as usize)
        };
        let mut devices = self.devices.borrow_mut();
        let physical_device = match devices.get_mut(&mkdid(event.sourceid as xinput::DeviceId)) {
            Some(device) => device,
            None => return,
        };

        let mut events = Vec::new();
        let mut value = event.valuators.values;
        for i in 0..event.valuators.mask_len * 8 {
            if !xinput2::XIMaskIsSet(mask, i) {
                continue;
            }

            let x = unsafe { *value };

            if let Some(&mut (_, ref mut info)) =
                physical_device.scroll_axes.iter_mut().find(|&&mut (axis, _)| axis == i as _)
            {
                let delta = (x - info.position) / info.increment;
                info.position = x;
                // X11 vertical scroll coordinates are opposite to winit's
                let delta = match info.orientation {
                    ScrollOrientation::Horizontal => {
                        MouseScrollDelta::LineDelta(-delta as f32, 0.0)
                    },
                    ScrollOrientation::Vertical => MouseScrollDelta::LineDelta(0.0, -delta as f32),
                };

                let event = WindowEvent::MouseWheel { device_id, delta, phase: TouchPhase::Moved };
                events.push(event);
            }

            value = unsafe { value.offset(1) };
        }

        for event in events {
            app.window_event(&self.target, window_id, event);
        }
    }

    fn xinput2_mouse_enter(&self, event: &XIEnterEvent, app: &mut dyn ApplicationHandler) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(event.time as xproto::Timestamp);

        let window = event.event as xproto::Window;
        let window_id = mkwid(window);
        let device_id = mkdid(event.deviceid as xinput::DeviceId);

        if let Some(all_info) = DeviceInfo::get(&self.target.xconn, super::ALL_DEVICES.into()) {
            let mut devices = self.devices.borrow_mut();
            for device_info in all_info.iter() {
                // The second expression is need for resetting to work correctly on i3, and
                // presumably some other WMs. On those, `XI_Enter` doesn't include the physical
                // device ID, so both `sourceid` and `deviceid` are the virtual device.
                if device_info.deviceid == event.sourceid
                    || device_info.attachment == event.sourceid
                {
                    let device_id = mkdid(device_info.deviceid as xinput::DeviceId);
                    if let Some(device) = devices.get_mut(&device_id) {
                        device.reset_scroll_position(device_info);
                    }
                }
            }
        }

        if self.window_exists(window) {
            let device_id = Some(device_id);
            let position = PhysicalPosition::new(event.event_x, event.event_y);

            let event = WindowEvent::PointerEntered {
                device_id,
                primary: true,
                position,
                kind: PointerKind::Mouse,
            };
            app.window_event(&self.target, window_id, event);
        }
    }

    fn xinput2_mouse_left(&self, event: &XILeaveEvent, app: &mut dyn ApplicationHandler) {
        let window = event.event as xproto::Window;

        // Set the timestamp.
        self.target.xconn.set_timestamp(event.time as xproto::Timestamp);

        // Leave, FocusIn, and FocusOut can be received by a window that's already
        // been destroyed, which the user presumably doesn't want to deal with.
        if self.window_exists(window) {
            let window_id = mkwid(window);
            let event = WindowEvent::PointerLeft {
                device_id: Some(mkdid(event.deviceid as xinput::DeviceId)),
                primary: true,
                position: Some(PhysicalPosition::new(event.event_x, event.event_y)),
                kind: PointerKind::Mouse,
            };
            app.window_event(&self.target, window_id, event);
        }
    }

    fn xinput2_focused(&mut self, xev: &XIFocusInEvent, app: &mut dyn ApplicationHandler) {
        let window = xev.event as xproto::Window;

        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        if let Some(ime) = self.target.ime.as_ref() {
            ime.borrow_mut().focus(xev.event).expect("Failed to focus input context");
        }

        if self.active_window == Some(window) {
            return;
        }

        self.active_window = Some(window);

        self.target.update_listen_device_events(true);

        let window_id = mkwid(window);
        let position = PhysicalPosition::new(xev.event_x, xev.event_y);

        if let Some(window) = self.with_window(window, Arc::clone) {
            window.shared_state_lock().has_focus = true;
        }

        app.window_event(&self.target, window_id, WindowEvent::Focused(true));

        // Issue key press events for all pressed keys
        Self::handle_pressed_keys(
            &self.target,
            window_id,
            ElementState::Pressed,
            &mut self.xkb_context,
            app,
        );

        self.update_mods_from_query(window_id, app);

        // The deviceid for this event is for a keyboard instead of a pointer,
        // so we have to do a little extra work.
        let device_id = self
            .devices
            .borrow()
            .get(&mkdid(xev.deviceid as xinput::DeviceId))
            .map(|device| mkdid(device.attachment as xinput::DeviceId));

        let event = WindowEvent::PointerMoved {
            device_id,
            primary: true,
            position,
            source: PointerSource::Mouse,
        };
        app.window_event(&self.target, window_id, event);
    }

    fn xinput2_unfocused(&mut self, xev: &XIFocusOutEvent, app: &mut dyn ApplicationHandler) {
        let window = xev.event as xproto::Window;

        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        if !self.window_exists(window) {
            return;
        }

        if let Some(ime) = self.target.ime.as_ref() {
            ime.borrow_mut().unfocus(xev.event).expect("Failed to unfocus input context");
        }

        if self.active_window.take() == Some(window) {
            let window_id = mkwid(window);

            self.target.update_listen_device_events(false);

            // Clear the modifiers when unfocusing the window.
            if let Some(xkb_state) = self.xkb_context.state_mut() {
                xkb_state.update_modifiers(0, 0, 0, 0, 0, 0);
                let mods = xkb_state.modifiers();
                self.send_modifiers(window_id, mods.into(), true, app);
            }

            // Issue key release events for all pressed keys
            Self::handle_pressed_keys(
                &self.target,
                window_id,
                ElementState::Released,
                &mut self.xkb_context,
                app,
            );

            // Clear this so detecting key repeats is consistently handled when the
            // window regains focus.
            self.held_key_press = None;

            if let Some(window) = self.with_window(window, Arc::clone) {
                window.shared_state_lock().has_focus = false;
            }

            app.window_event(&self.target, window_id, WindowEvent::Focused(false));
        }
    }

    fn xinput2_touch(&mut self, xev: &XIDeviceEvent, phase: i32, app: &mut dyn ApplicationHandler) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        let window = xev.event as xproto::Window;
        if self.window_exists(window) {
            let window_id = mkwid(window);
            let id = xev.detail as u32;
            let position = PhysicalPosition::new(xev.event_x, xev.event_y);

            // Mouse cursor position changes when touch events are received.
            // Only the first concurrently active touch ID moves the mouse cursor.
            let is_first_touch =
                is_first_touch(&mut self.first_touch, &mut self.num_touch, id, phase);
            if is_first_touch {
                let event = WindowEvent::PointerMoved {
                    device_id: None,
                    primary: true,
                    position: position.cast(),
                    source: PointerSource::Mouse,
                };
                app.window_event(&self.target, window_id, event);
            }

            let device_id = Some(mkdid(xev.deviceid as xinput::DeviceId));
            let finger_id = FingerId::from_raw(id as usize);

            match phase {
                xinput2::XI_TouchBegin => {
                    let event = WindowEvent::PointerEntered {
                        device_id,
                        primary: is_first_touch,
                        position,
                        kind: PointerKind::Touch(finger_id),
                    };
                    app.window_event(&self.target, window_id, event);
                    let event = WindowEvent::PointerButton {
                        device_id,
                        primary: is_first_touch,
                        state: ElementState::Pressed,
                        position,
                        button: ButtonSource::Touch { finger_id, force: None },
                    };
                    app.window_event(&self.target, window_id, event);
                },
                xinput2::XI_TouchUpdate => {
                    let event = WindowEvent::PointerMoved {
                        device_id,
                        primary: is_first_touch,
                        position,
                        source: PointerSource::Touch { finger_id, force: None },
                    };
                    app.window_event(&self.target, window_id, event);
                },
                xinput2::XI_TouchEnd => {
                    let event = WindowEvent::PointerButton {
                        device_id,
                        primary: is_first_touch,
                        state: ElementState::Released,
                        position,
                        button: ButtonSource::Touch { finger_id, force: None },
                    };
                    app.window_event(&self.target, window_id, event);
                    let event = WindowEvent::PointerLeft {
                        device_id,
                        primary: is_first_touch,
                        position: Some(position),
                        kind: PointerKind::Touch(finger_id),
                    };
                    app.window_event(&self.target, window_id, event);
                },
                _ => unreachable!(),
            }
        }
    }

    fn xinput2_raw_button_input(
        &self,
        xev: &XIRawEvent,
        state: ElementState,
        app: &mut dyn ApplicationHandler,
    ) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        if xev.flags & xinput2::XIPointerEmulated == 0 {
            let event = DeviceEvent::Button { state, button: xev.detail as u32 };
            app.device_event(&self.target, Some(mkdid(xev.deviceid as xinput::DeviceId)), event);
        }
    }

    fn xinput2_raw_mouse_motion(&self, xev: &XIRawEvent, app: &mut dyn ApplicationHandler) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        let did = Some(mkdid(xev.deviceid as xinput::DeviceId));

        let mask =
            unsafe { slice::from_raw_parts(xev.valuators.mask, xev.valuators.mask_len as usize) };
        let mut value = xev.raw_values;
        let mut mouse_delta = util::Delta::default();
        let mut scroll_delta = util::Delta::default();
        for i in 0..xev.valuators.mask_len * 8 {
            if !xinput2::XIMaskIsSet(mask, i) {
                continue;
            }
            let x = unsafe { value.read_unaligned() };

            // We assume that every XInput2 device with analog axes is a pointing device emitting
            // relative coordinates.
            match i {
                0 => mouse_delta.set_x(x),
                1 => mouse_delta.set_y(x),
                2 => scroll_delta.set_x(x as f32),
                3 => scroll_delta.set_y(x as f32),
                _ => {},
            }

            value = unsafe { value.offset(1) };
        }

        if let Some(mouse_delta) = mouse_delta.consume() {
            app.device_event(&self.target, did, DeviceEvent::PointerMotion { delta: mouse_delta });
        }

        if let Some(scroll_delta) = scroll_delta.consume() {
            let event = DeviceEvent::MouseWheel {
                delta: MouseScrollDelta::LineDelta(scroll_delta.0, scroll_delta.1),
            };
            app.device_event(&self.target, did, event);
        }
    }

    fn xinput2_raw_key_input(
        &mut self,
        xev: &XIRawEvent,
        state: ElementState,
        app: &mut dyn ApplicationHandler,
    ) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

        let device_id = Some(mkdid(xev.sourceid as xinput::DeviceId));
        let keycode = xev.detail as u32;
        if keycode < KEYCODE_OFFSET as u32 {
            return;
        }
        let physical_key = xkb::raw_keycode_to_physicalkey(keycode);

        let event = DeviceEvent::Key(RawKeyEvent { physical_key, state });
        app.device_event(&self.target, device_id, event);
    }

    fn xinput2_hierarchy_changed(&mut self, xev: &XIHierarchyEvent) {
        // Set the timestamp.
        self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);
        let infos = unsafe { slice::from_raw_parts(xev.info, xev.num_info as usize) };
        for info in infos {
            if 0 != info.flags & (xinput2::XISlaveAdded | xinput2::XIMasterAdded) {
                self.init_device(info.deviceid as xinput::DeviceId);
            } else if 0 != info.flags & (xinput2::XISlaveRemoved | xinput2::XIMasterRemoved) {
                let mut devices = self.devices.borrow_mut();
                devices.remove(&mkdid(info.deviceid as xinput::DeviceId));
            }
        }
    }

    fn xkb_event(&mut self, xev: &XkbAnyEvent, app: &mut dyn ApplicationHandler) {
        match xev.xkb_type {
            xlib::XkbNewKeyboardNotify => {
                let xev = unsafe { &*(xev as *const _ as *const xlib::XkbNewKeyboardNotifyEvent) };

                // Set the timestamp.
                self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

                let keycodes_changed_flag = 0x1;
                let geometry_changed_flag = 0x1 << 1;

                let keycodes_changed = util::has_flag(xev.changed, keycodes_changed_flag);
                let geometry_changed = util::has_flag(xev.changed, geometry_changed_flag);

                if xev.device == self.xkb_context.core_keyboard_id
                    && (keycodes_changed || geometry_changed)
                {
                    let xcb = self.target.xconn.xcb_connection().get_raw_xcb_connection();
                    self.xkb_context.set_keymap_from_x11(xcb);
                    self.xmodmap.reload_from_x_connection(&self.target.xconn);

                    let window_id = match self.active_window.map(super::mkwid) {
                        Some(window_id) => window_id,
                        None => return,
                    };

                    if let Some(state) = self.xkb_context.state_mut() {
                        let mods = state.modifiers().into();
                        self.send_modifiers(window_id, mods, true, app);
                    }
                }
            },
            xlib::XkbMapNotify => {
                let xcb = self.target.xconn.xcb_connection().get_raw_xcb_connection();
                self.xkb_context.set_keymap_from_x11(xcb);
                self.xmodmap.reload_from_x_connection(&self.target.xconn);
                let window_id = match self.active_window.map(super::mkwid) {
                    Some(window_id) => window_id,
                    None => return,
                };

                if let Some(state) = self.xkb_context.state_mut() {
                    let mods = state.modifiers().into();
                    self.send_modifiers(window_id, mods, true, app);
                }
            },
            xlib::XkbStateNotify => {
                let xev = unsafe { &*(xev as *const _ as *const xlib::XkbStateNotifyEvent) };

                // Set the timestamp.
                self.target.xconn.set_timestamp(xev.time as xproto::Timestamp);

                if let Some(state) = self.xkb_context.state_mut() {
                    state.update_modifiers(
                        xev.base_mods,
                        xev.latched_mods,
                        xev.locked_mods,
                        xev.base_group as u32,
                        xev.latched_group as u32,
                        xev.locked_group as u32,
                    );

                    let window_id = match self.active_window.map(super::mkwid) {
                        Some(window_id) => window_id,
                        None => return,
                    };

                    let mods = state.modifiers().into();
                    self.send_modifiers(window_id, mods, true, app);
                }
            },
            _ => {},
        }
    }

    pub(crate) fn update_mods_from_xinput2_event(
        &mut self,
        mods: &XIModifierState,
        group: &XIModifierState,
        force: bool,
        app: &mut dyn ApplicationHandler,
    ) {
        if let Some(state) = self.xkb_context.state_mut() {
            state.update_modifiers(
                mods.base as u32,
                mods.latched as u32,
                mods.locked as u32,
                group.base as u32,
                group.latched as u32,
                group.locked as u32,
            );

            // NOTE: we use active window since generally sub windows don't have keyboard input,
            // and winit assumes that unfocused window doesn't have modifiers.
            let window_id = match self.active_window.map(super::mkwid) {
                Some(window_id) => window_id,
                None => return,
            };

            let mods = state.modifiers();
            self.send_modifiers(window_id, mods.into(), force, app);
        }
    }

    fn update_mods_from_query(
        &mut self,
        window_id: winit_core::window::WindowId,
        app: &mut dyn ApplicationHandler,
    ) {
        let xkb_state = match self.xkb_context.state_mut() {
            Some(xkb_state) => xkb_state,
            None => return,
        };

        unsafe {
            let mut state: XkbStateRec = std::mem::zeroed();
            if (self.target.xconn.xlib.XkbGetState)(
                self.target.xconn.display,
                XkbId::USE_CORE_KBD.into(),
                &mut state,
            ) == xlib::True
            {
                xkb_state.update_modifiers(
                    state.base_mods as u32,
                    state.latched_mods as u32,
                    state.locked_mods as u32,
                    state.base_group as u32,
                    state.latched_group as u32,
                    state.locked_group as u32,
                );
            }
        }

        let mods = xkb_state.modifiers();
        self.send_modifiers(window_id, mods.into(), true, app)
    }

    pub(crate) fn update_mods_from_core_event(
        &mut self,
        window_id: winit_core::window::WindowId,
        state: u16,
        app: &mut dyn ApplicationHandler,
    ) {
        let xkb_mask = self.xkb_mod_mask_from_core(state);
        let xkb_state = match self.xkb_context.state_mut() {
            Some(xkb_state) => xkb_state,
            None => return,
        };

        // NOTE: this is inspired by Qt impl.
        let mut depressed = xkb_state.depressed_modifiers() & xkb_mask;
        let latched = xkb_state.latched_modifiers() & xkb_mask;
        let locked = xkb_state.locked_modifiers() & xkb_mask;
        // Set modifiers in depressed if they don't appear in any of the final masks.
        depressed |= !(depressed | latched | locked) & xkb_mask;

        xkb_state.update_modifiers(
            depressed,
            latched,
            locked,
            0,
            0,
            Self::core_keyboard_group(state),
        );

        let mods = xkb_state.modifiers();
        self.send_modifiers(window_id, mods.into(), false, app);
    }

    // Bits 13 and 14 report the state keyboard group.
    pub fn core_keyboard_group(state: u16) -> u32 {
        ((state >> 13) & 3) as u32
    }

    pub fn xkb_mod_mask_from_core(&mut self, state: u16) -> xkb_mod_mask_t {
        let mods_indices = match self.xkb_context.keymap_mut() {
            Some(keymap) => keymap.mods_indices(),
            None => return 0,
        };

        // Build the XKB modifiers from the regular state.
        let mut depressed = 0u32;
        if let Some(shift) = mods_indices.shift.filter(|_| ModMask::SHIFT.intersects(state)) {
            depressed |= 1 << shift;
        }
        if let Some(caps) = mods_indices.caps.filter(|_| ModMask::LOCK.intersects(state)) {
            depressed |= 1 << caps;
        }
        if let Some(ctrl) = mods_indices.ctrl.filter(|_| ModMask::CONTROL.intersects(state)) {
            depressed |= 1 << ctrl;
        }
        if let Some(alt) = mods_indices.alt.filter(|_| ModMask::M1.intersects(state)) {
            depressed |= 1 << alt;
        }
        if let Some(num) = mods_indices.num.filter(|_| ModMask::M2.intersects(state)) {
            depressed |= 1 << num;
        }
        if let Some(mod3) = mods_indices.mod3.filter(|_| ModMask::M3.intersects(state)) {
            depressed |= 1 << mod3;
        }
        if let Some(logo) = mods_indices.logo.filter(|_| ModMask::M4.intersects(state)) {
            depressed |= 1 << logo;
        }
        if let Some(mod5) = mods_indices.mod5.filter(|_| ModMask::M5.intersects(state)) {
            depressed |= 1 << mod5;
        }

        depressed
    }

    /// Send modifiers for the active window.
    ///
    /// The event won't be sent when the `modifiers` match the previously `sent` modifiers value,
    /// unless `force` is passed. The `force` should be passed when the active window changes.
    fn send_modifiers(
        &self,
        window_id: winit_core::window::WindowId,
        modifiers: ModifiersState,
        force: bool,
        app: &mut dyn ApplicationHandler,
    ) {
        // NOTE: Always update the modifiers to account for case when they've changed
        // and forced was `true`.
        if self.modifiers.replace(modifiers) != modifiers || force {
            let event = WindowEvent::ModifiersChanged(self.modifiers.get().into());
            app.window_event(&self.target, window_id, event);
        }
    }

    fn handle_pressed_keys(
        target: &ActiveEventLoop,
        window_id: winit_core::window::WindowId,
        state: ElementState,
        xkb_context: &mut Context,
        app: &mut dyn ApplicationHandler,
    ) {
        // Update modifiers state and emit key events based on which keys are currently pressed.
        let xcb = target.xconn.xcb_connection().get_raw_xcb_connection();

        let keymap = match xkb_context.keymap_mut() {
            Some(keymap) => keymap,
            None => return,
        };

        // Send the keys using the synthetic state to not alter the main state.
        let mut xkb_state = match XkbState::new_x11(xcb, keymap) {
            Some(xkb_state) => xkb_state,
            None => return,
        };
        let mut key_processor = match xkb_context.key_context_with_state(&mut xkb_state) {
            Some(key_processor) => key_processor,
            None => return,
        };

        for keycode in target.xconn.query_keymap().into_iter().filter(|k| *k >= KEYCODE_OFFSET) {
            let event = key_processor.process_key_event(keycode as u32, state, false);
            let event = WindowEvent::KeyboardInput { device_id: None, event, is_synthetic: true };
            app.window_event(target, window_id, event);
        }
    }

    fn process_dpi_change(&self, app: &mut dyn ApplicationHandler) {
        self.target.xconn.reload_database().expect("failed to reload Xft database");

        // In the future, it would be quite easy to emit monitor hotplug events.
        let prev_list = {
            let prev_list = self.target.xconn.invalidate_cached_monitor_list();
            match prev_list {
                Some(prev_list) => prev_list,
                None => return,
            }
        };

        let new_list = self.target.xconn.available_monitors().expect("Failed to get monitor list");
        for new_monitor in new_list {
            // Previous list may be empty, in case of disconnecting and
            // reconnecting the only one monitor. We still need to emit events in
            // this case.
            let maybe_prev_scale_factor = prev_list
                .iter()
                .find(|prev_monitor| prev_monitor.name == new_monitor.name)
                .map(|prev_monitor| prev_monitor.scale_factor);
            if Some(new_monitor.scale_factor) != maybe_prev_scale_factor {
                for window in self.target.windows.borrow().iter().filter_map(|(_, w)| w.upgrade()) {
                    window.refresh_dpi_for_monitor(
                        &new_monitor,
                        maybe_prev_scale_factor,
                        app,
                        &self.target,
                    )
                }
            }
        }
    }

    fn window_exists(&self, window_id: xproto::Window) -> bool {
        self.with_window(window_id, |_| ()).is_some()
    }
}

fn is_first_touch(first: &mut Option<u32>, num: &mut u32, id: u32, phase: i32) -> bool {
    match phase {
        xinput2::XI_TouchBegin => {
            if *num == 0 {
                *first = Some(id);
            }
            *num += 1;
        },
        xinput2::XI_TouchEnd => {
            if *first == Some(id) {
                *first = None;
            }
            *num = num.saturating_sub(1);
        },
        _ => (),
    }

    *first == Some(id)
}
