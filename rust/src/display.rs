//! 20x4 character LCD over an I2C backpack (PCF8574 + HD44780).
//!
//! Driven directly rather than through a crate - the HD44780 4-bit protocol
//! is a dozen lines and the backpack wiring varies between clones, so having
//! the bit mapping visible here makes it adjustable.
//!
//! Every screen answers "why" without the operator needing a manual. Line 1
//! is the headline; lines 2-4 carry the detail that explains it.

use rppal::i2c::I2c;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::state::{Machine, Mode, State};
use crate::settings::Settings;

// PCF8574 bit assignments, standard for these backpacks
const RS: u8 = 0x01;
const EN: u8 = 0x04;
const BACKLIGHT: u8 = 0x08;

/// The one thing the driver does to the outside world: push bytes onto the I2C
/// backpack. Abstracting it (over the real rppal I2C) lets a test capture the
/// exact byte stream and decode it back into on-screen text - so the HD44780
/// protocol the driver speaks is verified, not just its text layout. The
/// production path writes byte-for-byte what it always did.
trait I2cBus {
    /// Write the bytes; return true on success (false on a bus error).
    fn write_bytes(&mut self, bytes: &[u8]) -> bool;
}

impl I2cBus for I2c {
    fn write_bytes(&mut self, bytes: &[u8]) -> bool {
        self.write(bytes).is_ok()
    }
}


/// REISBOT in block letters, using all eight CGRAM slots.
///
/// Each custom character is 5x8, so one glyph per letter gives a banner
/// that reads clearly across a shed. Loaded once at startup; the slots are
/// not needed for anything else.
const LOGO_GLYPHS: [[u8; 8]; 7] = [
    [0x1e, 0x11, 0x11, 0x1e, 0x14, 0x12, 0x11, 0x00], // R
    [0x1f, 0x10, 0x10, 0x1e, 0x10, 0x10, 0x1f, 0x00], // E
    [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x1f, 0x00], // I
    [0x0f, 0x10, 0x10, 0x0e, 0x01, 0x01, 0x1e, 0x00], // S
    [0x1e, 0x11, 0x11, 0x1e, 0x11, 0x11, 0x1e, 0x00], // B
    [0x0e, 0x11, 0x11, 0x11, 0x11, 0x11, 0x0e, 0x00], // O
    [0x1f, 0x04, 0x04, 0x04, 0x04, 0x04, 0x04, 0x00], // T
];

/// The banner as characters 0-6, centred on a 20 column row.
pub const LOGO: &str = "      \u{0}\u{1}\u{2}\u{3}\u{4}\u{5}\u{6}       ";

pub const COLS: usize = 20;
pub const ROWS: usize = 4;

/// The two i2c addresses these PCF8574 LCD backpacks ship at, in probe order.
///
/// A blank LCD on a wired box is usually one of two things: the i2c bus is
/// off (fixed in the image), or the backpack answers at the *other* address.
/// The plain PCF8574 lands at 0x27 with its jumpers open; the very common
/// PCF8574**A** clone lands at 0x3F. They are otherwise identical, so a box
/// that assumes 0x27 shows nothing at all on an "A" backpack even with a
/// perfectly healthy bus. Probe both rather than guess.
pub const LCD_ADDRESSES: [u16; 2] = [0x27, 0x3F];

// DDRAM start address per row on a 20x4 HD44780
const ROW_ADDR: [u8; 4] = [0x00, 0x40, 0x14, 0x54];

pub struct Lcd {
    i2c: Box<dyn I2cBus>,
    /// Last text written per row, so unchanged rows skip the I2C traffic.
    shown: [Option<String>; ROWS],
    ok: bool,
    /// Consecutive write failures. A few in a row means the bus glitched
    /// mid-byte and the controller needs resynchronising.
    errors: u32,
    /// Refresh counter, driving the periodic resync.
    refreshes: u32,
}

/// Return the first address for which `open` succeeds, along with the opened
/// value. Pulled out of `Lcd::open_first` so the probe order is unit-testable
/// without real i2c hardware (the opener is injected).
fn probe_first<T>(
    addresses: &[u16],
    mut open: impl FnMut(u16) -> Result<T, String>,
) -> Result<(u16, T), String> {
    let mut last = String::from("no candidate LCD addresses");
    for &a in addresses {
        match open(a) {
            Ok(v) => return Ok((a, v)),
            Err(e) => last = e,
        }
    }
    Err(last)
}

impl Lcd {
    /// Open whichever backpack actually answers among `addresses`.
    ///
    /// `open` already returns `Err` when an address does not ACK (every write
    /// in the init handshake fails, so `init` reports no response), so trying
    /// the candidates in turn cleanly picks the live one and skips the dead one.
    pub fn open_first(addresses: &[u16]) -> Result<Self, String> {
        let (addr, lcd) = probe_first(addresses, Lcd::open)?;
        eprintln!("display: LCD responded at {addr:#04x}");
        Ok(lcd)
    }

