//! Turn a `learn.json` capture into a Mixxx controller mapping:
//! `Numark NS7II.midi.xml` + `Numark-NS7II-scripts.js`.
//!
//! The script is a static template (`templates/Numark-NS7II-scripts.js`)
//! that builds its components from a `NS7II.MAP` object; this module
//! produces that object and the XML that routes every MIDI message to the
//! right component input.

use crate::learn::{Capture, Kind, Learned, Msg};
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// How a control reaches Mixxx.
#[derive(Debug, Clone, PartialEq)]
enum Binding {
    /// Press message (status, number) and optional distinct release message.
    Button { press: (u8, u8), release: Option<(u8, u8)> },
    /// 7-bit absolute controller.
    Pot7 { msg: (u8, u8) },
    /// 14-bit absolute controller (MSB, LSB).
    Pot14 { msb: (u8, u8), lsb: (u8, u8) },
    /// Pitch bend (14-bit) on a channel.
    PitchBend { status: u8 },
    /// Relative encoder.
    Encoder { msg: (u8, u8) },
    /// Platter: relative ticks, or absolute 7/14-bit position.
    Platter { msg: (u8, u8), lsb: Option<(u8, u8)>, mode: &'static str },
}

fn kind(status: u8) -> u8 {
    status & 0xF0
}

fn analyze(c: &Capture) -> Option<Binding> {
    let msgs: Vec<&Msg> = c.messages.iter().filter(|m| m.status < 0xF0 && m.data.len() >= 2).collect();
    if msgs.is_empty() {
        return None;
    }
    // Group by (status kind, channel, number).
    let mut groups: BTreeMap<(u8, u8), Vec<u8>> = BTreeMap::new();
    for m in &msgs {
        groups.entry((m.status, m.data[0])).or_default().push(m.data[1]);
    }
    match c.kind {
        Kind::Button => {
            // Candidates: note-on or CC with a value > 0. The NS7II sends extra
            // activity notes next to some buttons (e.g. `90 6C 7F / 90 6C 00`
            // before BACK's own `90 06 7F / 80 06 00`), so prefer the candidate
            // that has a real note-off on its channel; otherwise take the first.
            let has_note_off = |p: &Msg| {
                msgs.iter().any(|m| kind(m.status) == 0x80 && m.status & 0x0F == p.status & 0x0F && m.data[0] == p.data[0])
            };
            let candidates: Vec<&Msg> = msgs.iter().filter(|m| matches!(kind(m.status), 0x90 | 0xB0) && m.data[1] > 0).copied().collect();
            let press = candidates.iter().find(|p| has_note_off(p)).or_else(|| candidates.first())?;
            let press_id = (press.status, press.data[0]);
            let release = msgs
                .iter()
                .find(|m| {
                    (kind(m.status) == 0x80 && m.status & 0x0F == press.status & 0x0F && m.data[0] == press.data[0])
                        || ((m.status, m.data[0]) == press_id && m.data[1] == 0)
                })
                .map(|m| (m.status, m.data[0]))
                .filter(|r| *r != press_id);
            Some(Binding::Button { press: press_id, release })
        }
        Kind::Pot => {
            if let Some(m) = msgs.iter().find(|m| kind(m.status) == 0xE0) {
                return Some(Binding::PitchBend { status: m.status });
            }
            let ccs: Vec<(u8, u8)> = groups.keys().filter(|(s, _)| kind(*s) == 0xB0).copied().collect();
            if ccs.len() >= 2 {
                // MSB/LSB pairs are 0x20 apart (or consecutive on some firmware).
                for a in &ccs {
                    for b in &ccs {
                        if a.0 == b.0 && (b.1 == a.1 + 0x20 || b.1 == a.1 + 1) {
                            return Some(Binding::Pot14 { msb: *a, lsb: *b });
                        }
                    }
                }
            }
            let (id, _) = groups.iter().max_by_key(|(_, v)| v.len())?;
            Some(Binding::Pot7 { msg: *id })
        }
        Kind::Encoder => {
            let (id, _) = groups.iter().filter(|((s, _), _)| kind(*s) == 0xB0).max_by_key(|(_, v)| v.len())?;
            Some(Binding::Encoder { msg: *id })
        }
        Kind::Platter | Kind::Strip => {
            let ccs: Vec<((u8, u8), &Vec<u8>)> = groups.iter().filter(|((s, _), _)| kind(*s) == 0xB0).map(|(k, v)| (*k, v)).collect();
            if ccs.is_empty() {
                return None;
            }
            // Most frequent CC is the main one.
            let (main, vals) = ccs.iter().max_by_key(|(_, v)| v.len())?;
            let lsb = ccs.iter().find(|(k, _)| k.0 == main.0 && (k.1 == main.1 + 0x20 || k.1 == main.1 + 1)).map(|(k, _)| *k);
            let relative = vals.iter().all(|v| (1..=16).contains(v) || (112..=127).contains(v) || (48..=80).contains(v))
                && !is_monotonic(vals);
            let near64 = vals.iter().filter(|v| (48..=80).contains(*v)).count() * 2 > vals.len();
            let mode = if c.kind == Kind::Strip {
                "absolute"
            } else if relative && near64 {
                "rel64"
            } else if relative {
                "rel2c"
            } else {
                "absolute"
            };
            Some(Binding::Platter { msg: *main, lsb, mode })
        }
    }
}

fn is_monotonic(vals: &[u8]) -> bool {
    if vals.len() < 6 {
        return false;
    }
    let mut up = 0;
    let mut down = 0;
    for w in vals.windows(2) {
        let d = w[1] as i32 - w[0] as i32;
        if d > 0 && d < 64 {
            up += 1;
        } else if d < 0 && d > -64 {
            down += 1;
        }
    }
    up > vals.len() * 3 / 4 || down > vals.len() * 3 / 4
}

const DECKS: [&str; 4] = ["deck1", "deck2", "deck3", "deck4"];

/// The PLAY message of a deck section, which anchors its numbering.
fn deck_play(learned: &Learned, deck: &str) -> Option<(u8, u8)> {
    match learned.get(deck, "play").and_then(analyze) {
        Some(Binding::Button { press, .. }) => Some(press),
        _ => None,
    }
}

/// Control ids the capture script asks for in a section.
fn expected_ids(section: &str) -> Vec<&'static str> {
    crate::learn::script()
        .into_iter()
        .find(|s| s.key == section)
        .map(|s| s.prompts.iter().map(|p| p.id).collect())
        .unwrap_or_default()
}

