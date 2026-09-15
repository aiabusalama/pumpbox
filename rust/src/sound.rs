//! Audio feedback through the panel speaker.
//!
//! Tones, not speech. Standing at the box you should know what just happened
//! without looking at the LCD, and from the far side of the yard you should
//! be able to tell a dry well from an electrical trip - because those need
//! different responses from you.
//!
//! The vocabulary is built so meaning survives being half-heard:
//!
//!   rising          something started
//!   falling         something stopped, normally
//!   fast and high   electrical trip, look now
//!   slow and low    dry well, it will retry on its own
//!   warbling        the meter stopped answering
//!   short blips     you moved the mode switch
//!
//! Nothing here may ever affect the pump. This crate builds with
//! `panic = "abort"`, so a panic in here would kill the controller and open
//! the contactor - which means there is not a single unwrap in this file.
//! Playback happens on its own thread behind a bounded channel; when the
//! channel is full the sound is dropped rather than making the control loop
//! wait. If aplay is missing the module quietly disables itself.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::OnceLock;

use crate::state::{Mode, State};

const RATE: u32 = 44_100;
const AMPLITUDE: f32 = 0.35; // headroom - a class-D amp clips ugly at full scale
const EDGE: f32 = 0.008; // attack/release seconds; without it every tone clicks

static TX: OnceLock<SyncSender<&'static str>> = OnceLock::new();
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Every tone, and the notes it is made of: (frequency Hz, seconds).
/// A frequency of 0 is silence.
const TONES: &[(&str, &[(f32, f32)])] = &[
    // the software started
    ("boot", &[(523.0, 0.09), (0.0, 0.03), (659.0, 0.09), (0.0, 0.03), (784.0, 0.16)]),
    // contactor closed - rising, confident
    ("pump_on", &[(587.0, 0.12), (0.0, 0.02), (880.0, 0.22)]),
    // stopped with nothing wrong - the same pair inverted
    ("pump_off", &[(880.0, 0.12), (0.0, 0.02), (587.0, 0.22)]),
    // electrical trip - three fast high beeps, hard to ignore
    ("trip", &[(1175.0, 0.09), (0.0, 0.06), (1175.0, 0.09), (0.0, 0.06), (1175.0, 0.09)]),
    // dry well - slow and low, needs time rather than attention
    ("dry", &[(440.0, 0.30), (0.0, 0.05), (330.0, 0.45)]),
    // no reply from the meter - a warble, unlike anything else here
    ("meter_fault", &[(660.0, 0.10), (495.0, 0.10), (660.0, 0.10),
                      (495.0, 0.10), (660.0, 0.10), (495.0, 0.10)]),
    // mode switch moved - short, so flipping it is not annoying
    ("mode_auto", &[(784.0, 0.07), (0.0, 0.03), (988.0, 0.11)]),
    ("mode_manual", &[(784.0, 0.07), (0.0, 0.05), (784.0, 0.11)]),
    ("mode_off", &[(392.0, 0.16)]),
];

fn dir() -> PathBuf {
    std::env::temp_dir().join("pumpd-sounds")
}

/// One note as 16-bit samples, faded in and out.
///
/// The fade matters more than it sounds like it should: a tone that starts at
/// full amplitude has a step edge in it, and a step edge through a class-D
/// amplifier is a click. On a small speaker the clicks end up louder than the
/// note itself.
fn render(notes: &[(f32, f32)]) -> Vec<u8> {
    let mut pcm: Vec<i16> = Vec::new();
    for &(freq, secs) in notes {
        let n = (RATE as f32 * secs) as usize;
        let edge = ((RATE as f32 * EDGE) as usize).max(1);
        for i in 0..n {
            if freq <= 0.0 {
                pcm.push(0);
                continue;
            }
            let env = if i < edge {
                i as f32 / edge as f32
            } else if i + edge > n {
                ((n - i) as f32 / edge as f32).max(0.0)
            } else {
                1.0
            };
            let t = i as f32 / RATE as f32;
            let v = (2.0 * std::f32::consts::PI * freq * t).sin() * env * AMPLITUDE;
            pcm.push((v.clamp(-1.0, 1.0) * 32767.0) as i16);
        }
    }

    let data_len = (pcm.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data_len as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes()); // PCM header size
    w.extend_from_slice(&1u16.to_le_bytes()); // format: PCM
    w.extend_from_slice(&1u16.to_le_bytes()); // channels: mono
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes()); // byte rate
    w.extend_from_slice(&2u16.to_le_bytes()); // block align
    w.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        w.extend_from_slice(&s.to_le_bytes());
    }
    w
}

fn write_all_tones() -> bool {
    let d = dir();
    if fs::create_dir_all(&d).is_err() {
        return false;
    }
    for (name, notes) in TONES {
        let path = d.join(format!("{name}.wav"));
        if path.exists() {
            continue;
        }
        let bytes = render(notes);
        match fs::File::create(&path) {
            Ok(mut f) => {
                if f.write_all(&bytes).is_err() {
                    return false;
                }
            }
            Err(_) => return false,
        }
    }
    true
}

fn aplay(path: &Path) {
    let _ = Command::new("aplay")
        .arg("-q")
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Render the tones and start the playback thread.
///
/// Any failure disables sound for good rather than propagating - the
/// controller must come up either way.
pub fn start() {
    if !write_all_tones() {
        eprintln!("sound: disabled - could not write tones");
        return;
    }

    let (tx, rx) = sync_channel::<&'static str>(4);
    if TX.set(tx).is_err() {
        return; // already started
    }

    let base = dir();
    let spawned = std::thread::Builder::new()
        .name("sound".into())
        .spawn(move || {
            while let Ok(name) = rx.recv() {
                aplay(&base.join(format!("{name}.wav")));
            }
        });

    if spawned.is_err() {
        eprintln!("sound: disabled - could not start playback thread");
        return;
    }
    ENABLED.store(true, Ordering::Relaxed);
}

/// Queue a tone. Never blocks, never panics, never delays the caller.
///
/// A full channel means tones are arriving faster than they can play - a
/// fault storm, say - so the newest is dropped. A missed beep is better than
/// a protection loop waiting on a speaker.
pub fn play(name: &'static str) {
    if !ENABLED.load(Ordering::Relaxed) {
        return;
    }
    if let Some(tx) = TX.get() {
        let _ = tx.try_send(name);
    }
}

/// Pick the tone for a transition and play it.
///
/// A mode change wins over a state change, because moving the switch is the
/// thing you just did with your hand and the state change is only its
/// consequence - two tones at once would tell you less, not more.
pub fn announce(old_state: State, new_state: State, old_mode: Mode, new_mode: Mode) {
    if old_mode != new_mode {
        play(match new_mode {
            Mode::Auto => "mode_auto",
            Mode::Manual => "mode_manual",
            Mode::Off => "mode_off",
        });
        return;
    }

    if old_state == new_state {
        return;
    }

    if new_state == State::Running {
        play("pump_on");
        return;
    }

    play(match new_state {
        State::Tripped => "trip",
        State::Lockout => "dry",
        State::Fault => "meter_fault",
        // leaving Running for any calm state is an ordinary stop; arriving at
        // a calm state from another calm state is not worth a sound
        _ if old_state == State::Running => "pump_off",
        _ => return,
    });
}