    pub fn open(address: u16) -> Result<Self, String> {
        let mut i2c = I2c::new().map_err(|e| format!("i2c unavailable: {e}"))?;
        i2c.set_slave_address(address)
            .map_err(|e| format!("cannot address {address:#04x}: {e}"))?;

        let mut lcd = Lcd {
            i2c: Box::new(i2c),
            shown: [const { None }; ROWS],
            ok: true,
            errors: 0,
            refreshes: 0,
        };
        lcd.init()?;
        Ok(lcd)
    }

    /// Construct over an injected bus (a captured virtual backpack) and run the
    /// real init handshake, so tests can decode the exact byte stream.
    #[cfg(test)]
    fn with_bus(bus: Box<dyn I2cBus>) -> Result<Self, String> {
        let mut lcd = Lcd {
            i2c: bus,
            shown: [const { None }; ROWS],
            ok: true,
            errors: 0,
            refreshes: 0,
        };
        lcd.init()?;
        Ok(lcd)
    }

    fn write_byte(&mut self, b: u8) {
        if !self.i2c.write_bytes(&[b | BACKLIGHT]) {
            self.ok = false;
            self.errors = self.errors.saturating_add(1);
        }
    }

    /// Re-run the 4-bit handshake to resynchronise a controller that was
    /// interrupted mid-byte.
    ///
    /// Each character is sent as two nibbles. If a write fails between
    /// them, every subsequent byte is decoded at the wrong offset and the
    /// screen fills with plausible-looking garbage. Only a full reset
    /// recovers it, so detect the condition and do that rather than leaving
    /// an unreadable display up.
    fn recover(&mut self) {
        eprintln!("display: resynchronising after {} write errors", self.errors);
        self.ok = true;
        self.errors = 0;
        self.shown = [const { None }; ROWS];
        if self.init().is_err() {
            eprintln!("display: reset failed, will retry");
        }
    }

    /// Called each refresh. Recovers the display if it has been failing,
    /// and re-initialises periodically regardless.
    ///
    /// The periodic reset matters because a controller knocked out of
    /// nibble alignment still acknowledges every write - the bus looks
    /// healthy while the screen shows nonsense. Nothing can detect that
    /// from this side, so the only defence is to resynchronise on a timer.
    pub fn tick(&mut self) {
        self.refreshes = self.refreshes.wrapping_add(1);

        if self.errors >= 3 {
            self.recover();
            return;
        }

        // Roughly every two minutes at a 500ms refresh.
        if self.refreshes % 240 == 0 {
            self.resync();
        }
    }

    /// Quietly re-run the init sequence and repaint. Unlike `recover` this
    /// is routine, so it does not log.
    fn resync(&mut self) {
        let _ = self.init();
        self.shown = [const { None }; ROWS];
    }

    /// One nibble, pulsed through the enable line.
    fn pulse(&mut self, data: u8) {
        self.write_byte(data);
        self.write_byte(data | EN);
        std::thread::sleep(Duration::from_micros(1));
        self.write_byte(data & !EN);
        std::thread::sleep(Duration::from_micros(50));
    }

    fn send(&mut self, value: u8, mode: u8) {
        self.pulse((value & 0xF0) | mode);
        self.pulse(((value << 4) & 0xF0) | mode);
    }

    fn command(&mut self, c: u8) {
        self.send(c, 0);
    }

    fn init(&mut self) -> Result<(), String> {
        std::thread::sleep(Duration::from_millis(50));

        // The 8-bit-to-4-bit handshake from the HD44780 datasheet.
        for _ in 0..3 {
            self.pulse(0x30);
            std::thread::sleep(Duration::from_millis(5));
        }
        self.pulse(0x20);
        std::thread::sleep(Duration::from_millis(5));

        self.command(0x28); // 4-bit, 2 lines, 5x8 font
        self.command(0x0C); // display on, cursor off, no blink
        self.command(0x06); // increment on write, no shift

        self.load_logo();

        self.command(0x01); // clear
        std::thread::sleep(Duration::from_millis(3));

        if self.ok {
            Ok(())
        } else {
            Err("no response from the display".into())
        }
    }

    /// Write the logo glyphs into CGRAM slots 0-6.
    fn load_logo(&mut self) {
        for (slot, rows) in LOGO_GLYPHS.iter().enumerate() {
            self.command(0x40 | ((slot as u8) << 3));
            for &r in rows {
                self.send(r, RS);
            }
        }
    }