fn list(ids: &[String]) -> String {
    ids.join(", ")
}

fn shift(b: &Binding, off: (i32, i32)) -> Binding {
    let s = |(st, n): (u8, u8)| (((st as i32) + off.0) as u8, ((n as i32) + off.1).clamp(0, 127) as u8);
    match b {
        Binding::Button { press, release } => Binding::Button { press: s(*press), release: release.map(s) },
        Binding::Pot7 { msg } => Binding::Pot7 { msg: s(*msg) },
        Binding::Pot14 { msb, lsb } => Binding::Pot14 { msb: s(*msb), lsb: s(*lsb) },
        Binding::PitchBend { status } => Binding::PitchBend { status: ((*status as i32) + off.0) as u8 },
        Binding::Encoder { msg } => Binding::Encoder { msg: s(*msg) },
        Binding::Platter { msg, lsb, mode } => Binding::Platter { msg: s(*msg), lsb: lsb.map(s), mode },
    }
}

fn hex(v: u8) -> String {
    format!("0x{v:02X}")
}

struct Xml {
    controls: String,
    outputs: String,
}

impl Xml {
    fn control(&mut self, group: &str, key: &str, status: u8, midino: u8, options: &[&str]) {
        let _ = writeln!(self.controls, "            <control>");
        let _ = writeln!(self.controls, "                <group>{group}</group>");
        let _ = writeln!(self.controls, "                <key>{key}</key>");
        let _ = writeln!(self.controls, "                <status>{}</status>", hex(status));
        let _ = writeln!(self.controls, "                <midino>{}</midino>", hex(midino));
        let _ = writeln!(self.controls, "                <options>");
        for o in options {
            let _ = writeln!(self.controls, "                    <{o}/>");
        }
        let _ = writeln!(self.controls, "                </options>");
        let _ = writeln!(self.controls, "            </control>");
    }

