//! Device discovery through sysfs (`/sys/bus/usb/devices`). No root needed.

use std::fs;
use std::path::{Path, PathBuf};

/// A USB device found in sysfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysfsDevice {
    /// sysfs directory, e.g. `/sys/bus/usb/devices/1-3`.
    pub sysfs_path: PathBuf,
    /// Bus number.
    pub bus: u8,
    /// Device address on the bus.
    pub address: u8,
    /// `idVendor`.
    pub vendor_id: u16,
    /// `idProduct`.
    pub product_id: u16,
    /// Speed string from sysfs (`1.5`, `12`, `480`, `5000`...).
    pub speed_mbps: Option<u32>,
    /// `manufacturer` string if exported.
    pub manufacturer: Option<String>,
    /// `product` string if exported.
    pub product: Option<String>,
    /// `bcdDevice` from sysfs.
    pub bcd_device: Option<u16>,
    /// Raw descriptors file (device + configuration descriptors, concatenated).
    pub descriptors: Vec<u8>,
}

impl SysfsDevice {
    /// Path of the usbfs node for this device.
    pub fn devnode(&self) -> String {
        format!("/dev/bus/usb/{:03}/{:03}", self.bus, self.address)
    }
}

fn read_trim(p: &Path, name: &str) -> Option<String> {
    fs::read_to_string(p.join(name)).ok().map(|s| s.trim().to_string())
}

fn read_hex16(p: &Path, name: &str) -> Option<u16> {
    read_trim(p, name).and_then(|s| u16::from_str_radix(&s, 16).ok())
}

/// List every USB device whose VID/PID match (`None` matches anything).
pub fn find(vendor: Option<u16>, product: Option<u16>) -> std::io::Result<Vec<SysfsDevice>> {
    let mut out = Vec::new();
    let root = Path::new("/sys/bus/usb/devices");
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let name = entry.file_name().to_string_lossy().into_owned();
        // Device directories look like "1-3" or "1-3.2"; interfaces contain ':'.
        if name.contains(':') || name.starts_with("usb") {
            continue;
        }
        let p = entry.path();
        let (Some(vid), Some(pid)) = (read_hex16(&p, "idVendor"), read_hex16(&p, "idProduct")) else {
            continue;
        };
        if vendor.is_some_and(|v| v != vid) || product.is_some_and(|v| v != pid) {
            continue;
        }
        let bus = read_trim(&p, "busnum").and_then(|s| s.parse().ok()).unwrap_or(0);
        let address = read_trim(&p, "devnum").and_then(|s| s.parse().ok()).unwrap_or(0);
        let speed_mbps = read_trim(&p, "speed").and_then(|s| s.parse::<f32>().ok()).map(|f| f as u32);
        let descriptors = fs::read(p.join("descriptors")).unwrap_or_default();
        out.push(SysfsDevice {
            sysfs_path: p.clone(),
            bus,
            address,
            vendor_id: vid,
            product_id: pid,
            speed_mbps,
            manufacturer: read_trim(&p, "manufacturer"),
            product: read_trim(&p, "product"),
            bcd_device: read_hex16(&p, "bcdDevice"),
            descriptors,
        });
    }
    out.sort_by_key(|d| (d.bus, d.address));
    Ok(out)
}

/// Kernel drivers currently bound to each interface of a device, from sysfs.
pub fn interface_drivers(dev: &SysfsDevice) -> Vec<(u8, Option<String>)> {
    let mut out = Vec::new();
    let Ok(rd) = fs::read_dir(&dev.sysfs_path) else { return out };
    let prefix = format!("{}:", dev.sysfs_path.file_name().unwrap_or_default().to_string_lossy());
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        let Some(ifnum) = read_trim(&e.path(), "bInterfaceNumber").and_then(|s| u8::from_str_radix(&s, 16).ok()) else {
            continue;
        };
        let driver = fs::read_link(e.path().join("driver"))
            .ok()
            .and_then(|l| l.file_name().map(|f| f.to_string_lossy().into_owned()));
        out.push((ifnum, driver));
    }
    out.sort();
    out
}
