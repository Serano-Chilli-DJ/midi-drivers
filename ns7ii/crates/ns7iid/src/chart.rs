//! The NS7II MIDI chart, built in, so `mixxx-map` needs no capture.
//!
//! Sources: SuperC0C0's capture spreadsheet on the Mixxx forum (2022),
//! cross-checked message by message against a `midi-learn` capture of a
//! real unit (2026-10-15) and against the control-surface firmware's MIDI
//! input handlers. Every message here was seen on hardware; see
//! `docs/05-midi-chart.md` for the table and the few rows left out.
//!
//! Layout: deck `n` (1..4) talks on MIDI channel `n` (status `0x9n`/`0x8n`/
//! `0xBn`); the left hardware deck is deck 1 or 3, the right one deck 2 or 4,
//! selected with the DECK buttons. Everything else (mixer, FX, library) is
//! on channel 0 (`0x90`/`0xB0`). Mixer strips are numbered by their printed
//! channel number; physically they sit in the order 3, 1, 2, 4.

use crate::learn::{Capture, Kind, Learned, Msg};
use std::collections::BTreeMap;

type Section = BTreeMap<String, Capture>;

fn cap(kind: Kind, msgs: &[(u8, u8, u8)]) -> Capture {
    Capture { kind, messages: msgs.iter().map(|&(s, a, b)| Msg { status: s, data: vec![a, b] }).collect() }
}

/// Note button with note-off release.
fn btn(ch: u8, note: u8) -> Capture {
    cap(Kind::Button, &[(0x90 | ch, note, 0x7F), (0x80 | ch, note, 0x00)])
}
/// 14-bit controller: MSB number and LSB number (always MSB + 0x20 on the NS7II).
fn pot14(ch: u8, msb: u8) -> Capture {
    let s = 0xB0 | ch;
    cap(Kind::Pot, &[(s, msb, 1), (s, msb + 0x20, 2), (s, msb, 3), (s, msb + 0x20, 4)])
}
/// 7-bit controller.
fn pot7(ch: u8, cc: u8) -> Capture {
    let s = 0xB0 | ch;
    cap(Kind::Pot, &[(s, cc, 1), (s, cc, 2), (s, cc, 3)])
}
/// Relative encoder, two's complement (clockwise 1, anticlockwise 0x7F).
fn enc(ch: u8, cc: u8) -> Capture {
    let s = 0xB0 | ch;
    cap(Kind::Encoder, &[(s, cc, 1), (s, cc, 1), (s, cc, 0x7F)])
}
/// Platter: 7-bit absolute position counter that wraps.
fn platter(ch: u8, cc: u8) -> Capture {
    let s = 0xB0 | ch;
    let msgs: Vec<(u8, u8, u8)> = (0..40u8).map(|i| (s, cc, i * 3)).collect();
    cap(Kind::Platter, &msgs)
}
/// Strip search: 7-bit absolute position.
fn strip(ch: u8, cc: u8) -> Capture {
    let s = 0xB0 | ch;
    let msgs: Vec<(u8, u8, u8)> = (0..20u8).map(|i| (s, cc, i * 6)).collect();
    cap(Kind::Strip, &msgs)
}

fn put(sec: &mut Section, id: &str, c: Capture) {
    sec.insert(id.to_string(), c);
}

