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
    WPARAM, LPARAM, LRESULT,
};
use windows::Win32::Graphics::Gdi::ScreenToClient;
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
use windows::Win32::UI::Shell::{
    DROPFILES, DefSubclassProc, RemoveWindowSubclass, SetWindowSubclass,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetClassNameW, GetCursorPos, GetWindowThreadProcessId, PostMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_GETMESSAGE, WM_CHAR, WM_KEYDOWN, WM_KEYUP,
    WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDBLCLK, WM_MBUTTONDOWN,
    WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_NULL, WM_PAINT,
    WM_RBUTTONDBLCLK, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SETCURSOR, WM_TIMER, MSG,
};
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

/// Subclass id for the input eater installed while an OLE drag runs.
const DRAG_SUBCLASS_ID: usize = 0xA37E;

/// Set while an outgoing drag runs: the hook below nulls OLE's apartment
/// marshalling messages so they can't re-enter gpui mid-borrow.
static DRAG_OUT_ACTIVE: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

/// OLE marshals IDropTarget calls into gpui's STA via SendMessage to the
/// hidden "OleMainThreadWndClass" window. While our drag callback holds
/// gpui's borrow, dispatching that would panic (RefCell already borrowed),
/// so the hook nulls the message — the RPC side gets a harmless error and
/// the drop effect over our own window becomes NONE, which is right
/// anyway mid-drag-out.
unsafe extern "system" fn com_shield_hook(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && DRAG_OUT_ACTIVE.load(std::sync::atomic::Ordering::Relaxed) {
        let msg = unsafe { &mut *(lparam.0 as *mut MSG) };
        if !msg.hwnd.0.is_null() {
            let mut class = [0u16; 32];
            let len = unsafe { GetClassNameW(msg.hwnd, &mut class) };
            if len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "OleMainThreadWndClass" {
                msg.message = WM_NULL;
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// Messages that would re-enter gpui's dispatch (and its held RefCell
/// borrows) while `DoDragDrop` pumps its nested loop. OLE tracks the
/// physical mouse itself, so gpui must not see input until the drag ends;
/// paint/timer messages are swallowed too (they would render or flush
/// effects mid-borrow). Everything else — COM plumbing above all — passes
/// through untouched.
unsafe extern "system" fn input_eater_subclass(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
    _id: usize,
    _data: usize,
) -> LRESULT {
    match msg {
        WM_MOUSEMOVE
        | WM_LBUTTONDOWN
        | WM_LBUTTONUP
        | WM_LBUTTONDBLCLK
        | WM_RBUTTONDOWN
        | WM_RBUTTONUP
        | WM_RBUTTONDBLCLK
        | WM_MBUTTONDOWN
        | WM_MBUTTONUP
        | WM_MBUTTONDBLCLK
        | WM_MOUSEWHEEL
        | WM_MOUSEHWHEEL
        | WM_SETCURSOR
        | WM_KEYDOWN
        | WM_KEYUP
        | WM_CHAR
        | WM_PAINT
        | WM_TIMER => LRESULT(0),
        _ => unsafe { DefSubclassProc(hwnd, msg, wparam, lparam) },
    }
}

/// Run an OLE file drag for the given staged path.
///
/// CALL ON THE UI THREAD, from a *deferred* gpui callback — never from
/// inside an event handler. `DoDragDrop` pumps a nested message loop; if
/// gpui dispatched that input normally it would re-enter dispatch while
/// the caller holds borrows and panic ("RefCell already borrowed"), so an
/// input-eating window subclass shields the gpui window for the drag's
/// duration. Capture is gpui's own from the mouse-down — owned by the
/// calling thread, as DoDragDrop requires (helper-thread variants with
/// their own capture windows never satisfied it: the drag starved before
/// reaching any target). Afterwards the subclass comes off and a synthetic
/// button-up releases gpui's internal drag state.
// [impl->req~windows-drag-out~1]
pub fn begin_file_drag(app_hwnd: isize, wait_path: Arc<dyn Fn() -> Option<PathBuf> + Send + Sync>) {
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
    let hwnd = HWND(app_hwnd as *mut _);
    let mut hook = None;
    if app_hwnd != 0 {
        DRAG_OUT_ACTIVE.store(true, std::sync::atomic::Ordering::Relaxed);
        unsafe {
            let thread_id = GetWindowThreadProcessId(hwnd, None);
            hook = SetWindowsHookExW(WH_GETMESSAGE, Some(com_shield_hook), None, thread_id).ok();
            let _ = SetWindowSubclass(
                hwnd,
                Some(input_eater_subclass),
                DRAG_SUBCLASS_ID,
                0,
            );
        }
    }
    let mut effect = DROPEFFECT(0);
    let result = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
    log::info!(
        "drag-out: DoDragDrop returned {result:?}, final effect {:?}",
        effect
    );
    DRAG_OUT_ACTIVE.store(false, std::sync::atomic::Ordering::Relaxed);
    if let Some(hook) = hook {
        unsafe { let _ = UnhookWindowsHookEx(hook); }
    }
    if app_hwnd != 0 {
        unsafe {
            let _ = RemoveWindowSubclass(hwnd, Some(input_eater_subclass), DRAG_SUBCLASS_ID);
        }
    }
    release_ghost_drag(app_hwnd);
    // Balances the OleInitialize above; gpui's own initialization keeps
    // its own reference count.
    unsafe { CoUninitialize() };
}

/// The real button-up was swallowed during the drag (the input eater), so
/// gpui's internal drag state is still armed. Post a synthetic one at the
/// current cursor position so it resets.
fn release_ghost_drag(hwnd: isize) {
    if hwnd == 0 {
        return;
    }
    unsafe {
        let hwnd = HWND(hwnd as *mut _);
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
