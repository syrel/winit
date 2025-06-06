use std::ffi::{c_void, OsString};
use std::os::windows::ffi::OsStringExt;
use std::path::PathBuf;
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};

use dpi::PhysicalPosition;
use tracing::debug;
use windows_sys::core::{GUID, HRESULT};
use windows_sys::Win32::Foundation::{DV_E_FORMATETC, HWND, POINT, POINTL, S_OK};
use windows_sys::Win32::Graphics::Gdi::ScreenToClient;
use windows_sys::Win32::System::Com::{DVASPECT_CONTENT, FORMATETC, TYMED_HGLOBAL};
use windows_sys::Win32::System::Ole::{CF_HDROP, DROPEFFECT_COPY, DROPEFFECT_NONE};
use windows_sys::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
use winit_core::event::WindowEvent;

use crate::platform_impl::platform::definitions::{
    IDataObject, IDataObjectVtbl, IDropTarget, IDropTargetVtbl, IUnknown, IUnknownVtbl,
};

#[repr(C)]
pub struct FileDropHandlerData {
    pub interface: IDropTarget,
    refcount: AtomicUsize,
    window: HWND,
    send_event: Box<dyn Fn(WindowEvent)>,
    cursor_effect: u32,
    valid: bool, /* If the currently hovered item is not valid there must not be any
                  * `DragLeft` emitted */
}

pub struct FileDropHandler {
    pub data: *mut FileDropHandlerData,
}

#[allow(non_snake_case)]
impl FileDropHandler {
    pub(crate) fn new(window: HWND, send_event: Box<dyn Fn(WindowEvent)>) -> FileDropHandler {
        let data = Box::new(FileDropHandlerData {
            interface: IDropTarget { lpVtbl: &DROP_TARGET_VTBL as *const IDropTargetVtbl },
            refcount: AtomicUsize::new(1),
            window,
            send_event,
            cursor_effect: DROPEFFECT_NONE,
            valid: false,
        });
        FileDropHandler { data: Box::into_raw(data) }
    }

    // Implement IUnknown
    pub unsafe extern "system" fn QueryInterface(
        _this: *mut IUnknown,
        _riid: *const GUID,
        _ppvObject: *mut *mut c_void,
    ) -> HRESULT {
        // This function doesn't appear to be required for an `IDropTarget`.
        // An implementation would be nice however.
        unimplemented!();
    }

    pub unsafe extern "system" fn AddRef(this: *mut IUnknown) -> u32 {
        let drop_handler_data = unsafe { Self::from_interface(this) };
        let count = drop_handler_data.refcount.fetch_add(1, Ordering::Release) + 1;
        count as u32
    }

    pub unsafe extern "system" fn Release(this: *mut IUnknown) -> u32 {
        let drop_handler = unsafe { Self::from_interface(this) };
        let count = drop_handler.refcount.fetch_sub(1, Ordering::Release) - 1;
        if count == 0 {
            // Destroy the underlying data
            drop(unsafe { Box::from_raw(drop_handler as *mut FileDropHandlerData) });
        }
        count as u32
    }

    pub unsafe extern "system" fn DragEnter(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        _grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        let mut pt = POINT { x: pt.x, y: pt.y };
        unsafe {
            ScreenToClient(drop_handler.window, &mut pt);
        }
        let position = PhysicalPosition::new(pt.x as f64, pt.y as f64);
        let mut paths = Vec::new();
        let hdrop = unsafe { Self::iterate_filenames(pDataObj, |path| paths.push(path)) };
        drop_handler.valid = hdrop.is_some();
        if drop_handler.valid {
            (drop_handler.send_event)(WindowEvent::DragEntered { paths, position });
        }
        drop_handler.cursor_effect =
            if drop_handler.valid { DROPEFFECT_COPY } else { DROPEFFECT_NONE };
        unsafe {
            *pdwEffect = drop_handler.cursor_effect;
        }

        S_OK
    }

    pub unsafe extern "system" fn DragOver(
        this: *mut IDropTarget,
        _grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        if drop_handler.valid {
            let mut pt = POINT { x: pt.x, y: pt.y };
            unsafe {
                ScreenToClient(drop_handler.window, &mut pt);
            }
            let position = PhysicalPosition::new(pt.x as f64, pt.y as f64);
            (drop_handler.send_event)(WindowEvent::DragMoved { position });
        }
        unsafe {
            *pdwEffect = drop_handler.cursor_effect;
        }

        S_OK
    }