    /// Bind a component input to every message of a binding.
    fn bind(&mut self, group: &str, key: &str, b: &Binding) {
        match b {
            Binding::Button { press, release } => {
                self.control(group, key, press.0, press.1, &["Script-Binding"]);
                if let Some(r) = release {
                    self.control(group, key, r.0, r.1, &["Script-Binding"]);
                }
            }
            Binding::Pot7 { msg } | Binding::Encoder { msg } => self.control(group, key, msg.0, msg.1, &["Script-Binding"]),
            Binding::Pot14 { msb, lsb } => {
                self.control(group, &format!("{key}MSB"), msb.0, msb.1, &["Script-Binding"]);
                self.control(group, &format!("{key}LSB"), lsb.0, lsb.1, &["Script-Binding"]);
                // keys end in ".input", so these become ".inputMSB" / ".inputLSB"
            }
            Binding::PitchBend { status } => {
                // Mixxx delivers pitch bend as status 0xEn with midino = LSB; bind all LSB values.
                for lsb in 0..128u8 {
                    self.control(group, key, *status, lsb, &["Script-Binding"]);
                }
            }
            Binding::Platter { msg, lsb, .. } => {
                self.control(group, key, msg.0, msg.1, &["Script-Binding"]);
                if let Some(l) = lsb {
                    self.control(group, &format!("{key}LSB"), l.0, l.1, &["Script-Binding"]);
                }
            }
        }
    }
}

fn binding_json(b: &Binding) -> Value {
    match b {
        Binding::Button { press, release } => json!({"type": "button", "midi": [press.0, press.1], "release": release.map(|r| vec![r.0, r.1])}),
        Binding::Pot7 { msg } => json!({"type": "pot", "midi": [msg.0, msg.1]}),
        Binding::Pot14 { msb, lsb } => json!({"type": "pot14", "midi": [msb.0, msb.1], "lsb": [lsb.0, lsb.1]}),
        Binding::PitchBend { status } => json!({"type": "pitchbend", "midi": [status, 0]}),
        Binding::Encoder { msg } => json!({"type": "encoder", "midi": [msg.0, msg.1]}),
        Binding::Platter { msg, lsb, mode } => json!({"type": "platter", "midi": [msg.0, msg.1], "lsb": lsb.map(|l| vec![l.0, l.1]), "mode": mode}),
    }
}

