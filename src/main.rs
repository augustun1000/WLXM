use std::ffi::{c_void, CString};
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::os::raw::{c_char, c_int};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

const CONNECTOR_ID: u32 = 102;
const DRM_CAP_DUMB_BUFFER: u64 = 1;
const DRM_FORMAT_XRGB8888: u32 = 0x3432_5258;
const DRM_MODE_TYPE_PREFERRED: u32 = 1 << 3;
const PIXMAN_X8R8G8B8: c_int = (32 << 24) | (2 << 16) | (8 << 8) | (8 << 4) | 8;
const PIXMAN_OP_SRC: c_int = 1;
const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);

#[repr(C)]
struct LibSeat { _opaque: [u8; 0] }
#[repr(C)]
struct LibSeatListener {
    enable_seat: Option<unsafe extern "C" fn(*mut LibSeat, *mut c_void)>,
    disable_seat: Option<unsafe extern "C" fn(*mut LibSeat, *mut c_void)>,
}
struct SeatState { active: AtomicBool }
unsafe extern "C" fn enable_seat(_seat: *mut LibSeat, data: *mut c_void) {
    if !data.is_null() {
        unsafe { &*(data as *const SeatState) }.active.store(true, Ordering::SeqCst);
    }
}
unsafe extern "C" fn disable_seat(seat: *mut LibSeat, data: *mut c_void) {
    if !data.is_null() {
        unsafe { &*(data as *const SeatState) }.active.store(false, Ordering::SeqCst);
    }
    // Obligatorio: confirmar enseguida al gestor de sesiones.
    unsafe { libseat_disable_seat(seat); }
}
static LISTENER: LibSeatListener = LibSeatListener {
    enable_seat: Some(enable_seat),
    disable_seat: Some(disable_seat),
};

#[link(name = "seat")]
unsafe extern "C" {
    fn libseat_open_seat(listener: *const LibSeatListener, userdata: *mut c_void) -> *mut LibSeat;
    fn libseat_close_seat(seat: *mut LibSeat) -> c_int;
    fn libseat_dispatch(seat: *mut LibSeat, timeout: c_int) -> c_int;
    fn libseat_disable_seat(seat: *mut LibSeat) -> c_int;
    fn libseat_open_device(seat: *mut LibSeat, path: *const c_char, fd: *mut c_int) -> c_int;
    fn libseat_close_device(seat: *mut LibSeat, device_id: c_int) -> c_int;
}

struct Seat { ptr: *mut LibSeat, state: Box<SeatState> }
impl Seat {
    fn open() -> io::Result<Self> {
        let mut state = Box::new(SeatState { active: AtomicBool::new(false) });
        let ptr = unsafe {
            libseat_open_seat(&LISTENER, (&mut *state as *mut SeatState).cast::<c_void>())
        };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let seat = Self { ptr, state };
        let limit = Instant::now() + Duration::from_secs(3);
        while !seat.active() && Instant::now() < limit {
            seat.dispatch()?;
            thread::sleep(Duration::from_millis(20));
        }
        if !seat.active() { return Err(io::Error::other("libseat no activó la sesión")); }
        Ok(seat)
    }
    fn active(&self) -> bool { self.state.active.load(Ordering::SeqCst) }
    fn dispatch(&self) -> io::Result<()> {
        if unsafe { libseat_dispatch(self.ptr, 0) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}
impl Drop for Seat {
    fn drop(&mut self) { unsafe { libseat_close_seat(self.ptr); } }
}

unsafe extern "C" {
    fn close(fd: c_int) -> c_int;
    fn mmap(addr: *mut c_void, len: usize, prot: c_int, flags: c_int, fd: c_int, offset: i64) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
    fn signal(sig: c_int, handler: extern "C" fn(c_int)) -> usize;
}
extern "C" fn request_stop(_: c_int) { STOP_REQUESTED.store(true, Ordering::SeqCst); }

struct DrmDevice<'a> { seat: &'a Seat, fd: RawFd, device_id: c_int }
impl<'a> DrmDevice<'a> {
    fn open(seat: &'a Seat, path: &str) -> io::Result<Self> {
        let path = CString::new(path).map_err(|_| io::Error::other("Ruta con NUL"))?;
        let mut fd = -1;
        let device_id = unsafe { libseat_open_device(seat.ptr, path.as_ptr(), &mut fd) };
        if device_id < 0 { return Err(io::Error::last_os_error()); }
        if fd < 0 {
            unsafe { libseat_close_device(seat.ptr, device_id); }
            return Err(io::Error::other("libseat devolvió un fd inválido"));
        }
        Ok(Self { seat, fd, device_id })
    }
}
impl Drop for DrmDevice<'_> {
    fn drop(&mut self) {
        unsafe {
            libseat_close_device(self.seat.ptr, self.device_id);
            close(self.fd);
        }
    }
}

