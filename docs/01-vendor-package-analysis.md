# What is in the vendor packages

Analysed on 2026-10-15 from the files in `tmp/`.

## Inventory

| File | Really is | Date |
|------|-----------|------|
| `drivers/windows/` (x86 + x64) | Ploytec "usb-audio.de" USB audio driver 2.9.55: `ns7ii_usb.sys` (bus driver), `ns7ii_wdm.sys` (WDM audio), `ns7ii_midi.sys` (WDM MIDI), ASIO DLLs, control panel | 2013-05-15 |
| `drivers/mac/Numark NS7 II_3.3.17.dmg` | Ploytec macOS driver 3.3.17: `NumarkNS7IIAudio.kext` (+ a signed copy for 10.9+), CoreAudio plugin, CoreMIDI plugin, HAL plugin, control panel app | 2016-12-05 |
| `firmware/mac/...dmg` | Byte-identical to the Mac driver DMG (same MD5) | 2016-12 |
| `firmware/windows/Numark_NS7II_Firmware_Updater_setup.exe` | InstallShield wrapper around an MSI containing `ns7ii_firmware_updater.exe`, a JUCE application with an embedded Intel-HEX firmware image | 2015-08-11 |

Nothing in these packages is Numark code except artwork, the INF strings and
the firmware image. Everything that talks USB is Ploytec GmbH's generic
driver, which also ships under the Allen & Heath, Reloop, TEAC, Dotec, SPL
and Elektron names. That matters because the same protocol has already been
reverse engineered for the Allen & Heath Xone mixers (the open-source Ozzy
driver, `github.com/mischa85/snd-xonedb4`) and for the Reloop Jockey 3 (a
Linux kernel patch series by Frank van de Pol, 2026). This project reuses
their findings wherever the NS7II shares the code path.

## Windows driver

* INF files bind `USB\VID_15E4&PID_002C` as a whole device (`ns7ii_usb.inf`,
  class USB) and `&MI_00` as a MIDI device (`ns7ii_midi.inf`, class MEDIA).
  The WDM audio driver exposes a device `MEDIA\NUMARKNS7IIAUDIOADAPTER`.
* `ns7ii_usb.sys` is the Ploytec `PGDevice` engine. Its strings name the
  code paths used: `PROCESSING ISOC OUT`, `rtsServiceBulkOutBulkIn`,
  `ISOC FEEDBACK CALLBACK`, `USBMidiPattern::initForBulk`, `Scanned Firmware
  Version is :%08X`, `MIDI readstringdesc`, `USB: Hide MIDI Interface`.
* The drivers are signed with a 2013 cross-certificate chain; Windows 10
  build 1607+ rejects such drivers on clean installs unless Secure Boot is
  off or the "cross-signed drivers" grandfather rule applies. There is no
  32-bit-USB issue; the x64 build is a native 64-bit kernel driver.

## macOS driver (the key source)

The kext `NumarkNS7IIAudio` is **not stripped**: 1267 C++ symbols such as
`PGDevice::scanFirmwareVersion`, `PGKernelDeviceNUMARKNS7II::setFrequency`,
`PGConfigurationDescriptor::findMidiInterface`, `pcmTo24Junction4CH`,
`isPloytecBulkInIsocOutDevice`. Disassembling it (capstone, see
`tools/disas.py`) gave the device-specific facts in `02-usb-protocol.md`.

Its `Info.plist` matches `IOUSBDevice` with `idVendor 5604 (0x15E4)`,
`idProduct 44 (0x2C)` and carries a `PtSpecific` dictionary:

* `kConfigurationData`: four stream records, all 24-bit at 44 100 Hz, with
  2, 2, 12 and 4 channels (full-speed in/out, high-speed in/out).
* `kInACR = 8`, `kMsWait = 200`, one MIDI port named "MIDI".
* `kOutChannelNames`: Master OUT, OUT 1, OUT 2, OUT 3, OUT 4.

The kext only loads on macOS 10.8 to 10.15 era kernels (it was built with
the 10.8 SDK and `IOUSBFamily`, which Apple replaced with `IOUSBHostFamily`),
is not notarized, and is Intel-only. That is why it "stopped working".

## Firmware updater

`ns7ii_firmware_updater.exe` (x86, JUCE framework) contains:

* an Intel-HEX image of 3 265 records mapped at `0x08004000..0x0801F83F`
  (112 704 bytes). The address and the Thumb code identify an **STM32**
  microcontroller with a 16 KB bootloader; this is the control-surface MCU
  (buttons, platters, LEDs, displays), not the USB audio chip.
* classes `FirmwareSysexSender`, `MidiManager`, strings "Erasing Current
  Firmware", "Sending App Firmware", "CRC error", "Press the blinking CUE
  button on the right deck to reset your NS7 II", "NS7 II Emergency Update:
  1. Power NS7II while holding LOAD A + LOAD B + FWD".
* It talks to the device through the ordinary Windows MIDI API
  (`midiOutLongMsg` to the port "Numark NS7 II MIDI"). So the firmware
  update protocol is **MIDI SysEx over the normal MIDI port**; the raw USB
  protocol is not involved, and the USB audio chip's firmware is never
  touched by this tool.

The USB descriptors therefore are not in any file we have; they live in the
audio chip's firmware. `ns7iid probe` reads them from the device.

## How the packages were opened (tools/)

* `udif.py`: decompress an Apple UDIF (`.dmg`) image (zlib/bzip2 chunks) to a raw disk image.
* `xar.py`: carve and extract the `.pkg` (xar) archive from the HFS+ image; payloads are gzip cpio.
* `isstream.py`: decode the InstallShield `ISSetupStream` container (key = file name ⊕ 13 35 86 07, nibble swap, inflate) to get the MSI; the MSI's CAB was opened with `olefile` + `cabarchive`.
* `disas.py`: disassemble a named function of a Mach-O kext with symbol and string annotations (lief + capstone).
