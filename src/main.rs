use std::ffi::c_void;
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::os::fd::AsRawFd;
use std::os::raw::c_int;
use std::ptr;

const WIDTH: u32 = 1366;
const HEIGHT: u32 = 768;
const BPP: u32 = 32;
const DRM_CAP_DUMB_BUFFER: u64 = 1;

const PROT_READ: c_int = 1;
const PROT_WRITE: c_int = 2;
const MAP_SHARED: c_int = 1;

// pixman.h: PIXMAN_FORMAT(32, PIXMAN_TYPE_ARGB, 0, 8, 8, 8)
const PIXMAN_X8R8G8B8: c_int = (32 << 24) | (2 << 16) | (8 << 8) | (8 << 4) | 8;
const PIXMAN_OP_SRC: c_int = 1;

#[repr(C)]
struct PixmanImage {
    _opaque: [u8; 0],
}

#[repr(C)]
struct PixmanColor {
    red: u16,
    green: u16,
    blue: u16,
    alpha: u16,
}

#[repr(C)]
struct PixmanBox32 {
    x1: i32,
    y1: i32,
    x2: i32,
    y2: i32,
}

#[link(name = "drm")]
unsafe extern "C" {
    fn drmGetCap(fd: c_int, capability: u64, value: *mut u64) -> c_int;
    fn drmModeCreateDumbBuffer(
        fd: c_int,
        width: u32,
        height: u32,
        bpp: u32,
        flags: u32,
        handle: *mut u32,
        pitch: *mut u32,
        size: *mut u64,
    ) -> c_int;
    fn drmModeMapDumbBuffer(fd: c_int, handle: u32, offset: *mut u64) -> c_int;
    fn drmModeDestroyDumbBuffer(fd: c_int, handle: u32) -> c_int;
}

#[link(name = "pixman-1")]
unsafe extern "C" {
    fn pixman_image_create_bits(
        format: c_int,
        width: c_int,
        height: c_int,
        bits: *mut u32,
        stride_bytes: c_int,
    ) -> *mut PixmanImage;
    fn pixman_image_fill_boxes(
        op: c_int,
        destination: *mut PixmanImage,
        color: *const PixmanColor,
        n_boxes: c_int,
        boxes: *const PixmanBox32,
    ) -> c_int;
    fn pixman_image_unref(image: *mut PixmanImage) -> c_int;
}

unsafe extern "C" {
    fn mmap(
        addr: *mut c_void,
        len: usize,
        prot: c_int,
        flags: c_int,
        fd: c_int,
        offset: i64,
    ) -> *mut c_void;
    fn munmap(addr: *mut c_void, len: usize) -> c_int;
}

fn drm_error(operation: &str, result: c_int) -> io::Error {
    let source = if result < -1 {
        io::Error::from_raw_os_error(-result)
    } else {
        io::Error::last_os_error()
    };
    io::Error::new(source.kind(), format!("{operation}: {source} (ret={result})"))
}

struct DumbBuffer<'a> {
    file: &'a File,
    handle: u32,
    pitch: u32,
    size: usize,
    map: *mut u8,
}

impl DumbBuffer<'_> {
    fn create(file: &File) -> io::Result<DumbBuffer<'_>> {
        let fd = file.as_raw_fd();
        let mut handle = 0;
        let mut pitch = 0;
        let mut size = 0;
        let result = unsafe {
            drmModeCreateDumbBuffer(
                fd, WIDTH, HEIGHT, BPP, 0,
                &mut handle, &mut pitch, &mut size,
            )
        };
        if result != 0 {
            return Err(drm_error("drmModeCreateDumbBuffer", result));
        }

        // Desde aquí Drop también cubre los errores de validación y mapeo.
        let mut buffer = DumbBuffer {
            file,
            handle,
            pitch,
            size: 0,
            map: ptr::null_mut(),
        };

        let size = usize::try_from(size)
            .map_err(|_| io::Error::other("Buffer demasiado grande"))?;
        let min_pitch = WIDTH.checked_mul(4).unwrap();
        let min_size = u64::from(pitch) * u64::from(HEIGHT);
        if pitch < min_pitch
            || pitch % 4 != 0
            || pitch > i32::MAX as u32
            || u64::try_from(size).unwrap_or(u64::MAX) < min_size
        {
            return Err(io::Error::other("Pitch o tamaño inesperado del driver"));
        }

        let mut offset: u64 = 0;
        let result = unsafe { drmModeMapDumbBuffer(fd, handle, &mut offset) };
        if result != 0 {
            return Err(drm_error("drmModeMapDumbBuffer", result));
        }
        let offset = i64::try_from(offset)
            .map_err(|_| io::Error::other("Offset de mmap no válido"))?;

        let ptr = unsafe {
            mmap(ptr::null_mut(), size, PROT_READ | PROT_WRITE, MAP_SHARED, fd, offset)
        };
        if ptr == usize::MAX as *mut c_void {
            return Err(io::Error::last_os_error());
        }
        buffer.map = ptr.cast::<u8>();
        buffer.size = size;
        Ok(buffer)
    }

    fn read_rgb(&self, x: usize, y: usize) -> [u8; 3] {
        assert!(x < WIDTH as usize && y < HEIGHT as usize);
        let offset = y * self.pitch as usize + x * 4;
        let raw = unsafe { std::slice::from_raw_parts(self.map.add(offset), 3) };
        [raw[2], raw[1], raw[0]] // XRGB8888 es B,G,R,X en memoria x86_64.
    }

    fn save_ppm(&self, path: &str) -> io::Result<()> {
        let mut writer = BufWriter::new(File::create(path)?);
        write!(writer, "P6\n{WIDTH} {HEIGHT}\n255\n")?;
        let mut line = vec![0_u8; WIDTH as usize * 3];
        for y in 0..HEIGHT as usize {
            let offset = y * self.pitch as usize;
            let row = unsafe {
                std::slice::from_raw_parts(self.map.add(offset), WIDTH as usize * 4)
            };
            for x in 0..WIDTH as usize {
                line[3 * x] = row[4 * x + 2];
                line[3 * x + 1] = row[4 * x + 1];
                line[3 * x + 2] = row[4 * x];
            }
            writer.write_all(&line)?;
        }
        writer.flush()
    }
}

