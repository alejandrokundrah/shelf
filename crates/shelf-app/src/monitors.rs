//! Lists the connected monitors (Win32 EnumDisplayMonitors, declared by hand).
//! Rectangles are the usable work area in physical pixels, in virtual-screen coordinates.

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}

#[repr(C)]
struct MonitorInfo {
    cb_size: u32,
    rc_monitor: Rect,
    rc_work: Rect,
    dw_flags: u32,
}

type MonitorEnumProc = unsafe extern "system" fn(isize, isize, *mut Rect, isize) -> i32;

#[link(name = "user32")]
extern "system" {
    fn EnumDisplayMonitors(hdc: isize, clip: *const Rect, proc_: MonitorEnumProc, data: isize) -> i32;
    fn GetMonitorInfoW(monitor: isize, info: *mut MonitorInfo) -> i32;
}

const MONITORINFOF_PRIMARY: u32 = 1;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Monitor {
    pub left: f32,
    pub top: f32,
    pub width: f32,
    pub height: f32,
    pub primary: bool,
}

unsafe extern "system" fn collect(monitor: isize, _hdc: isize, _rect: *mut Rect, data: isize) -> i32 {
    let list = &mut *(data as *mut Vec<Monitor>);
    let mut info = MonitorInfo {
        cb_size: std::mem::size_of::<MonitorInfo>() as u32,
        rc_monitor: Rect::default(),
        rc_work: Rect::default(),
        dw_flags: 0,
    };
    if GetMonitorInfoW(monitor, &mut info) != 0 {
        let w = info.rc_work;
        list.push(Monitor {
            left: w.left as f32,
            top: w.top as f32,
            width: (w.right - w.left) as f32,
            height: (w.bottom - w.top) as f32,
            primary: info.dw_flags & MONITORINFOF_PRIMARY != 0,
        });
    }
    1 // keep enumerating
}

/// All monitors, ordered left to right.
pub fn list() -> Vec<Monitor> {
    let mut found: Vec<Monitor> = Vec::new();
    // SAFETY: `collect` only runs during this call and `found` outlives it.
    unsafe {
        EnumDisplayMonitors(0, std::ptr::null(), collect, &mut found as *mut Vec<Monitor> as isize);
    }
    found.sort_by(|a, b| a.left.partial_cmp(&b.left).unwrap_or(std::cmp::Ordering::Equal));
    found
}
