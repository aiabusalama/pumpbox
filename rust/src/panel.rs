//! Physical controls: a three-position mode switch, start and stop buttons,
//! and a status LED.
//!
//! The shed is not always in phone range, and gloves do not work on
//! touchscreens. Everything needed day to day is reachable without a device.
//!
//! Everything is wired to ground with internal pull-ups enabled, so a broken
//! wire reads as "not pressed" or "centre" - never as a start request.

use crate::state::Mode;
use rppal::gpio::{Gpio, InputPin, Level, OutputPin};
use std::sync::atomic::{AtomicI8, AtomicU8, Ordering};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};


/// Contact bounce on a panel button is a few milliseconds.
const DEBOUNCE: Duration = Duration::from_millis(25);
// A float/probe relay is not a button. The SSRC-04 already debounces the
// water itself, but a wave at the electrode can still chatter the contact,
// so a level must hold for this long before we believe it changed. It only
// delays telemetry - nothing in the pump path waits on it.
const LEVEL_DEBOUNCE: Duration = Duration::from_millis(750);
/// How often the input thread samples. Fixed, unlike the main loop, which
/// stretches whenever the display or the meter takes a turn.
const INPUT_SCAN: Duration = Duration::from_millis(5);

/// Debounce that cannot lose an edge.
///
/// The obvious form ignores any change for a window after the last one.
/// That silently throws away a contact which both closes and re-opens
/// inside the window - and a spring-return lever does exactly that, which
/// is why flicks sometimes did nothing at all. This form only ever delays
/// acceptance: a level has to read the same for DEBOUNCE before it counts,
/// so bounce is filtered but no real transition is ever dropped.
struct Debounced {
    stable: bool,
    candidate: bool,
    since: Instant,
}

impl Debounced {
    fn new() -> Self {
        Debounced { stable: false, candidate: false, since: Instant::now() }
    }

    /// Adopt a level without waiting out the debounce, for startup.
    fn force(&mut self, raw: bool, now: Instant) {
        self.stable = raw;
        self.candidate = raw;
        self.since = now;
    }

    /// Some(true) when the contact has settled closed, Some(false) open.
    fn update(&mut self, raw: bool, now: Instant) -> Option<bool> {
        self.update_win(raw, now, DEBOUNCE)
    }

    /// As update, but with an explicit hold window - buttons want 25 ms, a
    /// water-level contact wants far longer.
    fn update_win(&mut self, raw: bool, now: Instant, window: Duration) -> Option<bool> {
        if raw != self.candidate {
            self.candidate = raw;
            self.since = now;
            return None;
        }
        if self.candidate != self.stable && now.duration_since(self.since) >= window {
            self.stable = self.candidate;
            return Some(self.stable);
        }
        None
    }
}
/// The switch is mechanical and only moves deliberately, so it can afford a
/// longer settle than a button.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Press {
    Short,
}

struct Button {
    pin: InputPin,
    contact: Debounced,
}

impl Button {
    fn new(gpio: &Gpio, bcm: u8) -> Result<Self, String> {
        let pin = gpio
            .get(bcm)
            .map_err(|e| format!("gpio {bcm}: {e}"))?
            .into_input_pullup();
        Ok(Button { pin, contact: Debounced::new() })
    }

    /// One event per physical press, fired the moment the button goes down.
    ///
    /// Acting on press rather than release means the operator gets immediate
    /// feedback and never has to think about how long they held it - a panel
    /// button on a shed door gets pressed firmly, not tapped.
    fn poll(&mut self, now: Instant) -> Option<Press> {
        let raw = self.pin.read() == Level::Low; // pulled up: low means pressed
        match self.contact.update(raw, now) {
            Some(true) => Some(Press::Short),
            _ => None,
        }
    }
}

