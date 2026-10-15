
use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::raw::{c_char, c_int};

#[repr(C)]
struct DrmResources {
    count_fbs: c_int,
    fbs: *mut u32,
    count_crtcs: c_int,
    crtcs: *mut u32,
    count_connectors: c_int,
    connectors: *mut u32,
    count_encoders: c_int,
    encoders: *mut u32,
    min_width: u32,
    max_width: u32,
    min_height: u32,
    max_height: u32,
}

#[repr(C)]
struct DrmMode {
    clock: u32,
    hdisplay: u16,
    hsync_start: u16,
    hsync_end: u16,
    htotal: u16,
    hskew: u16,
    vdisplay: u16,
    vsync_start: u16,
    vsync_end: u16,
    vtotal: u16,
    vscan: u16,
    vrefresh: u32,
    flags: u32,
    mode_type: u32,
    name: [c_char; 32],
}

#[repr(C)]
struct DrmConnector {
    connector_id: u32,
    encoder_id: u32,
    connector_type: u32,
    connector_type_id: u32,
    connection: c_int,
    mm_width: u32,
    mm_height: u32,
    subpixel: c_int,
    count_modes: c_int,
    modes: *mut DrmMode,
    count_props: c_int,
    props: *mut u32,
    prop_values: *mut u64,
    count_encoders: c_int,
    encoders: *mut u32,
}

#[link(name = "drm")]
unsafe extern "C" {
    fn drmModeGetResources(fd: c_int)
        -> *mut DrmResources;
    fn drmModeFreeResources(ptr: *mut DrmResources);

    fn drmModeGetConnector(
        fd: c_int,
        id: u32,
    ) -> *mut DrmConnector;

    fn drmModeFreeConnector(ptr: *mut DrmConnector);
}

fn tipo_conector(tipo: u32) -> &'static str {
    match tipo {
        1 => "VGA",
        7 => "LVDS",
        10 => "DisplayPort",
        11 => "HDMI-A",
        14 => "eDP",
        15 => "Virtual",
        16 => "DSI",
        _ => "Otro",
    }
}

fn estado(conexion: c_int) -> &'static str {
    match conexion {
        1 => "CONECTADO",
        2 => "desconectado",
        _ => "desconocido",
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card1".into());

    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;

    let fd = device.as_raw_fd();

    let resources = unsafe { drmModeGetResources(fd) };

    if resources.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }

    let res = unsafe { &*resources };

    println!("Dispositivo: {path}");
    println!("CRTCs: {}", res.count_crtcs);
    println!("Conectores: {}", res.count_connectors);

    for i in 0..res.count_connectors.max(0) as usize {
        if res.connectors.is_null() {
            break;
        }

        let id = unsafe { *res.connectors.add(i) };
        let ptr = unsafe { drmModeGetConnector(fd, id) };

        if ptr.is_null() {
            eprintln!("Error al consultar conector {id}");
            continue;
        }

        let conn = unsafe { &*ptr };

        println!("\nConector ID: {}", conn.connector_id);
        println!(
            "Tipo: {}-{}",
            tipo_conector(conn.connector_type),
            conn.connector_type_id
        );
        println!("Estado: {}", estado(conn.connection));
        println!("Encoder actual: {}", conn.encoder_id);

        if conn.connection == 1 && !conn.modes.is_null() {
            for j in 0..conn.count_modes.max(0) as usize {
                let mode = unsafe { &*conn.modes.add(j) };

                let preferido = if mode.mode_type & (1 << 3) != 0 {
                    " [PREFERIDO]"
                } else {
                    ""
                };

                println!(
                    "  {}x{} @ {} Hz{}",
                    mode.hdisplay,
                    mode.vdisplay,
                    mode.vrefresh,
                    preferido
                );
            }
        }

        unsafe { drmModeFreeConnector(ptr) };
    }

    unsafe { drmModeFreeResources(resources) };

    Ok(())
}