/// Generate both files. Returns `(xml, js, notes)`; the notes summarise
/// what was mapped and what the capture is still missing.
pub fn generate(learned: &Learned) -> Result<(String, String, Vec<String>), Box<dyn std::error::Error>> {
    let template = include_str!("../templates/Numark-NS7II-scripts.js");
    let mut xml = Xml { controls: String::new(), outputs: String::new() };
    let mut map = Map::new();
    let mut notes: Vec<String> = Vec::new();

    // Decks. Each deck's numbering is anchored on its PLAY message; a
    // control captured on any deck is carried to the others by that offset,
    // and a direct capture always wins. A deck whose PLAY equals an earlier
    // deck's was captured without switching layers and is not mapped twice.
    let reference = DECKS.iter().find_map(|k| deck_play(learned, k));
    let Some(reference) = reference else {
        return Err("learn.json has no PLAY capture for any deck; run midi-learn first".into());
    };
    let offsets: Vec<Option<(i32, i32)>> = DECKS
        .iter()
        .map(|k| deck_play(learned, k).map(|p| (p.0 as i32 - reference.0 as i32, p.1 as i32 - reference.1 as i32)))
        .collect();
    let ids: BTreeSet<String> = DECKS.iter().filter_map(|k| learned.sections.get(*k)).flat_map(|s| s.keys().cloned()).collect();
    let missing: Vec<String> = expected_ids("deck1").into_iter().filter(|id| !ids.contains(*id)).map(str::to_string).collect();
    if !missing.is_empty() && !learned.complete {
        notes.push(format!("deck controls never captured on any deck (run midi-learn again to add them): {}", list(&missing)));
    }
    let mut decks_json = Vec::new();
    let mut deck_bindings = 0usize;
    for (i, key) in DECKS.iter().enumerate() {
        let n = i + 1;
        let Some(off) = offsets[i] else {
            notes.push(format!("deck {n}: no PLAY captured in section {key}; deck not mapped"));
            continue;
        };
        if let Some(j) = (0..i).find(|j| offsets[*j] == Some(off)) {
            notes.push(format!(
                "deck {n}: PLAY sends the same message as deck {} (the DECK button did not switch layers during capture?); deck {n} not mapped",
                j + 1
            ));
            continue;
        }
        let mut controls = Map::new();
        for id in &ids {
            let direct = learned.get(key, id).and_then(analyze);
            let derived = || {
                if learned.complete {
                    return None;
                }
                DECKS.iter().enumerate().find_map(|(j, k)| {
                    let src = offsets[j]?;
                    let b = learned.get(k, id).and_then(analyze)?;
                    Some(shift(&b, (off.0 - src.0, off.1 - src.1)))
                })
            };
            let Some(b) = direct.or_else(derived) else { continue };
            let group = format!("[Channel{n}]");
            let comp = format!("NS7II.decks[{}].{}", n - 1, js_name(id));
            xml.bind(&group, &format!("{comp}.input"), &b);
            controls.insert(id.clone(), binding_json(&b));
            deck_bindings += 1;
        }
        decks_json.push(json!({"number": n, "controls": controls}));
    }
    map.insert("decks".into(), Value::Array(decks_json));

    // Mixer channels. The NS7II numbers channel strips irregularly (channel
    // faders are CC 19, 8, 13, 24), so nothing is extrapolated: only directly
    // captured controls are mapped.
    let mut mixer_json = Vec::new();
    let mut mixer_bindings = 0usize;
    for n in 1..=4u8 {
        let key = format!("mixer{n}");
        let Some(sec) = learned.sections.get(&key) else {
            notes.push(format!("mixer channel {n}: nothing captured; channel not mapped"));
            continue;
        };
        let mut controls = Map::new();
        for (id, cap) in sec {
            let Some(b) = analyze(cap) else { continue };
            let group = format!("[Channel{n}]");
            let comp = format!("NS7II.mixer[{}].{}", n - 1, js_name(id));
            xml.bind(&group, &format!("{comp}.input"), &b);
            controls.insert(id.clone(), binding_json(&b));
            mixer_bindings += 1;
        }
        let missing: Vec<String> = expected_ids("mixer1").into_iter().filter(|id| !sec.contains_key(*id)).map(str::to_string).collect();
        if !missing.is_empty() {
            notes.push(format!("mixer channel {n}: not captured (run midi-learn again to add them): {}", list(&missing)));
        }
        mixer_json.push(json!({"number": n, "controls": controls}));
    }
    map.insert("mixer".into(), Value::Array(mixer_json));

    // Master / library.
    let mut master = Map::new();
    let mut master_bindings = 0usize;
    if let Some(sec) = learned.sections.get("master") {
        for (id, cap) in sec {
            if let Some(b) = analyze(cap) {
                xml.bind("[Master]", &format!("NS7II.master.{}.input", js_name(id)), &b);
                master.insert(id.clone(), binding_json(&b));
                master_bindings += 1;
            }
        }
        let missing: Vec<String> = expected_ids("master").into_iter().filter(|id| !sec.contains_key(*id)).map(str::to_string).collect();
        if !missing.is_empty() {
            notes.push(format!("master section: not captured: {}", list(&missing)));
        }
    } else {
        notes.push("master section: nothing captured".into());
    }
    notes.insert(0, format!("mapped {deck_bindings} deck, {mixer_bindings} mixer and {master_bindings} master controls"));
    map.insert("master".into(), Value::Object(master));
    map.insert("device".into(), Value::String(learned.device.clone()));
    map.insert("firmware".into(), Value::String(learned.firmware.clone()));

    let map_json = serde_json::to_string_pretty(&Value::Object(map))?;
    let js = template.replace("/*__MAP__*/{}", &map_json);

    let mut out = String::new();
    out.push_str("<?xml version='1.0' encoding='utf-8'?>\n");
    out.push_str("<MixxxControllerPreset schemaVersion=\"1\" mixxxVersion=\"2.3.0+\">\n");
    out.push_str("    <info>\n        <name>Numark NS7II</name>\n        <author>ns7iid mixxx-map (generated)</author>\n");
    let _ = writeln!(
        out,
        "        <description>Generated from a guided capture of the NS7II ({}, firmware {}). Use with the ns7iid Linux driver.</description>",
        xml_escape(&learned.device),
        xml_escape(&learned.firmware)
    );
    out.push_str("    </info>\n    <controller id=\"Numark NS7II\">\n        <scriptfiles>\n");
    out.push_str("            <file functionprefix=\"\" filename=\"midi-components-0.0.js\"/>\n");
    out.push_str("            <file functionprefix=\"NS7II\" filename=\"Numark-NS7II-scripts.js\"/>\n");
    out.push_str("        </scriptfiles>\n        <controls>\n");
    out.push_str(&xml.controls);
    out.push_str("        </controls>\n        <outputs>\n");
    out.push_str(&xml.outputs);
    out.push_str("        </outputs>\n    </controller>\n</MixxxControllerPreset>\n");
    Ok((out, js, notes))
}