/// Three-position centre-off switch driving the mode.
///
/// The two outer positions each ground one pin; the centre grounds neither.
/// That makes centre the state a disconnected switch falls back to, so a
/// broken wire lands on MANUAL - safe, because manual never starts the pump
/// on its own.
///
///   left  grounded -> OFF
///   centre         -> MANUAL
///   right grounded -> AUTO
/// Three-position maintained selector: the handle *is* the mode.
///
/// A spring-return lever cannot show mode, so the software had to latch it,
/// persist it, and give it a screen. A maintained switch makes all of that
/// unnecessary - the position is readable across the yard with the power
/// off, and it cannot disagree with what the controller believes.
///
/// Left grounds one pin, right the other, centre grounds neither. Centre is
/// therefore what a disconnected switch falls back to, so a broken wire
/// lands on MANUAL - which arms nothing on its own.
///
/// The maintained 3-position switch has an open lever slot and will let
/// water into the enclosure. The sealed switch returns to centre the
/// instant it is let go, so position cannot be the mode - the mode is
/// latched here and each flick steps it one place along.
///
/// The order is Off - Manual - Auto. Left steps toward Off, right toward
/// Auto, and both ends clamp rather than wrap. That means flicking left
/// repeatedly always ends at Off and can never roll round into Auto,
/// which is the direction that matters when something is going wrong.
struct ModeSwitch {
    left: InputPin,
    right: InputPin,
    l: Debounced,
    r: Debounced,
    current: Mode,
}

impl ModeSwitch {
    fn new(gpio: &Gpio, left_bcm: u8, right_bcm: u8) -> Result<Self, String> {
        let left = gpio
            .get(left_bcm)
            .map_err(|e| format!("gpio {left_bcm}: {e}"))?
            .into_input_pullup();
        let right = gpio
            .get(right_bcm)
            .map_err(|e| format!("gpio {right_bcm}: {e}"))?
            .into_input_pullup();

        let mut s = ModeSwitch {
            left,
            right,
            l: Debounced::new(),
            r: Debounced::new(),
            current: Mode::Manual,
        };
        // Seed from where the handle actually is, so startup agrees with
        // the panel rather than waiting for someone to touch it.
        let now = Instant::now();
        s.l.force(s.left.read() == Level::Low, now);
        s.r.force(s.right.read() == Level::Low, now);
        s.current = s.settled();
        Ok(s)
    }

    /// One step per flick, on the closing edge. Holding the lever over does
    /// nothing further - it has to spring back before it counts again.
    /// Where the handle is now, from the debounced contacts.
    fn settled(&self) -> Mode {
        match (self.l.stable, self.r.stable) {
            (true, false) => Mode::Off,
            (false, true) => Mode::Auto,
            // Centre, or both closed mid-throw. Manual is the safe reading.
            _ => Mode::Manual,
        }
    }

    /// Reports the mode only on the tick it changes.
    fn poll(&mut self, now: Instant) -> Option<Mode> {
        self.l.update(self.left.read() == Level::Low, now);
        self.r.update(self.right.read() == Level::Low, now);

        let now_mode = self.settled();
        if now_mode != self.current {
            self.current = now_mode;
            return Some(now_mode);
        }
        None
    }

}

/// Test jumper selecting the threshold profile.
///
/// Open (no jumper) is the production state: the real pump thresholds, with
/// no hardware that could fall out in a shed years from now. Fitting the
/// jumper drops to bench values for testing on a low voltage supply.
///
/// Both failure directions land on "the pump will not start", never on
/// "the pump runs on the wrong limits": a jumper that falls out in the
/// field restores production thresholds, and one that shorts closed on a
/// real array gives bench limits that 143V can never satisfy.
struct ProfileJumper {
    pin: InputPin,
}

impl ProfileJumper {
    fn new(gpio: &Gpio, bcm: u8) -> Result<Self, String> {
        let pin = gpio
            .get(bcm)
            .map_err(|e| format!("gpio {bcm}: {e}"))?
            .into_input_pullup();

        // The internal pull-up needs a moment to charge the pin and any
        // wiring capacitance. Reading immediately samples a floating low,
        // which would report a fitted jumper when none is present.
        std::thread::sleep(Duration::from_millis(5));

        Ok(ProfileJumper { pin })
    }