impl Drop for DumbBuffer<'_> {
    fn drop(&mut self) {
        if !self.map.is_null() {
            let result = unsafe { munmap(self.map.cast::<c_void>(), self.size) };
            if result != 0 {
                eprintln!("Advertencia: munmap falló: {}", io::Error::last_os_error());
            }
        }
        let result = unsafe { drmModeDestroyDumbBuffer(self.file.as_raw_fd(), self.handle) };
        if result != 0 {
            eprintln!("Advertencia: drmModeDestroyDumbBuffer falló ({result})");
        }
    }
}

struct PixmanView(*mut PixmanImage);
impl Drop for PixmanView {
    fn drop(&mut self) {
        unsafe { pixman_image_unref(self.0); }
    }
}

fn color(red: u8, green: u8, blue: u8) -> PixmanColor {
    PixmanColor {
        red: u16::from(red) * 257,
        green: u16::from(green) * 257,
        blue: u16::from(blue) * 257,
        alpha: 65535,
    }
}

fn fill(image: *mut PixmanImage, box_: PixmanBox32, c: PixmanColor) -> io::Result<()> {
    let ok = unsafe { pixman_image_fill_boxes(PIXMAN_OP_SRC, image, &c, 1, &box_) };
    if ok == 0 {
        return Err(io::Error::other("pixman_image_fill_boxes falló"));
    }
    Ok(())
}

fn draw(buffer: &mut DumbBuffer<'_>) -> io::Result<()> {
    let ptr = unsafe {
        pixman_image_create_bits(
            PIXMAN_X8R8G8B8,
            WIDTH as c_int,
            HEIGHT as c_int,
            buffer.map.cast::<u32>(),
            buffer.pitch as c_int,
        )
    };
    if ptr.is_null() {
        return Err(io::Error::other("pixman_image_create_bits falló"));
    }
    let view = PixmanView(ptr);

    fill(view.0, PixmanBox32 { x1: 0, y1: 0, x2: WIDTH as i32, y2: HEIGHT as i32 }, color(22, 38, 56))?;
    fill(view.0, PixmanBox32 { x1: 100, y1: 80, x2: 780, y2: 480 }, color(14, 116, 144))?;
    fill(view.0, PixmanBox32 { x1: 400, y1: 250, x2: 920, y2: 550 }, color(245, 158, 11))?;
    Ok(())
}

fn main() -> io::Result<()> {
    let path = std::env::args().nth(1).unwrap_or_else(|| "/dev/dri/card1".into());
    let file = OpenOptions::new().read(true).write(true).open(&path)?;
    let mut dumb_cap = 0u64;
    let result = unsafe { drmGetCap(file.as_raw_fd(), DRM_CAP_DUMB_BUFFER, &mut dumb_cap) };
    if result != 0 {
        return Err(drm_error("drmGetCap", result));
    }
    if dumb_cap == 0 {
        return Err(io::Error::other("Este dispositivo no soporta dumb buffers"));
    }

    let mut buffer = DumbBuffer::create(&file)?;
    println!("Dispositivo: {path}");
    println!("Dimensiones: {WIDTH} x {HEIGHT} (XRGB8888)");
    println!("Handle: {} | pitch: {} bytes | tamaño: {} bytes", buffer.handle, buffer.pitch, buffer.size);

    draw(&mut buffer)?;
    for (x, y, expected) in [
        (10, 10, [22, 38, 56]),
        (150, 150, [14, 116, 144]),
        (500, 300, [245, 158, 11]),
    ] {
        let actual = buffer.read_rgb(x, y);
        println!("Píxel ({x},{y}): {actual:?}");
        if actual != expected {
            return Err(io::Error::other(format!("Píxel incorrecto en ({x},{y})")));
        }
    }

    buffer.save_ppm("drm-03.ppm")?;
    println!("Imagen guardada: drm-03.ppm");
    println!("Sin modeset: la pantalla no fue modificada.");
    Ok(())
}