/// One deck. `n` is the deck number (1..4) and also its MIDI channel.
fn deck(n: u8) -> Section {
    let ch = n;
    let mut s = Section::new();
    // Transport.
    put(&mut s, "load", btn(ch, 0x31));
    put(&mut s, "sync", btn(ch, 0x32));
    put(&mut s, "cue", btn(ch, 0x33));
    put(&mut s, "play", btn(ch, 0x34));
    put(&mut s, "shift", btn(ch, 0x35));
    // Hot cue buttons 1-5 (the dedicated row above the pads).
    for (i, note) in (0x36u8..=0x3A).enumerate() {
        put(&mut s, &format!("hotcue{}", i + 1), btn(ch, note));
    }
    put(&mut s, "pitch_bend_minus", btn(ch, 0x3B));
    put(&mut s, "pitch_bend_plus", btn(ch, 0x3C));
    put(&mut s, "range", btn(ch, 0x3D)); // RANGE / MASTER TEMPO (shift = keylock)
    put(&mut s, "censor", btn(ch, 0x3E)); // BLEEP position of the switch
    put(&mut s, "reverse", btn(ch, 0x3F)); // REVERSE position of the switch
    put(&mut s, "tap", btn(ch, 0x41)); // TAP (BPM)
    put(&mut s, "grid_adjust", btn(ch, 0x42)); // ADJUST / SET
    put(&mut s, "grid_slip", btn(ch, 0x43)); // SLIP / CLEAR (beat grid)
    put(&mut s, "slip", btn(ch, 0x44)); // SLIP MODE / MOTOR OFF
    put(&mut s, "param_left", btn(ch, 0x45));
    put(&mut s, "param_right", btn(ch, 0x46));
    // Performance pads (also send channel pressure on channel 0, ignored).
    for (i, note) in (0x47u8..=0x4E).enumerate() {
        put(&mut s, &format!("pad{}", i + 1), btn(ch, note));
    }
    put(&mut s, "pad_mode_cue", btn(ch, 0x4F));
    put(&mut s, "pad_mode_autoloop", btn(ch, 0x50)); // AUTO / ROLL
    put(&mut s, "pad_mode_loop", btn(ch, 0x51)); // LOOP (manual)
    put(&mut s, "pad_mode_sampler", btn(ch, 0x52));
    put(&mut s, "pad_mode_slicer", btn(ch, 0x53));
    // Continuous controls.
    put(&mut s, "platter_turn", platter(ch, 0x00));
    put(&mut s, "rate", pot14(ch, 0x01));
    put(&mut s, "strip", strip(ch, 0x02));
    put(&mut s, "stop_time", pot7(ch, 0x0C));
    put(&mut s, "start_time", pot7(ch, 0x0D));
    // DECK select button for this deck (channel 0).
    put(&mut s, "deck_select", btn(0, 0x01 + n));
    // FX section of this side (channel 0). Decks 1/3 share the left unit and
    // 2/4 the right one, so it is listed on decks 1 and 2 only.
    if n <= 2 {
        let (b, k) = if n == 1 { (0x0Fu8, 0x04u8) } else { (0x13, 0x49) };
        put(&mut s, "fx1_on", btn(0, b));
        put(&mut s, "fx2_on", btn(0, b + 1));
        put(&mut s, "fx3_on", btn(0, b + 2));
        put(&mut s, "fx_tap", btn(0, b + 3)); // BEAT / MODE
        put(&mut s, "fx1_knob", enc(0, k));
        put(&mut s, "fx2_knob", enc(0, k + 1));
        put(&mut s, "fx3_knob", enc(0, k + 2));
        put(&mut s, "fx_beats", enc(0, if n == 1 { 0x48 } else { 0x4C }));
    }
    s
}

/// Mixer strip for printed channel `n` (1..4).
fn mixer(n: u8) -> Section {
    let mut s = Section::new();
    let (gain, fader, filter, pfl, fx_a, xf_a) = match n {
        1 => (0x0C, 0x08, 0x5B, 0x17, 0x23, 0x1B),
        2 => (0x11, 0x0D, 0x5C, 0x18, 0x25, 0x1D),
        3 => (0x17, 0x13, 0x5A, 0x19, 0x27, 0x1F),
        _ => (0x1C, 0x18, 0x5D, 0x1A, 0x29, 0x21),
    };
    put(&mut s, "gain", pot14(0, gain));
    put(&mut s, "treble", pot14(0, gain - 1));
    put(&mut s, "mid", pot14(0, gain - 2));
    put(&mut s, "bass", pot14(0, gain - 3));
    put(&mut s, "filter", pot14(0, filter));
    put(&mut s, "fader", pot14(0, fader));
    put(&mut s, "pfl", btn(0, pfl));
    put(&mut s, "fx_a", btn(0, fx_a)); // FX ASSIGN A
    put(&mut s, "fx_b", btn(0, fx_a + 1)); // FX ASSIGN B
    put(&mut s, "xf_a", btn(0, xf_a)); // crossfader assign switch: A position
    put(&mut s, "xf_b", btn(0, xf_a + 1)); // crossfader assign switch: B position
    s
}