    /// Sample several times and require agreement. This decides whether the
    /// trip point is 145V or 18V, so it is worth being certain rather than
    /// fast.
    fn profile_settled(&self) -> crate::settings::Profile {
        let mut low = 0;
        for _ in 0..5 {
            if self.pin.read() == Level::Low {
                low += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        if low >= 4 {
            crate::settings::Profile::Bench
        } else {
            crate::settings::Profile::Pump
        }
    }
}

/// Blink patterns, chosen so a single green LED can still distinguish every
/// state. The panel has one LED between the start and stop buttons, so
/// anything that relies on a second colour would read as "off" - which is
/// also what a dead controller looks like.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Led {
    /// Nothing happening, deliberately. Only the OFF position uses this.
    Off,
    /// Solid: pumping.
    Solid,
    /// Slow heartbeat: ready or waiting for conditions.
    Slow,
    /// Steady fast blink: stopped on a fault, will retry.
    Fast,
    /// Two quick blinks then a pause: over-voltage.
    DoubleBlink,
    /// Three quick blinks then a pause: under-voltage.
    TripleBlink,
    /// Long-short, like a heartbeat skipping: the well is resting.
    Heartbeat,
    /// Rapid stutter: no reading from the meter, running blind is refused.
    Stutter,
}

impl Led {
    fn from_u8(v: u8) -> Led {
        match v {
            1 => Led::Solid,
            2 => Led::Slow,
            3 => Led::Fast,
            4 => Led::DoubleBlink,
            5 => Led::TripleBlink,
            6 => Led::Heartbeat,
            7 => Led::Stutter,
            _ => Led::Off,
        }
    }
}

struct StatusLed {
    green: OutputPin,
    red: Option<OutputPin>,
    started: Instant,
}

impl StatusLed {
    fn new(gpio: &Gpio, green_bcm: u8, red_bcm: Option<u8>) -> Result<Self, String> {
        Ok(StatusLed {
            green: gpio.get(green_bcm).map_err(|e| e.to_string())?.into_output(),
            red: match red_bcm {
                Some(b) => Some(gpio.get(b).map_err(|e| e.to_string())?.into_output()),
                None => None,
            },
            started: Instant::now(),
        })
    }

    /// Whether the LED should be lit right now.
    ///
    /// Driven from elapsed wall-clock time rather than a call counter,
    /// because this is called both on the poll tick and on every button
    /// press - a counter would advance irregularly and the blink would
    /// visibly stutter.
    fn lit(&self, want: Led) -> bool {
        let ms = self.started.elapsed().as_millis() as u64;

        match want {
            Led::Off => false,
            Led::Solid => true,
            // 2s period, on for the first half.
            Led::Slow => ms % 2000 < 1000,
            // 500ms period.
            Led::Fast => ms % 500 < 250,
            // Two 150ms blinks, then dark until the 1.6s mark.
            Led::DoubleBlink => {
                let t = ms % 1600;
                t < 150 || (t >= 300 && t < 450)
            }
            // Three blinks in the same window.
            Led::TripleBlink => {
                let t = ms % 1600;
                t < 150 || (t >= 300 && t < 450) || (t >= 600 && t < 750)
            }
            // Long pulse, short pulse, pause.
            Led::Heartbeat => {
                let t = ms % 2000;
                t < 400 || (t >= 600 && t < 750)
            }
            // Rapid, unmistakably wrong.
            Led::Stutter => ms % 200 < 100,
        }
    }

    fn apply(&mut self, want: Led) {
        let on = self.lit(want);
        if on {
            self.green.set_high();
        } else {
            self.green.set_low();
        }
        // A second LED, if one is ever fitted, mirrors the fault patterns.
        if let Some(r) = &mut self.red {
            let fault = matches!(
                want,
                Led::Fast | Led::DoubleBlink | Led::TripleBlink | Led::Stutter
            );
            if fault && on {
                r.set_high();
            } else {
                r.set_low();
            }
        }
    }
}

/// Drives the status LED from its own thread.
///
/// Blink timing has to be independent of the control loop. The loop does
/// I2C writes, serial reads and occasional file saves, any of which can
/// take tens of milliseconds - enough to visibly stutter a 150ms pulse.
/// The thread does nothing but toggle a pin on a 10ms tick, and the loop
/// only ever tells it which pattern to show.
pub struct LedThread {
    want: Arc<AtomicU8>,
}

impl LedThread {
    fn spawn(mut led: StatusLed) -> Self {
        let want = Arc::new(AtomicU8::new(Led::Off as u8));
        let shared = Arc::clone(&want);

        std::thread::spawn(move || loop {
            let pattern = Led::from_u8(shared.load(Ordering::Relaxed));
            led.apply(pattern);
            std::thread::sleep(Duration::from_millis(10));
        });

        LedThread { want }
    }

