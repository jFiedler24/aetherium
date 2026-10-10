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
    E_OUTOFMEMORY, HWND, OLE_E_ADVISENOTSUPPORTED, POINT, S_FALSE, S_OK, STG_E_MEDIUMFULL,
};
use windows::Win32::System::Com::{
    CoUninitialize, DVASPECT_CONTENT, FORMATETC, IAdviseSink, IDataObject, IDataObject_Impl,
    IEnumFORMATETC, IEnumFORMATETC_Impl, IEnumSTATDATA, STGMEDIUM, STGMEDIUM_0, TYMED_HGLOBAL,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::{
    CF_HDROP, DROPEFFECT, DROPEFFECT_COPY, DoDragDrop, IDropSource, IDropSource_Impl,
    OleInitialize,
};
use windows::Win32::System::SystemServices::{MODIFIERKEYS_FLAGS, MK_LBUTTON};
use windows::Win32::UI::Shell::DROPFILES;
use windows::Win32::UI::WindowsAndMessaging::{KillTimer, SetTimer, WM_TIMER};
use windows::core::{HRESULT, Interface, Ref, implement};

/// Not defined by the `windows` crate (data format not supported).
const DATA_E_FORMATETC: HRESULT = HRESULT(0x80040064_u32 as i32);
/// The input and output formats are identical (canonical-format query).
const DATA_S_SAMEFORMATETC: HRESULT = HRESULT(0x0004_0130);

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
        && (format.lindex == -1 || format.lindex == 0)
        && format.tymed & (TYMED_HGLOBAL.0 as u32) != 0
}

/// The one format we advertise: CF_HDROP as a moveable global.
fn hdrop_formatetc() -> FORMATETC {
    FORMATETC {
        cfFormat: CF_HDROP.0,
        ptd: std::ptr::null_mut(),
        dwAspect: DVASPECT_CONTENT.0,
        lindex: -1,
        tymed: TYMED_HGLOBAL.0 as u32,
    }
}

/// Enumerator over the data object's formats. Drop targets (Explorer
/// included) typically learn what a drag offers through `EnumFormatEtc`;
/// failing it — the old behavior — makes targets reject the drag outright,
/// which surfaced as "the drop never leaves the window".
#[implement(IEnumFORMATETC)]
struct FormatEnumerator {
    formats: Vec<FORMATETC>,
    index: std::sync::Mutex<usize>,
}

impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
    fn Next(
        &self,
        celt: u32,
        rgelt: *mut FORMATETC,
        pcelt_fetched: *mut u32,
    ) -> HRESULT {
        let mut index = self.index.lock().unwrap();
        let available = self.formats.len().saturating_sub(*index);
        let count = (celt as usize).min(available);
        if count > 0 {
            unsafe {
                std::ptr::copy_nonoverlapping(self.formats.as_ptr().add(*index), rgelt, count);
            }
            *index += count;
        }
        if !pcelt_fetched.is_null() {
            unsafe { *pcelt_fetched = count as u32 };
        }
        if count == celt as usize {
            S_OK
        } else {
            S_FALSE
        }
    }

    fn Skip(&self, celt: u32) -> windows::core::Result<()> {
        let mut index = self.index.lock().unwrap();
        let next = *index + celt as usize;
        if next <= self.formats.len() {
            *index = next;
            Ok(())
        } else {
            *index = self.formats.len();
            Err(windows::core::Error::from(S_FALSE))
        }
    }

    fn Reset(&self) -> windows::core::Result<()> {
        *self.index.lock().unwrap() = 0;
        Ok(())
    }

    fn Clone(&self) -> windows::core::Result<IEnumFORMATETC> {
        let enumerator = FormatEnumerator {
            formats: self.formats.clone(),
            index: std::sync::Mutex::new(*self.index.lock().unwrap()),
        };
        Ok(enumerator.into())
    }
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
        log::info!("drag-out: target requested data, resolved {}", path.display());
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
        DATA_S_SAMEFORMATETC
    }

    fn SetData(
        &self,
        _pformatetc: *const FORMATETC,
        _pmedium: *const STGMEDIUM,
        _frelease: windows::core::BOOL,
    ) -> windows::core::Result<()> {
        Err(E_NOTIMPL.into())
    }

    fn EnumFormatEtc(&self, dwdirection: u32) -> windows::core::Result<IEnumFORMATETC> {
        // DATADIR_GET (1) is what drag targets ask for; DATADIR_SET gets an
        // empty enumerator instead of an error.
        let formats = if dwdirection == 1 { vec![hdrop_formatetc()] } else { Vec::new() };
        Ok(FormatEnumerator {
            formats,
            index: std::sync::Mutex::new(0),
        }
        .into())
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

/// One-shot timer id used to kick the drag outside any gpui dispatch.
const DRAG_TIMER_ID: usize = 0xA37F;

/// The staged-path resolver handed from the drag gesture to the timer proc.
static PENDING_DRAG: std::sync::Mutex<
    Option<std::sync::Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>>,
> = std::sync::Mutex::new(None);

/// Schedule an outgoing OLE file drag for the staged path.
///
/// The actual `DoDragDrop` runs in a `SetTimer` TIMERPROC, which Windows
/// invokes while gpui's thread sits in `GetMessage` — BETWEEN message
/// dispatches, holding no gpui borrow. Every earlier design failed on
/// borrow conflicts: running DoDragDrop inside a gpui callback (event
/// handler, defer) held the `AppCell` borrow across the whole nested
/// pump, and gpui activity that interleaved with the drag — executor
/// tasks on Windows thread-pool threads, COM marshalling of gpui's own
/// registered IDropTarget into this thread — then panicked with
/// "RefCell already borrowed". Between dispatches no borrow is held, so
/// the nested pump's messages each take short sequential borrows and
/// everything (COM calls in both directions, gpui's drop-target
/// notifications) works as designed. gpui's mouse capture from the
/// button-down belongs to this thread, which is exactly what DoDragDrop
/// requires.
// [impl->req~windows-drag-out~1]
pub fn schedule_file_drag(
    app_hwnd: isize,
    wait_path: std::sync::Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>,
) {
    if app_hwnd == 0 {
        return;
    }
    *PENDING_DRAG.lock().unwrap() = Some(wait_path);
    unsafe {
        // 1 ms; the proc kills the timer regardless (uElapse=0's meaning
        // is ambiguous on some Windows versions).
        SetTimer(
            Some(HWND(app_hwnd as *mut _)),
            DRAG_TIMER_ID,
            1,
            Some(drag_timer_proc),
        );
    }
}

unsafe extern "system" fn drag_timer_proc(hwnd: HWND, msg: u32, id: usize, _time: u32) {
    if msg != WM_TIMER || id != DRAG_TIMER_ID {
        return;
    }
    unsafe {
        let _ = KillTimer(Some(hwnd), DRAG_TIMER_ID);
    }
    let wait = PENDING_DRAG.lock().unwrap().take();
    if let Some(wait) = wait {
        run_file_drag(wait);
    }
}

/// The drag itself. Runs between gpui dispatches (see
/// [`schedule_file_drag`]): DoDragDrop's nested pump handles each message
/// with its own short borrow, gpui receives the real button-up through
/// that pump and ends its internal drag, and the drop target under the
/// cursor gets the staged CF_HDROP.
fn run_file_drag(wait_path: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>) {
    if let Err(err) = unsafe { OleInitialize(None) } {
        log::error!("drag-out: OleInitialize failed: {err}");
        return;
    }
    let drag = FileDrag { wait_path };
    // Both interfaces live on the one COM object.
    let data: IDataObject = drag.into();
    let Ok(source) = data.cast::<IDropSource>() else {
        log::error!("drag-out: could not get IDropSource from the data object");
        unsafe { CoUninitialize() };
        return;
    };
    let mut effect = DROPEFFECT(0);
    let result = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    log::info!(
        "drag-out: DoDragDrop returned {result:?}, final effect {:?}",
        effect
    );
    // Balances the OleInitialize above; gpui's own initialization keeps
    // its own reference count.
    unsafe { CoUninitialize() };
}
