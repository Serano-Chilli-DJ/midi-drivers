//! Guided MIDI capture: asks the user to touch each control once and records
//! what the NS7II sends, producing a `learn.json` that `mixxx-map` turns
//! into a Mixxx mapping.

use ns7ii_proto::midi::{strip_filler, Message, Parser};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

/// What kind of control a prompt expects, which decides how it is captured
/// and later mapped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Kind {
    /// Momentary or toggle button: note on/off (or CC 127/0).
    Button,
    /// Absolute knob or fader, 7- or 14-bit.
    Pot,
    /// Relative encoder (browse knob, loop size).
    Encoder,
    /// Platter rotation (relative ticks or absolute position).
    Platter,
    /// Touch strip (absolute position, touch on/off).
    Strip,
}

/// One prompt in the script.
#[derive(Debug, Clone)]
pub struct Prompt {
    /// Stable identifier used by the generator (e.g. `play`, `pad3`).
    pub id: &'static str,
    /// Text shown to the user.
    pub text: &'static str,
    /// Control kind.
    pub kind: Kind,
}

/// A section of prompts.
#[derive(Debug, Clone)]
pub struct Section {
    /// Section key (`deck1`, `deck2`, `mixer1`, `master`, ...).
    pub key: &'static str,
    /// Title shown to the user.
    pub title: &'static str,
    /// Advice shown before the section.
    pub intro: &'static str,
    /// Prompts.
    pub prompts: Vec<Prompt>,
}

fn b(id: &'static str, text: &'static str) -> Prompt {
    Prompt { id, text, kind: Kind::Button }
}
fn p(id: &'static str, text: &'static str) -> Prompt {
    Prompt { id, text, kind: Kind::Pot }
}