    fn write_row(&mut self, row: usize, text: &str) {
        if row >= ROWS {
            return;
        }
        // Pad so leftovers from a longer message never linger.
        let mut line: String = text.chars().take(COLS).collect();
        while line.chars().count() < COLS {
            line.push(' ');
        }
        if self.shown[row].as_deref() == Some(line.as_str()) {
            return;
        }

        // Write only the run of characters that actually changed. The
        // spinner alters one cell per tick; repainting all twenty would be
        // twenty times the I2C traffic for no visible difference.
        if let Some(prev) = &self.shown[row] {
            if prev.chars().count() == COLS {
                let new: Vec<char> = line.chars().collect();
                let old: Vec<char> = prev.chars().collect();
                let first = (0..COLS).find(|&i| new[i] != old[i]);
                if let Some(first) = first {
                    let last = (0..COLS).rev().find(|&i| new[i] != old[i]).unwrap_or(first);
                    self.command(0x80 | (ROW_ADDR[row] + first as u8));
                    for &ch in &new[first..=last] {
                        self.put(ch);
                    }
                    self.shown[row] = Some(line);
                }
                return;
            }
        }

        self.command(0x80 | ROW_ADDR[row]);
        for ch in line.chars() {
            self.put(ch);
        }
        self.shown[row] = Some(line);
    }

    /// One character, mapping the codes the HD44780 font cannot show.
    fn put(&mut self, ch: char) {
        {
            // Codes 0-6 are our CGRAM logo glyphs. Everything else must be
            // ASCII: the HD44780 font is not UTF-8, so anything outside it
            // becomes a space rather than a garbage glyph.
            let code = ch as u32;
            let byte = if code < 8 {
                code as u8
            } else if ch.is_ascii() {
                ch as u8
            } else {
                b' '
            };
            self.send(byte, RS);
        }
    }

    pub fn show(&mut self, lines: &[String; ROWS]) {
        self.tick();
        let before = self.errors;
        for (i, l) in lines.iter().enumerate() {
            self.write_row(i, l);
        }
        // A clean pass means the bus is behaving again.
        if self.errors == before {
            self.errors = 0;
            self.ok = true;
        }
    }

    /// Branded splash, held briefly at startup.
    ///
    /// The LCD is volatile, so after a power cut it shows whatever noise
    /// the controller powers up with. Painting this immediately means the
    /// first thing anyone sees is the logo, not garbage.
    pub fn splash(&mut self, ip: Option<&str>) {
        self.show(&[
            LOGO.to_string(),
            "  pump protection".to_string(),
            match ip {
                Some(a) => format!("  {a}"),
                None => "  starting up...".to_string(),
            },
            String::new(),
        ]);
    }

    pub fn healthy(&self) -> bool {
        self.ok
    }
}

/// The display, run on its OWN thread.
///
/// The i2c write to a HD44780 backpack can block indefinitely if the bus wedges
/// (a marginal connector clamping the line in a hot enclosure) - and this used
/// to sit in the control loop, so a flaky screen could freeze the whole
/// controller mid-STARTING with the contactor stuck open and the pump dead.
/// Now the loop only ever hands over already-rendered lines through a mutex it
/// releases before the blocking write, so a wedged bus can hang ONLY this
/// thread. The pump keeps running and protecting even if the screen is gone.
pub struct Display {
    slot: Arc<Mutex<Frame>>,
    healthy: Arc<AtomicBool>,
}

#[derive(Default)]
struct Frame {
    lines: Option<[String; ROWS]>,
    stop: bool,
}

