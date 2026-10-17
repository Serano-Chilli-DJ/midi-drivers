//! USB transport for the Numark NS7II.
//!
//! * [`sysfs`] finds the device and reads its descriptors without root.
//! * [`descriptors`] turns them into a [`descriptors::Topology`].
//! * [`usbfs`] is the only module with `unsafe`: direct Linux usbfs ioctls,
//!   including isochronous URBs which `nusb` does not offer yet.
//! * [`reactor`] owns every URB of a device on one thread.
//! * [`device`] is the NS7II: endpoint plan, handshake, streams.

#![warn(missing_docs)]

pub mod descriptors;
pub mod device;
pub mod feedback;
pub mod reactor;
pub mod sysfs;
pub mod usbfs;

pub use device::{AudioSource, Error, IsoOutConfig, Ns7ii, Overrides, Plan, Result};
