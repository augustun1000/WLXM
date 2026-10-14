use std::fs::OpenOptions;
use std::os::fd::AsRawFd;
use std::os::raw::c_int;

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

#[link(name = "drm")]
unsafe extern "C" {
    fn drmModeGetResources(fd: c_int)
        -> *mut DrmResources;

    fn drmModeFreeResources(ptr: *mut DrmResources);
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let path = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "/dev/dri/card0".into());

    let device = OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)?;

    println!("Dispositivo DRM: {path}");

    let ptr = unsafe {
        drmModeGetResources(device.as_raw_fd())
    };

    if ptr.is_null() {
        return Err(std::io::Error::last_os_error().into());
    }

    let resources = unsafe { &*ptr };

    println!("CRTCs: {}", resources.count_crtcs);
    println!("Encoders: {}", resources.count_encoders);
    println!("Conectores: {}", resources.count_connectors);

    if resources.count_connectors > 0
        && !resources.connectors.is_null()
    {
        for i in 0..resources.count_connectors as usize {
            let id = unsafe {
                *resources.connectors.add(i)
            };

            println!("Conector {}: ID {}", i, id);
        }
    }

    unsafe {
        drmModeFreeResources(ptr);
    }

    Ok(())
}
