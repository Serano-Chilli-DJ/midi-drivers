//! Parses the descriptors of every USB device on the build machine. Hubs and
//! keyboards are enough to exercise the sysfs and descriptor code paths.
use ns7ii_usb::{descriptors, sysfs};

#[test]
fn parse_every_local_usb_device() {
    let devs = match sysfs::find(None, None) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("no sysfs USB tree here ({e}); skipping");
            return;
        }
    };
    for d in &devs {
        let topo = descriptors::parse(&d.descriptors).unwrap_or_else(|| panic!("parse failed for {}", d.devnode()));
        let dump = topo.dump();
        assert!(dump.contains("Configuration"), "{}", d.devnode());
        let plan = ns7ii_usb::Plan::derive(&topo, &ns7ii_usb::Overrides::default());
        let _ = plan.describe();
        let _ = sysfs::interface_drivers(d);
    }
    eprintln!("parsed {} devices", devs.len());
}