impl Display {
    pub fn spawn(addresses: &'static [u16]) -> Self {
        let slot: Arc<Mutex<Frame>> = Arc::new(Mutex::new(Frame::default()));
        let healthy = Arc::new(AtomicBool::new(false));
        let s = Arc::clone(&slot);
        let h = Arc::clone(&healthy);
        std::thread::spawn(move || {
            let mut lcd: Option<Lcd> = None;
            let mut splashed = false;
            let mut retry = 0u32;
            loop {
                // Take the pending frame under the lock, then RELEASE it before
                // the blocking i2c write - so the loop's set() is never blocked.
                let (lines, stop) = {
                    let mut g = s.lock().expect("display slot poisoned");
                    (g.lines.take(), g.stop)
                };
                if lcd.is_none() {
                    retry = retry.wrapping_add(1);
                    if retry % 20 == 0 {
                        // ~every 2s: a reseated connector recovers on its own.
                        lcd = Lcd::open_first(addresses).ok();
                        splashed = false;
                    }
                }
                if let Some(l) = &mut lcd {
                    if !splashed {
                        l.splash(None);
                        splashed = true;
                    }
                    if let Some(ln) = &lines {
                        l.show(ln);
                    }
                    h.store(l.healthy(), Ordering::Relaxed);
                }
                if stop {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        Display { slot, healthy }
    }

    /// Hand the latest rendered frame to the display thread. Never touches the
    /// i2c bus - just drops it in the slot for the thread to paint.
    pub fn set(&self, lines: [String; ROWS]) {
        if let Ok(mut g) = self.slot.lock() {
            g.lines = Some(lines);
        }
    }

    /// Whether the last paint reached the screen cleanly. Read from an atomic,
    /// so it is a snapshot and never blocks even if the bus is wedged.
    pub fn healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Best-effort final frame + stop. If the thread is wedged it simply never
    /// runs, which is fine - the contactor is already open by now.
    pub fn stop_with(&self, lines: [String; ROWS]) {
        if let Ok(mut g) = self.slot.lock() {
            g.lines = Some(lines);
            g.stop = true;
        }
    }
}

/// A short progress bar, used to show "how close are we".
fn bar(fraction: f32, width: usize) -> String {
    let filled = ((fraction.clamp(0.0, 1.0)) * width as f32).round() as usize;
    let mut s = String::with_capacity(width);
    for i in 0..width {
        s.push(if i < filled { '#' } else { '.' });
    }
    s
}

fn hms(seconds: u64) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    if h > 0 {
        format!("{h}h {m:02}m")
    } else {
        format!("{m}m {:02}s", seconds % 60)
    }
}

/// A spinner that turns while the pump runs.
///
/// Not decoration: a frozen screen showing plausible numbers looks identical
/// to a working one. If this stops turning, the controller has stopped
/// updating and the reading on screen is stale.
fn spinner(tick: u64) -> char {
    const FRAMES: [char; 4] = ['-', '\\', '|', '/'];
    FRAMES[(tick % 4) as usize]
}

/// Build the four lines for the current state.
///
/// Every screen keeps the same four jobs in the same four rows, so the
/// layout is learned once and afterwards read by position:
///
///   1  what is happening      state word at column 1, brand, spinner
///   2  the number that matters now
///   3  why, in plain words
///   4  what happens next, or what to press
///
/// The spinner is on every screen without exception: a wedged controller
/// showing a plausible stale reading is the failure this display exists to
/// make visible.
/// One screen, always.
///
/// There is no menu because there is no control to drive one: the handle
/// gives you the mode and the two buttons give you the pump. Everything
/// that would need navigating - thresholds, commissioning, history - lives
/// on the phone, where there is room to explain itself.
pub fn render(
    m: &Machine,
    s: &Settings,
    now: u64,
    ip: Option<&str>,
    tick: u64,
    refusal: Option<&str>,
) -> [String; ROWS] {

    // A refusal swaps only the headline. The reason is already on screen
    // underneath, and nothing has to block or blank to show it.
    if let Some(msg) = refusal {
        let mut lines = render_state(m, s, now, tick);
        lines[0] = format!("{:<11} REISBOT{}", "NOT YET", spinner(tick));
        lines[3] = msg.chars().take(COLS).collect();
        return lines;
    }

    let mut lines = render_state(m, s, now, tick);
    // A headless box has to tell you how to reach it. Rotate the network address
    // onto the bottom line for ~5s out of every 25s - only when there IS one,
    // and never over a refusal (which returns above) - so the state rows stay
    // put the rest of the time. On the hotspot it reads 192.168.4.1; on home
    // wifi it is the DHCP address. `:8080` is the dashboard port.
    if let Some(addr) = ip {
        if now % 25 < 5 {
            lines[3] = format!("{addr}:8080").chars().take(COLS).collect();
        }
    }
    lines
}

/// Tank and well contacts on one 20-char line. A dash where no relay is
/// wired yet. Kept terse on purpose - this is glanced at, not read.
fn levels(m: &Machine) -> String {
    fn word(c: Option<bool>, yes: &str, no: &str) -> String {
        match c {
            Some(true) => yes.to_string(),
            Some(false) => no.to_string(),
            None => "--".to_string(),
        }
    }
    format!("TANK {:<4} WELL {}", word(m.tank_contact, "full", "low"),
            word(m.well_contact, "wet", "dry"))
}

fn render_state(m: &Machine, s: &Settings, now: u64, tick: u64) -> [String; ROWS] {
    let r = m.last;
    let volts = r.map_or(0.0, |x| x.volts);
    let amps = r.map_or(0.0, |x| x.amps);
    let spin = spinner(tick);

    let head = |word: &str| -> String {
        let word: String = word.chars().take(11).collect();
        format!("{:<11} REISBOT{}", word, spin)
    };

    // Whether the voltage is climbing or falling is the single most useful
    // word on a waiting screen: it turns "something is wrong" into "wait".
    let trend = match (m.last, m.previous) {
        (Some(now_r), Some(prev)) if now_r.volts > prev.volts + 0.3 => "rising",
        (Some(now_r), Some(prev)) if now_r.volts < prev.volts - 0.3 => "falling",
        _ => "steady",
    };

    match m.state {
        State::Running => [
            head("PUMPING"),
            format!("{:.0}V{:>15}", volts, format!("{amps:.1}A")),
            levels(m),
            format!("run {}", hms(m.uptime(now))),
        ],

        // The headline already says STOPPED or READY, so these rows carry
        // live numbers rather than restating it. The prompt sits on one
        // line, not two.
        State::Ready if m.held_by_operator => [
            head("STOPPED"),
            format!("{:.0}V{:>15}", volts, format!("{amps:.1}A")),
            levels(m),
            "green to start".to_string(),
        ],

        State::Ready => [
            head("READY"),
            format!("{:.0}V{:>15}", volts, format!("{amps:.1}A")),
            levels(m),
            "press green".to_string(),
        ],

        State::Starting => {
            let left = (s.settle_readings.saturating_sub(m.stable) as f32 * 0.5).ceil() as u32;
            let frac = m.stable as f32 / s.settle_readings.max(1) as f32;
            [
                head("STARTING"),
                format!("{:.0}V - {}", volts, trend),
                format!("checking volts, {left}s"),
                bar(frac, COLS),
            ]
        }

        State::Waiting => {
            let (why, need) = if volts >= s.v_high_reset {
                ("too high", format!("{trend} - needs {:.0}V", s.v_high_reset))
            } else if volts <= s.v_low_reset {
                ("too low", format!("{trend} - needs {:.0}V", s.v_low_reset))
            } else {
                ("", format!("{trend}"))
            };
            let next = match m.mode {
                Mode::Auto => "starts on its own",
                _ => "then PRESS GREEN",
            };
            [
                head("WAITING"),
                format!("{:.0}V - {}", volts, why),
                need,
                next.to_string(),
            ]
        }

        State::Lockout => {
            let mins = m.lockout_remaining(now) / 60 + 1;
            [
                head("NO WATER"),
                format!("{:.0}V{:>15}", volts, format!("{amps:.1}A")),
                "pump is not lifting".to_string(),
                format!("restarts in {mins} min"),
            ]
        }

        State::Tripped => {
            let reason = m.reason;
            let third = match reason {
                Some(x) => x.plain().to_string(),
                None => m.detail.chars().take(COLS).collect(),
            };
            let next = match (m.mode, m.retry_in(now)) {
                (Mode::Auto, Some(secs)) if secs > 0 => format!("restarts in {secs}s"),
                (Mode::Auto, _) => "waits for safe volts".to_string(),
                _ => "PRESS GREEN TO START".to_string(),
            };
            [
                head(reason.map_or("STOPPED", |x| x.short())),
                format!("{:.0}V - {}", volts, trend),
                third,
                next,
            ]
        }

        State::Fault => [
            head("NO METER"),
            "meter not answering".to_string(),
            "pump kept off - safe".to_string(),
            "check meter cable".to_string(),
        ],

        State::Off => [
            head("PUMP OFF"),
            format!("{:.0}V on the panels", volts),
            "move switch for AUTO".to_string(),
            "or centre for manual".to_string(),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::meter::Reading;
    use crate::state::{Machine, Mode};
    use std::cell::RefCell;
    use std::rc::Rc;

    // ---- a virtual HD44780 over a PCF8574 backpack ---------------------------
    // Decodes the exact nibble stream the driver writes (RS/EN/data bits, the
    // 8->4-bit handshake, DDRAM addressing) back into a 4x20 character screen.
    // If the driver spoke the protocol wrong, the reconstructed text would be
    // garbled - so an equality check against render() proves byte-fidelity.
    struct Screen {
        prev_en: bool,
        four_bit: bool,
        hi: Option<u8>, // pending high nibble (4-bit mode)
        rs: bool,       // RS captured with the high nibble
        addr: usize,    // DDRAM address
        in_cgram: bool, // a 0x40 command aims writes at CGRAM, not the screen
        cells: [[u8; COLS]; ROWS],
    }
    impl Screen {
        fn new() -> Self {
            Screen {
                prev_en: false,
                four_bit: false,
                hi: None,
                rs: false,
                addr: 0,
                in_cgram: false,
                cells: [[b' '; COLS]; ROWS],
            }
        }
        fn on_byte(&mut self, b: u8) {
            let en = b & EN != 0;
            if self.prev_en && !en {
                // HD44780 latches the nibble on EN's falling edge.
                let nib = (b >> 4) & 0x0F;
                let rs = b & RS != 0;
                if !self.four_bit {
                    // 8-bit init: each nibble is a whole instruction (high nibble).
                    if nib << 4 == 0x20 {
                        self.four_bit = true;
                    }
                } else {
                    match self.hi.take() {
                        None => {
                            self.hi = Some(nib);
                            self.rs = rs;
                        }
                        Some(h) => self.process((h << 4) | nib, self.rs),
                    }
                }
            }
            self.prev_en = en;
        }
        fn process(&mut self, byte: u8, rs: bool) {
            if rs {
                if self.in_cgram {
                    return; // custom-glyph load, not screen text
                }
                if let Some((row, col)) = Self::rc(self.addr) {
                    self.cells[row][col] = byte;
                }
                self.addr = self.addr.wrapping_add(1);
            } else if byte & 0x80 != 0 {
                self.addr = (byte & 0x7F) as usize; // set DDRAM address
                self.in_cgram = false;
            } else if byte & 0x40 != 0 {
                self.in_cgram = true; // set CGRAM address
            } else if byte == 0x01 {
                self.cells = [[b' '; COLS]; ROWS]; // clear
                self.addr = 0;
            } else if byte == 0x02 {
                self.addr = 0; // home
            }
        }
        fn rc(addr: usize) -> Option<(usize, usize)> {
            for (row, &base) in ROW_ADDR.iter().enumerate() {
                let base = base as usize;
                if addr >= base && addr < base + COLS {
                    return Some((row, addr - base));
                }
            }
            None
        }
        fn row_text(&self, row: usize) -> String {
            self.cells[row].iter().map(|&c| c as char).collect()
        }
    }
    struct SimBus(Rc<RefCell<Screen>>);
    impl I2cBus for SimBus {
        fn write_bytes(&mut self, bytes: &[u8]) -> bool {
            let mut s = self.0.borrow_mut();
            for &b in bytes {
                s.on_byte(b);
            }
            true
        }
    }

    // The physical LCD row is always COLS wide: write_row truncates+space-pads
    // every line so a shorter message never leaves a stale tail. Expect that.
    fn padded(text: &str) -> String {
        let mut l: String = text.chars().take(COLS).collect();
        while l.chars().count() < COLS {
            l.push(' ');
        }
        l
    }

    #[test]
    fn the_init_handshake_puts_the_controller_into_4bit_mode() {
        let screen = Rc::new(RefCell::new(Screen::new()));
        let _lcd = Lcd::with_bus(Box::new(SimBus(screen.clone()))).expect("init");
        assert!(
            screen.borrow().four_bit,
            "the 8->4-bit handshake must leave the HD44780 in 4-bit mode"
        );
    }

    #[test]
    fn the_driver_paints_real_dashboard_text_onto_a_virtual_hd44780() {
        // Drive the ACTUAL render() output through the ACTUAL driver into the
        // virtual controller, and require the reconstructed screen to match the
        // rendered lines byte-for-byte. This proves the LCD device end-to-end:
        // pumpd state -> render() -> HD44780 protocol -> readable screen.
        let s = Settings::default();
        let mut m = Machine::new(Mode::Auto);
        feed(&mut m, &s, 130.0, 8.0, s.settle_readings);
        let lines = render(&m, &s, 100, Some("192.168.4.1"), 0, None);

        let screen = Rc::new(RefCell::new(Screen::new()));
        let mut lcd = Lcd::with_bus(Box::new(SimBus(screen.clone()))).expect("init");
        lcd.show(&lines);

        let scr = screen.borrow();
        for row in 0..ROWS {
            assert_eq!(
                scr.row_text(row),
                padded(&lines[row]),
                "row {row} on the virtual LCD must match what render() produced"
            );
        }
        // And it must be genuinely readable, not blank/garbled: some row carries
        // the dashboard address that render() rotated in.
        assert!(
            (0..ROWS).any(|r| scr.row_text(r).contains("192.168.4.1")),
            "the virtual screen should show the dashboard address"
        );
    }

    // A backpack whose writes can be made to fail on demand (simulating a
    // glitching i2c bus in a hot enclosure - the field's garbled-LCD cause).
    struct FailBus {
        screen: Rc<RefCell<Screen>>,
        fail: Rc<std::cell::Cell<bool>>,
    }
    impl I2cBus for FailBus {
        fn write_bytes(&mut self, bytes: &[u8]) -> bool {
            if self.fail.get() {
                return false; // write dropped -> driver counts an error, controller desyncs
            }
            let mut s = self.screen.borrow_mut();
            for &b in bytes {
                s.on_byte(b);
            }
            true
        }
    }

    #[test]
    fn the_display_recovers_after_a_burst_of_write_errors() {
        // The field bug was a FROZEN, GARBLED LCD. The driver's defence: count
        // write errors and, past a threshold, re-init the controller and force a
        // full repaint (clearing the per-row cache). Prove that mechanism fires.
        let screen = Rc::new(RefCell::new(Screen::new()));
        let fail = Rc::new(std::cell::Cell::new(false));
        let mut lcd = Lcd::with_bus(Box::new(FailBus {
            screen: screen.clone(),
            fail: fail.clone(),
        }))
        .expect("init");

        let s = Settings::default();
        let mut m = Machine::new(Mode::Manual);
        feed(&mut m, &s, 130.0, 0.0, s.settle_readings);

        // 1) a clean paint populates the per-row cache
        lcd.show(&render(&m, &s, 1, None, 0, None));
        assert!(lcd.shown.iter().all(|r| r.is_some()), "a good paint fills the row cache");

        // 2) the bus starts glitching AND the screen content changes (a trip), so
        //    the driver actually writes the changed rows - and those writes drop.
        fail.set(true);
        feed(&mut m, &s, 155.0, 0.0, 5); // overvolt -> trip -> different text on several rows
        lcd.show(&render(&m, &s, 2, None, 0, None));
        assert!(lcd.errors >= 3, "dropped writes must accrue errors past the recover threshold");

        // 3) the bus heals; the next show() must recover(): re-init + clear cache +
        //    full repaint, and a clean pass then clears the error count.
        fail.set(false);
        feed(&mut m, &s, 130.0, 0.0, s.settle_readings); // recover -> running text again
        lcd.show(&render(&m, &s, 3, None, 0, None));
        assert_eq!(lcd.errors, 0, "a clean pass after recovery clears the error count");
        assert!(lcd.ok, "the display is marked healthy again after recovery");
        assert!(lcd.shown.iter().all(|r| r.is_some()), "recovery repaints every row");
    }

    #[test]
    fn a_changed_line_repaints_correctly_on_the_virtual_lcd() {
        // The partial-repaint fast path (only changed cells) must still leave the
        // screen equal to the new line - a decode bug there would corrupt it.
        let s = Settings::default();
        let mut m = Machine::new(Mode::Manual);
        feed(&mut m, &s, 130.0, 0.0, s.settle_readings);
        let screen = Rc::new(RefCell::new(Screen::new()));
        let mut lcd = Lcd::with_bus(Box::new(SimBus(screen.clone()))).expect("init");

        let first = render(&m, &s, 1, None, 0, None);
        lcd.show(&first);
        // change state so a different screen renders, exercising the diff path
        feed(&mut m, &s, 150.0, 0.0, 3); // overvolt -> trip -> different text
        let second = render(&m, &s, 50, None, 0, None);
        lcd.show(&second);

        let scr = screen.borrow();
        for row in 0..ROWS {
            assert_eq!(scr.row_text(row), padded(&second[row]), "row {row} after repaint");
        }
    }

    fn feed(m: &mut Machine, s: &Settings, v: f32, a: f32, n: u32) -> u64 {
        let mut t = 0;
        for _ in 0..n {
            m.update(Some(Reading { volts: v, amps: a, watts: v * a, watt_hours: 5000.0 }),
                     t, "2026-08-11", s);
            t += 1;
        }
        t
    }

    #[test]
    fn the_lcd_probe_falls_through_to_the_alternate_backpack_address() {
        // A PCF8574A backpack answers ONLY at 0x3F; 0x27 does not ACK.
        // The probe must skip the dead 0x27 and open at 0x3F. (Negative
        // control: a hardcoded-0x27 open would leave this board blank — the
        // exact field symptom this fix targets.)
        let mut tried = Vec::new();
        let r = probe_first(&LCD_ADDRESSES, |a| {
            tried.push(a);
            if a == 0x3F { Ok("lcd") } else { Err(format!("no ACK at {a:#04x}")) }
        });
        assert_eq!(r, Ok((0x3F, "lcd")), "must open the backpack that actually answers");
        assert_eq!(tried, vec![0x27, 0x3F], "probes 0x27 first, then falls through to 0x3F");
    }

    #[test]
    fn the_lcd_probe_prefers_the_first_responding_address() {
        // A plain PCF8574 at 0x27 is taken immediately; 0x3F is never touched.
        let mut tried = Vec::new();
        let r = probe_first(&LCD_ADDRESSES, |a| {
            tried.push(a);
            Ok::<&str, String>("lcd")
        });
        assert_eq!(r, Ok((0x27, "lcd")));
        assert_eq!(tried, vec![0x27], "stops at the first address that responds");
    }

    #[test]
    fn the_lcd_probe_errors_when_no_backpack_answers() {
        // No display on the bus -> a clean Err, never a false "opened".
        let r: Result<(u16, &str), String> =
            probe_first(&LCD_ADDRESSES, |a| Err(format!("no ACK at {a:#04x}")));
        assert!(r.is_err(), "an empty bus must not report a device");
    }

    #[test]
    fn every_line_fits_the_display() {
        let s = Settings::default();
        for mode in [Mode::Auto, Mode::Manual, Mode::Off] {
            let mut m = Machine::new(mode);
            for (v, a, n) in [(163.0, 0.0, 5), (130.0, 0.0, 40), (126.0, 10.8, 5)] {
                feed(&mut m, &s, v, a, n);
                for tick in 0..4 {
                let lines = render(&m, &s, 100, Some("192.168.1.20"), tick, None);
                for (i, l) in lines.iter().enumerate() {
                    assert!(l.chars().count() <= COLS,
                            "{:?}/{:?} line {} is {} chars: {:?}",
                            mode, m.state, i, l.chars().count(), l);
                }
                }
            }
        }
    }

    #[test]
    fn the_dashboard_address_rotates_onto_the_bottom_line() {
        let s = Settings::default();
        let mut m = Machine::new(Mode::Auto);
        feed(&mut m, &s, 126.0, 0.0, 5); // give it a reading; state is Waiting
        let addr = "192.168.4.1";

        // In the ~5s window (now % 25 < 5) the bottom line shows the address.
        let shown = render(&m, &s, 2, Some(addr), 0, None);
        assert_eq!(shown[3], "192.168.4.1:8080", "IP window shows the dashboard address");

        // Outside the window the normal state line returns.
        let hidden = render(&m, &s, 10, Some(addr), 0, None);
        assert_ne!(hidden[3], "192.168.4.1:8080", "outside the window the state line returns");

        // With no address, the bottom line is never an address.
        let none = render(&m, &s, 2, None, 0, None);
        assert!(!none[3].contains(":8080"), "no IP -> no address shown");

        // A refusal must take precedence over the IP rotation.
        let refused = render(&m, &s, 2, Some(addr), 0, Some("well dry"));
        assert!(refused[3].contains("well dry"), "refusal must win over the IP");
        assert!(!refused[3].contains(":8080"));
    }

    #[test]
    fn every_trip_reason_fits_the_headline() {
        // Two reasons used to overflow and silently truncate, because the
        // width test never drove an overcurrent or a hand stop.
        use crate::state::TripReason::*;
        for reason in [OverVoltage, UnderVoltage, OverCurrent, DryRun, MaxRun, MeterLost, Manual] {
            let head = format!("{:<11} reisbot-", reason.short());
            assert!(
                head.chars().count() <= COLS,
                "{:?} headline is {} chars: {:?}",
                reason, head.chars().count(), head
            );
            assert!(
                reason.plain().chars().count() <= COLS,
                "{:?} plain text is too long: {:?}", reason, reason.plain()
            );
        }
    }

    #[test]
    fn a_refusal_only_swaps_the_headline() {
        let s = Settings::default();
        let mut m = Machine::new(Mode::Manual);
        feed(&mut m, &s, 163.0, 0.0, 3);
        let normal = render(&m, &s, 10, None, 0, None);
        let refused = render(&m, &s, 10, None, 0, Some("Not safe to start"));
        assert_ne!(normal[0], refused[0], "headline should change");
        assert_eq!(normal[1], refused[1], "the reading must stay put");
        assert_eq!(normal[2], refused[2], "the reason must stay put");
        for l in refused.iter() {
            assert!(l.chars().count() <= COLS);
        }
    }

    #[test]
    fn spinner_cycles_through_four_frames() {
        let seen: Vec<char> = (0..8).map(spinner).collect();
        assert_eq!(seen[0], seen[4], "should repeat every 4 ticks");
        assert_eq!(seen[..4].iter().collect::<std::collections::HashSet<_>>().len(), 4);
    }

    #[test]
    fn waiting_screen_states_the_target() {
        let s = Settings::default();
        let mut m = Machine::new(Mode::Auto);
        feed(&mut m, &s, 163.0, 0.0, 3);
        let lines = render(&m, &s, 10, None, 0, None);
        assert!(lines[1].contains("too high"));
        assert!(lines[2].contains("138"));
    }

    #[test]
    fn ready_screen_prompts_the_operator() {
        let s = Settings::default();
        let mut m = Machine::new(Mode::Manual);
        feed(&mut m, &s, 130.0, 0.0, s.settle_readings + 2);
        let lines = render(&m, &s, 100, None, 0, None);
        // The instruction lives on row 4 now - row 1 is the state word.
        // Row 4 names the button by colour - the only property a gloved
        // operator reads at a glance.
        assert!(
            lines[3].to_lowercase().contains("green"),
            "row 4 should name the button: {:?}",
            lines[3]
        );
    }

    #[test]
    fn bar_fills_proportionally() {
        assert_eq!(bar(0.0, 4), "....");
        assert_eq!(bar(0.5, 4), "##..");
        assert_eq!(bar(1.0, 4), "####");
        assert_eq!(bar(9.9, 4), "####");
    }
}

