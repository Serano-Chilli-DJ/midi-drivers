//! Turn a `learn.json` capture into a Mixxx controller mapping:
//! `Numark NS7II.midi.xml` + `Numark-NS7II-scripts.js`.
//!
//! The script is a static template (`templates/Numark-NS7II-scripts.js`)
//! that builds its components from a `NS7II.MAP` object; this module
//! produces that object and the XML that routes every MIDI message to the
//! right component input.

use crate::learn::{Capture, Kind, Learned, Msg};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
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
            // Press = first note-on with velocity > 0, or first CC with value > 0.
            let press = msgs.iter().find(|m| matches!(kind(m.status), 0x90 | 0xB0) && m.data[1] > 0)?;
            let press_id = (press.status, press.data[0]);
            let release = msgs
                .iter()
                .find(|m| (kind(m.status) == 0x80 && m.data[0] == press.data[0]) || ((m.status, m.data[0]) == press_id && m.data[1] == 0))
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

/// Offset of a learned deck relative to deck 1: (status delta, note delta).
fn deck_offset(learned: &Learned, deck: &str) -> Option<(i32, i32)> {
    let a = analyze(learned.get("deck1", "play")?)?;
    let b = analyze(learned.get(deck, "play")?)?;
    match (a, b) {
        (Binding::Button { press: pa, .. }, Binding::Button { press: pb, .. }) => {
            Some((pb.0 as i32 - pa.0 as i32, pb.1 as i32 - pa.1 as i32))
        }
        _ => None,
    }
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

/// Generate both files. Returns `(xml, js)`.
pub fn generate(learned: &Learned) -> Result<(String, String), Box<dyn std::error::Error>> {
    let template = include_str!("../templates/Numark-NS7II-scripts.js");
    let mut xml = Xml { controls: String::new(), outputs: String::new() };
    let mut map = Map::new();
    let mut notes: Vec<String> = Vec::new();

    // Decks: deck 1 is the reference; 2..4 are offsets of it, with any
    // directly captured control for that deck taking precedence.
    let deck1 = learned.sections.get("deck1").ok_or("learn.json has no deck1 section")?;
    let mut decks_json = Vec::new();
    for (n, key) in [(1, "deck1"), (2, "deck2"), (3, "deck3"), (4, "deck4")] {
        let off = if n == 1 { Some((0, 0)) } else { deck_offset(learned, key) };
        let Some(off) = off else {
            notes.push(format!("deck {n}: no PLAY captured in section {key}; deck not mapped"));
            continue;
        };
        let mut controls = Map::new();
        for (id, cap) in deck1 {
            let b = match learned.get(key, id).and_then(analyze) {
                Some(direct) => direct,
                None => match analyze(cap) {
                    Some(b1) => shift(&b1, off),
                    None => continue,
                },
            };
            let group = format!("[Channel{n}]");
            let comp = format!("NS7II.decks[{}].{}", n - 1, js_name(id));
            xml.bind(&group, &format!("{comp}.input"), &b);
            controls.insert(id.clone(), binding_json(&b));
        }
        decks_json.push(json!({"number": n, "controls": controls}));
    }
    map.insert("decks".into(), Value::Array(decks_json));

    // Mixer channels.
    let mut mixer_json = Vec::new();
    let mixer1 = learned.sections.get("mixer1").cloned().unwrap_or_default();
    let ch1_fader = mixer1.get("fader").and_then(analyze);
    for n in 1..=4u8 {
        let key = format!("mixer{n}");
        let off = if n == 1 {
            Some((0, 0))
        } else {
            match (ch1_fader.as_ref(), learned.get(&key, "fader").and_then(analyze)) {
                (Some(Binding::Pot7 { msg: a }), Some(Binding::Pot7 { msg: b })) => Some((b.0 as i32 - a.0 as i32, b.1 as i32 - a.1 as i32)),
                (Some(Binding::Pot14 { msb: a, .. }), Some(Binding::Pot14 { msb: b, .. })) => Some((b.0 as i32 - a.0 as i32, b.1 as i32 - a.1 as i32)),
                _ => None,
            }
        };
        let Some(off) = off else {
            notes.push(format!("mixer channel {n}: no fader captured in section {key}; channel not mapped"));
            continue;
        };
        let mut controls = Map::new();
        for (id, cap) in &mixer1 {
            let b = match learned.get(&key, id).and_then(analyze) {
                Some(direct) => direct,
                None => match analyze(cap) {
                    Some(b1) => shift(&b1, off),
                    None => continue,
                },
            };
            let group = format!("[Channel{n}]");
            let comp = format!("NS7II.mixer[{}].{}", n - 1, js_name(id));
            xml.bind(&group, &format!("{comp}.input"), &b);
            controls.insert(id.clone(), binding_json(&b));
        }
        mixer_json.push(json!({"number": n, "controls": controls}));
    }
    map.insert("mixer".into(), Value::Array(mixer_json));

    // Master / library.
    let mut master = Map::new();
    if let Some(sec) = learned.sections.get("master") {
        for (id, cap) in sec {
            if let Some(b) = analyze(cap) {
                xml.bind("[Master]", &format!("NS7II.master.{}.input", js_name(id)), &b);
                master.insert(id.clone(), binding_json(&b));
            }
        }
    }
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
    for n in notes {
        eprintln!("note: {n}");
    }
    Ok((out, js))
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
        let (xml, js) = generate(&l).unwrap();
        assert!(xml.contains("<status>0xB1</status>"));
        assert!(xml.contains("NS7II.decks[1].rate.input"));
        assert!(js.contains("\"number\": 2"));
    }

    #[test]
    fn js_names() {
        assert_eq!(js_name("pad_mode_cue"), "padModeCue");
        assert_eq!(js_name("play"), "play");
        assert_eq!(js_name("shift"), "shiftButton");
    }
}