#[repr(C)]
struct DrmResources {
    count_fbs: c_int, fbs: *mut u32,
    count_crtcs: c_int, crtcs: *mut u32,
    count_connectors: c_int, connectors: *mut u32,
    count_encoders: c_int, encoders: *mut u32,
    min_width: u32, max_width: u32, min_height: u32, max_height: u32,
}
#[repr(C)]
#[derive(Copy, Clone)]
struct DrmMode {
    clock: u32,
    hdisplay: u16, hsync_start: u16, hsync_end: u16, htotal: u16, hskew: u16,
    vdisplay: u16, vsync_start: u16, vsync_end: u16, vtotal: u16, vscan: u16,
    vrefresh: u32, flags: u32, mode_type: u32, name: [c_char; 32],
}
#[repr(C)]
struct DrmConnector {
    connector_id: u32, encoder_id: u32, connector_type: u32, connector_type_id: u32,
    connection: c_int, mm_width: u32, mm_height: u32, subpixel: c_int,
    count_modes: c_int, modes: *mut DrmMode,
    count_props: c_int, props: *mut u32, prop_values: *mut u64,
    count_encoders: c_int, encoders: *mut u32,
}
#[repr(C)]
struct DrmEncoder {
    encoder_id: u32, encoder_type: u32, crtc_id: u32,
    possible_crtcs: u32, possible_clones: u32,
}
#[repr(C)]
struct DrmCrtc {
    crtc_id: u32, buffer_id: u32, x: u32, y: u32,
    width: u32, height: u32, mode_valid: c_int, mode: DrmMode, gamma_size: c_int,
}

#[link(name = "drm")]
unsafe extern "C" {
    fn drmGetCap(fd: c_int, capability: u64, value: *mut u64) -> c_int;
    fn drmModeGetResources(fd: c_int) -> *mut DrmResources;
    fn drmModeFreeResources(ptr: *mut DrmResources);
    fn drmModeGetConnector(fd: c_int, connector_id: u32) -> *mut DrmConnector;
    fn drmModeFreeConnector(ptr: *mut DrmConnector);
    fn drmModeGetEncoder(fd: c_int, encoder_id: u32) -> *mut DrmEncoder;
    fn drmModeFreeEncoder(ptr: *mut DrmEncoder);
    fn drmModeGetCrtc(fd: c_int, crtc_id: u32) -> *mut DrmCrtc;
    fn drmModeFreeCrtc(ptr: *mut DrmCrtc);
    fn drmModeSetCrtc(fd: c_int, crtc_id: u32, fb_id: u32,
        x: u32, y: u32, connectors: *const u32, count: c_int, mode: *const DrmMode) -> c_int;
    fn drmModeCreateDumbBuffer(fd: c_int, width: u32, height: u32, bpp: u32,
        flags: u32, handle: *mut u32, pitch: *mut u32, size: *mut u64) -> c_int;
    fn drmModeMapDumbBuffer(fd: c_int, handle: u32, offset: *mut u64) -> c_int;
    fn drmModeDestroyDumbBuffer(fd: c_int, handle: u32) -> c_int;
    fn drmModeAddFB2(fd: c_int, width: u32, height: u32, format: u32,
        handles: *const u32, pitches: *const u32, offsets: *const u32,
        buf_id: *mut u32, flags: u32) -> c_int;
    fn drmModeRmFB(fd: c_int, id: u32) -> c_int;
}