/// The capture script. Deck 1 is captured completely; decks 2 to 4 only
/// need a few controls to learn how the device numbers them.
pub fn script() -> Vec<Section> {
    let deck_full = |key: &'static str, title: &'static str| Section {
        key,
        title,
        intro: "Touch each control once when asked. Press Enter to skip a control the NS7II does not have.",
        prompts: vec![
            b("play", "PLAY/PAUSE"),
            b("cue", "CUE"),
            b("sync", "SYNC"),
            b("shift", "SHIFT (press and release)"),
            b("range", "RANGE / MASTER TEMPO"),
            b("slip", "SLIP"),
            b("censor", "BLEEP (push the BLEEP/REVERSE switch to BLEEP and back)"),
            b("reverse", "REVERSE (push the switch to REVERSE and back)"),
            b("pitch_bend_minus", "PITCH BEND - (press and release)"),
            b("pitch_bend_plus", "PITCH BEND + (press and release)"),
            p("rate", "PITCH fader: move it from one end to the other"),
            b("tap", "TAP (BPM tap)"),
            b("grid_adjust", "ADJUST / SET (beat grid)"),
            b("grid_slip", "SLIP / CLEAR (beat grid, next to ADJUST)"),
            p("start_time", "START TIME knob: turn fully"),
            p("stop_time", "STOP TIME knob: turn fully"),
            b("hotcue1", "HOT CUE button 1 (the dedicated row of five)"),
            b("hotcue2", "HOT CUE button 2"),
            b("hotcue3", "HOT CUE button 3"),
            b("hotcue4", "HOT CUE button 4"),
            b("hotcue5", "HOT CUE button 5"),
            Prompt { id: "platter_turn", text: "turn the PLATTER slowly clockwise by about a quarter turn", kind: Kind::Platter },
            Prompt { id: "strip", text: "slide a finger along the STRIP SEARCH from left to right", kind: Kind::Strip },
            b("pad_mode_cue", "pad mode button CUES (hot cues)"),
            b("pad_mode_autoloop", "pad mode button AUTO / ROLL"),
            b("pad_mode_loop", "pad mode button LOOP (manual loop)"),
            b("pad_mode_sampler", "pad mode button SAMPLER"),
            b("pad_mode_slicer", "pad mode button SLICER"),
            b("pad1", "performance PAD 1 (top left) — make sure CUE pad mode is selected first"),
            b("pad2", "PAD 2"),
            b("pad3", "PAD 3"),
            b("pad4", "PAD 4"),
            b("pad5", "PAD 5 (bottom left)"),
            b("pad6", "PAD 6"),
            b("pad7", "PAD 7"),
            b("pad8", "PAD 8"),
            b("param_left", "PARAMETER < (left arrow next to the pads)"),
            b("param_right", "PARAMETER >"),
            b("fx1_on", "FX 1 ON/OFF button (this side's FX unit)"),
            b("fx2_on", "FX 2 ON/OFF"),
            b("fx3_on", "FX 3 ON/OFF"),
            p("fx1_knob", "FX 1 knob: turn fully"),
            p("fx2_knob", "FX 2 knob: turn fully"),
            p("fx3_knob", "FX 3 knob: turn fully"),
            Prompt { id: "fx_beats", text: "FX BEATS knob: turn one click clockwise", kind: Kind::Encoder },
            b("fx_tap", "BEAT / MODE button in the FX section"),
            b("deck_select", "DECK select button (switches this side between decks 1/3 or 2/4)"),
            b("load", "LOAD button for this deck"),
        ],
    };
    let mixer_full = |key: &'static str, title: &'static str, intro: &'static str| Section {
        key,
        title,
        intro,
        prompts: vec![
            p("gain", "GAIN knob: turn fully"),
            p("treble", "TREBLE / HIGH knob: turn fully"),
            p("mid", "MID knob: turn fully"),
            p("bass", "BASS / LOW knob: turn fully"),
            p("filter", "FILTER knob: turn fully"),
            b("pfl", "PFL (headphone cue) button"),
            p("fader", "channel line fader: move it from bottom to top"),
            b("fx_a", "FX ASSIGN A button for this channel"),
            b("fx_b", "FX ASSIGN B button for this channel"),
            b("xf_a", "crossfader assign switch (front panel): move it to A and back to the middle"),
            b("xf_b", "crossfader assign switch: move it to B and back to the middle"),
        ],
    };
    vec![
        deck_full("deck1", "Deck 1 (left side, deck 1 selected)"),
        Section {
            key: "deck2",
            title: "Deck 2 (right side, deck 2 selected)",
            intro: "Only a few controls are needed to learn how the right side is numbered.",
            prompts: vec![
                b("play", "PLAY/PAUSE on the RIGHT deck"),
                b("cue", "CUE on the RIGHT deck"),
                b("pad1", "PAD 1 on the RIGHT deck"),
                p("rate", "RIGHT PITCH fader: move it"),
                Prompt { id: "platter_turn", text: "turn the RIGHT platter a quarter turn", kind: Kind::Platter },
                b("fx1_on", "RIGHT FX 1 ON/OFF"),
                b("load", "LOAD for the RIGHT deck"),
            ],
        },
        Section {
            key: "deck3",
            title: "Deck 3 (left side after pressing its DECK button)",
            intro: "Press the left DECK button so the left side controls deck 3, then:",
            prompts: vec![b("play", "PLAY/PAUSE (left side, deck 3)"), b("pad1", "PAD 1 (left side, deck 3)")],
        },
        Section {
            key: "deck4",
            title: "Deck 4 (right side after pressing its DECK button)",
            intro: "Press the right DECK button so the right side controls deck 4, then:",
            prompts: vec![b("play", "PLAY/PAUSE (right side, deck 4)"), b("pad1", "PAD 1 (right side, deck 4)")],
        },
        mixer_full("mixer1", "Mixer channel 1", "Mixer channel 1 is the leftmost channel strip. The NS7II numbers the strips irregularly, so every channel is captured in full."),
        mixer_full("mixer2", "Mixer channel 2", ""),
        mixer_full("mixer3", "Mixer channel 3", ""),
        mixer_full("mixer4", "Mixer channel 4", ""),
        Section {
            key: "master",
            title: "Master section and library",
            intro: "",
            prompts: vec![
                p("crossfader", "CROSSFADER: move it fully left to right"),
                p("master", "MASTER volume knob"),
                p("booth", "BOOTH volume knob"),
                p("cue_gain", "CUE GAIN (headphone volume)"),
                p("cue_mix", "CUE MIX (headphone cue/master blend)"),
                Prompt { id: "browse_turn", text: "BROWSE knob: turn one click clockwise", kind: Kind::Encoder },
                b("browse_push", "press the BROWSE knob"),
                b("back", "BACK button"),
                b("fwd", "FWD / forward button"),
                b("view", "PANEL / VIEW button"),
                b("load_prepare", "LOAD PREPARE button"),
                b("crates", "CRATES button"),
                b("prepare", "PREPARE button"),
                b("files", "FILES button"),
                b("master_fx_a", "FX A send button under the MASTER knob"),
                b("master_fx_b", "FX B send button under the MASTER knob"),
                b("split_cue", "SPLIT CUE switch (front panel): on and off"),
                b("mic_on", "MIC 1 ON/OFF switch (front panel): on and off"),
                p("mic", "MIC GAIN knob (front panel)"),
                p("mic_treble", "MIC TREBLE knob"),
                p("mic_bass", "MIC BASS knob"),
                p("xf_curve", "CROSSFADER SLOPE knob (front panel)"),
            ],
        },
    ]
}