fn js_name(id: &str) -> String {
    // snake_case -> camelCase; "shift" must not shadow ComponentContainer.shift()
    if id == "shift" {
        return "shiftButton".to_string();
    }
    let mut out = String::new();
    let mut up = false;
    for c in id.chars() {
        if c == '_' {
            up = true;
        } else if up {
            out.push(c.to_ascii_uppercase());
            up = false;
        } else {
            out.push(c);
        }
    }
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::learn::Msg;

    fn cap(kind: Kind, msgs: &[(u8, u8, u8)]) -> Capture {
        Capture { kind, messages: msgs.iter().map(|(s, a, b)| Msg { status: *s, data: vec![*a, *b] }).collect() }
    }

    #[test]
    fn button_press_release() {
        let b = analyze(&cap(Kind::Button, &[(0x90, 0x34, 0x7F), (0x80, 0x34, 0x00)])).unwrap();
        assert_eq!(b, Binding::Button { press: (0x90, 0x34), release: Some((0x80, 0x34)) });
        let b = analyze(&cap(Kind::Button, &[(0x90, 0x34, 0x7F), (0x90, 0x34, 0x00)])).unwrap();
        assert_eq!(b, Binding::Button { press: (0x90, 0x34), release: None });
    }

    #[test]
    fn pot_14bit_detection() {
        let b = analyze(&cap(Kind::Pot, &[(0xB0, 0x09, 0x10), (0xB0, 0x29, 0x55), (0xB0, 0x09, 0x11), (0xB0, 0x29, 0x02)])).unwrap();
        assert_eq!(b, Binding::Pot14 { msb: (0xB0, 0x09), lsb: (0xB0, 0x29) });
        let b = analyze(&cap(Kind::Pot, &[(0xB0, 0x09, 0x10), (0xB0, 0x09, 0x11)])).unwrap();
        assert_eq!(b, Binding::Pot7 { msg: (0xB0, 0x09) });
    }

    #[test]
    fn platter_relative_vs_absolute() {
        let rel: Vec<(u8, u8, u8)> = (0..20).map(|i| (0xB0, 0x06, if i % 3 == 0 { 65 } else { 66 })).collect();
        let b = analyze(&cap(Kind::Platter, &rel)).unwrap();
        assert!(matches!(b, Binding::Platter { mode: "rel64", .. }));
        let abs: Vec<(u8, u8, u8)> = (0..40).map(|i| (0xB0, 0x06, (i * 3) as u8)).collect();
        let b = analyze(&cap(Kind::Platter, &abs)).unwrap();
        assert!(matches!(b, Binding::Platter { mode: "absolute", .. }));
    }

    #[test]
    fn deck_offsets_shift_bindings() {
        let mut l = Learned::default();
        let mut d1 = BTreeMap::new();
        d1.insert("play".to_string(), cap(Kind::Button, &[(0x90, 0x34, 0x7F), (0x80, 0x34, 0)]));
        d1.insert("rate".to_string(), cap(Kind::Pot, &[(0xB0, 0x09, 1), (0xB0, 0x09, 2)]));
        let mut d2 = BTreeMap::new();
        d2.insert("play".to_string(), cap(Kind::Button, &[(0x91, 0x34, 0x7F), (0x81, 0x34, 0)]));
        l.sections.insert("deck1".into(), d1);
        l.sections.insert("deck2".into(), d2);
        let (xml, js, _) = generate(&l).unwrap();
        assert!(xml.contains("<status>0xB1</status>"));
        assert!(xml.contains("NS7II.decks[1].rate.input"));
        assert!(js.contains("\"number\": 2"));
    }

    #[test]
    fn button_prefers_candidate_with_real_note_off() {
        // BACK on the NS7II: activity note 90 6C first, then the button itself.
        let b = analyze(&cap(Kind::Button, &[(0x90, 0x6C, 0x7F), (0x90, 0x6C, 0x00), (0x90, 0x06, 0x7F), (0x80, 0x06, 0x00)])).unwrap();
        assert_eq!(b, Binding::Button { press: (0x90, 0x06), release: Some((0x80, 0x06)) });
        // Note-off on another channel does not count.
        let b = analyze(&cap(Kind::Button, &[(0x90, 0x62, 0x7F), (0x90, 0x62, 0x00), (0x92, 0x31, 0x7F), (0x82, 0x31, 0x00)])).unwrap();
        assert_eq!(b, Binding::Button { press: (0x92, 0x31), release: Some((0x82, 0x31)) });
    }

    #[test]
    fn controls_captured_on_any_deck_reach_every_deck() {
        let mut l = Learned::default();
        let mut d1 = BTreeMap::new();
        d1.insert("play".to_string(), cap(Kind::Button, &[(0x93, 0x34, 0x7F), (0x83, 0x34, 0)]));
        let mut d2 = BTreeMap::new();
        d2.insert("play".to_string(), cap(Kind::Button, &[(0x92, 0x34, 0x7F), (0x82, 0x34, 0)]));
        d2.insert("pad1".to_string(), cap(Kind::Button, &[(0x92, 0x47, 0x7F), (0x82, 0x47, 0)]));
        l.sections.insert("deck1".into(), d1);
        l.sections.insert("deck2".into(), d2);
        let (xml, _, notes) = generate(&l).unwrap();
        // deck 1's pad1 is derived from deck 2 by the PLAY offset (channel 4 vs 3).
        assert!(xml.contains("NS7II.decks[0].pad1.input"));
        let i = xml.find("NS7II.decks[0].pad1.input").unwrap();
        assert!(xml[i..].contains("<status>0x93</status>"));
        assert!(notes.iter().any(|n| n.contains("never captured") && n.contains("pad2")));
    }

    #[test]
    fn deck_with_same_play_as_earlier_deck_is_not_mapped() {
        let mut l = Learned::default();
        for key in ["deck1", "deck3"] {
            let mut d = BTreeMap::new();
            d.insert("play".to_string(), cap(Kind::Button, &[(0x93, 0x34, 0x7F), (0x83, 0x34, 0)]));
            l.sections.insert(key.into(), d);
        }
        let (xml, _, notes) = generate(&l).unwrap();
        assert!(xml.contains("[Channel1]"));
        assert!(!xml.contains("[Channel3]"));
        assert!(notes.iter().any(|n| n.starts_with("deck 3: PLAY sends the same message as deck 1")));
    }

    #[test]
    fn mixer_channels_are_not_extrapolated() {
        let mut l = Learned::default();
        let mut d1 = BTreeMap::new();
        d1.insert("play".to_string(), cap(Kind::Button, &[(0x90, 0x34, 0x7F), (0x80, 0x34, 0)]));
        l.sections.insert("deck1".into(), d1);
        let mut m1 = BTreeMap::new();
        m1.insert("fader".to_string(), cap(Kind::Pot, &[(0xB0, 19, 1), (0xB0, 51, 2), (0xB0, 19, 3)]));
        m1.insert("bass".to_string(), cap(Kind::Pot, &[(0xB0, 20, 1), (0xB0, 52, 2), (0xB0, 20, 3)]));
        let mut m2 = BTreeMap::new();
        m2.insert("fader".to_string(), cap(Kind::Pot, &[(0xB0, 8, 1), (0xB0, 40, 2), (0xB0, 8, 3)]));
        l.sections.insert("mixer1".into(), m1);
        l.sections.insert("mixer2".into(), m2);
        let (xml, _, notes) = generate(&l).unwrap();
        assert!(xml.contains("NS7II.mixer[1].fader.inputMSB"));
        assert!(!xml.contains("NS7II.mixer[1].bass.inputMSB"));
        assert!(notes.iter().any(|n| n.starts_with("mixer channel 2: not captured") && n.contains("bass")));
    }

    #[test]
    fn js_names() {
        assert_eq!(js_name("pad_mode_cue"), "padModeCue");
        assert_eq!(js_name("play"), "play");
        assert_eq!(js_name("shift"), "shiftButton");
    }
}