fn drm_error(name: &str, code: c_int) -> io::Error {
    let source = if code < -1 { io::Error::from_raw_os_error(-code) }
                 else { io::Error::last_os_error() };
    io::Error::new(source.kind(), format!("{name}: {source} (ret={code})"))
}

struct SelectedOutput { mode: DrmMode, crtc_id: u32 }

fn select_output(fd: RawFd) -> io::Result<SelectedOutput> {
    let res_ptr = unsafe { drmModeGetResources(fd) };
    if res_ptr.is_null() { return Err(io::Error::last_os_error()); }
    let res = unsafe { &*res_ptr };
    let output = (|| -> io::Result<SelectedOutput> {
        if res.count_crtcs <= 0 || res.crtcs.is_null() {
            return Err(io::Error::other("No hay CRTCs disponibles"));
        }
        let crtc_ids = unsafe { std::slice::from_raw_parts(res.crtcs, res.count_crtcs as usize) };
        let ptr = unsafe { drmModeGetConnector(fd, CONNECTOR_ID) };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let connector = unsafe { &*ptr };
        let answer = (|| -> io::Result<SelectedOutput> {
            if connector.connection != 1 || connector.count_modes <= 0 || connector.modes.is_null() {
                return Err(io::Error::other("eDP-1 no está conectado o no tiene modos"));
            }
            let modes = unsafe { std::slice::from_raw_parts(connector.modes, connector.count_modes as usize) };
            let mode = *modes.iter().find(|m| m.mode_type & DRM_MODE_TYPE_PREFERRED != 0)
                .unwrap_or(&modes[0]);
            if (mode.hdisplay, mode.vdisplay) != (1366, 768) {
                return Err(io::Error::other(format!("Modo inesperado: {}x{} (se esperaba 1366x768)", mode.hdisplay, mode.vdisplay)));
            }
            let mut encoder_ids = Vec::new();
            if connector.encoder_id != 0 { encoder_ids.push(connector.encoder_id); }
            if connector.count_encoders > 0 && !connector.encoders.is_null() {
                let encoders = unsafe { std::slice::from_raw_parts(connector.encoders, connector.count_encoders as usize) };
                for &id in encoders { if !encoder_ids.contains(&id) { encoder_ids.push(id); } }
            }
            // Preferimos el CRTC que el conector ya tenía asignado.
            for id in encoder_ids {
                let eptr = unsafe { drmModeGetEncoder(fd, id) };
                if eptr.is_null() { continue; }
                let e = unsafe { &*eptr };
                let chosen = if e.crtc_id != 0 && crtc_ids.contains(&e.crtc_id) {
                    Some(e.crtc_id)
                } else {
                    crtc_ids.iter().enumerate()
                        .find(|(i, _)| *i < 32 && (e.possible_crtcs & (1u32 << *i)) != 0)
                        .map(|(_, id)| *id)
                };
                unsafe { drmModeFreeEncoder(eptr) };
                if let Some(crtc_id) = chosen { return Ok(SelectedOutput { mode, crtc_id }); }
            }
            Err(io::Error::other("No se encontró CRTC compatible con eDP-1"))
        })();
        unsafe { drmModeFreeConnector(ptr) };
        answer
    })();
    unsafe { drmModeFreeResources(res_ptr) };
    output
}