/// A recorded message, compact for JSON.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Msg {
    /// Status byte.
    pub status: u8,
    /// Data bytes.
    pub data: Vec<u8>,
}

impl From<&Message> for Msg {
    fn from(m: &Message) -> Self {
        Msg { status: m.status, data: m.data.clone() }
    }
}

/// Everything captured for one prompt.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capture {
    /// Control kind.
    pub kind: Kind,
    /// Messages in arrival order (capped).
    pub messages: Vec<Msg>,
}

/// The whole capture file.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Learned {
    /// Device identification for the mapping header.
    pub device: String,
    /// Firmware string.
    pub firmware: String,
    /// Every deck is captured in full (the built-in chart): the generator
    /// must not carry controls from one deck to another by offset.
    #[serde(default)]
    pub complete: bool,
    /// `section -> control id -> capture`.
    pub sections: BTreeMap<String, BTreeMap<String, Capture>>,
}

impl Learned {
    /// Load from JSON.
    pub fn load(path: &str) -> Result<Self, Box<dyn std::error::Error>> {
        Ok(serde_json::from_str(&std::fs::read_to_string(path)?)?)
    }
    /// Save as pretty JSON.
    pub fn save(&self, path: &str) -> Result<(), Box<dyn std::error::Error>> {
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }
    /// Look up a capture.
    pub fn get(&self, section: &str, id: &str) -> Option<&Capture> {
        self.sections.get(section)?.get(id)
    }
}

