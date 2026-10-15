//! Descriptor parsing (via `nusb`'s parser) into a small topology model, plus
//! an `lsusb -v`-style dump used by `ns7iid probe`.

use nusb::descriptors::{ConfigurationDescriptor, DeviceDescriptor, TransferType};
use nusb::transfer::Direction;
use std::fmt::Write as _;

/// One endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint {
    /// Address including the direction bit (0x8n = IN).
    pub address: u8,
    /// Transfer type.
    pub kind: EndpointKind,
    /// `wMaxPacketSize` (low 11 bits).
    pub max_packet: usize,
    /// Additional transactions per microframe + 1 (high-speed high-bandwidth).
    pub packets_per_microframe: u8,
    /// `bInterval`.
    pub interval: u8,
    /// `bmAttributes` sync/usage bits for isochronous endpoints.
    pub attributes: u8,
}

impl Endpoint {
    /// True for IN endpoints.
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }
}

/// Transfer type of an endpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointKind {
    /// Control.
    Control,
    /// Isochronous.
    Isochronous,
    /// Bulk.
    Bulk,
    /// Interrupt.
    Interrupt,
}

/// One alternate setting of an interface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AltSetting {
    /// `bAlternateSetting`.
    pub alt: u8,
    /// `bInterfaceClass`.
    pub class: u8,
    /// `bInterfaceSubClass`.
    pub subclass: u8,
    /// `bInterfaceProtocol`.
    pub protocol: u8,
    /// Endpoints in this setting.
    pub endpoints: Vec<Endpoint>,
    /// Class-specific descriptors (raw), e.g. MIDIStreaming jacks.
    pub class_specific: Vec<Vec<u8>>,
}

/// One interface with all its alternate settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interface {
    /// `bInterfaceNumber`.
    pub number: u8,
    /// Alternate settings, ascending.
    pub alts: Vec<AltSetting>,
}

/// Whole device topology.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Topology {
    /// `bcdUSB`.
    pub usb_version: u16,
    /// Device class triple.
    pub device_class: (u8, u8, u8),
    /// `bcdDevice`.
    pub device_version: u16,
    /// `bNumConfigurations`.
    pub num_configurations: u8,
    /// Configurations (usually one).
    pub configurations: Vec<Configuration>,
}

/// One configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Configuration {
    /// `bConfigurationValue`.
    pub value: u8,
    /// `bmAttributes`.
    pub attributes: u8,
    /// `bMaxPower` in 2 mA units.
    pub max_power_2ma: u8,
    /// Interfaces.
    pub interfaces: Vec<Interface>,
}

/// Parse the sysfs `descriptors` blob (device descriptor followed by every
/// configuration descriptor).
pub fn parse(blob: &[u8]) -> Option<Topology> {
    let dev = DeviceDescriptor::new(blob)?;
    let mut topo = Topology {
        usb_version: dev.usb_version(),
        device_class: (dev.class(), dev.subclass(), dev.protocol()),
        device_version: dev.device_version(),
        num_configurations: dev.num_configurations(),
        configurations: Vec::new(),
    };
    let mut off = 18usize;
    while off + 9 <= blob.len() {
        let total = u16::from_le_bytes([blob[off + 2], blob[off + 3]]) as usize;
        if blob[off + 1] != 0x02 || total < 9 || off + total > blob.len() {
            break;
        }
        if let Some(cfg) = ConfigurationDescriptor::new(&blob[off..off + total]) {
            topo.configurations.push(convert_config(&cfg));
        }
        off += total;
    }
    Some(topo)
}

fn convert_config(cfg: &ConfigurationDescriptor<'_>) -> Configuration {
    let mut interfaces = Vec::new();
    for ig in cfg.interfaces() {
        let mut alts = Vec::new();
        for alt in ig.alt_settings() {
            let mut endpoints = Vec::new();
            for ep in alt.endpoints() {
                let kind = match ep.transfer_type() {
                    TransferType::Control => EndpointKind::Control,
                    TransferType::Isochronous => EndpointKind::Isochronous,
                    TransferType::Bulk => EndpointKind::Bulk,
                    TransferType::Interrupt => EndpointKind::Interrupt,
                };
                let _ = Direction::In; // keep the import meaningful on all nusb versions
                endpoints.push(Endpoint {
                    address: ep.address(),
                    kind,
                    max_packet: ep.max_packet_size(),
                    packets_per_microframe: ep.packets_per_microframe(),
                    interval: ep.interval(),
                    attributes: ep.attributes(),
                });
            }
            let class_specific = alt
                .descriptors()
                .filter(|d| d.descriptor_type() == 0x24 || d.descriptor_type() == 0x25)
                .map(|d| d.to_vec())
                .collect();
            alts.push(AltSetting {
                alt: alt.alternate_setting(),
                class: alt.class(),
                subclass: alt.subclass(),
                protocol: alt.protocol(),
                endpoints,
                class_specific,
            });
        }
        alts.sort_by_key(|a| a.alt);
        interfaces.push(Interface { number: ig.interface_number(), alts });
    }
    interfaces.sort_by_key(|i| i.number);
    Configuration {
        value: cfg.configuration_value(),
        attributes: cfg.attributes(),
        max_power_2ma: cfg.max_power(),
        interfaces,
    }
}