struct SavedCrtc { crtc_id: u32, fb_id: u32, x: u32, y: u32, mode: Option<DrmMode>, connectors: Vec<u32> }
impl SavedCrtc {
    fn capture(fd: RawFd, crtc_id: u32) -> io::Result<Self> {
        let ptr = unsafe { drmModeGetCrtc(fd, crtc_id) };
        if ptr.is_null() { return Err(io::Error::last_os_error()); }
        let c = unsafe { &*ptr };
        let mut snap = Self {
            crtc_id: c.crtc_id, fb_id: c.buffer_id, x: c.x, y: c.y,
            mode: if c.mode_valid != 0 { Some(c.mode) } else { None },
            connectors: Vec::new(),
        };
        unsafe { drmModeFreeCrtc(ptr) };
        let res_ptr = unsafe { drmModeGetResources(fd) };
        if res_ptr.is_null() { return Err(io::Error::last_os_error()); }
        let res = unsafe { &*res_ptr };
        if res.count_connectors > 0 && !res.connectors.is_null() {
            let ids = unsafe { std::slice::from_raw_parts(res.connectors, res.count_connectors as usize) };
            for &id in ids {
                let con = unsafe { drmModeGetConnector(fd, id) };
                if con.is_null() { continue; }
                let en_id = unsafe { (*con).encoder_id };
                if en_id != 0 {
                    let en = unsafe { drmModeGetEncoder(fd, en_id) };
                    if !en.is_null() {
                        if unsafe { (*en).crtc_id } == crtc_id { snap.connectors.push(id); }
                        unsafe { drmModeFreeEncoder(en) };
                    }
                }
                unsafe { drmModeFreeConnector(con) };
            }
        }
        unsafe { drmModeFreeResources(res_ptr) };
        if snap.fb_id != 0 && snap.mode.is_some() && snap.connectors.is_empty() {
            return Err(io::Error::other("CRTC activo sin conectores recuperables: se cancela por seguridad"));
        }
        Ok(snap)
    }
    fn restore(&self, fd: RawFd) -> io::Result<()> {
        let (fb, x, y, connectors, mode) = match self.mode.as_ref() {
            Some(mode) if self.fb_id != 0 =>
                (self.fb_id, self.x, self.y, self.connectors.as_slice(), mode as *const DrmMode),
            _ => (0, 0, 0, &[][..], ptr::null()),
        };
        let conn_ptr = if connectors.is_empty() { ptr::null() } else { connectors.as_ptr() };
        let ret = unsafe { drmModeSetCrtc(fd, self.crtc_id, fb, x, y,
            conn_ptr, connectors.len() as c_int, mode) };
        if ret != 0 { return Err(drm_error("restaurar drmModeSetCrtc", ret)); }
        Ok(())
    }
}

struct DumbBuffer { fd: RawFd, handle: u32, pitch: u32, len: usize, map: *mut u8, width: u32, height: u32 }
impl DumbBuffer {
    fn create(fd: RawFd, width: u32, height: u32) -> io::Result<Self> {
        let (mut handle, mut pitch, mut size) = (0, 0, 0u64);
        let ret = unsafe { drmModeCreateDumbBuffer(fd, width, height, 32, 0,
            &mut handle, &mut pitch, &mut size) };
        if ret != 0 { return Err(drm_error("drmModeCreateDumbBuffer", ret)); }
        let mut buf = Self { fd, handle, pitch, len: 0, map: ptr::null_mut(), width, height };
        let len = usize::try_from(size).map_err(|_| io::Error::other("Tamaño fuera de rango"))?;
        if pitch < width.saturating_mul(4) || pitch > c_int::MAX as u32 || pitch % 4 != 0
            || size < u64::from(pitch) * u64::from(height) {
            return Err(io::Error::other("Pitch o tamaño inválido"));
        }
        let mut offset: u64 = 0;
        let ret = unsafe { drmModeMapDumbBuffer(fd, handle, &mut offset) };
        if ret != 0 { return Err(drm_error("drmModeMapDumbBuffer", ret)); }
        let offset = i64::try_from(offset).map_err(|_| io::Error::other("Offset demasiado grande"))?;
        let p = unsafe { mmap(ptr::null_mut(), len, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset) };
        if p == usize::MAX as *mut c_void { return Err(io::Error::last_os_error()); }
        buf.map = p.cast::<u8>();
        buf.len = len;
        Ok(buf)
    }
}
impl Drop for DumbBuffer {
    fn drop(&mut self) {
        if !self.map.is_null() { unsafe { munmap(self.map.cast::<c_void>(), self.len); } }
        if self.handle != 0 { unsafe { drmModeDestroyDumbBuffer(self.fd, self.handle); } }
    }
}