    pub unsafe extern "system" fn DragLeave(this: *mut IDropTarget) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        if drop_handler.valid {
            (drop_handler.send_event)(WindowEvent::DragLeft { position: None });
        }

        S_OK
    }

    pub unsafe extern "system" fn Drop(
        this: *mut IDropTarget,
        pDataObj: *const IDataObject,
        _grfKeyState: u32,
        pt: POINTL,
        pdwEffect: *mut u32,
    ) -> HRESULT {
        let drop_handler = unsafe { Self::from_interface(this) };
        if drop_handler.valid {
            let mut pt = POINT { x: pt.x, y: pt.y };
            unsafe {
                ScreenToClient(drop_handler.window, &mut pt);
            }
            let position = PhysicalPosition::new(pt.x as f64, pt.y as f64);
            let mut paths = Vec::new();
            let hdrop = unsafe { Self::iterate_filenames(pDataObj, |path| paths.push(path)) };
            (drop_handler.send_event)(WindowEvent::DragDropped { paths, position });
            if let Some(hdrop) = hdrop {
                unsafe {
                    DragFinish(hdrop);
                }
            }
        }
        unsafe {
            *pdwEffect = drop_handler.cursor_effect;
        }

        S_OK
    }

    unsafe fn from_interface<'a, InterfaceT>(this: *mut InterfaceT) -> &'a mut FileDropHandlerData {
        unsafe { &mut *(this as *mut _) }
    }

    unsafe fn iterate_filenames<F>(data_obj: *const IDataObject, mut callback: F) -> Option<HDROP>
    where
        F: FnMut(PathBuf),
    {
        let drop_format = FORMATETC {
            cfFormat: CF_HDROP,
            ptd: ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT,
            lindex: -1,
            tymed: TYMED_HGLOBAL as u32,
        };

        let mut medium = unsafe { std::mem::zeroed() };
        let get_data_fn = unsafe { (*(*data_obj).cast::<IDataObjectVtbl>()).GetData };
        let get_data_result = unsafe { get_data_fn(data_obj as *mut _, &drop_format, &mut medium) };
        if get_data_result >= 0 {
            let hdrop = unsafe { medium.u.hGlobal as HDROP };

            // The second parameter (0xFFFFFFFF) instructs the function to return the item count
            let item_count = unsafe { DragQueryFileW(hdrop, 0xffffffff, ptr::null_mut(), 0) };

            for i in 0..item_count {
                // Get the length of the path string NOT including the terminating null character.
                // Previously, this was using a fixed size array of MAX_PATH length, but the
                // Windows API allows longer paths under certain circumstances.
                let character_count =
                    unsafe { DragQueryFileW(hdrop, i, ptr::null_mut(), 0) as usize };
                let str_len = character_count + 1;

                // Fill path_buf with the null-terminated file name
                let mut path_buf = Vec::with_capacity(str_len);
                unsafe {
                    DragQueryFileW(hdrop, i, path_buf.as_mut_ptr(), str_len as u32);
                    path_buf.set_len(str_len);
                }

                callback(OsString::from_wide(&path_buf[0..character_count]).into());
            }

            Some(hdrop)
        } else if get_data_result == DV_E_FORMATETC {
            // If the dropped item is not a file this error will occur.
            // In this case it is OK to return without taking further action.
            debug!("Error occurred while processing dropped/hovered item: item is not a file.");
            None
        } else {
            debug!("Unexpected error occurred while processing dropped/hovered item.");
            None
        }
    }
}

impl Drop for FileDropHandler {
    fn drop(&mut self) {
        unsafe {
            FileDropHandler::Release(self.data as *mut IUnknown);
        }
    }
}

static DROP_TARGET_VTBL: IDropTargetVtbl = IDropTargetVtbl {
    parent: IUnknownVtbl {
        QueryInterface: FileDropHandler::QueryInterface,
        AddRef: FileDropHandler::AddRef,
        Release: FileDropHandler::Release,
    },
    DragEnter: FileDropHandler::DragEnter,
    DragOver: FileDropHandler::DragOver,
    DragLeave: FileDropHandler::DragLeave,
    Drop: FileDropHandler::Drop,
};