fn master() -> Section {
    let mut s = Section::new();
    put(&mut s, "crossfader", pot14(0, 0x07));
    put(&mut s, "cue_mix", pot14(0, 0x12)); // headphone CUE BLEND
    put(&mut s, "master", pot7(0, 0x42));
    put(&mut s, "booth", pot7(0, 0x43));
    put(&mut s, "cue_gain", pot7(0, 0x44)); // headphone volume
    put(&mut s, "xf_curve", pot7(0, 0x41)); // crossfader slope
    put(&mut s, "mic_treble", pot7(0, 0x45));
    put(&mut s, "mic_bass", pot7(0, 0x46));
    put(&mut s, "mic", pot7(0, 0x47)); // mic gain
    put(&mut s, "browse_turn", enc(0, 0x03));
    put(&mut s, "split_cue", btn(0, 0x00)); // front panel switch
    put(&mut s, "view", btn(0, 0x01)); // PANEL / VIEW
    put(&mut s, "back", btn(0, 0x06));
    put(&mut s, "fwd", btn(0, 0x07));
    put(&mut s, "browse_push", btn(0, 0x08));
    put(&mut s, "prepare", btn(0, 0x09));
    put(&mut s, "files", btn(0, 0x0A));
    put(&mut s, "crates", btn(0, 0x0B));
    put(&mut s, "load_prepare", btn(0, 0x0C));
    put(&mut s, "touch_mode", btn(0, 0x0D));
    put(&mut s, "filter_mode", btn(0, 0x0E)); // FILTER ROLL / FILTER FX
    put(&mut s, "master_fx_a", btn(0, 0x2B));
    put(&mut s, "master_fx_b", btn(0, 0x2C));
    put(&mut s, "mic_on", btn(0, 0x6E)); // front panel switch
    put(&mut s, "browse_touch", btn(0, 0x6C));
    put(&mut s, "motor_torque", btn(0, 0x74)); // rear panel switch
    s
}

/// The complete built-in chart as a capture, so the mapping generator
/// treats it exactly like a `midi-learn` result.
pub fn builtin() -> Learned {
    let mut l = Learned { device: "Numark NS7II".into(), firmware: "built-in chart".into(), complete: true, sections: BTreeMap::new() };
    for n in 1..=4u8 {
        l.sections.insert(format!("deck{n}"), deck(n));
        l.sections.insert(format!("mixer{n}"), mixer(n));
    }
    l.sections.insert("master".into(), master());
    l
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decks_use_their_own_channel_and_known_notes() {
        let l = builtin();
        for n in 1..=4u8 {
            let d = &l.sections[&format!("deck{n}")];
            assert_eq!(d["play"].messages[0].status, 0x90 | n);
            assert_eq!(d["play"].messages[0].data[0], 0x34);
            assert_eq!(d["pad1"].messages[0].data[0], 0x47);
            assert_eq!(d["rate"].messages[0].status, 0xB0 | n);
        }
        // FX lives on the side decks only.
        assert!(l.sections["deck1"].contains_key("fx1_on"));
        assert!(!l.sections["deck3"].contains_key("fx1_on"));
        // Mixer strip 3 is the leftmost: fader CC 0x13, filter 0x5A.
        let m3 = &l.sections["mixer3"];
        assert_eq!(m3["fader"].messages[0].data[0], 0x13);
        assert_eq!(m3["filter"].messages[0].data[0], 0x5A);
    }

    #[test]
    fn chart_matches_hardware_capture() {
        // Values seen on a real unit on 2026-10-15 (left side on deck 3, right on deck 2).
        let l = builtin();
        let d3 = &l.sections["deck3"];
        assert_eq!((d3["cue"].messages[0].status, d3["cue"].messages[0].data[0]), (0x93, 51));
        assert_eq!((d3["shift"].messages[0].status, d3["shift"].messages[0].data[0]), (0x93, 53));
        let d2 = &l.sections["deck2"];
        assert_eq!((d2["load"].messages[0].status, d2["load"].messages[0].data[0]), (0x92, 49));
        assert_eq!((d2["fx1_on"].messages[0].status, d2["fx1_on"].messages[0].data[0]), (0x90, 19));
        assert_eq!(d2["platter_turn"].messages[0].status, 0xB2);
        let m = &l.sections["master"];
        assert_eq!(m["crossfader"].messages[1].data[0], 39);
        assert_eq!(m["cue_gain"].messages[0].data[0], 68);
        assert_eq!(m["back"].messages[0].data[0], 6);
        let m1 = &l.sections["mixer1"];
        assert_eq!(m1["fader"].messages[0].data[0], 8);
        let m4 = &l.sections["mixer4"];
        assert_eq!(m4["fader"].messages[0].data[0], 24);
        assert_eq!(m4["filter"].messages[0].data[0], 93);
    }
}