impl Topology {
    /// First interface whose first-or-any alt has the given class/subclass.
    pub fn find_interface(&self, class: u8, subclass: Option<u8>) -> Option<&Interface> {
        self.configurations.first()?.interfaces.iter().find(|i| {
            i.alts.iter().any(|a| a.class == class && subclass.is_none_or(|s| a.subclass == s))
        })
    }

    /// Every endpoint of the first configuration, with its interface and alt.
    pub fn endpoints(&self) -> impl Iterator<Item = (u8, u8, &Endpoint)> {
        self.configurations
            .iter()
            .take(1)
            .flat_map(|c| c.interfaces.iter())
            .flat_map(|i| i.alts.iter().map(move |a| (i.number, a)))
            .flat_map(|(n, a)| a.endpoints.iter().map(move |e| (n, a.alt, e)))
    }

    /// Human-readable dump.
    pub fn dump(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "Device: USB {:x}.{:02x}  class {:02x}/{:02x}/{:02x}  bcdDevice {:04x}  configurations {}",
            self.usb_version >> 8,
            self.usb_version & 0xFF,
            self.device_class.0,
            self.device_class.1,
            self.device_class.2,
            self.device_version,
            self.num_configurations
        );
        for c in &self.configurations {
            let _ = writeln!(
                s,
                "  Configuration {}  attrs {:02x}  max power {} mA",
                c.value,
                c.attributes,
                c.max_power_2ma as u32 * 2
            );
            for i in &c.interfaces {
                for a in &i.alts {
                    let _ = writeln!(
                        s,
                        "    Interface {} alt {}  class {:02x}/{:02x}/{:02x}  {} endpoint(s){}",
                        i.number,
                        a.alt,
                        a.class,
                        a.subclass,
                        a.protocol,
                        a.endpoints.len(),
                        describe_class(a.class, a.subclass)
                    );
                    for cs in &a.class_specific {
                        let _ = writeln!(s, "      class-specific: {}", hex(cs));
                    }
                    for e in &a.endpoints {
                        let _ = writeln!(
                            s,
                            "      EP {:02x} {:<4} {:<11} wMaxPacketSize {:>4} x{}  bInterval {}  attrs {:02x}",
                            e.address,
                            if e.is_in() { "IN" } else { "OUT" },
                            format!("{:?}", e.kind).to_lowercase(),
                            e.max_packet,
                            e.packets_per_microframe,
                            e.interval,
                            e.attributes
                        );
                    }
                }
            }
        }
        s
    }
}

fn describe_class(class: u8, subclass: u8) -> &'static str {
    match (class, subclass) {
        (0xFF, _) => "  [vendor specific: Ploytec audio]",
        (0x01, 0x01) => "  [audio control]",
        (0x01, 0x02) => "  [audio streaming]",
        (0x01, 0x03) => "  [USB-MIDI class: Linux snd-usb-audio handles this]",
        (0x03, _) => "  [HID]",
        _ => "",
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Xone:DB2 descriptor set from Ozzy's capture, as a stand-in until an
    /// NS7II `lsusb -v` is available. Exercises vendor-class parsing.
    fn db2_blob() -> Vec<u8> {
        let mut b = vec![
            0x12, 0x01, 0x00, 0x02, 0x00, 0x00, 0x00, 0x40, 0x4A, 0x0A, 0xD2, 0xFF, 0x00, 0x01, 0x01, 0x02, 0x03, 0x01,
        ];
        let cfg: Vec<u8> = vec![
            0x09, 0x02, 0x50, 0x00, 0x02, 0x01, 0x00, 0xC0, 0x00, // config, 80 bytes
            0x09, 0x04, 0x00, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, // if0 alt0
            0x09, 0x04, 0x00, 0x01, 0x04, 0xFF, 0x00, 0x00, 0x00, // if0 alt1
            0x07, 0x05, 0x02, 0x01, 0x04, 0x00, 0x01, // EP 02 iso 4
            0x07, 0x05, 0x83, 0x02, 0x00, 0x02, 0x00, // EP 83 bulk 512
            0x07, 0x05, 0x05, 0x03, 0x00, 0x02, 0x01, // EP 05 int 512
            0x07, 0x05, 0x04, 0x02, 0x00, 0x02, 0x00, // EP 04 bulk 512
            0x09, 0x04, 0x01, 0x00, 0x00, 0xFF, 0x00, 0x00, 0x00, // if1 alt0
            0x09, 0x04, 0x01, 0x01, 0x01, 0xFF, 0x00, 0x00, 0x00, // if1 alt1
            0x07, 0x05, 0x86, 0x03, 0x00, 0x02, 0x01, // EP 86 int 512
        ];
        assert_eq!(cfg.len(), 80);
        b.extend(cfg);
        b
    }

    #[test]
    fn parses_vendor_topology() {
        let t = parse(&db2_blob()).unwrap();
        assert_eq!(t.configurations.len(), 1);
        let c = &t.configurations[0];
        assert_eq!(c.interfaces.len(), 2);
        assert_eq!(c.interfaces[0].alts[1].endpoints.len(), 4);
        let ep = &c.interfaces[0].alts[1].endpoints[2];
        assert_eq!(ep.address, 0x05);
        assert_eq!(ep.kind, EndpointKind::Interrupt);
        assert_eq!(ep.max_packet, 512);
        assert!(t.find_interface(0xFF, None).is_some());
        assert!(t.find_interface(0x01, Some(0x03)).is_none());
        assert_eq!(t.endpoints().count(), 5);
        assert!(t.dump().contains("EP 86 IN"));
    }
}