struct Framebuffer<'a> { fd: RawFd, id: u32, _buffer: &'a DumbBuffer }
impl<'a> Framebuffer<'a> {
    fn new(fd: RawFd, buffer: &'a DumbBuffer) -> io::Result<Self> {
        let handles = [buffer.handle, 0, 0, 0];
        let pitches = [buffer.pitch, 0, 0, 0];
        let offsets = [0u32; 4];
        let mut id = 0;
        let ret = unsafe { drmModeAddFB2(fd, buffer.width, buffer.height, DRM_FORMAT_XRGB8888,
            handles.as_ptr(), pitches.as_ptr(), offsets.as_ptr(), &mut id, 0) };
        if ret != 0 { return Err(drm_error("drmModeAddFB2", ret)); }
        Ok(Self { fd, id, _buffer: buffer })
    }
}
impl Drop for Framebuffer<'_> {
    fn drop(&mut self) {
        if unsafe { drmModeRmFB(self.fd, self.id) } != 0 {
            eprintln!("Aviso: no se pudo retirar framebuffer {}", self.id);
        }
    }
}

#[repr(C)]
struct PixmanImage { _opaque: [u8; 0] }
#[repr(C)]
struct PixmanColor { red: u16, green: u16, blue: u16, alpha: u16 }
#[repr(C)]
struct PixmanBox32 { x1: i32, y1: i32, x2: i32, y2: i32 }
#[link(name = "pixman-1")]
unsafe extern "C" {
    fn pixman_image_create_bits(fmt: c_int, width: c_int, height: c_int,
        bits: *mut u32, stride: c_int) -> *mut PixmanImage;
    fn pixman_image_fill_boxes(op: c_int, dest: *mut PixmanImage,
        color: *const PixmanColor, n_boxes: c_int, boxes: *const PixmanBox32) -> c_int;
    fn pixman_image_unref(image: *mut PixmanImage) -> c_int;
}
fn color(r: u8, g: u8, b: u8) -> PixmanColor {
    PixmanColor { red: u16::from(r) * 257, green: u16::from(g) * 257,
        blue: u16::from(b) * 257, alpha: 65535 }
}
fn box_fill(image: *mut PixmanImage, (x1,y1,x2,y2): (i32,i32,i32,i32), c: PixmanColor) -> io::Result<()> {
    let rect = PixmanBox32 { x1,y1,x2,y2 };
    if unsafe { pixman_image_fill_boxes(PIXMAN_OP_SRC, image, &c, 1, &rect) } == 0 {
        return Err(io::Error::other("pixman_image_fill_boxes falló"));
    }
    Ok(())
}
fn draw(buf: &DumbBuffer) -> io::Result<()> {
    let p = unsafe { pixman_image_create_bits(PIXMAN_X8R8G8B8,
        buf.width as c_int, buf.height as c_int, buf.map.cast::<u32>(), buf.pitch as c_int) };
    if p.is_null() { return Err(io::Error::other("pixman_image_create_bits falló")); }
    let result = (|| {
        box_fill(p, (0, 0, buf.width as i32, buf.height as i32), color(22,38,56))?;
        box_fill(p, (100,80,780,480), color(14,116,144))?;
        box_fill(p, (400,250,920,550), color(245,158,11))?;
        Ok(())
    })();
    unsafe { pixman_image_unref(p); }
    result
}

struct RestoreGuard<'a> { seat: &'a Seat, fd: RawFd, saved: SavedCrtc, showing: bool }
impl<'a> RestoreGuard<'a> {
    fn new(seat: &'a Seat, fd: RawFd, saved: SavedCrtc) -> Self {
        Self { seat, fd, saved, showing: false }
    }
    fn present(&mut self, fb: u32, mode: &DrmMode) -> io::Result<()> {
        if !self.seat.active() { return Err(io::Error::other("Sesión inactiva")); }
        let connector = [CONNECTOR_ID];
        let ret = unsafe { drmModeSetCrtc(self.fd, self.saved.crtc_id, fb, 0, 0,
            connector.as_ptr(), 1, mode) };
        if ret != 0 { return Err(drm_error("presentar drmModeSetCrtc", ret)); }
        self.showing = true;
        Ok(())
    }
    fn restore(&mut self) -> io::Result<()> {
        if !self.showing { return Ok(()); }
        if !self.seat.active() {
            return Err(io::Error::other("La sesión cambió; no es seguro restaurar un CRTC desde sesión inactiva"));
        }
        self.saved.restore(self.fd)?;
        self.showing = false;
        Ok(())
    }
}
impl Drop for RestoreGuard<'_> {
    fn drop(&mut self) {
        if self.showing {
            if let Err(e) = self.restore() { eprintln!("ADVERTENCIA: no se restauró KMS: {e}"); }
        }
    }
}