/// Run the interactive capture. `rx` delivers raw EP 83 transfers.
pub fn run(rx: &Receiver<Vec<u8>>, out_path: &str, mut learned: Learned) -> Result<Learned, Box<dyn std::error::Error>> {
    let mut parser;
    let mut clean = Vec::new();
    println!();
    println!("Guided capture. For each control: touch it as described, wait for the line to");
    println!("confirm what was received, then press Enter for the next one.");
    println!("  Enter         = next (or skip if nothing was received)");
    println!("  r + Enter     = redo this control");
    println!("  s + Enter     = skip the rest of this section");
    println!("  q + Enter     = save and quit");
    println!("Progress is saved to {out_path} after every control, so you can stop and resume.");
    for section in script() {
        println!();
        println!("=== {} ===", section.title);
        if !section.intro.is_empty() {
            println!("{}", section.intro);
        }
        let mut skip_section = false;
        for prompt in &section.prompts {
            if skip_section {
                break;
            }
            if learned.get(section.key, prompt.id).is_some() {
                println!("  [{}] already captured, skipping (delete it from the JSON to redo)", prompt.id);
                continue;
            }
            loop {
                // Drain stale messages.
                while rx.try_recv().is_ok() {}
                parser = Parser::new();
                print!("> {}: ", prompt.text);
                io::stdout().flush()?;
                // Collect until the user presses Enter, in a background-free way:
                // poll the USB channel while waiting for a line on stdin.
                let (line_tx, line_rx) = std::sync::mpsc::channel();
                let mut collected: Vec<Message> = Vec::new();
                let mut first_seen: Option<Instant> = None;
                // Read a line in a helper thread so USB polling continues.
                let handle = std::thread::spawn(move || {
                    let mut s = String::new();
                    let _ = io::stdin().read_line(&mut s);
                    let _ = line_tx.send(s);
                });
                let user_line;
                loop {
                    match rx.recv_timeout(Duration::from_millis(20)) {
                        Ok(raw) => {
                            clean.clear();
                            strip_filler(&raw, &mut clean);
                            for m in parser.parse(&clean) {
                                if m.status >= 0xF8 {
                                    continue;
                                }
                                if first_seen.is_none() {
                                    first_seen = Some(Instant::now());
                                    print!("\n    received: ");
                                }
                                if collected.len() < 400 {
                                    if collected.len() < 6 {
                                        print!("{} ", fmt_msg(&m));
                                        io::stdout().flush()?;
                                    } else if collected.len() == 6 {
                                        print!("... ");
                                        io::stdout().flush()?;
                                    }
                                    collected.push(m);
                                }
                            }
                        }
                        Err(RecvTimeoutError::Timeout) => {}
                        Err(RecvTimeoutError::Disconnected) => return Err("USB stream closed".into()),
                    }
                    if let Ok(l) = line_rx.try_recv() {
                        user_line = l;
                        break;
                    }
                }
                let _ = handle.join();
                let cmd = user_line.trim().to_lowercase();
                if cmd == "q" {
                    learned.save(out_path)?;
                    println!("saved {out_path}");
                    return Ok(learned);
                }
                if cmd == "s" {
                    let left = section.prompts.iter().filter(|p| learned.get(section.key, p.id).is_none()).count();
                    println!("    skipped the rest of \"{}\" ({left} controls); run midi-learn again later to capture them", section.title);
                    skip_section = true;
                    break;
                }
                if cmd == "r" {
                    continue;
                }
                if collected.is_empty() {
                    println!("    (nothing received, skipped)");
                    break;
                }
                let summary = summarize(prompt.kind, &collected);
                println!("    -> {summary}");
                if let Some(twin) = same_as_other_deck(&learned, section.key, prompt.id, &collected) {
                    println!("    this is exactly what {twin} sends: the DECK button did not switch layers.");
                    println!("    Press DECK (its LED changes), then touch the control again. Enter without touching anything skips.");
                    continue;
                }
                learned
                    .sections
                    .entry(section.key.to_string())
                    .or_default()
                    .insert(prompt.id.to_string(), Capture { kind: prompt.kind, messages: collected.iter().map(Msg::from).collect() });
                learned.save(out_path)?;
                break;
            }
        }
    }
    learned.save(out_path)?;
    println!();
    println!("capture complete: {out_path}");
    Ok(learned)
}

/// For deck 3 / deck 4 captures: the deck 1 / deck 2 section that already
/// holds the identical message, if any. Identical means the DECK layer
/// button had no effect, and mapping both would drive two Mixxx decks at once.
fn same_as_other_deck(learned: &Learned, section: &str, id: &str, collected: &[Message]) -> Option<&'static str> {
    let twin = match section {
        "deck3" => "deck1",
        "deck4" => "deck2",
        _ => return None,
    };
    let other = learned.get(twin, id)?;
    let first = |ms: &[Msg]| ms.iter().find(|m| m.data.len() >= 2 && m.status >= 0x80 && m.status < 0xF0).map(|m| (m.status, m.data[0]));
    let mine: Vec<Msg> = collected.iter().map(Msg::from).collect();
    (first(&mine).is_some() && first(&mine) == first(&other.messages)).then_some(twin)
}

fn fmt_msg(m: &Message) -> String {
    let mut s = format!("{:02X}", m.status);
    for d in &m.data {
        s.push_str(&format!(" {d:02X}"));
    }
    s
}

/// One-line description of what a control sends.
pub fn summarize(kind: Kind, msgs: &[Message]) -> String {
    let mut ids: BTreeMap<(u8, u8), (u8, u8, usize)> = BTreeMap::new();
    for m in msgs {
        if let (Some(d1), Some(d2)) = (m.d1(), m.d2()) {
            let e = ids.entry((m.status, d1)).or_insert((d2, d2, 0));
            e.0 = e.0.min(d2);
            e.1 = e.1.max(d2);
            e.2 += 1;
        }
    }
    let parts: Vec<String> = ids
        .iter()
        .map(|((st, d1), (lo, hi, n))| format!("{st:02X} {d1:02X} [{lo}..{hi}] x{n}"))
        .collect();
    format!("{kind:?}: {}", parts.join(", "))
}