    fn set(&self, pattern: Led) {
        self.want.store(pattern as u8, Ordering::Relaxed);
    }
}

/// Reads the switch and buttons on a thread of their own.
///
/// Scanning from the control loop meant the sample interval stretched
/// whenever the display redrew or the meter was read, so a quick flick
/// could fall between two samples. Here the interval is fixed, and events
/// go into a queue, so nothing is lost between one drain and the next.
struct InputThread {
    events: Arc<Mutex<VecDeque<Input>>>,
    /// Current settled level-relay states, index 0 = tank, 1 = well.
    /// -1 unknown, 0 open, 1 closed. Read by Panel::levels; never affects
    /// the event queue, so a level change can never look like a button press.
    levels: Arc<[AtomicI8; 2]>,
}

impl InputThread {
    fn spawn(
        mut sw: Option<ModeSwitch>,
        mut start: Option<Button>,
        mut stop: Option<Button>,
        mut tank: Option<Button>,
        mut well: Option<Button>,
    ) -> Self {
        let events: Arc<Mutex<VecDeque<Input>>> = Arc::new(Mutex::new(VecDeque::new()));
        let queue = Arc::clone(&events);
        let levels: Arc<[AtomicI8; 2]> = Arc::new([AtomicI8::new(-1), AtomicI8::new(-1)]);
        let level_state = Arc::clone(&levels);

        std::thread::spawn(move || loop {
            let now = Instant::now();
            let mut found = Vec::new();

            // Level relays: publish the settled contact state, do NOT queue
            // it as an event. A relay reads pressed=low=closed=water present.
            for (idx, sensor) in [tank.as_mut(), well.as_mut()].into_iter().enumerate() {
                if let Some(b) = sensor {
                    let raw = b.pin.read() == Level::Low;
                    let settled = b.contact.update_win(raw, now, LEVEL_DEBOUNCE);
                    let cur = if let Some(v) = settled {
                        if v { 1 } else { 0 }
                    } else if b.contact.stable { 1 } else { 0 };
                    level_state[idx].store(cur, Ordering::Relaxed);
                }
            }

            if let Some(m) = sw.as_mut().and_then(|s| s.poll(now)) {
                found.push(Input { mode: Some(m), ..Input::default() });
            }
            if let Some(p) = start.as_mut().and_then(|b| b.poll(now)) {
                found.push(Input { start: Some(p), ..Input::default() });
            }
            if let Some(p) = stop.as_mut().and_then(|b| b.poll(now)) {
                found.push(Input { stop: Some(p), ..Input::default() });
            }

            if !found.is_empty() {
                let mut q = queue.lock().expect("input queue poisoned");
                // A human cannot outrun the drain, so this only grows if the
                // control loop has stopped. Cap it rather than bank presses
                // to replay in a burst later.
                for ev in found {
                    if q.len() < 16 {
                        q.push_back(ev);
                    }
                }
            }

            std::thread::sleep(INPUT_SCAN);
        });

        InputThread { events, levels }
    }

    /// (tank, well) settled contact states, None when no relay is fitted.
    fn levels(&self) -> (Option<bool>, Option<bool>) {
        let read = |i: usize| match self.levels[i].load(Ordering::Relaxed) {
            1 => Some(true),
            0 => Some(false),
            _ => None,
        };
        (read(0), read(1))
    }

