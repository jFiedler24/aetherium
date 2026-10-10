//! Windows drag-out via OLE. gpui only implements external file drags on
//! macOS and Wayland, so on Windows we run our own `DoDragDrop` on a helper
//! thread whenever a file-tree drag starts. The data object hands the target
//! (Explorer, …) a CF_HDROP with the staged temp download, waiting briefly
//! for the staging download to finish if it hasn't yet.
//!
//! Dropping back onto aetherium's own window goes through gpui_windows'
//! registered `IDropTarget`, so it lands in the existing
//! `ExternalPaths` handlers like any OS file drop.

use std::mem::ManuallyDrop;
use std::os::windows::ffi::OsStrExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{
    DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS, DV_E_TYMED, E_NOTIMPL,
    E_OUTOFMEMORY, LPARAM, OLE_E_ADVISENOTSUPPORTED, POINT, S_OK, STG_E_MEDIUMFULL, WPARAM,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::System::Com::{
    CoUninitialize, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource, IDropSource_Impl,
    OleInitialize,
};
use windows::Win32::System::SystemServices::{MODIFIERKEYS_FLAGS, MK_LBUTTON};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, PostMessageW, WM_LBUTTONUP};
use windows::core::{HRESULT, Interface, Ref, implement};

/// Not defined by the `windows` crate (data format not supported).
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as i32);

/// One COM object implementing both the data object (one CF_HDROP whose path
/// resolves when the target asks) and the drop source.
#[implement(IDataObject, IDropSource)]
struct FileDrag {
    /// Polls the staging cache until the temp download lands.
    wait_path: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
}

/// How long GetData waits for the staged download before failing the drop.
const STAGING_DEADLINE: Duration = Duration::from_secs(15);

fn matches_hdrop(format: &FORMATETC) -> bool {
    format.cfFormat == CF_HDROP.0
        && format.dwAspect == DVASPECT_CONTENT.0
        && format.tymed == TYMED_HGLOBAL.0 as u32
}

/// Build a CF_HDROP STGMEDIUM holding `path`: a `DROPFILES` header followed
/// by the double-null-terminated wide path list, in a moveable global.
fn build_hdrop(path: &std::path::Path) -> windows::core::Result<STGMEDIUM> {
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .chain(std::iter::once(0))
        .collect();
    let header_len = std::mem::size_of::<DROPFILES>();
    unsafe {
        let hglobal = GlobalAlloc(GMEM_MOVEABLE, header_len + wide.len() * 2)?;
        let ptr = GlobalLock(hglobal);
        if ptr.is_null() {
            return Err(windows::core::Error::from(E_OUTOFMEMORY));
        }
        let header = DROPFILES {
            pFiles: header_len as u32,
            pt: POINT { x: 0, y: 0 },
            fNC: windows::core::BOOL::from(false),
            fWide: windows::core::BOOL::from(true),
        };
        std::ptr::write(ptr.cast::<DROPFILES>(), header);
        std::ptr::copy_nonoverlapping(
            wide.as_ptr(),
            ptr.cast::<u16>().add(header_len / 2),
            wide.len(),
        );
        let _ = GlobalUnlock(hglobal);
        Ok(STGMEDIUM {
            tymed: TYMED_HGLOBAL.0 as u32,
            u: STGMEDIUM_0 { hGlobal: hglobal },
            pUnkForRelease: ManuallyDrop::new(None),
        })
    }
}

impl IDataObject_Impl for FileDrag_Impl {
    fn GetData(&self, pformatetcin: *const FORMATETC) -> windows::core::Result<STGMEDIUM> {
        let format = unsafe { &*pformatetcin };
        if !matches_hdrop(format) {
            return Err(DATA_E_FORMATETC.into());
        }
        // The staging download may still be running; the target only asks
        // for the data at drop time, so wait (bounded) instead of dying.
        let deadline = Instant::now() + STAGING_DEADLINE;
        let path = loop {
            if let Some(path) = (self.wait_path)() {
                break Some(path);
            }
            if Instant::now() >= deadline {
                break None;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let path = path.ok_or_else(|| windows::core::Error::from(STG_E_MEDIUMFULL))?;
        build_hdrop(&path)
    }

    fn GetDataHere(
        &self,
        pformatetc: *const FORMATETC,
        _pmedium: *mut STGMEDIUM,
    ) -> windows::core::Result<()> {
        let format = unsafe { &*pformatetc };
        if !matches_hdrop(format) {
            Err(DATA_E_FORMATETC.into())
        } else {
            Err(windows::core::Error::from(DV_E_TYMED))
        }
    }

    fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
        let format = unsafe { &*pformatetc };
        if matches_hdrop(format) {
            S_OK
        } else {
            DATA_E_FORMATETC
        }
    }

    fn GetCanonicalFormatEtc(
        &self,
        _pformatetcin: *const FORMATETC,
        pformatetcout: *mut FORMATETC,
    ) -> HRESULT {
        unsafe {
            (*pformatetcout).ptd = std::ptr::null_mut();
        }
        E_NOTIMPL
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, _dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        Err(E_NOTIMPL.into())
    }

    fn DAdvise(
        &self,
        _pformatetc: *const FORMATETC,
        _advf: u32,
        _padvsink: Ref<IAdviseSink>,
    ) -> windows::core::Result<u32> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn DUnadvise(&self, _dwconnection: u32) -> windows::core::Result<()> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }

    fn EnumDAdvise(&self) -> windows::core::Result<IEnumSTATDATA> {
        Err(OLE_E_ADVISENOTSUPPORTED.into())
    }
}

impl IDropSource_Impl for FileDrag_Impl {
    fn QueryContinueDrag(
        &self,
        fescapepressed: windows::core::BOOL,
        grfkeystate: MODIFIERKEYS_FLAGS,
    ) -> HRESULT {
        if fescapepressed.as_bool() {
            DRAGDROP_S_CANCEL
        } else if !grfkeystate.contains(MK_LBUTTON) {
            DRAGDROP_S_DROP
        } else {
            S_OK
        }
    }

    fn GiveFeedback(&self, _dweffect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Start an OLE file drag for the given staged path, from a fresh thread so
/// gpui's own (internal) drag tracking is unaffected. `hwnd` is the app
/// window used to synthesize the button-up that the OLE loop consumes, so
/// gpui doesn't think the button stays pressed.
pub fn begin_file_drag(
    hwnd: isize,
    wait_path: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
) {
    std::thread::spawn(move || {
        unsafe {
            if OleInitialize(None).is_err() {
                return;
            }
            let drag = FileDrag { wait_path };
            // Both interfaces live on the one COM object.
            let data: IDataObject = drag.into();
            let Ok(source) = data.cast::<IDropSource>() else {
                CoUninitialize();
                return;
            };
            let mut effect = DROPEFFECT(0);
            let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect);
            CoUninitialize();
        }
        release_ghost_drag(hwnd);
    });
}

/// The OLE drag loop eats the real left-button-up, leaving gpui's internal
/// drag (and its frozen drag image) stuck. Post a synthetic one at the
/// current cursor position so gpui's state resets.
fn release_ghost_drag(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let hwnd = windows::Win32::Foundation::HWND(hwnd as *mut _);
        let mut point = POINT { x: 0, y: 0 };
        if GetCursorPos(&mut point).is_err() {
            return;
        }
        let mut client = point;
        let _ = ScreenToClient(hwnd, &mut client);
        let lparam = (((client.y as u16 as u32) << 16) | (client.x as u16 as u32)) as isize;
        let _ = PostMessageW(Some(hwnd), WM_LBUTTONUP, WPARAM(0), LPARAM(lparam));
    }
}