fn require_real_tty() -> io::Result<()> {
    let target = std::fs::read_link("/proc/self/fd/0")?;
    let path = target.to_string_lossy();
    let vt = path.strip_prefix("/dev/tty")
        .map(|s| !s.is_empty() && s.bytes().all(|c| c.is_ascii_digit()))
        .unwrap_or(false);
    if !vt {
        return Err(io::Error::other(format!(
            "--show exige una TTY Linux real (/dev/ttyN), no una terminal gráfica: stdin={path}"
        )));
    }
    Ok(())
}

fn show(path: &str) -> io::Result<()> {
    require_real_tty()?;
    unsafe { signal(2, request_stop); signal(15, request_stop); }
    let seat = Seat::open()?;
    let dev = DrmDevice::open(&seat, path)?;
    let fd = dev.fd;
    let mut cap = 0u64;
    let ret = unsafe { drmGetCap(fd, DRM_CAP_DUMB_BUFFER, &mut cap) };
    if ret != 0 { return Err(drm_error("drmGetCap", ret)); }
    if cap == 0 { return Err(io::Error::other("GPU sin soporte dumb buffer")); }
    let output = select_output(fd)?;
    let saved = SavedCrtc::capture(fd, output.crtc_id)?;
    eprintln!("eDP-1 (ID {CONNECTOR_ID}) en CRTC {}: {}x{} @ {} Hz",
        output.crtc_id, output.mode.hdisplay, output.mode.vdisplay, output.mode.vrefresh);
    eprintln!("Estado previo: framebuffer={}, {} conectores", saved.fb_id, saved.connectors.len());
    let buffer = DumbBuffer::create(fd, output.mode.hdisplay.into(), output.mode.vdisplay.into())?;
    draw(&buffer)?;
    let fb = Framebuffer::new(fd, &buffer)?;
    eprintln!("Dumb buffer: {} bytes | pitch={} | framebuffer KMS={}", buffer.len, buffer.pitch, fb.id);
    eprintln!("Mostrando la imagen durante 3 segundos. Ctrl+C solicita una salida ordenada.");
    let mut guard = RestoreGuard::new(&seat, fd, saved);
    guard.present(fb.id, &output.mode)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !STOP_REQUESTED.load(Ordering::SeqCst) {
        seat.dispatch()?;
        if !seat.active() { break; }
        thread::sleep(Duration::from_millis(20));
    }
    if seat.active() {
        guard.restore()?;
        eprintln!("Pantalla anterior restaurada. FB temporal retirado al salir.");
    } else {
        eprintln!("La sesión perdió el control; no se intentará modeset fuera de la sesión activa.");
    }
    // Importante: guard -> framebuffer -> dumb buffer -> device -> seat.
    drop(guard);
    drop(fb);
    drop(buffer);
    drop(dev);
    drop(seat);
    Ok(())
}

fn main() -> io::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 || args[1] != "--show" {
        eprintln!("DRM-05: primera imagen en pantalla mediante DRM/KMS + Pixman + libseat");
        eprintln!("Por seguridad, no se realiza ningún modeset sin --show.");
        eprintln!("Uso (desde TTY real): {} --show /dev/dri/card1", args[0]);
        return Ok(());
    }
    let path = args.get(2).map(String::as_str).unwrap_or("/dev/dri/card1");
    io::stderr().flush()?;
    show(path)
}