    /// One event per call, oldest first, so two quick flicks stay two steps.
    fn next(&self) -> Input {
        self.events
            .lock()
            .expect("input queue poisoned")
            .pop_front()
            .unwrap_or_default()
    }
}

pub struct Panel {
    inputs: InputThread,
    profile_jumper: Option<ProfileJumper>,
    led: Option<LedThread>,
}

#[derive(Debug, Default, Clone, Copy)]
pub struct Input {
    /// Set on the tick the selector settles into a new position.
    pub mode: Option<Mode>,
    pub start: Option<Press>,
    pub stop: Option<Press>,
}

pub struct Pins {
    /// Ground this pin to select the pump profile. Left open it stays on
    /// bench thresholds.
    pub profile_jumper: Option<u8>,
    pub switch_left: Option<u8>,
    pub switch_right: Option<u8>,
    pub start: Option<u8>,
    pub stop: Option<u8>,
    pub led_green: Option<u8>,
    pub led_red: Option<u8>,
    /// Dry-contact level relays (e.g. hacked SSRC-04): pin pulled up, the
    /// relay's isolated C-NO/NC shorts it to ground. Left None on a board
    /// that has no level relays fitted.
    pub tank_relay: Option<u8>,
    pub well_relay: Option<u8>,
}

impl Panel {
    /// Every control is optional so the controller runs headless on a bench.
    /// A missing one simply never fires.
    pub fn new(pins: Pins) -> Self {
        let gpio = match Gpio::new() {
            Ok(g) => g,
            Err(e) => {
                eprintln!("gpio unavailable, panel disabled: {e}");
                return Panel {
                    inputs: InputThread::spawn(None, None, None, None, None),
                    profile_jumper: None,
                    led: None,
                };
            }
        };

        let btn = |p: Option<u8>| -> Option<Button> {
            p.and_then(|bcm| match Button::new(&gpio, bcm) {
                Ok(b) => Some(b),
                Err(e) => {
                    eprintln!("button on gpio {bcm} unavailable: {e}");
                    None
                }
            })
        };

        let mode_switch = match (pins.switch_left, pins.switch_right) {
            (Some(l), Some(r)) => match ModeSwitch::new(&gpio, l, r) {
                Ok(s) => {
                    println!("mode switch reads {}", s.current.label());
                    Some(s)
                }
                Err(e) => {
                    eprintln!("selector unavailable: {e}");
                    None
                }
            },
            _ => None,
        };

        let led = match pins.led_green {
            Some(g) => match StatusLed::new(&gpio, g, pins.led_red) {
                Ok(l) => Some(LedThread::spawn(l)),
                Err(e) => {
                    eprintln!("status led unavailable: {e}");
                    None
                }
            },
            _ => None,
        };

        let profile_jumper = pins.profile_jumper.and_then(|bcm| {
            match ProfileJumper::new(&gpio, bcm) {
                Ok(j) => {
                    println!("profile jumper reads {}", j.profile_settled().label());
                    Some(j)
                }
                Err(e) => {
                    eprintln!("profile jumper unavailable: {e}");
                    None
                }
            }
        });

        let tank_b = btn(pins.tank_relay);
        let well_b = btn(pins.well_relay);
        println!("level relays: tank(gpio{:?})={}, well(gpio{:?})={}",
            pins.tank_relay, if tank_b.is_some() {"ok"} else {"MISSING"},
            pins.well_relay, if well_b.is_some() {"ok"} else {"MISSING"});
        Panel {
            inputs: InputThread::spawn(
                mode_switch,
                btn(pins.start),
                btn(pins.stop),
                tank_b,
                well_b,
            ),
            profile_jumper,
            led,
        }
    }

    pub fn poll(&mut self) -> Input {
        self.inputs.next()
    }

    /// (tank, well) level-relay contact states. true = contact closed.
    /// None when no relay is wired to that pin. Interpretation of closed
    /// vs open into "full" / "has water" is the caller's job, so the
    /// fail-safe polarity lives in one place.
    pub fn levels(&self) -> (Option<bool>, Option<bool>) {
        self.inputs.levels()
    }

    /// Which profile the commissioning jumper selects, if one is fitted.
    pub fn profile(&self) -> Option<crate::settings::Profile> {
        self.profile_jumper.as_ref().map(|j| j.profile_settled())
    }

    /// Hand the pattern to the LED thread. Returns immediately - the
    /// control loop never waits on a blink.
    pub fn set_led(&mut self, want: Led) {
        if let Some(l) = &self.led {
            l.set(want);
        }
    }
}

/// What the LED shows for a given state and trip reason.
///
/// The panel has a single green LED, so every state has to be told apart by
/// rhythm alone. Over- and under-voltage get distinct blink counts because
/// they are the two faults an operator can actually act on: one means wait
/// for the sun to warm the panels, the other means wait for more of it.
/// `blocked` says why the machine is sitting in Waiting, which Waiting
/// itself does not carry. Without it a pump refusing to start on 25V looks
/// exactly like one that is armed and happy - same slow pulse - and the
/// only way to tell them apart is to walk over and read the screen.
pub fn led_for(
    state: crate::state::State,
    reason: Option<crate::state::TripReason>,
    blocked: Option<crate::state::TripReason>,
) -> Led {
    use crate::state::State::*;
    use crate::state::TripReason;

    match state {
        Running => Led::Solid,
        Ready | Starting => Led::Slow,
        Waiting => match blocked {
            Some(TripReason::OverVoltage) => Led::DoubleBlink,
            Some(TripReason::UnderVoltage) => Led::TripleBlink,
            _ => Led::Slow,
        },
        Lockout => Led::Heartbeat,
        Fault => Led::Stutter,
        Off => Led::Off,
        Tripped => match reason {
            Some(TripReason::OverVoltage) => Led::DoubleBlink,
            Some(TripReason::UnderVoltage) => Led::TripleBlink,
            Some(TripReason::MeterLost) => Led::Stutter,
            // A hand stop is not a fault - show the same calm pulse as
            // Ready, because that is what it is. Reaching the run limit is
            // not a fault either: the pump did its work and the controller
            // ended it, so it gets the same calm pulse rather than the
            // fast blink that means something broke.
            Some(TripReason::Manual) | Some(TripReason::MaxRun) => Led::Slow,
            _ => Led::Fast,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{State, TripReason};

    #[test]
    fn a_refused_start_is_told_apart_from_a_happy_one() {
        // A pump sitting at 25V on a 17V limit must not look the same as
        // one waiting patiently in band.
        assert_eq!(
            led_for(State::Waiting, None, Some(TripReason::OverVoltage)),
            Led::DoubleBlink
        );
        assert_eq!(
            led_for(State::Waiting, None, Some(TripReason::UnderVoltage)),
            Led::TripleBlink
        );
        assert_eq!(led_for(State::Waiting, None, None), Led::Slow);
    }

    #[test]
    fn volts_read_the_same_whether_refused_or_tripped() {
        // One vocabulary: two blinks means high, three means low, and it
        // must not matter which side of a start it happened on.
        for r in [TripReason::OverVoltage, TripReason::UnderVoltage] {
            assert_eq!(
                led_for(State::Waiting, None, Some(r)),
                led_for(State::Tripped, Some(r), None)
            );
        }
    }

    #[test]
    fn led_distinguishes_every_state() {
        // With one LED, rhythm is the only thing telling states apart, so
        // the pairs most easily confused must not share a pattern.
        assert_ne!(led_for(State::Running, None, None), led_for(State::Ready, None, None));
        assert_ne!(led_for(State::Lockout, None, None), led_for(State::Waiting, None, None));
        assert_ne!(
            led_for(State::Tripped, Some(TripReason::OverVoltage), None),
            led_for(State::Tripped, Some(TripReason::UnderVoltage), None),
            "over and under voltage must look different"
        );
        assert_ne!(
            led_for(State::Fault, None, None),
            led_for(State::Tripped, Some(TripReason::OverVoltage), None)
        );

        // Dark means one thing only: deliberately switched off.
        assert_eq!(led_for(State::Off, None, None), Led::Off);
        for st in [State::Running, State::Ready, State::Waiting,
                   State::Starting, State::Lockout, State::Tripped, State::Fault] {
            assert_ne!(led_for(st, None, None), Led::Off, "{st:?} must not look dead");
        }
    }

    #[test]
    fn blink_patterns_are_time_based_not_call_based() {
        // apply() is called both on the poll tick and on every button press.
        // A call counter would make the blink stutter; elapsed time must not.
        let gpio = match Gpio::new() {
            Ok(g) => g,
            Err(_) => return, // no hardware in CI
        };
        if let Ok(led) = StatusLed::new(&gpio, 19, None) {
            let a = led.lit(Led::Slow);
            let b = led.lit(Led::Slow);
            assert_eq!(a, b, "two reads in the same instant must agree");
        }
    }
}

#[cfg(test)]
mod debounce {
    use super::*;

    /// Drive a contact through a sequence of (level, milliseconds-held)
    /// steps sampled every INPUT_SCAN, and collect the settled events.
    fn run(steps: &[(bool, u64)]) -> Vec<bool> {
        let mut d = Debounced::new();
        let base = Instant::now();
        let mut t = 0u64;
        let mut out = Vec::new();
        for (level, hold) in steps {
            let until = t + hold;
            while t < until {
                if let Some(e) = d.update(*level, base + Duration::from_millis(t)) {
                    out.push(e);
                }
                t += INPUT_SCAN.as_millis() as u64;
            }
        }
        out
    }

    #[test]
    fn a_normal_flick_registers_once() {
        assert_eq!(run(&[(false, 50), (true, 120), (false, 200)]), vec![true, false]);
    }

    #[test]
    fn two_quick_flicks_both_register() {
        // The regression. The old lockout debounce discarded any edge
        // inside its window, so the second flick vanished and the operator
        // had to do it again.
        let events = run(&[
            (false, 50),
            (true, 80),
            (false, 60),
            (true, 80),
            (false, 100),
        ]);
        assert_eq!(events, vec![true, false, true, false]);
        assert_eq!(events.iter().filter(|e| **e).count(), 2, "both closes seen");
    }

    #[test]
    fn contact_bounce_is_one_event_not_several() {
        let mut steps = vec![(false, 50)];
        for _ in 0..6 {
            steps.push((true, 5));
            steps.push((false, 5));
        }
        steps.push((true, 120));
        steps.push((false, 200));
        assert_eq!(run(&steps), vec![true, false], "bounce must not step twice");
    }

    #[test]
    fn a_glitch_shorter_than_the_window_is_ignored() {
        assert!(run(&[(false, 50), (true, 10), (false, 200)]).is_empty());
    }

    #[test]
    fn a_held_contact_fires_once_not_repeatedly() {
        assert_eq!(run(&[(false, 50), (true, 3000)]), vec![true]);
    }

    /// Mirror the InputThread publish rule: cur = a freshly settled value,
    /// otherwise the last stable one. Returns whether "wet" was ever published.
    fn ever_published_wet(steps: &[(bool, u64)]) -> bool {
        let mut d = Debounced::new(); // starts stable=false -> dry/open, fail-safe
        let base = Instant::now();
        let mut t = 0u64;
        let mut wet = false;
        for (level, hold) in steps {
            let until = t + hold;
            while t < until {
                let settled = d.update_win(*level, base + Duration::from_millis(t), LEVEL_DEBOUNCE);
                let cur = match settled {
                    Some(v) => v as i8,
                    None => d.stable as i8,
                };
                if cur == 1 {
                    wet = true;
                }
                t += INPUT_SCAN.as_millis() as u64;
            }
        }
        wet
    }

    #[test]
    fn a_flickering_well_never_reads_wet_below_the_level_window() {
        // A dry well whose contact merely twitches (flaps faster than the
        // 750 ms level window) must NEVER be published as wet - otherwise the
        // pump could be told there is water when there is not, and dry-run.
        let mut steps = Vec::new();
        for _ in 0..12 {
            steps.push((true, 200)); // wet, but only 200 ms < 750 ms
            steps.push((false, 200));
        }
        assert!(!ever_published_wet(&steps), "flapping under the window must stay dry");
    }

    #[test]
    fn a_well_reads_wet_only_after_a_full_level_window() {
        let mut d = Debounced::new();
        let base = Instant::now();
        let mut t = 0u64;
        let mut got = None;
        while t < 700 {
            got = got.or(d.update_win(true, base + Duration::from_millis(t), LEVEL_DEBOUNCE));
            t += INPUT_SCAN.as_millis() as u64;
        }
        assert_eq!(got, None, "700 ms is under the 750 ms window - not wet yet");
        while t < 950 {
            if let Some(v) = d.update_win(true, base + Duration::from_millis(t), LEVEL_DEBOUNCE) {
                got = Some(v);
            }
            t += INPUT_SCAN.as_millis() as u64;
        }
        assert_eq!(got, Some(true), "held past 750 ms - now genuinely wet");
    }

    #[test]
    fn a_well_going_dry_is_published_dry_after_the_window() {
        // Settle wet, then lose water: after the window it must read dry.
        let steps = [(true, 1000u64), (false, 1000u64)];
        let mut d = Debounced::new();
        let base = Instant::now();
        let mut t = 0u64;
        let mut last = 0i8;
        for (level, hold) in steps {
            let until = t + hold;
            while t < until {
                let settled = d.update_win(level, base + Duration::from_millis(t), LEVEL_DEBOUNCE);
                last = match settled {
                    Some(v) => v as i8,
                    None => d.stable as i8,
                };
                t += INPUT_SCAN.as_millis() as u64;
            }
        }
        assert_eq!(last, 0, "a well that lost its water must read dry");
    }
}
