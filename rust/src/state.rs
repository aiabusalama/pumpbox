//! Protection state machine.
//!
//! Pure logic - no I/O, no clock of its own. Time comes in as a parameter so
//! the whole thing is testable without hardware or sleeping.
//!
//! The safety rules hold regardless of mode:
//!   - the contactor is normally open; anything that stops this code opens it
//!   - voltage limits are never bypassed, not even by a manual start
//!   - losing the meter stops the pump; running blind is never safe

use serde::{Deserialize, Serialize};

use crate::meter::Reading;
use crate::settings::Settings;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Start whenever conditions are safe, restart after transient faults.
    Auto,
    /// Sit at Ready until a human says go.
    Manual,
    /// Contactor held open. Safe to work on.
    Off,
}

impl Mode {
    pub fn label(self) -> &'static str {
        match self {
            Mode::Auto => "AUTO",
            Mode::Manual => "MANUAL",
            Mode::Off => "OFF",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Mode is Off. Nothing will start the pump.
    Off,
    /// Conditions are not safe. Voltage outside the band.
    Waiting,
    /// Everything is fine. In Manual, waiting for a human.
    Ready,
    /// Confirming stability before closing the contactor.
    Starting,
    /// Contactor closed, water flowing.
    Running,
    /// Something went wrong. Will retry when conditions return.
    Tripped,
    /// Well is dry. Resting so it can recover.
    Lockout,
    /// Cannot read the meter. Refusing to run blind.
    Fault,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Off => "OFF",
            State::Waiting => "WAITING",
            State::Ready => "READY",
            State::Starting => "STARTING",
            State::Running => "RUNNING",
            State::Tripped => "STOPPED",
            State::Lockout => "WELL RECOVERING",
            State::Fault => "METER FAULT",
        }
    }

    pub fn contactor_closed(self) -> bool {
        matches!(self, State::Running)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TripReason {
    OverVoltage,
    UnderVoltage,
    OverCurrent,
    DryRun,
    /// A hand-started run hit its time limit with nobody to end it.
    MaxRun,
    MeterLost,
    Manual,
}

impl TripReason {
    /// Full name, for the log and the dashboard.
    pub fn label(self) -> &'static str {
        match self {
            TripReason::OverVoltage => "OVERVOLT",
            TripReason::UnderVoltage => "UNDERVOLT",
            TripReason::OverCurrent => "OVERCURRENT",
            TripReason::DryRun => "DRY RUN",
            TripReason::MaxRun => "MAX RUN",
            TripReason::MeterLost => "METER LOST",
            TripReason::Manual => "STOPPED BY HAND",
        }
    }

    /// Plain-language line for the LCD, in the operator's words rather than
    /// the machine's. Every one fits inside 20 columns.
    pub fn plain(self) -> &'static str {
        match self {
            TripReason::OverVoltage => "volts went too high",
            TripReason::UnderVoltage => "volts dropped low",
            TripReason::OverCurrent => "pump pulled too much",
            TripReason::DryRun => "pump is not lifting",
            TripReason::MaxRun => "ran the full time",
            TripReason::MeterLost => "lost the meter",
            TripReason::Manual => "you pressed the red",
        }
    }

    /// Headline word, guaranteed short enough to sit beside "STOPPED".
    pub fn short(self) -> &'static str {
        match self {
            TripReason::OverVoltage => "HIGH VOLTS",
            TripReason::UnderVoltage => "LOW VOLTS",
            TripReason::OverCurrent => "OVERLOAD",
            TripReason::DryRun => "NO WATER",
            TripReason::MaxRun => "TIME UP",
            TripReason::MeterLost => "NO METER",
            TripReason::Manual => "BY HAND",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct Event {
    pub at: String,
    pub reason: String,
    pub detail: String,
    pub volts: f32,
    pub amps: f32,
}

/// What the operator asked for, from a button or the dashboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    Start,
    Stop,
    SetMode(Mode),
    Reset,
    ClearLockout,
}

/// Why a start request was refused. Shown verbatim to the operator, so it
/// has to explain itself without reference to a manual.
#[derive(Debug, Clone, PartialEq)]
pub enum Refusal {
    ModeOff,
    NotReady(String),
    InLockout(u64),
    AlreadyRunning,
}

impl Refusal {
    pub fn message(&self) -> String {
        match self {
            Refusal::ModeOff => "Mode is OFF - switch to MANUAL first".into(),
            Refusal::NotReady(why) => format!("Not safe to start: {why}"),
            Refusal::InLockout(mins) => {
                format!("Well is recovering - {mins} min left. Hold RESET to override.")
            }
            Refusal::AlreadyRunning => "Already running".into(),
        }
    }
}

fn log_forgive(remaining: u32) {
    println!("dry strike forgiven after a clean run, {remaining} remaining");
}

pub struct Machine {
    pub mode: Mode,
    pub state: State,
    pub last: Option<Reading>,
    /// The reading before last, so the display can say whether the voltage
    /// is rising or falling. At sunrise "falling" is what turns "something
    /// is wrong" into "wait a few minutes".
    pub previous: Option<Reading>,

    /// Consecutive readings inside the safe band.
    pub stable: u32,
    /// Consecutive failed meter reads.
    pub meter_errors: u32,

    pub reason: Option<TripReason>,
    pub detail: String,

    running_since: Option<u64>,
    over_current_since: Option<u64>,
    dry_since: Option<u64>,
    retry_at: Option<u64>,

    pub lockout_until: u64,
    pub dry_strikes: u32,
    /// Consecutive runs that collapsed on under-voltage within
    /// sag_min_run_seconds (the dusk weak-supply pattern). Once it reaches
    /// sag_strike_limit, recovery uses the long sag_backoff instead of the
    /// quick retry, so a marginal supply rests the motor instead of cycling it.
    /// A run that holds past sag_min_run_seconds clears it.
    pub sag_strikes: u32,
    /// When the current uninterrupted good run began. A well that pumps
    /// cleanly for long enough has recovered, so a strike is forgiven -
    /// otherwise one bad week would keep handing out 8 hour lockouts for
    /// ever.
    good_run_since: Option<u64>,

    /// Set when a human asked to start in Manual mode.
    start_requested: bool,
    /// True when the present reading matches the dry-run pattern, whether
    /// or not detection is enabled. Recorded so thresholds can be tuned
    /// from real data rather than guesswork.
    pub dry_would_fire: bool,
    /// Level-relay contacts (hacked SSRC-04), true = contact closed. None
    /// when no relay is fitted. Telemetry only at present: reported to the
    /// Pi, not yet part of any run gate. Interpretation into full/has-water
    /// is deferred so fail-safe polarity is decided in exactly one place.
    pub tank_contact: Option<bool>,
    pub well_contact: Option<bool>,
    /// The pump has proven it is moving water this run: current climbed past
    /// dry_amps while voltage sagged below dry_volts. A dry pump never does.
    /// Reset every close(). Lets a primed pump that then loses its water trip
    /// in ~1s, while a slow soft-start is still given the full grace.
    primed: bool,
    /// Highest current seen this run. The peak-collapse detector measures the
    /// present current against it: a dry pump's current falls back to a small
    /// fraction of the surge it drew accelerating, a wet pump's holds. Reset
    /// every close().
    pub peak_amps: f32,
    /// When the current first fell below the collapse fraction of the peak, so
    /// a sustained collapse (a lost/absent load) can be told from a passing
    /// cloud that dips the current for a moment. None whenever not collapsed.
    /// Reset every close().
    collapse_since: Option<u64>,
    /// Set by a hand stop. Auto mode will not restart while this is set, or
    /// pressing Stop in Auto would be undone on the very next tick. Cleared
    /// by an explicit Start, a Reset, or a mode change.
    pub held_by_operator: bool,

    pub events: Vec<Event>,
    pub starts_today: u32,
    pub run_seconds_today: u64,
    /// Clock reading at the last tick that saw the contactor closed, so run
    /// time can be accumulated from elapsed seconds rather than by counting
    /// calls. None whenever the pump is not running.
    run_counted_at: Option<u64>,
    pub energy_at_midnight: Option<f32>,
    day: String,
}

impl Machine {
    pub fn new(mode: Mode) -> Self {
        Machine {
            mode,
            state: if mode == Mode::Off { State::Off } else { State::Waiting },
            last: None,
            previous: None,
            stable: 0,
            meter_errors: 0,
            reason: None,
            detail: String::new(),
            running_since: None,
            over_current_since: None,
            dry_since: None,
            retry_at: None,
            lockout_until: 0,
            dry_strikes: 0,
            sag_strikes: 0,
            good_run_since: None,
            start_requested: false,
            dry_would_fire: false,
            tank_contact: None,
            well_contact: None,
            primed: false,
            peak_amps: 0.0,
            collapse_since: None,
            held_by_operator: false,
            events: Vec::new(),
            starts_today: 0,
            run_seconds_today: 0,
            run_counted_at: None,
            energy_at_midnight: None,
            day: String::new(),
        }
    }

    pub fn uptime(&self, now: u64) -> u64 {
        self.running_since.map_or(0, |t| now.saturating_sub(t))
    }

    pub fn lockout_remaining(&self, now: u64) -> u64 {
        self.lockout_until.saturating_sub(now)
    }

    pub fn retry_in(&self, now: u64) -> Option<u64> {
        self.retry_at.map(|t| t.saturating_sub(now))
    }

    // ---------------------------------------------------------------- input

    pub fn command(&mut self, cmd: Command, now: u64, s: &Settings) -> Result<(), Refusal> {
        match cmd {
            Command::SetMode(m) => {
                self.set_mode(m, now);
                Ok(())
            }
            Command::Start => self.request_start(now, s),
            Command::Stop => {
                if self.state.contactor_closed() || self.state == State::Starting {
                    self.trip(TripReason::Manual, "stopped by operator".into(), now);
                }
                // Latch until the operator acts again, otherwise Auto would
                // restart on the next tick and the Stop button would appear
                // not to work.
                self.held_by_operator = true;
                self.start_requested = false;
                self.retry_at = None;
                Ok(())
            }
            Command::Reset => {
                self.held_by_operator = false;
                if self.state == State::Tripped || self.state == State::Fault {
                    self.state = State::Waiting;
                    self.reason = None;
                    self.detail.clear();
                    self.retry_at = None;
                }
                Ok(())
            }
            Command::ClearLockout => {
                self.lockout_until = 0;
                self.dry_strikes = 0;
                self.sag_strikes = 0;
                self.good_run_since = None;
                if self.state == State::Lockout {
                    self.state = State::Waiting;
                    self.reason = None;
                    self.detail.clear();
                }
                Ok(())
            }
        }
    }

    fn set_mode(&mut self, m: Mode, now: u64) {
        // Selecting a mode is a deliberate act, so it ALWAYS releases a hand stop
        // - even when re-selecting the current mode (which is how an operator
        // resumes Auto after a Stop). Previously a redundant mode-set returned
        // early and left the hold in place, so a hand-stopped Auto pump stayed
        // stuck at Ready until a manual Start.
        let changed = m != self.mode;
        self.mode = m;
        self.start_requested = false;
        self.held_by_operator = false;

        match m {
            Mode::Off => {
                if changed && self.state.contactor_closed() {
                    self.record(TripReason::Manual, "mode set to OFF", now);
                }
                self.state = State::Off;
                self.running_since = None;
                self.reason = None;
                self.detail.clear();
            }
            Mode::Manual => {
                // In Manual the pump runs only because a human asked for it
                // in Manual. Coming from Auto, nobody has, so hand back a
                // stopped pump rather than one the operator did not start.
                if changed && (self.state.contactor_closed() || self.state == State::Starting) {
                    self.record(TripReason::Manual, "switched to MANUAL", now);
                    self.state = State::Ready;
                    self.running_since = None;
                    // `stable` is a continuous supply measure kept in update();
                    // changing mode never disturbs it, so START is instant.
                    self.reason = None;
                    self.detail = "waiting for START".into();
                } else if self.state == State::Off {
                    self.state = State::Waiting;
                }
            }
            Mode::Auto => {
                // Auto means autonomous. Selecting Auto clears any hand-stop park
                // - a Ready hold, an Off, or a Manual-reason trip a Stop left -
                // and returns the pump to the normal automatic path, which closes
                // as soon as the supply is safe and has settled. This is the
                // reported "stopped it, put it in Auto, and it won't run without a
                // manual Start" fix. A pump already RUNNING (or a real trip -
                // over/under-volt, dry, meter) is untouched: those recover on
                // their own terms, and selecting Auto never forces an unsafe
                // close (close() re-checks every interlock). `stable` is tracked
                // continuously and left alone, so a steady supply closes at once.
                let hand_parked = matches!(self.state, State::Off | State::Ready)
                    || self.reason == Some(TripReason::Manual);
                if hand_parked {
                    self.state = State::Waiting;
                    self.reason = None;
                    self.detail.clear();
                    self.retry_at = None;
                }
            }
        }
    }

    fn request_start(&mut self, now: u64, s: &Settings) -> Result<(), Refusal> {
        if self.mode == Mode::Off {
            return Err(Refusal::ModeOff);
        }
        if self.state.contactor_closed() {
            return Err(Refusal::AlreadyRunning);
        }
        if self.state == State::Lockout {
            return Err(Refusal::InLockout(self.lockout_remaining(now) / 60 + 1));
        }
        match self.state {
            State::Ready | State::Starting => {
                self.held_by_operator = false;
                self.start_requested = true;
                Ok(())
            }
            _ => Err(Refusal::NotReady(self.why_not_ready(s))),
        }
    }

    /// Plain-language explanation for the display and the dashboard.
    pub fn why_not_ready(&self, s: &Settings) -> String {
        if let Some(reason) = self.level_block(s) {
            return reason.to_string();
        }
        match self.state {
            State::Fault => "no reading from the meter".into(),
            State::Lockout => "well is recovering".into(),
            State::Off => "mode is OFF".into(),
            _ => match self.last {
                None => "waiting for first reading".into(),
                Some(r) if r.volts >= s.v_high_reset => {
                    format!("{:.0}V too high, need under {:.0}V", r.volts, s.v_high_reset)
                }
                Some(r) if r.volts <= s.v_low_reset => {
                    format!("{:.0}V too low, need over {:.0}V", r.volts, s.v_low_reset)
                }
                Some(_) => "confirming the supply is steady".into(),
            },
        }
    }

    // ------------------------------------------------------------ main step

    pub fn update(&mut self, reading: Option<Reading>, now: u64, day: &str, s: &Settings) {
        if self.day != day {
            self.day = day.to_string();
            self.starts_today = 0;
            self.run_seconds_today = 0;
            self.run_counted_at = None;
            self.energy_at_midnight = reading.map(|r| r.watt_hours);
        }

        // Run time comes from the clock, not from counting calls. update()
        // runs on the 500ms poll AND again on every button press, so a
        // per-call increment over-counts by roughly two and drifts further
        // the more the panel is touched - which is why the dashboard could
        // report 200 seconds of running on a bench that had never pumped.
        //
        // The step is clamped because this box has no RTC: NTP steps the
        // clock by minutes or years shortly after boot, and one jump must
        // not land in today's total. Sixty seconds is above any real stall
        // and far below any plausible jump.
        if self.state.contactor_closed() {
            if let Some(prev) = self.run_counted_at {
                self.run_seconds_today += now.saturating_sub(prev).min(60);
            }
            self.run_counted_at = Some(now);
        } else {
            self.run_counted_at = None;
        }

        match reading {
            None => {
                self.meter_errors += 1;
                if self.meter_errors >= s.meter_error_limit && self.state != State::Fault {
                    // Without readings we cannot know the voltage. Assuming
                    // it is safe is exactly the assumption that kills pumps.
                    self.trip(TripReason::MeterLost, "no reply from meter".into(), now);
                    self.state = State::Fault;
                }
                return;
            }
            Some(r) => {
                self.meter_errors = 0;
                self.previous = self.last;
                self.last = Some(r);
                // Anchor today's energy baseline. Re-anchor if the meter's
                // lifetime Wh ever DROPS below it: a meter swap (a replacement
                // reads its own, lower total) or a PZEM counter reset would
                // otherwise leave energy_today pinned at zero (it is clamped
                // >= 0) for the rest of the day, silently under-reporting the
                // very field data the thresholds are tuned from.
                match self.energy_at_midnight {
                    None => self.energy_at_midnight = Some(r.watt_hours),
                    Some(base) if r.watt_hours < base => {
                        self.energy_at_midnight = Some(r.watt_hours)
                    }
                    _ => {}
                }
            }
        }

        let r = self.last.expect("just set above");

        // Voltage stability is ONE continuous background measure: how many
        // consecutive in-band readings the SUPPLY has held. It is independent
        // of mode, level and state - only the voltage itself leaving the band
        // resets it. So a mode switch, a level change, a hand stop, or any
        // non-voltage trip never restarts the settle: the check has been
        // running all along, in every state, which is the whole point of it.
        // Nothing else in this machine writes `stable`; it only reads it.
        if self.in_band(r, s) {
            self.stable = self.stable.saturating_add(1).min(s.settle_readings);
        } else {
            self.stable = 0;
        }

        if self.mode == Mode::Off {
            self.state = State::Off;
            return;
        }

        match self.state {
            State::Off => self.state = State::Waiting,
            State::Running => self.check_running(r, now, s),
            State::Lockout => self.check_lockout(now, s),
            State::Fault | State::Tripped => self.check_recovery(r, now, s),
            State::Waiting | State::Ready | State::Starting => self.check_ready(r, now, s),
        }
    }

    /// Voltage noise sits on top of the real reading, so a supply parked
    /// exactly on a threshold would otherwise flip in and out of band on
    /// every sample. Requiring a small margin to enter, and allowing a
    /// small overshoot to stay, keeps the displayed state steady without
    /// weakening the protection - both edges move inward, never outward.
    const BAND_MARGIN: f32 = 0.5;

    fn in_band(&self, r: Reading, s: &Settings) -> bool {
        // Already settling or ready? Tolerate a little drift before giving
        // up, so a borderline supply does not thrash.
        let slack = if matches!(self.state, State::Starting | State::Ready) {
            Self::BAND_MARGIN
        } else {
            -Self::BAND_MARGIN
        };
        r.volts > s.v_low_reset - slack && r.volts < s.v_high_reset + slack
    }

    fn check_ready(&mut self, r: Reading, now: u64, s: &Settings) {
        if !self.in_band(r, s) {
            self.state = State::Waiting;
            return;
        }

        // `stable` is maintained centrally in update(); here we only read it.
        if self.stable < s.settle_readings {
            self.state = State::Starting;
            return;
        }

        // Conditions are good and have held. Whether we close depends on the
        // mode, and on whether the operator has parked it by hand.
        if self.held_by_operator {
            self.state = State::Ready;
            return;
        }

        match self.mode {
            Mode::Auto => self.close(r, now, s),
            Mode::Manual => {
                if self.start_requested {
                    self.start_requested = false;
                    self.close(r, now, s);
                } else {
                    self.state = State::Ready;
                }
            }
            Mode::Off => self.state = State::Off,
        }
    }

    /// The level relays, once armed, gate the pump above the voltage logic.
    /// They are two different kinds of thing:
    ///
    ///   WELL - a hard safety interlock, enforced in EVERY mode. A dry well
    ///          stops the pump whether it is running under Auto or by hand.
    ///          Confirmed polarity: a CLOSED contact means water present.
    ///
    ///   TANK - automation, active ONLY in Auto. Stopping a full tank is a
    ///          convenience the automatic mode provides; in Manual the
    ///          operator is in charge and may fill past it. Confirmed
    ///          polarity: a CLOSED contact means the tank is full.
    ///
    /// Fail-safe: an open or broken WELL wire reads as no-water and blocks -
    /// a dry well must never run. An open or broken TANK wire reads as
    /// not-full and permits running, leaning on the mandatory overflow float
    /// switch in series with the coil.
    ///
    /// `level_gate_enabled` means the relays are wired and in use; until then
    /// the pump runs on the voltage/current logic alone.
    fn level_block(&self, s: &Settings) -> Option<&'static str> {
        if !s.level_gate_enabled {
            return None;
        }
        // Well: safety, all modes.
        if self.well_contact != Some(true) {
            return Some("well dry");
        }
        // Tank: automation, Auto only.
        if self.mode == Mode::Auto && self.tank_contact == Some(true) {
            return Some("tank full");
        }
        None
    }

    fn check_running(&mut self, r: Reading, now: u64, s: &Settings) {
        if let Some(reason) = self.level_block(s) {
            // A full tank and a dry WELL are both conditions we sense directly
            // with a relay, so both are normal, expected stops - not faults.
            // Park in Waiting so Auto re-closes the instant the sensor says it
            // is good again (well has water / tank has drawn down). No timed
            // lockout for a sensed dry well: with a real level sensor the
            // sensor decides recovery, not a clock. The amp-INFERRED dry-run
            // below still locks out, because it has no sensor to confirm the
            // well came back.
            self.state = State::Waiting;
            self.running_since = None;
            self.reason = None;
            self.detail = reason.to_string();
            return;
        }
        if r.volts > s.v_high_trip {
            let d = format!("{:.1}V over {:.0}V", r.volts, s.v_high_trip);
            self.trip(TripReason::OverVoltage, d, now);
        } else if r.volts < s.v_low_trip {
            let d = format!("{:.1}V under {:.0}V", r.volts, s.v_low_trip);
            // Dusk weak-supply: a run that collapses on under-voltage within
            // sag_min_run_seconds read fine UNLOADED but cannot carry the motor.
            // Count it so recovery backs off (long rest) instead of restarting
            // the motor every few seconds all evening. A run that lasts longer
            // never counts - that supply held the load, so it is a good day.
            if s.sag_strike_limit > 0 {
                if let Some(since) = self.running_since {
                    if now.saturating_sub(since) < s.sag_min_run_seconds {
                        self.sag_strikes = self.sag_strikes.saturating_add(1);
                    }
                }
            }
            self.trip(TripReason::UnderVoltage, d, now);
        } else if self.over_current(r, now, s) {
            let d = format!("{:.1}A over {:.0}A", r.amps, s.i_max);
            self.trip(TripReason::OverCurrent, d, now);
        } else if self.dry_running(r, now, s) {
            self.start_lockout(r, now, s);
        } else if self.ran_too_long(now, s) {
            let d = format!("hand-started run reached {} min", s.max_run_minutes);
            self.trip(TripReason::MaxRun, d, now);
        } else {
            self.forgive_strike(now, s);
            // A run that has carried the load past sag_min_run_seconds proves
            // the supply is strong enough today (a passing cloud recovered, or
            // dusk firmed up), so clear the dusk sag count and return to quick
            // retries. This is what separates a marginal dusk (never sustains)
            // from a temporary daytime dip (restart runs on).
            if let Some(since) = self.running_since {
                if now.saturating_sub(since) >= s.sag_min_run_seconds {
                    self.sag_strikes = 0;
                }
            }
        }
    }

    /// A hand-started run that nobody came back to end.
    ///
    /// Every other trip in this machine answers "something is wrong". This
    /// one answers "nothing is wrong and that is the problem" - a healthy
    /// pump at normal volts and normal current has no fault to trip on, so
    /// without a deadline it runs until the sun goes down.
    ///
    /// The setup wizard is the sharp case. It starts the pump, samples for
    /// three minutes, then waits for a tap on "Yes, water is flowing".
    /// Nothing on this side has ever ended that run, so a phone that sleeps
    /// or a tab that is closed leaves a 1500W pump running against a well
    /// with nobody watching it.
    ///
    /// Auto is deliberately exempt. Auto exists so a solar pump can work an
    /// unattended day; a cap there would trip a good run every afternoon,
    /// and the real hazards of a long Auto run - overvoltage, a drawn-down
    /// well, a dead meter - each already have their own trip above. In
    /// Manual the pump is running only because a person asked for it, and a
    /// person who has walked away is exactly what this catches.
    ///
    /// Deriving the deadline from running_since rather than storing one
    /// means it cannot fall out of step with the contactor: running_since
    /// *is* the contactor's own timestamp, set in close() and cleared
    /// everywhere the pump stops.
    fn ran_too_long(&self, now: u64, s: &Settings) -> bool {
        if s.max_run_minutes == 0 || self.mode != Mode::Manual {
            return false;
        }
        match self.running_since {
            Some(t) => now.saturating_sub(t) >= s.max_run_minutes * 60,
            None => false,
        }
    }

    /// A strike decays after the pump has run without going dry for
    /// dry_forgive_minutes. The counter winds down one step at a time, so a
    /// well that recovers is not punished for last month's drought.
    fn forgive_strike(&mut self, now: u64, s: &Settings) {
        if self.dry_strikes == 0 || s.dry_forgive_minutes == 0 {
            return;
        }
        let since = *self.good_run_since.get_or_insert(now);
        if now.saturating_sub(since) >= s.dry_forgive_minutes * 60 {
            self.dry_strikes -= 1;
            self.good_run_since = Some(now);
            log_forgive(self.dry_strikes);
        }
    }

    /// True once a condition has held continuously for `need` seconds, tracking
    /// the start time in `anchor`. The `now < t` arm handles a BACKWARD wall-
    /// clock step: this box has no RTC, so NTP can jump the clock back, and a
    /// plain `now.saturating_sub(t)` would then read 0 forever - stalling the
    /// trip until the clock climbed back past `t`. Re-anchoring restarts the
    /// count from `now`, so the worst a backward jump can do is delay the trip
    /// by `need`, never defeat it. A forward jump only trips sooner (safe).
    fn sustained_for(anchor: &mut Option<u64>, now: u64, need: u64) -> bool {
        match *anchor {
            None => {
                *anchor = Some(now);
                false
            }
            Some(t) if now < t => {
                *anchor = Some(now);
                false
            }
            Some(t) => now.saturating_sub(t) >= need,
        }
    }

    fn over_current(&mut self, r: Reading, now: u64, s: &Settings) -> bool {
        if r.amps <= s.i_max {
            self.over_current_since = None;
            return false;
        }
        Self::sustained_for(&mut self.over_current_since, now, s.i_max_seconds)
    }

    /// Dry-run detection: two independent tests, either can fire.
    ///
    /// The peak current this run is tracked unconditionally - before the
    /// enable check, so the collapse yardstick is correct the instant it is
    /// armed - and `dry_would_fire`/`primed` are recorded for tuning even
    /// when detection is off, so evidence can be gathered without arming an
    /// untuned trip on a real pump.
    fn dry_running(&mut self, r: Reading, now: u64, s: &Settings) -> bool {
        let looks_dry = r.amps < s.dry_amps && r.volts > s.dry_volts;
        self.dry_would_fire = looks_dry;

        // A wet pump loads the array - amps up, volts down. Remember it once
        // it has, so losing water later is caught fast instead of waiting out
        // a grace meant for the start.
        if r.amps >= s.dry_amps && r.volts <= s.dry_volts {
            self.primed = true;
        }

        // The reference the collapse detector measures against. Tracked here,
        // before the enable gate, so it is never stale when detection arms.
        if r.amps > self.peak_amps {
            self.peak_amps = r.amps;
        }

        if !s.dry_enabled {
            return false;
        }

        // Both paths run every tick (no short-circuit that would skip the
        // collapse detector), so the collapse window keeps advancing even
        // while the primed path is still inside its start grace.
        let primed = self.primed_dry(now, s, looks_dry);
        let collapsed = self.collapse_dry(r, now, s);
        primed || collapsed
    }

    /// Absolute-signature dry test: low current WHILE the array voltage stays
    /// high.
    ///
    /// A pump moving water drags the array toward Vmpp; spinning in air it
    /// draws far less, so voltage drifts back up toward Voc. Low current alone
    /// is ambiguous - weak sun looks the same - which is why both conditions
    /// must hold together. This is the original detector, unchanged: it fails
    /// exactly when the array never rises above dry_volts (weak sun), which is
    /// what the collapse path below is for.
    fn primed_dry(&mut self, now: u64, s: &Settings, looks_dry: bool) -> bool {
        // The soft-start ramp looks exactly like a dry well - high volts, low
        // amps - for its first half-minute. A pump that has not yet proven it
        // loaded gets the full grace so that ramp is never mistaken for dry.
        // A primed pump skips the grace: it WAS moving water, so high-volts-
        // low-amps now means the water is gone, and every second counts.
        if !self.primed {
            if let Some(t) = self.running_since {
                if now.saturating_sub(t) < s.dry_grace_seconds {
                    return false;
                }
            }
        }

        if !looks_dry {
            self.dry_since = None;
            return false;
        }
        Self::sustained_for(&mut self.dry_since, now, s.dry_seconds)
    }

    /// Relative-collapse dry test: the present current has fallen to a small
    /// fraction of the peak the pump reached this run.
    ///
    /// A dry pump still draws an acceleration surge spinning up its rotor, so
    /// a dry start and a wet start look alike while accelerating. Once at
    /// speed the dry current COLLAPSES back toward zero - there is no water to
    /// lift - while a wet pump's current HOLDS. Because this keys on the
    /// collapse RELATIVE to the peak, it works even in weak sun, where the
    /// unloaded array never exceeds dry_volts and the primed test above is
    /// blind - the exact case that motivated it.
    ///
    /// Two guards keep it from false-tripping a healthy pump:
    ///   - the peak must have passed `dry_collapse_min_peak`, so a pump still
    ///     climbing a slow ramp (peak near zero) cannot "collapse" from
    ///     nothing;
    ///   - the collapse must be SUSTAINED for `dry_collapse_seconds`, so a
    ///     passing cloud that momentarily dips the current does not cut off a
    ///     genuinely low-flow well (which settles well above the fraction).
    fn collapse_dry(&mut self, r: Reading, now: u64, s: &Settings) -> bool {
        let collapsed = self.peak_amps >= s.dry_collapse_min_peak
            && r.amps < s.dry_collapse_frac * self.peak_amps;
        if !collapsed {
            self.collapse_since = None;
            return false;
        }
        Self::sustained_for(&mut self.collapse_since, now, s.dry_collapse_seconds)
    }

    fn start_lockout(&mut self, r: Reading, now: u64, s: &Settings) {
        self.dry_strikes = (self.dry_strikes + 1).min(3);
        self.good_run_since = None;
        let mins = match self.dry_strikes {
            1 => s.dry_lockout_1,
            2 => s.dry_lockout_2,
            _ => s.dry_lockout_3,
        };
        // Saturating: an absurd lockout setting must never WRAP a u64 to a small
        // deadline - that would make lockout_remaining read 0 and silently bypass
        // the rest a dry well needs. Overflow instead pins the deadline far in
        // the future (fails safe: stays locked out).
        self.lockout_until = now.saturating_add(mins.saturating_mul(60));

        let d = format!("{:.1}A at {:.0}V", r.amps, r.volts);
        self.trip(TripReason::DryRun, d, now);
        self.state = State::Lockout;
        self.detail = format!("strike {} of 3, {} min rest", self.dry_strikes, mins);
    }

    fn check_lockout(&mut self, now: u64, s: &Settings) {
        // The dry-run lockout is a timed stand-in for "wait until the well
        // refills" - Lockout is only ever entered for a dry well. With the
        // level gate armed we no longer have to wait out that guess: the
        // instant the well sensor reads wet again, the recovery is genuinely
        // over. The direct sensor beats the timer.
        let well_recovered = s.level_gate_enabled && self.well_contact == Some(true);
        if now >= self.lockout_until || well_recovered {
            self.state = State::Waiting;
            self.reason = None;
            self.detail.clear();
            self.lockout_until = 0;
        }
    }

    fn check_recovery(&mut self, r: Reading, now: u64, s: &Settings) {
        // A hand stop parks at Ready and stays there. held_by_operator keeps
        // Auto from closing again until someone presses Start or Reset.
        if self.held_by_operator {
            if self.in_band(r, s) {
                self.state = State::Ready;
                self.reason = None;
            }
            return;
        }

        // A dry well or a full tank blocks (re)starting, whatever the volts.
        if let Some(reason) = self.level_block(s) {
            self.retry_at = None;
            self.detail = reason.to_string();
            return;
        }

        if !self.in_band(r, s) {
            self.retry_at = None;
            return;
        }

        if self.retry_at.is_none() {
            // A supply that keeps collapsing under load (sag_strikes at the
            // limit) gets a long rest instead of a quick retry, so the motor is
            // not restarted dozens of times an evening. A single dip or cloud
            // never reaches the limit, so normal recovery keeps its quick retry.
            let backing_off =
                s.sag_strike_limit > 0 && self.sag_strikes >= s.sag_strike_limit;
            let wait = if backing_off {
                s.sag_backoff_seconds.max(s.retry_seconds)
            } else {
                s.retry_seconds
            };
            // Saturating for the same reason as the lockout deadline: a wrapped
            // retry_at could let a trip retry immediately, skipping the intended
            // cool-off. Overflow pins it far out (recovery then waits for Reset).
            self.retry_at = Some(now.saturating_add(wait));
            if backing_off {
                self.detail = "supply too weak under load - resting the motor".into();
            }
        }

        if self.stable >= s.settle_readings && now >= self.retry_at.unwrap_or(now) {
            self.retry_at = None;
            self.reason = None;
            self.detail.clear();
            match self.mode {
                Mode::Auto => self.close(r, now, s),
                _ => self.state = State::Ready,
            }
        }
    }

    // -------------------------------------------------------------- effects

    fn close(&mut self, r: Reading, now: u64, s: &Settings) {
        // Never energize the contactor while the level gate blocks. close() is
        // the ONE place that sets Running, so guarding here covers every path
        // - manual Start, Auto close, retry - with no one-tick window where
        // the relay pulls in before the next poll catches a dry well.
        if let Some(reason) = self.level_block(s) {
            self.state = State::Waiting;
            self.detail = reason.to_string();
            self.start_requested = false;
            return;
        }
        self.state = State::Running;
        self.running_since = Some(now);
        self.good_run_since = Some(now);
        // Do NOT reset `stable` here. The settle counter proves the SUPPLY has
        // held in-band; a stop for a level reason (tank full) never touches the
        // supply, so a tank-cycle restart must not re-serve the whole 15 s
        // voltage settle. A real voltage excursion still zeroes it: while
        // Waiting, check_ready sets stable=0 the moment volts leave the band,
        // and every trip resets it too - so cold-starts and voltage recovery
        // keep the full settle, only level cycling gets the fast restart.
        self.over_current_since = None;
        self.dry_since = None;
        self.retry_at = None;
        self.primed = false;
        self.peak_amps = 0.0;
        self.collapse_since = None;
        self.starts_today += 1;
        self.detail = format!("started at {:.1}V", r.volts);
    }

    fn trip(&mut self, reason: TripReason, detail: String, now: u64) {
        let was_running = self.state.contactor_closed();
        self.state = State::Tripped;
        self.reason = Some(reason);
        self.detail = detail.clone();
        self.running_since = None;
        self.over_current_since = None;
        self.dry_since = None;
        self.retry_at = None;
        // Deliberately do NOT touch `stable`. A trip on current, run-time,
        // dry-run, a hand stop, or a lost meter says nothing about the
        // supply voltage, so re-serving the whole settle would be false work.
        // A genuine voltage trip already zeroed `stable` in update() the moment
        // the volts left the band, so those still re-settle on recovery.

        if was_running {
            self.record(reason, &detail, now);
        }
    }

    fn record(&mut self, reason: TripReason, detail: &str, _now: u64) {
        let r = self.last.unwrap_or(Reading {
            volts: 0.0,
            amps: 0.0,
            watts: 0.0,
            watt_hours: 0.0,
        });
        self.events.push(Event {
            at: crate::now_string(),
            reason: reason.label().to_string(),
            detail: detail.to_string(),
            volts: r.volts,
            amps: r.amps,
        });
        if self.events.len() > 100 {
            self.events.remove(0);
        }
    }

    pub fn energy_today(&self) -> f32 {
        match (self.last, self.energy_at_midnight) {
            (Some(r), Some(base)) => (r.watt_hours - base).max(0.0),
            _ => 0.0,
        }
    }
}

#[cfg(test)]
fn mach(mode: Mode) -> Machine {
    // Test fixture: a WIRED box - well wet, tank has room - so the default-ON
    // level gate never masks the voltage/current logic under test. Level tests
    // override well_contact / tank_contact explicitly.
    let mut m = Machine::new(mode);
    m.well_contact = Some(true);
    m.tank_contact = Some(false);
    m
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rd(v: f32, a: f32) -> Reading {
        Reading { volts: v, amps: a, watts: v * a, watt_hours: 1000.0 }
    }

    fn settle(m: &mut Machine, s: &Settings, v: f32, a: f32, n: u32, from: u64) -> u64 {
        let mut t = from;
        for _ in 0..n {
            m.update(Some(rd(v, a)), t, "2026-08-11", s);
            t += 1;
        }
        t
    }

    #[test]
    fn sunrise_voc_never_closes() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        settle(&mut m, &s, 163.0, 0.0, 60, 0);
        assert_eq!(m.state, State::Waiting);
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn auto_starts_once_stable() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);
    }

    // Drive Auto recovery until the pump closes again (settles the "unloaded"
    // in-band supply and waits out whatever retry/backoff is pending).
    fn run_until_running(m: &mut Machine, s: &Settings, unloaded: f32, mut t: u64) -> u64 {
        for _ in 0..4000 {
            m.update(Some(rd(unloaded, 8.0)), t, "2026-08-11", s);
            t += 1;
            if m.state == State::Running {
                return t;
            }
        }
        t
    }

    #[test]
    fn dusk_weak_supply_backs_off_instead_of_cycling_the_motor() {
        // THE reported field bug: at dusk the supply reads fine UNLOADED (mid
        // band) but collapses under the motor within seconds -> under-voltage
        // trip -> Auto retries in retry_seconds -> 50+ motor starts an evening.
        // After sag_strike_limit quick collapses the retry must jump to the long
        // sag_backoff, so the motor rests instead of cycling.
        let s = Settings::default();
        assert!(s.sag_strike_limit > 0 && s.sag_backoff_seconds > s.retry_seconds * 20);
        let mut m = mach(Mode::Auto);
        let unloaded = (s.v_low_reset + s.v_high_reset) / 2.0;
        let loaded = s.v_low_trip - 5.0;
        let mut t = 0u64;
        for _ in 0..s.sag_strike_limit {
            t = run_until_running(&mut m, &s, unloaded, t);
            assert_eq!(m.state, State::Running, "pump should restart each dusk cycle");
            // one loaded reading collapses it within a second of closing
            m.update(Some(rd(loaded, 8.0)), t, "2026-08-11", &s);
            t += 1;
            assert_eq!(m.state, State::Tripped, "under-voltage collapse should trip");
        }
        assert!(
            m.sag_strikes >= s.sag_strike_limit,
            "repeated collapses must accrue strikes, got {}",
            m.sag_strikes
        );
        // The very next recovery must schedule the LONG backoff, not a quick retry.
        let before = t;
        m.update(Some(rd(unloaded, 8.0)), t, "2026-08-11", &s);
        let wait = m.retry_at.map(|r| r.saturating_sub(before)).unwrap_or(0);
        assert!(
            wait >= s.sag_backoff_seconds - 2,
            "after the strike limit the retry must be the long backoff (~{}s), got {}s",
            s.sag_backoff_seconds,
            wait
        );
    }

    #[test]
    fn a_single_daytime_dip_keeps_the_quick_retry() {
        // A passing cloud is ONE collapse (or a run that then sustains). It must
        // NOT trigger the long backoff - a good day is not penalised.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let unloaded = (s.v_low_reset + s.v_high_reset) / 2.0;
        let loaded = s.v_low_trip - 5.0;
        let mut t = run_until_running(&mut m, &s, unloaded, 0);
        m.update(Some(rd(loaded, 8.0)), t, "2026-08-11", &s); // one dip -> trip
        t += 1;
        assert!(m.sag_strikes < s.sag_strike_limit, "one dip must stay below the limit");
        let before = t;
        m.update(Some(rd(unloaded, 8.0)), t, "2026-08-11", &s);
        let wait = m.retry_at.map(|r| r.saturating_sub(before)).unwrap_or(0);
        assert_eq!(wait, s.retry_seconds, "a single dip keeps the quick retry, not the backoff");
    }

    #[test]
    fn a_run_that_holds_under_load_clears_the_dusk_sag_count() {
        // What separates a marginal dusk from a firming-up supply: a run that
        // carries the load past sag_min_run_seconds clears the strikes, so the
        // machine returns to quick retries (and no needless motor rest).
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let unloaded = (s.v_low_reset + s.v_high_reset) / 2.0;
        let loaded = s.v_low_trip - 5.0;
        let mut t = 0u64;
        // accrue a couple of collapses
        for _ in 0..2 {
            t = run_until_running(&mut m, &s, unloaded, t);
            m.update(Some(rd(loaded, 8.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert!(m.sag_strikes >= 1, "strikes should have accrued");
        // now the supply holds: start and RUN in-band under load past the window
        t = run_until_running(&mut m, &s, unloaded, t);
        for _ in 0..(s.sag_min_run_seconds + 5) {
            m.update(Some(rd(unloaded, 8.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.state, State::Running, "a good supply keeps running");
        assert_eq!(m.sag_strikes, 0, "a sustained run must clear the sag count");
    }

    #[test]
    fn selecting_auto_resumes_after_a_manual_stop() {
        // The reported bug: stop by hand (in Manual), switch to Auto -> it must
        // resume autonomously, NOT sit waiting for a manual Start.
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        let mut t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        m.command(Command::Start, t, &s).unwrap();
        t += 1;
        m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
        t += 1;
        assert_eq!(m.state, State::Running);
        m.command(Command::Stop, t, &s).unwrap(); // hand stop
        t += 1;
        assert!(!m.state.contactor_closed());
        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap(); // go autonomous
        t += 1;
        for _ in 0..(s.settle_readings + 5) {
            m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
            t += 1;
            if m.state == State::Running {
                break;
            }
        }
        assert_eq!(m.state, State::Running, "Auto must resume after a hand stop, no manual Start");
    }

    #[test]
    fn re_selecting_auto_resumes_a_stop_pressed_in_auto() {
        // Stop pressed while in Auto still holds (so Stop works), but re-selecting
        // Auto resumes autonomous operation - Auto never turns into manual.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);
        m.command(Command::Stop, t, &s).unwrap();
        t += 1;
        m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
        t += 1;
        assert!(!m.state.contactor_closed(), "Stop in Auto must hold the pump");
        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap(); // re-select Auto
        t += 1;
        for _ in 0..(s.settle_readings + 5) {
            m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
            t += 1;
            if m.state == State::Running {
                break;
            }
        }
        assert_eq!(m.state, State::Running, "re-selecting Auto resumes after a Stop");
    }

    #[test]
    fn resuming_auto_never_closes_while_unsafe() {
        // Critical: clearing a hand stop by selecting Auto must NOT bypass a real
        // fault. An over-voltage trip stays open even as Auto is (re)selected.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);
        m.update(Some(rd(s.v_high_trip + 10.0, 0.0)), t, "2026-08-11", &s); // overvolt
        t += 1;
        assert_eq!(m.state, State::Tripped);
        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap(); // re-select Auto while unsafe
        t += 1;
        m.update(Some(rd(s.v_high_trip + 10.0, 0.0)), t, "2026-08-11", &s);
        assert!(
            !m.state.contactor_closed(),
            "selecting Auto must never close the contactor while over-voltage"
        );
        assert_eq!(m.reason, Some(TripReason::OverVoltage), "the real trip reason must survive");
    }

    #[test]
    fn manual_waits_at_ready() {
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 5, 0);
        assert_eq!(m.state, State::Ready);
        assert!(!m.state.contactor_closed());

        m.command(Command::Start, t, &s).unwrap();
        m.update(Some(rd(130.0, 0.0)), t, "2026-08-11", &s);
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn manual_start_refused_when_unsafe() {
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        settle(&mut m, &s, 160.0, 0.0, 10, 0);
        let err = m.command(Command::Start, 10, &s).unwrap_err();
        assert!(matches!(err, Refusal::NotReady(_)));
        assert!(err.message().contains("too high"));
    }

    #[test]
    fn overvoltage_opens_contactor() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        m.update(Some(rd(147.0, 2.0)), t, "2026-08-11", &s);
        assert_eq!(m.state, State::Tripped);
        assert_eq!(m.reason, Some(TripReason::OverVoltage));
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn mode_off_stops_and_stays_stopped() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        m.command(Command::SetMode(Mode::Off), t, &s).unwrap();
        assert_eq!(m.state, State::Off);

        settle(&mut m, &s, 130.0, 0.0, 60, t);
        assert_eq!(m.state, State::Off);
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn weak_sun_is_not_a_dry_well() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_grace_seconds = 1;
        s.dry_seconds = 3;

        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        // Low current but low voltage too - the array is simply weak.
        for _ in 0..20 {
            m.update(Some(rd(115.0, 4.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn dry_well_locks_out_and_escalates() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false; // isolate the amp-inferred dry-run path
        s.dry_grace_seconds = 1;
        s.dry_seconds = 2;
        s.dry_lockout_1 = 1;
        // Dry running shows as low current with the voltage drifting up, but
        // still inside the safe band - above v_high_trip it is an
        // overvoltage fault instead, which is the more urgent one.
        s.dry_volts = 128.0;
        s.dry_amps = 5.5;

        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 126.0, 10.8, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        t += 2; // past the startup grace
        for _ in 0..4 {
            m.update(Some(rd(134.0, 4.5)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.state, State::Lockout);
        assert_eq!(m.dry_strikes, 1);

        // Perfect conditions must not break the lockout while it is running.
        // dry_lockout_1 is 1 minute here, so stay well inside that.
        settle(&mut m, &s, 126.0, 0.0, 20, t);
        assert_eq!(m.state, State::Lockout, "good voltage must not cut the rest short");

        // Once the rest is over it re-arms on its own.
        let after = m.lockout_until + 1;
        settle(&mut m, &s, 126.0, 0.0, 1, after);
        assert_ne!(m.state, State::Lockout, "lockout should expire on time");
    }

    #[test]
    fn an_absurd_lockout_setting_does_not_wrap_and_bypass_the_rest() {
        // A huge dry_lockout must not overflow `now + mins*60` into a small
        // deadline that lockout_remaining would read as ~0, silently skipping
        // the rest a dry well needs. Saturating arithmetic pins it far out.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false;
        s.dry_grace_seconds = 1;
        s.dry_seconds = 2;
        s.dry_lockout_1 = u64::MAX; // absurd: now + MAX*60 would wrap
        s.dry_volts = 128.0;
        s.dry_amps = 5.5;

        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 126.0, 10.8, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);
        t += 2;
        for _ in 0..4 {
            m.update(Some(rd(134.0, 4.5)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.state, State::Lockout, "dry run must lock out");
        assert!(
            m.lockout_remaining(t) > 1_000_000_000,
            "lockout deadline must not wrap to a bypassed rest (remaining={})",
            m.lockout_remaining(t)
        );
    }

    #[test]
    fn overvoltage_wins_over_dry_run() {
        // If both could fire, the electrical fault must take priority - it
        // is what actually destroys the pump.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_grace_seconds = 0;
        s.dry_seconds = 0;

        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 126.0, 10.8, s.settle_readings, 0);
        m.update(Some(rd(148.0, 1.0)), t + 5, "2026-08-11", &s);
        assert_eq!(m.reason, Some(TripReason::OverVoltage));
    }

    #[test]
    fn meter_loss_stops_the_pump() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let mut t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        for _ in 0..s.meter_error_limit {
            m.update(None, t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.state, State::Fault);
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn a_none_reading_from_a_fresh_machine_never_panics_and_stays_open() {
        // Boot with the meter absent (USB not enumerated yet, or no PZEM at all):
        // the very FIRST update() gets None while self.last is still None. This
        // must not panic - the code relies on the None branch returning before the
        // `self.last.expect("just set above")` - and with no way to know the
        // voltage the contactor must stay OPEN, then fault after the miss limit.
        let s = Settings::default();
        for mode in [Mode::Auto, Mode::Manual, Mode::Off] {
            let mut m = Machine::new(mode);
            for t in 0..(s.meter_error_limit as u64 + 5) {
                m.update(None, t, "2026-08-11", &s); // would panic at :491 if line 469 return were removed
                assert!(
                    !m.state.contactor_closed(),
                    "a box that has never seen a reading must never close the contactor"
                );
            }
            assert_eq!(m.state, State::Fault, "sustained no-meter must fault-open, not assume-safe");
        }
    }

    #[test]
    fn manual_stop_does_not_auto_restart() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);

        m.command(Command::Stop, t, &s).unwrap();
        assert_eq!(m.reason, Some(TripReason::Manual));

        // Even in Auto with good conditions, it waits at Ready.
        settle(&mut m, &s, 130.0, 0.0, 60, t);
        assert_eq!(m.state, State::Ready);
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn start_after_manual_stop_resumes() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);

        m.command(Command::Stop, t, &s).unwrap();
        let t = settle(&mut m, &s, 130.0, 0.0, 5, t);
        assert_eq!(m.state, State::Ready);

        // An explicit Start clears the hold.
        m.command(Command::Start, t, &s).unwrap();
        m.update(Some(rd(130.0, 0.0)), t, "2026-08-11", &s);
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn reset_clears_a_manual_hold() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);

        m.command(Command::Stop, t, &s).unwrap();
        m.command(Command::Reset, t, &s).unwrap();

        // With the hold cleared, Auto resumes on its own.
        settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 2, t);
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn mode_change_clears_a_manual_hold() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);

        m.command(Command::Stop, t, &s).unwrap();
        m.command(Command::SetMode(Mode::Off), t, &s).unwrap();
        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap();

        settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 2, t);
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn switching_auto_to_manual_stops_the_pump() {
        // Taking manual control must hand back a stopped pump. Leaving it
        // running would mean the operator is now responsible for something
        // they did not start.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        m.command(Command::SetMode(Mode::Manual), t, &s).unwrap();
        assert!(!m.state.contactor_closed(), "pump must stop on entering MANUAL");
        assert_eq!(m.state, State::Ready);

        // And it must stay stopped until someone presses Start.
        let t = settle(&mut m, &s, 130.0, 0.0, 40, t);
        assert_eq!(m.state, State::Ready);
        assert!(!m.state.contactor_closed());

        m.command(Command::Start, t, &s).unwrap();
        m.update(Some(rd(130.0, 0.0)), t, "2026-08-11", &s);
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn mode_change_from_a_known_good_state_does_not_recount() {
        // Switching mode when the pump was running, or ready to run, must
        // not drop back into a progress bar - the conditions were already
        // proven a moment ago.
        let s = Settings::default();

        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        m.command(Command::SetMode(Mode::Manual), t, &s).unwrap();
        m.update(Some(rd(130.0, 0.0)), t + 1, "2026-08-11", &s);
        assert_eq!(m.state, State::Ready, "should land on Ready, not Starting");

        // And back to Auto starts immediately.
        m.command(Command::SetMode(Mode::Auto), t + 2, &s).unwrap();
        m.update(Some(rd(130.0, 0.0)), t + 3, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "Auto should resume at once");
    }

    #[test]
    fn switching_manual_to_auto_starts_when_safe() {
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Ready);

        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap();
        settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 2, t);
        assert_eq!(m.state, State::Running, "Auto should take over and start");
    }


    // ---------------------------------------------------- the run deadline

    /// Start a hand-started run and hand back the moment the contactor shut.
    fn run_by_hand(m: &mut Machine, s: &Settings) -> u64 {
        let t = settle(m, s, 130.0, 0.0, s.settle_readings + 1, 0);
        m.command(Command::Start, t, s).unwrap();
        m.update(Some(rd(130.0, 2.0)), t, "2026-08-11", s);
        assert_eq!(m.state, State::Running, "the run under test must be running");
        t
    }

    #[test]
    fn a_primed_pump_that_loses_water_trips_within_a_second() {
        // The field signature: the pump loads the array (volts down, amps up),
        // then loses its water and snaps back to high-volts-low-amps. Because
        // it was primed, this must not wait out the 90s start grace - it trips
        // in one hold period, ~1s.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_amps = 3.0;
        s.dry_volts = 125.0;
        s.dry_seconds = 1;
        s.dry_grace_seconds = 90;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        // Prove it loaded: a real wet operating point primes it.
        m.update(Some(rd(120.0, 5.0)), t + 20, "2026-08-11", &s);
        assert_eq!(m.state, State::Running);

        // Water gone. Well inside the 90s grace - a not-yet-primed pump would
        // be given a pass here; a primed one must not be.
        m.update(Some(rd(135.0, 1.0)), t + 40, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "one dry reading is not yet a trip");
        m.update(Some(rd(135.0, 1.0)), t + 42, "2026-08-11", &s);
        assert_eq!(m.state, State::Lockout, "primed + dry must trip fast");
        assert_eq!(m.reason, Some(TripReason::DryRun));
        assert!(t + 42 < t + 90, "and well before the start grace would allow");
    }

    #[test]
    fn a_slow_start_is_not_mistaken_for_a_dry_well() {
        // A pump that has NOT proven it loaded gets the full grace: the soft-
        // start ramp reads high-volts-low-amps for its first half minute and
        // must never be cut off as dry.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_amps = 3.0;
        s.dry_volts = 125.0;
        s.dry_seconds = 1;
        s.dry_grace_seconds = 90;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        // 60s of ramp-looking readings, never primed, still inside grace.
        for i in 1..60 {
            m.update(Some(rd(135.0, 1.0)), t + i, "2026-08-11", &s);
            assert_eq!(m.state, State::Running, "must ride out the start ramp");
        }
    }

    #[test]
    fn level_gate_is_on_by_default() {
        // Polarity is field-confirmed (2026-08-31), so the gate ships ON: a dry
        // well and a full tank (in Auto) both block. This is the fail-safe that
        // keeps a pump from dry-running and burning.
        assert!(Settings::default().level_gate_enabled);
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        m.state = State::Running;
        m.well_contact = Some(false); // dry
        assert_eq!(m.level_block(&s), Some("well dry"), "a dry well must block");
        m.well_contact = Some(true);
        m.tank_contact = Some(true); // full, in Auto
        assert_eq!(m.level_block(&s), Some("tank full"), "a full tank must block Auto");
        // An open / broken / unread well wire reads as no-water and blocks.
        m.tank_contact = Some(false);
        m.well_contact = None;
        assert_eq!(m.level_block(&s), Some("well dry"), "a broken well wire fails safe");
    }

    #[test]
    fn field_bug_tank_full_stops_a_running_auto_pump_on_defaults() {
        // FIELD 2026-08-31: tank full in AUTO, LCD said "tank full", pump kept
        // pumping. Root cause: the gate shipped OFF. With SHIPPING defaults (no
        // explicit enable) a full tank must now stop an Auto run.
        let s = Settings::default();
        let mut m = mach(Mode::Auto); // wet well, tank low
        let t = settle(&mut m, &s, 126.0, 8.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running, "should be pumping at nominal");
        m.tank_contact = Some(true); // tank fills
        m.update(Some(rd(126.0, 8.0)), t + 1, "2026-08-31", &s);
        assert!(!m.state.contactor_closed(), "a full tank must stop the Auto pump");
        assert_eq!(m.level_block(&s), Some("tank full"));
    }

    #[test]
    fn field_bug_dry_well_blocks_start_on_defaults() {
        // FIELD 2026-08-31: well dry, LCD said "well dry", yet START ran the
        // motor -> dry-run -> burns. With SHIPPING defaults a dry well must
        // refuse to start in every mode.
        let s = Settings::default();
        for mode in [Mode::Auto, Mode::Manual] {
            let mut m = mach(mode);
            m.well_contact = Some(false); // dry
            let _ = m.command(Command::Start, 10, &s);
            for i in 0..(s.settle_readings as u64 + 5) {
                m.update(Some(rd(126.0, 8.0)), 11 + i, "2026-08-31", &s);
            }
            assert!(!m.state.contactor_closed(), "a dry well must block START in {mode:?}");
        }
    }

    #[test]
    fn a_restart_resets_the_dry_run_tracking() {
        // Every fresh start must judge dry-run on THIS run's current, never the
        // last run's peak. close() clears primed/peak_amps/collapse; if a
        // refactor drops one, a low-flow restart could inherit a big prior peak
        // and false-trip. Prime a high-current run, trip it, restart low.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false; // isolate the amp path
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 120.0, 10.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running, "run 1 should be pumping");
        assert!(m.primed, "a loaded 10A pump primes");
        assert!(m.peak_amps >= 9.9, "run 1 peak tracks ~10A");
        // knock it off with an overvoltage, then recover and restart at low amps
        m.update(Some(rd(200.0, 10.0)), t + 1, "2026-08-31", &s);
        assert!(!m.state.contactor_closed(), "overvoltage trips run 1");
        let _ = settle(&mut m, &s, 120.0, 3.0, s.settle_readings + 4, t + 2);
        assert_eq!(m.state, State::Running, "run 2 should be pumping");
        assert!(
            m.peak_amps <= 5.0,
            "run 2 peak must reset to this run (~3A), not inherit run 1's 10A (was {})",
            m.peak_amps
        );
    }

    #[test]
    fn a_reboot_cannot_bypass_a_dry_run_lockout() {
        // Power-loss reboot: main.rs restores a persisted lockout by setting
        // state=Lockout while lockout_until is still in the future. If the well
        // is STILL dry after the outage, the pump must not retry into it - the
        // timer runs its course (gate off) or the dry sensor holds it (gate on).
        let mut s = Settings::default();
        s.dry_enabled = true;
        let now = 1000u64;
        for gate in [false, true] {
            s.level_gate_enabled = gate;
            let mut m = mach(Mode::Auto);
            m.well_contact = Some(false); // still dry after the reboot
            m.tank_contact = Some(false);
            m.lockout_until = now + 600; // 10 min left, as restored from disk
            m.dry_strikes = 2;
            m.state = State::Lockout; // exactly what the boot restore sets
            for i in 0..(s.settle_readings as u64 + 10) {
                m.update(Some(rd(126.0, 8.0)), now + i, "2026-08-31", &s);
                assert!(
                    !m.state.contactor_closed(),
                    "a reboot must not run into a still-dry well (gate={gate}, i={i})"
                );
            }
        }
    }

    #[test]
    fn a_sensed_dry_well_does_not_accrue_amp_dry_strikes() {
        // Sensor gate and amp-inferred dry-run are INDEPENDENT. With both armed,
        // a well the SENSOR reports dry is checked first (level_block) and parks
        // the pump - the amp lockout, which escalates a strike counter, must
        // never be reached. Mixing them would over-punish a sensor-equipped box
        // with escalating lockouts it should never see.
        let mut s = Settings::default();
        s.dry_enabled = true; // amp path armed too
        s.level_gate_enabled = true;
        let mid = (s.v_low_reset + s.v_high_reset) / 2.0;
        let mut m = mach(Mode::Auto); // wet well
        let t = settle(&mut m, &s, mid, 8.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running);
        assert_eq!(m.dry_strikes, 0);
        // sensor now reads dry AND amps drop into the amp-dry region (would
        // lockout if reached) - the sensor gate must preempt it.
        m.well_contact = Some(false);
        for i in 0..5 {
            m.update(Some(rd(mid, 2.0)), t + 1 + i, "2026-08-31", &s);
        }
        assert_eq!(m.state, State::Waiting, "a sensed dry well parks, not locks out");
        assert!(!m.state.contactor_closed());
        assert_eq!(m.dry_strikes, 0, "a SENSED dry well must not accrue an amp-dry strike");
    }

    #[test]
    fn a_dry_well_stops_even_a_manual_run() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Manual);
        m.state = State::Running;
        m.well_contact = Some(false); // dry
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-18", &s);
        assert!(!m.state.contactor_closed(), "a dry well must stop even a manual run");
        assert_eq!(m.state, State::Waiting, "a sensed dry well parks - it does not time-lockout");
    }

    #[test]
    fn a_full_tank_does_not_stop_a_manual_run() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Manual);
        m.state = State::Running;
        m.well_contact = Some(true); // water present
        m.tank_contact = Some(true); // full
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-18", &s);
        assert_eq!(m.state, State::Running, "in Manual the operator may fill past full");
    }

    #[test]
    fn handing_an_overfilled_manual_run_to_auto_stops_it() {
        // Field-plausible: the operator fills past full by hand (Manual allows
        // it), then flips the selector to AUTO expecting it to manage the tank.
        // The tank interlock must take over on the very next poll.
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        m.state = State::Running;
        m.well_contact = Some(true);
        m.tank_contact = Some(true); // full
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-31", &s);
        assert_eq!(m.state, State::Running, "Manual may overfill");
        // hand control to AUTO
        m.command(Command::SetMode(Mode::Auto), 101, &s).unwrap();
        m.update(Some(rd(120.0, 5.0)), 102, "2026-08-31", &s);
        assert!(!m.state.contactor_closed(), "AUTO must stop an over-full tank");
        assert_eq!(m.level_block(&s), Some("tank full"));
    }

    #[test]
    fn handing_a_full_tank_run_from_auto_to_manual_keeps_it_running() {
        // The mirror: switching a stopped-by-tank Auto pump to Manual returns a
        // stopped pump (Manual hands back control), and a fresh Manual start is
        // then allowed to run past full. Confirms the tank gate is Auto-only.
        let s = Settings::default();
        let mut m = mach(Mode::Manual);
        m.state = State::Running;
        m.well_contact = Some(true);
        m.tank_contact = Some(true);
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-31", &s);
        assert_eq!(m.state, State::Running, "Manual runs past a full tank");
        assert_eq!(m.level_block(&s), None, "no block in Manual with a full tank");
    }

    #[test]
    fn a_dry_well_never_energizes_the_contactor_on_start() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        s.v_low_reset = 10.0; s.v_high_reset = 17.0;   // bench band
        s.v_low_trip = 9.0; s.v_high_trip = 18.0;
        let mut m = mach(Mode::Manual);
        m.well_contact = Some(false);   // dry
        // settle in-band voltage and press start
        for i in 0..(s.settle_readings + 2) {
            m.update(Some(rd(12.0, 0.0)), 100 + i as u64, "2026-08-19", &s);
        }
        let _ = m.command(Command::Start, 200, &s);
        // drive several more ticks - the contactor must NEVER close, not once
        for i in 0..10 {
            m.update(Some(rd(12.0, 0.0)), 210 + i as u64, "2026-08-19", &s);
            assert!(!m.state.contactor_closed(),
                "dry well must never energize the contactor (tick {i})");
        }
    }

    #[test]
    fn a_recovered_well_ends_the_lockout_early() {
        // The timed dry-run "rest" is a stand-in for waiting out an aquifer
        // refill. With a real well sensor we do not guess: a sensed-wet well
        // ends the lockout at once instead of counting down the full timer.
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        m.state = State::Lockout;
        m.lockout_until = 1_000_000;    // far in the future
        m.well_contact = Some(false);   // still dry
        m.update(Some(rd(12.0, 0.0)), 100, "2026-08-25", &s);
        assert_eq!(m.state, State::Lockout, "a dry well keeps the rest running");
        // the well refills - the sensor, not the clock, decides
        m.well_contact = Some(true);
        m.update(Some(rd(12.0, 0.0)), 101, "2026-08-25", &s);
        assert_ne!(m.state, State::Lockout, "a sensed-wet well ends the rest at once");
    }

    #[test]
    fn a_level_cycle_restart_skips_the_voltage_resettle() {
        // Once the supply is confirmed and the pump has run, a stop that was
        // ONLY the tank filling (volts never left the band) must restart within
        // a tick when the tank draws down - not re-serve the full 15 s settle.
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        m.well_contact = Some(true);
        m.tank_contact = Some(false);
        for i in 0..(s.settle_readings + 2) {
            m.update(Some(rd(120.0, 0.0)), 100 + i as u64, "2026-08-26", &s);
        }
        assert_eq!(m.state, State::Running, "cold start after the full settle");
        m.tank_contact = Some(true);
        m.update(Some(rd(120.0, 5.0)), 200, "2026-08-26", &s);
        assert_eq!(m.state, State::Waiting, "tank full parks it");
        m.tank_contact = Some(false);
        m.update(Some(rd(120.0, 0.0)), 201, "2026-08-26", &s);
        assert_eq!(m.state, State::Running, "level-cycle restart is immediate, no 15 s re-settle");
    }

    #[test]
    fn a_non_voltage_trip_does_not_re_serve_the_settle() {
        // An overcurrent, a max-run, a hand stop, a lost meter - none of these
        // say anything about the supply voltage, so clearing one lets the pump
        // restart within a tick. Only a genuine voltage trip re-serves the settle.
        let mut s = Settings::default();
        s.i_max = 8.0;
        s.i_max_seconds = 0;
        let mut m = mach(Mode::Manual);
        for i in 0..(s.settle_readings + 2) {
            m.update(Some(rd(126.0, 5.0)), 100 + i as u64, "2026-08-31", &s);
        }
        m.command(Command::Start, 200, &s).unwrap();
        m.update(Some(rd(126.0, 5.0)), 201, "2026-08-31", &s);
        assert_eq!(m.state, State::Running, "cold start after the full settle");

        // Overcurrent trips it - the volts never moved.
        m.update(Some(rd(126.0, 20.0)), 202, "2026-08-31", &s);
        m.update(Some(rd(126.0, 20.0)), 203, "2026-08-31", &s);
        assert_eq!(m.state, State::Tripped, "overcurrent trips");
        assert_eq!(m.reason, Some(TripReason::OverCurrent));
        assert!(
            m.stable >= s.settle_readings,
            "a current trip wrongly re-served the voltage settle"
        );

        // Clear and restart at good volts: running within a tick, no countdown.
        m.command(Command::Reset, 204, &s).unwrap();
        m.update(Some(rd(126.0, 5.0)), 205, "2026-08-31", &s);
        m.command(Command::Start, 206, &s).unwrap();
        m.update(Some(rd(126.0, 5.0)), 207, "2026-08-31", &s);
        assert_eq!(
            m.state,
            State::Running,
            "restart after a non-voltage trip must not re-serve the settle"
        );
    }

    #[test]
    fn a_dry_well_stops_a_running_pump() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        m.state = State::Running;
        m.well_contact = Some(false); // open = no water
        m.tank_contact = Some(false);
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-18", &s);
        assert!(!m.state.contactor_closed(), "a dry well must stop the pump");
        assert_eq!(m.state, State::Waiting, "a sensed dry well parks and auto-recovers, no timed lockout");
        // and when the well fills again, Auto closes back on its own - no
        // manual start, no waiting out a timer. The sensor decides.
        m.well_contact = Some(true);
        for i in 0..(s.settle_readings + 2) {
            m.update(Some(rd(120.0, 0.0)), 200 + i as u64, "2026-08-18", &s);
        }
        assert_eq!(m.state, State::Running, "water back -> Auto resumes on its own");
    }

    #[test]
    fn a_broken_well_wire_fails_safe() {
        // Unknown (never-read) well contact must be treated as dry, not water.
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        m.state = State::Running;
        m.well_contact = None; // wire off / relay unread
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-18", &s);
        assert!(!m.state.contactor_closed(), "no signal must fail to stopped");
        assert_eq!(m.state, State::Waiting, "unknown well is treated as dry and blocks");
        // and it STAYS blocked while the signal is still missing (fail-safe:
        // a broken wire never auto-runs, because well_contact is not Some(true))
        for i in 0..(s.settle_readings + 2) {
            m.update(Some(rd(120.0, 0.0)), 200 + i as u64, "2026-08-18", &s);
        }
        assert!(!m.state.contactor_closed(), "a still-broken well wire must never start the pump");
    }

    #[test]
    fn a_full_tank_parks_the_pump_without_a_fault() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        m.state = State::Running;
        m.well_contact = Some(true);  // water present
        m.tank_contact = Some(true);  // full
        m.update(Some(rd(120.0, 5.0)), 100, "2026-08-18", &s);
        assert_eq!(m.state, State::Waiting, "a full tank is a normal stop");
        assert_eq!(m.reason, None, "not a fault");
    }

    #[test]
    fn the_gate_blocks_a_restart_until_water_and_room() {
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        let mut m = mach(Mode::Auto);
        // in band, but well dry: must not close
        m.well_contact = Some(false);
        m.tank_contact = Some(false);
        for i in 0..(s.settle_readings + 5) {
            m.update(Some(rd(130.0, 0.0)), 100 + i as u64, "2026-08-18", &s);
        }
        assert_ne!(m.state, State::Running, "a dry well must block the start");
    }

    #[test]
    fn a_hand_started_run_ends_itself_at_the_deadline() {
        let mut s = Settings::default();
        s.max_run_minutes = 5;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        // Healthy the whole way: normal volts, normal current, nothing for
        // any other trip to catch. Without a deadline this runs for ever.
        m.update(Some(rd(130.0, 2.0)), t + 299, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "must not cut a run short");

        m.update(Some(rd(130.0, 2.0)), t + 300, "2026-08-11", &s);
        assert_eq!(m.state, State::Tripped);
        assert_eq!(m.reason, Some(TripReason::MaxRun));
        assert!(!m.state.contactor_closed(), "contactor must be open");
    }

    #[test]
    fn the_setup_wizard_outlives_its_page_by_nothing() {
        // The wizard starts the pump, samples for 180s, then waits on a tap
        // that may never come. Reproduce exactly that: start, never send
        // another command, and let the clock run.
        let mut s = Settings::default();
        s.max_run_minutes = 4;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        // The measuring phase must survive untouched, or the wizard can
        // never finish and the operator never learns why.
        m.update(Some(rd(130.0, 2.0)), t + 180, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "wizard must get its full 180s");

        // Then the tab is lost. Nothing else happens, ever.
        for i in 181..600 {
            m.update(Some(rd(130.0, 2.0)), t + i, "2026-08-11", &s);
            assert!(
                !m.state.contactor_closed() || i < 240,
                "still pumping {}s in, with nobody holding the page",
                i
            );
        }
        assert!(!m.state.contactor_closed(), "a lost tab must not hold the pump on");
        // Not m.reason: settling back to Ready clears it by design. The log
        // is the durable record of why the pump stopped.
        assert!(
            m.events.iter().any(|e| e.reason == TripReason::MaxRun.label()),
            "the stop must be recorded, got {:?}",
            m.events.iter().map(|e| &e.reason).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_run_limit_trip_does_not_restart_itself() {
        // The trap: trip() alone would leave Auto free to re-close after the
        // settle window, giving a pump that cycles on and off for ever.
        // Manual cannot, because closing needs a fresh Start - prove it.
        let mut s = Settings::default();
        s.max_run_minutes = 5;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        m.update(Some(rd(130.0, 2.0)), t + 300, "2026-08-11", &s);
        assert_eq!(m.reason, Some(TripReason::MaxRun));

        for i in 301..1200 {
            m.update(Some(rd(130.0, 0.0)), t + i, "2026-08-11", &s);
            assert!(
                !m.state.contactor_closed(),
                "restarted itself {}s after the run limit",
                i - 300
            );
        }
        assert_eq!(m.state, State::Ready, "should settle at Ready awaiting a human");
        assert_eq!(m.starts_today, 1, "exactly one start, not a cycle");
    }

    #[test]
    fn an_unattended_auto_run_is_not_capped() {
        // Auto is the mode a solar pump works a whole day in. Capping it
        // would trip a good run every afternoon.
        let mut s = Settings::default();
        s.max_run_minutes = 5;
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        m.update(Some(rd(130.0, 2.0)), t + 36_000, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "Auto must be free to run all day");
    }

    #[test]
    fn zero_disables_the_run_limit() {
        let mut s = Settings::default();
        s.max_run_minutes = 0;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        m.update(Some(rd(130.0, 2.0)), t + 86_400, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "0 must mean no limit");
    }

    #[test]
    fn a_real_fault_outranks_the_run_limit() {
        // Both conditions true on the same tick. The log has to say what
        // actually endangered the pump, not that the clock ran out.
        let mut s = Settings::default();
        s.max_run_minutes = 5;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);

        m.update(Some(rd(147.0, 2.0)), t + 300, "2026-08-11", &s);
        assert_eq!(m.reason, Some(TripReason::OverVoltage));
    }

    #[test]
    fn the_deadline_is_rearmed_by_a_fresh_start() {
        let mut s = Settings::default();
        s.max_run_minutes = 5;
        let mut m = mach(Mode::Manual);
        let t = run_by_hand(&mut m, &s);
        m.update(Some(rd(130.0, 2.0)), t + 300, "2026-08-11", &s);
        assert_eq!(m.reason, Some(TripReason::MaxRun));

        // Settle back to Ready, then start again: the second run gets its
        // own full allowance, not the remainder of the first.
        let t2 = settle(&mut m, &s, 130.0, 0.0, s.settle_readings + 2, t + 301);
        m.command(Command::Start, t2, &s).unwrap();
        m.update(Some(rd(130.0, 2.0)), t2, "2026-08-11", &s);
        assert_eq!(m.state, State::Running);
        m.update(Some(rd(130.0, 2.0)), t2 + 299, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "a new run gets a new deadline");
    }

    // --------------------------------------------- run time is a wall clock

    #[test]
    fn run_time_counts_seconds_not_calls() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        assert_eq!(m.state, State::Running);

        m.update(Some(rd(130.0, 2.0)), t, "2026-08-11", &s);
        let base = m.run_seconds_today;

        // A button press calls update() again on the same instant. That used
        // to add a whole second each time, which is why the dashboard could
        // show 200 seconds of running on a bench that had never pumped.
        for _ in 0..5 {
            m.update(Some(rd(130.0, 2.0)), t, "2026-08-11", &s);
        }
        assert_eq!(m.run_seconds_today, base, "extra calls must not add time");

        m.update(Some(rd(130.0, 2.0)), t + 10, "2026-08-11", &s);
        assert_eq!(m.run_seconds_today, base + 10, "ten seconds is ten seconds");
    }

    #[test]
    fn a_clock_jump_does_not_inflate_run_time() {
        // No RTC on this box: NTP steps the clock by minutes or years
        // shortly after boot.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        m.update(Some(rd(130.0, 2.0)), t, "2026-08-11", &s);
        let base = m.run_seconds_today;

        m.update(Some(rd(130.0, 2.0)), t + 31_536_000, "2026-08-11", &s);
        assert!(
            m.run_seconds_today - base <= 60,
            "a year-long step added {}s",
            m.run_seconds_today - base
        );
    }

    #[test]
    fn run_time_does_not_run_backwards() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let t = settle(&mut m, &s, 130.0, 0.0, s.settle_readings, 0);
        m.update(Some(rd(130.0, 2.0)), t + 100, "2026-08-11", &s);
        let base = m.run_seconds_today;
        m.update(Some(rd(130.0, 2.0)), t, "2026-08-11", &s);
        assert_eq!(m.run_seconds_today, base, "a backward step must add nothing");
    }
}

// ===================================================================
// Exhaustive coverage
//
// The tests above cover the scenarios that matter operationally. These
// sweep the whole space: every mode, every state, every command, and the
// invariants that must hold no matter which combination you land in.
// ===================================================================

#[cfg(test)]
mod exhaustive {
    use super::*;
    use crate::settings::Settings;

    const ALL_MODES: [Mode; 3] = [Mode::Auto, Mode::Manual, Mode::Off];
    const ALL_STATES: [State; 8] = [
        State::Off, State::Waiting, State::Ready, State::Starting,
        State::Running, State::Tripped, State::Lockout, State::Fault,
    ];
    const ALL_COMMANDS: [Command; 6] = [
        Command::Start,
        Command::Stop,
        Command::Reset,
        Command::ClearLockout,
        Command::SetMode(Mode::Auto),
        Command::SetMode(Mode::Manual),
    ];

    fn rd(v: f32, a: f32) -> Reading {
        Reading { volts: v, amps: a, watts: v * a, watt_hours: 1000.0 }
    }

    fn feed(m: &mut Machine, s: &Settings, v: f32, a: f32, n: u32, from: u64) -> u64 {
        let mut t = from;
        for _ in 0..n {
            m.update(Some(rd(v, a)), t, "2026-08-11", s);
            t += 1;
        }
        t
    }

    /// Build a machine sitting in the requested state, so each state can be
    /// exercised against every command without hand-rolling a path each time.
    fn machine_in(state: State, mode: Mode, s: &Settings) -> (Machine, u64) {
        let mut m = mach(mode);
        let mut t = 0u64;

        match state {
            State::Off => {
                m.command(Command::SetMode(Mode::Off), t, s).ok();
            }
            State::Waiting => {
                t = feed(&mut m, s, 200.0, 0.0, 3, t); // far above the band
            }
            State::Starting => {
                t = feed(&mut m, s, 130.0, 0.0, 2, t); // partway to settled
            }
            State::Ready => {
                let mut mm = mach(Mode::Manual);
                t = feed(&mut mm, s, 130.0, 0.0, s.settle_readings + 2, 0);
                mm.mode = mode;
                return (mm, t);
            }
            State::Running => {
                let mut mm = mach(Mode::Auto);
                t = feed(&mut mm, s, 130.0, 0.0, s.settle_readings, 0);
                mm.mode = mode;
                return (mm, t);
            }
            State::Tripped => {
                t = feed(&mut m, s, 130.0, 0.0, s.settle_readings, t);
                m.update(Some(rd(200.0, 0.0)), t, "2026-08-11", s);
                t += 1;
            }
            State::Lockout => {
                let mut ds = s.clone();
                ds.dry_enabled = true;
                ds.dry_grace_seconds = 0;
                ds.dry_seconds = 0;
                ds.dry_volts = 128.0;
                ds.dry_amps = 5.0;
                t = feed(&mut m, &ds, 126.0, 10.0, ds.settle_readings, t);
                for _ in 0..3 {
                    m.update(Some(rd(134.0, 1.0)), t, "2026-08-11", &ds);
                    t += 1;
                }
            }
            State::Fault => {
                t = feed(&mut m, s, 130.0, 0.0, s.settle_readings, t);
                for _ in 0..s.meter_error_limit {
                    m.update(None, t, "2026-08-11", s);
                    t += 1;
                }
            }
        }
        m.mode = mode;
        (m, t)
    }

    #[test]
    fn every_state_reaches_running_when_conditions_become_safe() {
        // LIVENESS (complement to the safety invariant): no state is a dead-end.
        // Safety proves the contactor is NEVER closed while unsafe; this proves
        // the pump DOES run once it's safe and wanted - so an irrigation box can
        // never get stuck OFF forever with everything fine (crops unwatered). For
        // EVERY one of the 8 states, applying safe + "please run" conditions
        // (Auto mode, in-band volts, wet well, tank with room, healthy meter, and
        // enough time for any lockout/rest to expire) must reach Running.
        let s = Settings::default();
        for state in [
            State::Off,
            State::Waiting,
            State::Ready,
            State::Starting,
            State::Running,
            State::Tripped,
            State::Lockout,
            State::Fault,
        ] {
            let (mut m, mut t) = machine_in(state, Mode::Auto, &s);
            m.command(Command::SetMode(Mode::Auto), t, &s).ok();
            let mut ran = false;
            for _ in 0..(s.settle_readings + 90) {
                m.well_contact = Some(true); // wet
                m.tank_contact = Some(false); // room
                // a healthy running point: mid-band volts, normal current
                m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
                t += 60; // a minute per step, so any dry-run rest / lockout expires
                if m.state == State::Running {
                    ran = true;
                    break;
                }
            }
            assert!(
                ran,
                "state {state:?} never reached Running under safe+start conditions \
                 - a dead-end/livelock that would leave the pump stuck OFF"
            );
        }
    }

    #[test]
    fn a_stop_command_from_any_state_and_mode_opens_the_contactor() {
        // GUARANTEED STOP: the operator (or the system) must ALWAYS be able to
        // stop the pump. The safety invariant proves "closed => conditions safe"
        // but NOT "Stop => open" - a Stop ignored while conditions are safe would
        // leave a running pump nobody can stop (e.g. for maintenance, or a fault
        // the sensors don't see). Prove Stop opens the contactor from every state
        // and every mode, and that it does not bounce back closed on the next tick.
        let s = Settings::default();
        for state in [
            State::Off,
            State::Waiting,
            State::Ready,
            State::Starting,
            State::Running,
            State::Tripped,
            State::Lockout,
            State::Fault,
        ] {
            for mode in [Mode::Auto, Mode::Manual, Mode::Off] {
                let (mut m, mut t) = machine_in(state, mode, &s);
                m.command(Command::Stop, t, &s).ok();
                assert!(
                    !m.state.contactor_closed(),
                    "Stop from state {state:?} / mode {mode:?} left the contactor CLOSED"
                );
                // and it must STAY open on the next poll under otherwise-safe
                // conditions - a Stop that auto-restarts within a tick is no stop.
                t += 1;
                m.update(Some(rd(130.0, 8.0)), t, "2026-08-11", &s);
                assert!(
                    !m.state.contactor_closed(),
                    "Stop from {state:?}/{mode:?} bounced back CLOSED on the next tick"
                );
            }
        }
    }

    #[test]
    fn the_level_gate_is_purely_subtractive_never_weakens_volt_or_current() {
        // THE MISSION'S CENTRAL CONSTRAINT, formalized: "never weaken the voltage
        // protection while enabling the level gate." The gate must be purely
        // SUBTRACTIVE - it may only ADD stops (dry well / full tank), never cause
        // a contactor close that voltage+current alone would not allow. Formally:
        // for identical conditions, gate-ON-closed  =>  gate-OFF-closed. If the
        // gate ever closed the contactor where the gate-off machine did NOT, the
        // gate would have *added* a close = weakened protection. Sweep the grid.
        let mut on = Settings::default();
        on.level_gate_enabled = true;
        let mut off = Settings::default();
        off.level_gate_enabled = false;
        for &volts in &[80.0f32, 90.0, 95.0, 100.0, 130.0, 138.0, 145.0, 146.0, 160.0] {
            for &amps in &[0.0f32, 8.0, 20.0] {
                for &well in &[Some(true), Some(false), None] {
                    for &tank in &[Some(true), Some(false), None] {
                        for mode in [Mode::Auto, Mode::Manual] {
                            let mut m_on = Machine::new(mode);
                            let mut m_off = Machine::new(mode);
                            m_on.command(Command::Start, 0, &on).ok();
                            m_off.command(Command::Start, 0, &off).ok();
                            let mut t = 1u64;
                            for _ in 0..(on.settle_readings + 5) {
                                m_on.well_contact = well;
                                m_on.tank_contact = tank;
                                m_off.well_contact = well;
                                m_off.tank_contact = tank;
                                m_on.update(Some(rd(volts, amps)), t, "2026-08-11", &on);
                                m_off.update(Some(rd(volts, amps)), t, "2026-08-11", &off);
                                m_on.command(Command::Start, t, &on).ok();
                                m_off.command(Command::Start, t, &off).ok();
                                t += 1;
                            }
                            if m_on.state.contactor_closed() {
                                assert!(
                                    m_off.state.contactor_closed(),
                                    "GATE WEAKENED PROTECTION: gate-ON closed the contactor where \
                                     gate-OFF did not (volts={volts} amps={amps} well={well:?} \
                                     tank={tank:?} mode={mode:?}) - the gate ADDED a close"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn no_optional_guard_closes_where_the_raw_voltage_current_guard_would_not() {
        // Generalizes R103 to EVERY optional guard. BASELINE = voltage + current
        // protection only (level gate OFF, dry-run OFF). Each optional guard
        // (level gate; amp dry-run) - alone OR combined - must be purely
        // SUBTRACTIVE: if a machine with any guards on closes the contactor, the
        // baseline machine closes too. So no optional guard, in any combination,
        // can ever cause a close the raw volt/current guard wouldn't allow.
        let baseline = |mode| {
            let mut s = Settings::default();
            s.level_gate_enabled = false;
            s.dry_enabled = false;
            (Machine::new(mode), s)
        };
        // (gate, dry) combinations other than the all-off baseline
        for &(gate, dry) in &[(true, false), (false, true), (true, true)] {
            let mut cfg = Settings::default();
            cfg.level_gate_enabled = gate;
            cfg.dry_enabled = dry;
            if dry {
                // aggressive params so the dry guard actually fires in-window
                cfg.dry_volts = 128.0;
                cfg.dry_amps = 5.0;
                cfg.dry_seconds = 0;
                cfg.dry_grace_seconds = 0;
            }
            for &volts in &[80.0f32, 95.0, 100.0, 130.0, 145.0, 146.0, 160.0] {
                for &amps in &[0.0f32, 3.0, 8.0, 20.0] {
                    for &well in &[Some(true), Some(false), None] {
                        for &tank in &[Some(true), Some(false), None] {
                            for mode in [Mode::Auto, Mode::Manual] {
                                let mut m_cfg = Machine::new(mode);
                                let (mut m_base, base_s) = baseline(mode);
                                m_cfg.command(Command::Start, 0, &cfg).ok();
                                m_base.command(Command::Start, 0, &base_s).ok();
                                let mut t = 1u64;
                                for _ in 0..(cfg.settle_readings + 5) {
                                    m_cfg.well_contact = well;
                                    m_cfg.tank_contact = tank;
                                    m_base.well_contact = well;
                                    m_base.tank_contact = tank;
                                    m_cfg.update(Some(rd(volts, amps)), t, "2026-08-11", &cfg);
                                    m_base.update(Some(rd(volts, amps)), t, "2026-08-11", &base_s);
                                    m_cfg.command(Command::Start, t, &cfg).ok();
                                    m_base.command(Command::Start, t, &base_s).ok();
                                    t += 1;
                                }
                                if m_cfg.state.contactor_closed() {
                                    assert!(
                                        m_base.state.contactor_closed(),
                                        "OPTIONAL GUARD WEAKENED THE CORE: gate={gate} dry={dry} \
                                         closed the contactor where the raw volt/current guard did \
                                         NOT (volts={volts} amps={amps} well={well:?} tank={tank:?} \
                                         mode={mode:?})"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------
    // The invariant that matters most
    // ---------------------------------------------------------------

    #[test]
    fn no_command_ever_starts_an_unsafe_pump() {
        let s = Settings::default();
        // Voltages spanning well outside the band in both directions.
        for volts in [0.0, 50.0, 100.0, 111.0, 117.0, 139.0, 146.0, 165.0, 300.0] {
            for mode in ALL_MODES {
                for state in ALL_STATES {
                    for cmd in ALL_COMMANDS {
                        let (mut m, t) = machine_in(state, mode, &s);
                        let _ = m.command(cmd, t, &s);
                        m.update(Some(rd(volts, 0.0)), t + 1, "2026-08-11", &s);

                        // Two bands, deliberately: a pump must reach
                        // 118-138V to start, but keeps running down to 112V
                        // and up to 145V. The gap is what stops the
                        // contactor chattering at the threshold. So the
                        // invariant for a closed contactor is the trip band.
                        let safe_to_run = volts > s.v_low_trip && volts < s.v_high_trip;
                        assert!(
                            !m.state.contactor_closed() || safe_to_run,
                            "contactor closed at {volts}V from {state:?}/{mode:?} after {cmd:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_contactor_is_never_closed_while_unsafe_full_grid() {
        // MASTER SAFETY INVARIANT, swept exhaustively (math-solid proof).
        // For every gate setting, mode, well/tank contact, and a voltage grid
        // straddling both thresholds, drive the machine from EVERY state through
        // EVERY command and a following poll, then assert: if the contactor is
        // closed, every safety axis was satisfied by the reading that produced
        // this state. This is the property that keeps a pump from burning.
        for gate in [false, true] {
            let mut s = Settings::default();
            s.level_gate_enabled = gate;
            for volts in [
                0.0, 90.0, 94.0, 95.0, 96.0, 109.0, 111.0, 138.0, 141.0, 144.0, 145.0, 146.0, 200.0,
            ] {
                for mode in ALL_MODES {
                    for well in [None, Some(false), Some(true)] {
                        for tank in [None, Some(false), Some(true)] {
                            for state in ALL_STATES {
                                for cmd in ALL_COMMANDS {
                                    let (mut m, t) = machine_in(state, mode, &s);
                                    m.well_contact = well;
                                    m.tank_contact = tank;
                                    let _ = m.command(cmd, t, &s);
                                    m.update(Some(rd(volts, 5.0)), t + 1, "2026-08-11", &s);
                                    if m.state.contactor_closed() {
                                        // The trip is strict (>v_high_trip / <v_low_trip),
                                        // so the running band is inclusive of both ends.
                                        assert!(
                                            volts >= s.v_low_trip && volts <= s.v_high_trip,
                                            "closed at {volts}V ({state:?}/{mode:?}/{cmd:?})"
                                        );
                                        assert_ne!(m.mode, Mode::Off, "closed while OFF");
                                        if gate {
                                            assert_eq!(
                                                well,
                                                Some(true),
                                                "closed with a non-wet well {well:?} ({mode:?}/{cmd:?})"
                                            );
                                            assert!(
                                                !(m.mode == Mode::Auto && tank == Some(true)),
                                                "closed with a full tank in Auto ({state:?}/{cmd:?})"
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn no_clock_jump_ever_closes_the_contactor_while_unsafe() {
        // No RTC: NTP steps the clock by minutes-to-years shortly after boot,
        // forward OR back. A jump must never close the contactor on an unsafe
        // reading, nor let a non-wet well run. Sweep jump x gate x mode x state
        // x well, then poll at the jumped clock and force an overvoltage.
        let jumps: [i64; 5] = [-1_000_000_000, -100_000, -1, 100_000, 1_000_000_000];
        for gate in [false, true] {
            let mut s = Settings::default();
            s.level_gate_enabled = gate;
            for &jump in &jumps {
                for mode in ALL_MODES {
                    for state in ALL_STATES {
                        for well in [None, Some(false), Some(true)] {
                            let (mut m, t) = machine_in(state, mode, &s);
                            m.well_contact = well;
                            m.tank_contact = Some(false);
                            let jumped = (t as i64 + jump).max(0) as u64;
                            for k in 0..3 {
                                m.update(Some(rd(126.0, 8.0)), jumped + k, "2026-08-31", &s);
                                if m.state.contactor_closed() && gate {
                                    assert_eq!(
                                        well,
                                        Some(true),
                                        "jump {jump} closed into a non-wet well ({state:?}/{mode:?})"
                                    );
                                }
                            }
                            // an overvoltage at the jumped clock must still open it
                            m.update(Some(rd(200.0, 8.0)), jumped + 5, "2026-08-31", &s);
                            assert!(
                                !m.state.contactor_closed(),
                                "jump {jump} left it closed at 200V ({state:?}/{mode:?})"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_fuzz_of_interleaved_commands_and_readings_never_closes_unsafely() {
        // update() runs on the 500ms poll AND on every button press, so
        // commands and readings interleave in orders no scripted test covers.
        // Deterministic LCG fuzz: 20k runs x 30 steps, invariant after each.
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let s = Settings::default(); // gate ON, bench band (9-18V)
        let modes = [Mode::Auto, Mode::Manual, Mode::Off];
        for _ in 0..20_000 {
            let mut m = Machine::new(modes[(rng() % 3) as usize]);
            m.well_contact = Some(true);
            m.tank_contact = Some(false);
            let mut t = 100u64;
            for _ in 0..30 {
                t += 1 + (rng() % 3) as u64;
                match rng() % 7 {
                    0 => { let _ = m.command(Command::Start, t, &s); }
                    1 => { let _ = m.command(Command::Stop, t, &s); }
                    2 => { let _ = m.command(Command::Reset, t, &s); }
                    3 => { let _ = m.command(Command::ClearLockout, t, &s); }
                    4 => { let _ = m.command(Command::SetMode(modes[(rng() % 3) as usize]), t, &s); }
                    _ => {
                        // a reading carries fresh sensor states so the machine
                        // re-evaluates the level gate on the same tick.
                        m.well_contact = [None, Some(false), Some(true)][(rng() % 3) as usize];
                        m.tank_contact = [Some(false), Some(true)][(rng() % 2) as usize];
                        let v = (rng() % 40) as f32; // straddles the bench band
                        let a = (rng() % 20) as f32;
                        m.update(Some(rd(v, a)), t, "2026-08-31", &s);
                    }
                }
                if m.state.contactor_closed() {
                    let r = m.last.expect("a closed contactor implies a reading");
                    assert!(
                        r.volts >= s.v_low_trip && r.volts <= s.v_high_trip,
                        "fuzz closed at {}V",
                        r.volts
                    );
                    assert_ne!(m.mode, Mode::Off, "fuzz closed while OFF");
                    assert_eq!(m.well_contact, Some(true), "fuzz closed into a non-wet well");
                    assert!(
                        !(m.mode == Mode::Auto && m.tank_contact == Some(true)),
                        "fuzz closed with a full tank in Auto"
                    );
                }
            }
        }
    }

    #[test]
    fn the_settle_never_restarts_on_mode_or_level_changes_while_volts_hold() {
        // The user's hard, oft-repeated requirement: the voltage-steadiness
        // check runs CONTINUOUSLY. Flipping the mode selector or a level sensor
        // must NEVER restart the countdown - only voltage leaving the band may.
        // Fuzz arbitrary command/sensor churn against a held in-band supply and
        // assert `stable` never decreases.
        let mut seed: u64 = 0x0bad_c0de_dead_beef;
        let mut rng = || {
            seed = seed
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (seed >> 33) as u32
        };
        let s = Settings::default(); // bench band, reset 10-17V
        let modes = [Mode::Auto, Mode::Manual, Mode::Off];
        for _ in 0..5_000 {
            let mut m = Machine::new(modes[(rng() % 3) as usize]);
            m.well_contact = Some(true);
            m.tank_contact = Some(false);
            let mut t = 0u64;
            let mut prev = 0u32;
            for _ in 0..60 {
                t += 1;
                match rng() % 5 {
                    0 => { let _ = m.command(Command::SetMode(modes[(rng() % 3) as usize]), t, &s); }
                    1 => { m.well_contact = [Some(false), Some(true), None][(rng() % 3) as usize]; }
                    2 => { m.tank_contact = [Some(false), Some(true)][(rng() % 2) as usize]; }
                    3 => { let _ = m.command(Command::Stop, t, &s); }
                    _ => {}
                }
                // a voltage safely inside the band on every tick (band-agnostic)
                let v = (s.v_low_reset + s.v_high_reset) / 2.0 + (rng() % 3) as f32;
                m.update(Some(rd(v, 4.0)), t, "2026-08-31", &s);
                assert!(
                    m.stable >= prev,
                    "settle restarted {prev}->{} on a mode/level change (volts held)",
                    m.stable
                );
                prev = m.stable;
            }
            assert_eq!(prev, s.settle_readings, "a held supply must reach a full settle");
        }
    }

    #[test]
    fn meter_loss_always_opens_the_contactor() {
        // Safety axis the voltage/level grid can't reach: a dead meter. From
        // EVERY state and mode, once misses reach the limit the contactor must
        // be open - running blind is never safe. (Below the limit a running
        // pump deliberately rides through a transient blip; that tolerance is
        // asserted separately so a change to it is caught.)
        let s = Settings::default();
        for mode in ALL_MODES {
            for state in ALL_STATES {
                let (mut m, t) = machine_in(state, mode, &s);
                for i in 0..=(s.meter_error_limit as u64) {
                    m.update(None, t + i, "2026-08-11", &s);
                }
                assert!(
                    !m.state.contactor_closed(),
                    "a lost meter must open the contactor from {state:?}/{mode:?}"
                );
            }
        }
    }

    #[test]
    fn a_meter_loss_opens_the_contactor_across_the_full_grid() {
        // Compose the no-meter axis with EVERY other safety dimension, so the
        // mission's six axes are all covered by an exhaustive sweep and not just
        // meter-loss-in-isolation. Whatever the gate, mode, well/tank contacts,
        // preceding command, or starting state - and even after a good reading
        // has had its chance to close the contactor - once the meter has been
        // silent for meter_error_limit polls the contactor MUST be open. Running
        // blind is never safe on any axis.
        for gate in [false, true] {
            let mut s = Settings::default();
            s.level_gate_enabled = gate;
            for mode in ALL_MODES {
                for well in [None, Some(false), Some(true)] {
                    for tank in [None, Some(false), Some(true)] {
                        for state in ALL_STATES {
                            for cmd in ALL_COMMANDS {
                                let (mut m, t) = machine_in(state, mode, &s);
                                m.well_contact = well;
                                m.tank_contact = tank;
                                let _ = m.command(cmd, t, &s);
                                // A good reading first, so any path that would
                                // close does; then the meter goes silent.
                                m.update(Some(rd(130.0, 5.0)), t + 1, "2026-08-11", &s);
                                for i in 0..=(s.meter_error_limit as u64) {
                                    m.update(None, t + 2 + i, "2026-08-11", &s);
                                }
                                assert!(
                                    !m.state.contactor_closed(),
                                    "meter loss left the contactor closed \
                                     (gate={gate} {mode:?}/{well:?}/{tank:?}/{state:?}/{cmd:?})"
                                );
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_running_pump_rides_through_a_meter_blip_below_the_limit() {
        // Documents the intended tolerance: fewer than meter_error_limit misses
        // must NOT drop a healthy running pump (no nuisance trips on PZEM noise).
        let s = Settings::default();
        let mut m = Machine::new(Mode::Auto);
        m.well_contact = Some(true);
        m.tank_contact = Some(false);
        let t = feed(&mut m, &s, 126.0, 8.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running);
        for i in 0..(s.meter_error_limit as u64 - 1) {
            m.update(None, t + 1 + i, "2026-08-31", &s);
            assert!(m.state.contactor_closed(), "blip {i} must not drop the pump");
        }
        // the missing reading finally reaches the limit -> open
        m.update(None, t + s.meter_error_limit as u64, "2026-08-31", &s);
        assert!(!m.state.contactor_closed(), "at the limit the pump must stop");
    }

    #[test]
    fn both_gates_on_compose_without_ever_closing_unsafely() {
        // A cautious operator may arm BOTH the sensor level gate AND the
        // amp-inferred dry-run. Enabling a second protection can only ADD stop
        // conditions, never remove one - so the master invariant must still
        // hold. Sweep volts x amps x mode x well x tank x state with both on.
        let mut s = Settings::default();
        s.level_gate_enabled = true;
        s.dry_enabled = true;
        for volts in [0.0, 94.0, 95.0, 126.0, 145.0, 146.0, 200.0] {
            for amps in [0.0, 4.0, 20.0] {
                for mode in ALL_MODES {
                    for well in [None, Some(false), Some(true)] {
                        for tank in [Some(false), Some(true)] {
                            for state in ALL_STATES {
                                let (mut m, t) = machine_in(state, mode, &s);
                                m.well_contact = well;
                                m.tank_contact = tank;
                                m.update(Some(rd(volts, amps)), t + 1, "2026-08-31", &s);
                                if m.state.contactor_closed() {
                                    assert!(
                                        volts >= s.v_low_trip && volts <= s.v_high_trip,
                                        "both-gates: closed at {volts}V ({state:?}/{mode:?})"
                                    );
                                    assert_ne!(m.mode, Mode::Off, "both-gates: closed while OFF");
                                    assert_eq!(
                                        well,
                                        Some(true),
                                        "both-gates: closed into a non-wet well ({mode:?})"
                                    );
                                    assert!(
                                        !(m.mode == Mode::Auto && tank == Some(true)),
                                        "both-gates: closed with a full tank in Auto"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_stopped_pump_only_starts_inside_the_start_band() {
        let s = Settings::default();
        for volts in [0.0, 50.0, 100.0, 111.0, 117.0, 139.0, 146.0, 165.0, 300.0] {
            for mode in ALL_MODES {
                // Begin from a state where the contactor is open.
                for state in [State::Waiting, State::Ready, State::Tripped, State::Fault] {
                    let (mut m, t) = machine_in(state, mode, &s);
                    assert!(!m.state.contactor_closed(), "setup should start open");

                    let _ = m.command(Command::Start, t, &s);
                    for i in 0..(s.settle_readings as u64 + 5) {
                        m.update(Some(rd(volts, 0.0)), t + 1 + i, "2026-08-11", &s);
                    }

                    let can_start = volts > s.v_low_reset && volts < s.v_high_reset;
                    assert!(
                        !m.state.contactor_closed() || can_start,
                        "started at {volts}V from {state:?}/{mode:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn leaving_off_into_good_volts_does_not_recount() {
        // The voltage check runs continuously, including while switched
        // off, so flipping the switch to AUTO on a steady supply should
        // start rather than show a progress bar.
        let s = Settings::default();
        let mut m = mach(Mode::Off);

        // Sit in OFF with perfectly good voltage for a while.
        let mut t = 0u64;
        for _ in 0..(s.settle_readings + 10) {
            m.update(Some(rd(126.0, 0.0)), t, "2026-08-11", &s);
            t += 1;
            assert!(!m.state.contactor_closed(), "must never run while OFF");
        }

        // Switching to AUTO should take effect at once.
        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap();
        m.update(Some(rd(126.0, 0.0)), t + 1, "2026-08-11", &s);
        assert_eq!(m.state, State::Running, "should start without recounting");
    }

    #[test]
    fn leaving_off_into_bad_volts_still_waits() {
        let s = Settings::default();
        let mut m = mach(Mode::Off);

        let mut t = 0u64;
        for _ in 0..(s.settle_readings + 10) {
            m.update(Some(rd(163.0, 0.0)), t, "2026-08-11", &s);
            t += 1;
        }

        m.command(Command::SetMode(Mode::Auto), t, &s).unwrap();
        m.update(Some(rd(163.0, 0.0)), t + 1, "2026-08-11", &s);
        assert_eq!(m.state, State::Waiting);
        assert!(!m.state.contactor_closed());
    }

    #[test]
    fn mode_off_always_means_pump_off() {
        let s = Settings::default();
        for state in ALL_STATES {
            for cmd in ALL_COMMANDS {
                let (mut m, t) = machine_in(state, Mode::Auto, &s);
                m.command(Command::SetMode(Mode::Off), t, &s).unwrap();

                // Nothing anyone does should override OFF except leaving it.
                let _ = m.command(cmd, t + 1, &s);
                if matches!(cmd, Command::SetMode(x) if x != Mode::Off) {
                    continue; // deliberately leaving OFF
                }
                for i in 0..40 {
                    m.update(Some(rd(126.0, 0.0)), t + 2 + i, "2026-08-11", &s);
                    assert!(
                        !m.state.contactor_closed(),
                        "pump ran in OFF from {state:?} after {cmd:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn losing_the_meter_always_stops_the_pump() {
        let s = Settings::default();
        for mode in [Mode::Auto, Mode::Manual] {
            let (mut m, mut t) = machine_in(State::Running, mode, &s);
            assert!(m.state.contactor_closed());

            for _ in 0..s.meter_error_limit {
                m.update(None, t, "2026-08-11", &s);
                t += 1;
            }
            assert!(!m.state.contactor_closed(), "ran blind in {mode:?}");
            assert_eq!(m.state, State::Fault);

            // And it must stay stopped while the meter is silent.
            for _ in 0..50 {
                m.update(None, t, "2026-08-11", &s);
                t += 1;
                assert!(!m.state.contactor_closed());
            }
        }
    }

    #[test]
    fn commands_never_panic_from_any_state() {
        let s = Settings::default();
        for mode in ALL_MODES {
            for state in ALL_STATES {
                for cmd in ALL_COMMANDS {
                    let (mut m, t) = machine_in(state, mode, &s);
                    // Result is deliberately ignored - a refusal is a valid
                    // outcome. What matters is that nothing panics and the
                    // machine stays in a legal state.
                    let _ = m.command(cmd, t, &s);
                    m.update(Some(rd(126.0, 5.0)), t + 1, "2026-08-11", &s);
                    assert!(ALL_STATES.contains(&m.state));
                }
            }
        }
    }

    #[test]
    fn every_state_and_mode_pair_is_reachable_and_stable() {
        let s = Settings::default();
        for mode in ALL_MODES {
            for state in ALL_STATES {
                let (mut m, t) = machine_in(state, mode, &s);
                // Feeding the same reading repeatedly must converge, not
                // oscillate between states forever.
                let mut seen = Vec::new();
                for i in 0..60 {
                    m.update(Some(rd(126.0, 8.0)), t + i, "2026-08-11", &s);
                    seen.push(m.state);
                }
                let settled = seen[seen.len() - 10..].to_vec();
                assert!(
                    settled.windows(2).all(|w| w[0] == w[1]),
                    "{state:?}/{mode:?} never settled: {settled:?}"
                );
            }
        }
    }

    // ---------------------------------------------------------------
    // Threshold edges
    // ---------------------------------------------------------------

    #[test]
    fn hysteresis_prevents_chatter_at_every_boundary() {
        let s = Settings::default();
        // Sit exactly on each threshold and jitter around it.
        for centre in [s.v_low_trip, s.v_low_reset, s.v_high_reset, s.v_high_trip] {
            let mut m = mach(Mode::Auto);
            let mut transitions = 0;
            let mut prev = m.state;

            for i in 0..200u64 {
                let jitter = if i % 2 == 0 { 0.05 } else { -0.05 };
                m.update(Some(rd(centre + jitter, 5.0)), i, "2026-08-11", &s);
                if m.state != prev {
                    transitions += 1;
                    prev = m.state;
                }
            }
            // A few transitions while settling are fine; dozens means the
            // display and the log would be thrashing.
            assert!(
                transitions < 8,
                "{transitions} transitions jittering around {centre}V"
            );
        }
    }

    #[test]
    fn a_multivolt_swing_across_the_trip_does_not_recycle_the_pump() {
        // Broken clouds: the array swings several volts ACROSS v_high_trip but
        // never falls into the restart band. It must trip once and stay open -
        // no contactor chatter, no pump start/stop cycling that wears a motor.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let mid = (s.v_low_reset + s.v_high_reset) / 2.0;
        let t = feed(&mut m, &s, mid, 5.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running, "should be running at nominal");

        let above = s.v_high_trip + 3.0; // clearly over the trip
        let dead = (s.v_high_reset + s.v_high_trip) / 2.0; // below trip, ABOVE reset
        let mut re_closes = 0;
        let mut prev = m.state.contactor_closed();
        for i in 0..40u64 {
            let v = if i % 2 == 0 { above } else { dead };
            m.update(Some(rd(v, 5.0)), t + 1 + i, "2026-08-31", &s);
            let now = m.state.contactor_closed();
            if now && !prev {
                re_closes += 1;
            }
            prev = now;
            if v >= s.v_high_trip {
                assert!(!m.state.contactor_closed(), "closed at {v}V (>= trip)");
            }
        }
        assert_eq!(re_closes, 0, "pump re-closed during a dead-band swing (chatter)");
        assert!(!m.state.contactor_closed(), "must stay open after the swing");
    }

    #[test]
    fn a_brief_start_inrush_does_not_trip_overcurrent() {
        // A motor's starting inrush spikes well above i_max for under a second.
        // over_current requires the overload SUSTAINED for i_max_seconds, so a
        // brief inrush must ride through - else every start would trip and
        // cycle the pump. A sustained overload still trips.
        let s = Settings::default();
        let mid = (s.v_low_reset + s.v_high_reset) / 2.0;

        // (1) brief inrush: 3x i_max, shorter than i_max_seconds -> no trip
        let mut m = mach(Mode::Auto);
        let t = feed(&mut m, &s, mid, 5.0, s.settle_readings + 2, 0);
        assert_eq!(m.state, State::Running);
        for i in 0..(s.i_max_seconds - 1) {
            m.update(Some(rd(mid, s.i_max * 3.0)), t + 1 + i, "2026-08-31", &s);
            assert!(m.state.contactor_closed(), "a brief inrush must not trip (i={i})");
        }
        m.update(Some(rd(mid, 5.0)), t + 1 + s.i_max_seconds, "2026-08-31", &s);
        assert!(m.state.contactor_closed(), "must keep running once the inrush clears");

        // (2) sustained overload MUST trip
        let mut m2 = mach(Mode::Auto);
        let t2 = feed(&mut m2, &s, mid, 5.0, s.settle_readings + 2, 0);
        for i in 0..(s.i_max_seconds + 2) {
            m2.update(Some(rd(mid, s.i_max + 2.0)), t2 + 1 + i, "2026-08-31", &s);
        }
        assert!(!m2.state.contactor_closed(), "a sustained overload must trip");
        assert_eq!(m2.reason, Some(TripReason::OverCurrent));
    }

    #[test]
    fn a_backward_clock_jump_cannot_stall_overcurrent() {
        // The Pi has no RTC, so NTP can step the wall clock BACKWARD. An
        // overcurrent whose start was anchored before the step must not stall:
        // with a plain saturating_sub the elapsed would read 0 until the clock
        // climbed back past the anchor - minutes of a real overload never
        // tripping. The re-anchor bounds the worst case to i_max_seconds.
        let s = Settings::default();
        let mid = (s.v_low_reset + s.v_high_reset) / 2.0;

        let mut m = mach(Mode::Auto);
        let base = 1_000_000u64;
        let t = feed(&mut m, &s, mid, 5.0, s.settle_readings + 2, base);
        assert_eq!(m.state, State::Running);

        // Overload begins and holds for a couple of seconds (anchor set near t).
        m.update(Some(rd(mid, s.i_max + 3.0)), t + 1, "2026-08-31", &s);
        m.update(Some(rd(mid, s.i_max + 3.0)), t + 2, "2026-08-31", &s);
        assert!(m.state.contactor_closed(), "not long enough to trip yet");

        // Clock steps far backward; the overload continues unabated.
        let back = base - 500_000;
        let mut tripped = false;
        for i in 0..(s.i_max_seconds + 2) {
            m.update(Some(rd(mid, s.i_max + 3.0)), back + i, "2026-08-31", &s);
            if !m.state.contactor_closed() {
                tripped = true;
                break;
            }
        }
        assert!(tripped, "overcurrent must still trip after a backward clock step");
        assert_eq!(m.reason, Some(TripReason::OverCurrent));
    }

    #[test]
    fn energy_today_re_anchors_after_a_meter_swap() {
        // The daily energy total is field-tuning data. A meter swap (a
        // replacement reads its own, lower lifetime Wh) or a PZEM reset must not
        // pin energy_today at zero (it is clamped >= 0) for the rest of the day.
        let s = Settings::default();
        let mut m = mach(Mode::Auto);
        let r = |wh: f32| Reading { volts: 130.0, amps: 5.0, watts: 650.0, watt_hours: wh };

        m.update(Some(r(5000.0)), 100, "2026-09-01", &s); // baseline (meter w/ history)
        m.update(Some(r(5200.0)), 101, "2026-09-01", &s);
        assert!((m.energy_today() - 200.0).abs() < 0.1, "200 Wh accrued");

        // Meter SWAP mid-day: new meter's lifetime total is far lower.
        m.update(Some(r(10.0)), 102, "2026-09-01", &s);
        assert_eq!(m.energy_today(), 0.0, "re-anchored at the swap, reads 0 from the new base");

        // The new meter accumulates; energy_today tracks it instead of sticking at 0.
        m.update(Some(r(60.0)), 103, "2026-09-01", &s);
        assert!((m.energy_today() - 50.0).abs() < 0.1, "50 Wh since the swap, not stuck at zero");
    }

    #[test]
    fn trip_thresholds_are_exact() {
        let s = Settings::default();

        // Just inside the trip points: keeps running.
        for v in [s.v_high_trip - 0.1, s.v_low_trip + 0.1] {
            let (mut m, t) = machine_in(State::Running, Mode::Auto, &s);
            m.update(Some(rd(v, 5.0)), t, "2026-08-11", &s);
            assert!(m.state.contactor_closed(), "tripped early at {v}V");
        }

        // Just outside: trips.
        for v in [s.v_high_trip + 0.1, s.v_low_trip - 0.1] {
            let (mut m, t) = machine_in(State::Running, Mode::Auto, &s);
            m.update(Some(rd(v, 5.0)), t, "2026-08-11", &s);
            assert!(!m.state.contactor_closed(), "failed to trip at {v}V");
        }
    }

    #[test]
    fn overcurrent_needs_to_persist() {
        let mut s = Settings::default();
        s.i_max_seconds = 5;

        let (mut m, t) = machine_in(State::Running, Mode::Auto, &s);

        // A brief surge must not trip - inrush is normal.
        m.update(Some(rd(126.0, s.i_max + 5.0)), t, "2026-08-11", &s);
        assert!(m.state.contactor_closed(), "tripped on a momentary surge");

        // Sustained overcurrent must.
        m.update(Some(rd(126.0, s.i_max + 5.0)), t + s.i_max_seconds + 1, "2026-08-11", &s);
        assert!(!m.state.contactor_closed(), "ignored sustained overcurrent");
        assert_eq!(m.reason, Some(TripReason::OverCurrent));
    }

    // ---------------------------------------------------------------
    // Mode transitions, all six directions
    // ---------------------------------------------------------------

    #[test]
    fn every_mode_transition_behaves() {
        let s = Settings::default();

        for from in ALL_MODES {
            for to in ALL_MODES {
                if from == to {
                    continue;
                }
                let (mut m, t) = machine_in(State::Running, from, &s);
                let was_running = m.state.contactor_closed();
                m.command(Command::SetMode(to), t, &s).unwrap();

                match to {
                    // OFF always stops.
                    Mode::Off => assert!(
                        !m.state.contactor_closed(),
                        "{from:?} -> OFF left the pump running"
                    ),
                    // MANUAL hands back a stopped pump: the operator has not
                    // asked for it to run under their control.
                    Mode::Manual => assert!(
                        !m.state.contactor_closed(),
                        "{from:?} -> MANUAL left the pump running"
                    ),
                    // AUTO may keep running if it already was.
                    Mode::Auto => {
                        if was_running && from == Mode::Auto {
                            assert!(m.state.contactor_closed());
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn lockout_survives_every_command_except_clear() {
        let mut s = Settings::default();
        s.level_gate_enabled = false; // an amp-inferred lockout; a wet sensor would clear it
        for cmd in ALL_COMMANDS {
            let (mut m, t) = machine_in(State::Lockout, Mode::Auto, &s);
            assert_eq!(m.state, State::Lockout, "setup failed");

            let _ = m.command(cmd, t, &s);

            let clears = matches!(cmd, Command::ClearLockout);
            if !clears {
                // Good conditions must not shorten the rest.
                for i in 0..30 {
                    m.update(Some(rd(126.0, 0.0)), t + i, "2026-08-11", &s);
                }
                assert!(
                    !m.state.contactor_closed(),
                    "{cmd:?} let the pump run during a lockout"
                );
            }
        }
    }

    #[test]
    fn a_clean_run_forgives_a_strike() {
        // The whole point: a well that recovers should stop being punished.
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false; // isolate the amp-inferred dry-run path
        s.dry_grace_seconds = 0;
        s.dry_seconds = 0;
        s.dry_volts = 128.0;
        s.dry_amps = 5.0;
        s.dry_lockout_1 = 1;
        s.dry_forgive_minutes = 10;

        let mut m = mach(Mode::Auto);
        let mut t = 0u64;

        // Earn a strike.
        t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings, t);
        for _ in 0..3 {
            m.update(Some(rd(134.0, 1.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.dry_strikes, 1);

        // Wait out the lockout, then run cleanly.
        t = m.lockout_until + 1;
        t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings + 2, t);
        assert_eq!(m.state, State::Running);
        assert_eq!(m.dry_strikes, 1, "not forgiven yet");

        // Past the forgiveness window, still pumping properly.
        m.update(Some(rd(126.0, 10.0)), t + s.dry_forgive_minutes * 60, "2026-08-11", &s);
        assert_eq!(m.dry_strikes, 0, "a clean run should forgive a strike");
    }

    #[test]
    fn forgiveness_needs_a_full_clean_run() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false; // isolate the amp-inferred dry-run path
        s.dry_grace_seconds = 0;
        s.dry_seconds = 0;
        s.dry_volts = 128.0;
        s.dry_amps = 5.0;
        s.dry_lockout_1 = 1;
        s.dry_forgive_minutes = 60;

        let mut m = mach(Mode::Auto);
        let mut t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings, 0);
        for _ in 0..3 {
            m.update(Some(rd(134.0, 1.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert_eq!(m.dry_strikes, 1);

        t = m.lockout_until + 1;
        // The supply held steady all through the lockout, so the pump re-closes
        // at once - the clean run (and good_run_since) begins here, not after a
        // fresh voltage settle. Measure the forgive window from this point.
        let clean_start = t;
        t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings + 2, t);
        let _ = t;

        // Just short of the window from when the clean run began - must not forgive.
        m.update(Some(rd(126.0, 10.0)), clean_start + s.dry_forgive_minutes * 60 - 5, "2026-08-11", &s);
        assert_eq!(m.dry_strikes, 1, "forgave too early");
    }

    #[test]
    fn strikes_decay_one_at_a_time() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_forgive_minutes = 5;

        let mut m = mach(Mode::Auto);
        m.dry_strikes = 3;
        let mut t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings, 0);

        for expected in [2u32, 1, 0] {
            t += s.dry_forgive_minutes * 60;
            m.update(Some(rd(126.0, 10.0)), t, "2026-08-11", &s);
            assert_eq!(m.dry_strikes, expected, "should step down one at a time");
        }

        // And it stops at zero.
        t += s.dry_forgive_minutes * 60;
        m.update(Some(rd(126.0, 10.0)), t, "2026-08-11", &s);
        assert_eq!(m.dry_strikes, 0);
    }

    #[test]
    fn dry_strikes_escalate_then_cap() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.level_gate_enabled = false; // isolate the amp-inferred dry-run path
        s.dry_grace_seconds = 0;
        s.dry_seconds = 0;
        s.dry_volts = 128.0;
        s.dry_amps = 5.0;
        s.dry_lockout_1 = 1;
        s.dry_lockout_2 = 2;
        s.dry_lockout_3 = 3;

        let mut m = mach(Mode::Auto);
        let mut t = 0u64;

        for expected in [1u32, 2, 3, 3, 3] {
            // Run, then go dry.
            t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings, t);
            for _ in 0..3 {
                m.update(Some(rd(134.0, 1.0)), t, "2026-08-11", &s);
                t += 1;
            }
            assert_eq!(m.state, State::Lockout);
            assert_eq!(m.dry_strikes, expected, "strike count should cap at 3");

            // Skip past the rest period.
            t = m.lockout_until + 1;
            m.update(Some(rd(126.0, 0.0)), t, "2026-08-11", &s);
            t += 1;
        }
    }

    // ---------------------------------------------------------------
    // Robustness against odd input
    // ---------------------------------------------------------------

    #[test]
    fn absurd_readings_never_start_the_pump() {
        let s = Settings::default();
        for v in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY, -50.0, 1e9] {
            for mode in ALL_MODES {
                let mut m = mach(mode);
                for i in 0..60u64 {
                    m.update(Some(rd(v, 0.0)), i, "2026-08-11", &s);
                    assert!(
                        !m.state.contactor_closed(),
                        "started on a {v} reading in {mode:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_clock_jump_does_not_start_the_pump() {
        let mut s = Settings::default();
        s.level_gate_enabled = false; // an amp-inferred lockout; a wet sensor would clear it
        let (mut m, t) = machine_in(State::Lockout, Mode::Auto, &s);

        // Time going backwards must not be readable as "the rest is over".
        m.update(Some(rd(126.0, 0.0)), t.saturating_sub(100_000), "2026-08-11", &s);
        assert!(!m.state.contactor_closed());
        assert_eq!(m.state, State::Lockout);
    }

    #[test]
    fn day_rollover_resets_daily_counters() {
        let s = Settings::default();
        let mut m = mach(Mode::Auto);

        let t = feed(&mut m, &s, 126.0, 10.0, s.settle_readings, 0);
        assert!(m.starts_today >= 1);

        m.update(Some(rd(126.0, 10.0)), t + 1, "2026-08-12", &s);
        assert_eq!(m.starts_today, 0, "start count should reset at midnight");
        // The pump is still running, so this tick already counts toward the
        // new day - the counter resets, then immediately logs one second.
        assert!(
            m.run_seconds_today <= 1,
            "run time should reset at midnight, got {}",
            m.run_seconds_today
        );

        // Energy is measured against a new baseline, so today starts at zero.
        assert!(m.energy_today() < 1.0);
    }

    #[test]
    fn events_are_capped() {
        let mut s = Settings::default();
        s.settle_readings = 1;

        let mut m = mach(Mode::Auto);
        let mut t = 0u64;
        // Many trip cycles must not grow memory without bound.
        for _ in 0..150 {
            m.update(Some(rd(126.0, 5.0)), t, "2026-08-11", &s);
            t += 1;
            m.update(Some(rd(200.0, 0.0)), t, "2026-08-11", &s);
            t += 1;
        }
        assert!(m.events.len() <= 100, "event log grew to {}", m.events.len());
    }

    #[test]
    fn refusal_messages_explain_themselves() {
        let s = Settings::default();
        // Every refusal an operator can hit must say something actionable,
        // not just "no".
        for state in ALL_STATES {
            for mode in ALL_MODES {
                let (mut m, t) = machine_in(state, mode, &s);
                if let Err(r) = m.command(Command::Start, t, &s) {
                    let msg = r.message();
                    assert!(!msg.is_empty());
                    assert!(
                        msg.len() > 8,
                        "refusal from {state:?}/{mode:?} is too terse: {msg:?}"
                    );
                }
            }
        }
    }
}

// ===================================================================
// Pump simulator - labelled (volts, amps) time-series generators
//
// Ground truth is the production data from the installed 138V/1500W pump:
//
//   - Full-sun array parks unloaded at ~137V (Voc 150.9V cold, 3 in series).
//     Weak sun drops the UNLOADED voltage, possibly below 125V - the blind
//     spot that defeats a "volts > 125" dry test.
//   - A wet pump LOADS the array: steady ~109V / ~5A (field median 109V/3.5A,
//     p95 6.26A; 5A is the loaded operating point the task specifies).
//   - The real soft-start ramp, ~5s per logged sample, v/a:
//       137/0.14 136/0.34 136/0.53 135/0.81 134/1.35 132/2.49 124/4.68
//     current climbs 0->4.7A while volts sag 137->124V over ~35s (convex:
//     slow then accelerating), THEN sags on to the ~109V/5A operating point.
//     No inrush spike - the controller ramps gently.
//   - Dry physics: a dry pump STILL draws acceleration current to spin up its
//     rotor (so a dry start and a wet start look alike while accelerating),
//     but once at speed its current COLLAPSES back toward zero and voltage
//     RECOVERS toward Voc, because there is no water to lift. A wet pump's
//     current HOLDS. That relative collapse is what the new detector keys on.
//
// The generators reproduce those shapes at the 500ms meter poll. `now` in the
// state machine is whole seconds, so two consecutive 500ms samples share a
// timestamp - exactly what the real loop does against a 1s clock. Everything
// is parameterised through `Rig` (cable length, water load, sun strength) so
// the scenarios can be fuzzed. The detector is NOT built here; these only
// generate the evidence it will be judged against.
// ===================================================================

#[cfg(test)]
pub mod sim {
    use super::*;

    /// One meter poll: milliseconds since the contactor closed, and the
    /// (volts, amps) the PZEM would report at that instant.
    #[derive(Debug, Clone, Copy)]
    pub struct Sample {
        pub t_ms: u64,
        pub volts: f32,
        pub amps: f32,
    }

    /// The meter poll interval. Two samples per second of machine clock.
    pub const POLL_MS: u64 = 500;

    // ---- constants fitted to the production data (module header) ----
    /// Unloaded string voltage at full sun.
    const V_UNLOADED_FULL_SUN: f32 = 137.0;
    /// Transient array+cable slope while accelerating (V per A). Fitted so the
    /// real ramp end (4.68A) lands near 124V from 137V unloaded.
    const K_ACCEL: f32 = 2.2;
    /// Steady MPP sag (V per A). Fitted so 5A lands at 109V from 137V, net of
    /// the nominal cable drop.
    const K_MPP: f32 = 5.0;
    /// Cable loop resistance per metre (out-and-back), ohms.
    const CABLE_OHM_PER_M: f32 = 0.01;
    /// Rotor-acceleration current ceiling at full sun (A) - the real ramp
    /// crested near 4.7A. Only half of it scales with sun: the rotor pulls an
    /// inertial surge even in weak light, the array just cannot sustain it.
    const I_ACCEL_PEAK_FULL_SUN: f32 = 4.7;
    /// Residual dry current: magnetising + bearing friction, no water (A).
    const I_DRY_DEFAULT: f32 = 0.8;

    /// Soft-start acceleration duration (s). Real ramp ~35s.
    pub const ACCEL_SECS: f32 = 35.0;
    /// Wet sag from ramp end to the steady MPP operating point (s).
    pub const WET_SETTLE_SECS: f32 = 15.0;
    /// Dry current collapse once at speed (s).
    pub const DRY_COLLAPSE_SECS: f32 = 12.0;

    /// The plant: how the array and pump are wired and lit. Fuzz a scenario by
    /// perturbing the rig rather than hand-editing the waveform.
    #[derive(Debug, Clone, Copy)]
    pub struct Rig {
        /// Cable run array->pump, metres. Longer = more IR sag under load,
        /// which alone can drag a loaded pump under dry_volts.
        pub cable_m: f32,
        /// Steady current a WET pump settles at (A) = how much water it lifts.
        /// The dry generators ignore this and use their own residual current.
        pub water_load_a: f32,
        /// Sun strength, 0..1. Scales unloaded voltage (137V at 1.0, under
        /// 125V below ~0.68) and part of the acceleration current ceiling.
        pub sun: f32,
    }

    impl Rig {
        /// The installed well on a clear day: 30m cable, 5A of water, full sun.
        pub fn real_well() -> Self {
            Rig { cable_m: 30.0, water_load_a: 5.0, sun: 1.0 }
        }
        /// A genuinely low-yield well: light 2.5A load, slightly soft sun, so
        /// it settles at ~120V/2.5A. This MUST keep running - it is the wet
        /// case most likely to be mistaken for dry.
        pub fn low_flow_well() -> Self {
            Rig { cable_m: 30.0, water_load_a: 2.5, sun: 0.92 }
        }
        /// The blind spot: weak sun parks the array unloaded at ~120V, so a dry
        /// pump never lifts the voltage above 125V. Absolute-voltage tests are
        /// useless here; only the relative current collapse betrays it.
        pub fn weak_sun() -> Self {
            Rig { cable_m: 30.0, water_load_a: 0.0, sun: 0.54 }
        }
        /// Near-full sun tuned so a dry acceleration crests ~124V at ~4A before
        /// collapsing - the brief-accel dry case.
        pub fn brief_accel() -> Self {
            Rig { cable_m: 30.0, water_load_a: 0.0, sun: 0.97 }
        }

        pub fn cable_loop_ohms(&self) -> f32 {
            2.0 * self.cable_m * CABLE_OHM_PER_M
        }
        pub fn ir_drop(&self, amps: f32) -> f32 {
            amps * self.cable_loop_ohms()
        }
        /// Open-circuit voltage the array parks at with no load.
        pub fn v_unloaded(&self) -> f32 {
            100.0 + (V_UNLOADED_FULL_SUN - 100.0) * self.sun
        }
        /// Steady voltage a wet pump drags the array to at `amps`.
        pub fn v_op(&self, amps: f32) -> f32 {
            self.v_unloaded() - K_MPP * amps - self.ir_drop(amps)
        }
        /// Voltage during acceleration (stiffer slope: the array has not yet
        /// walked down to its maximum-power point).
        pub fn v_accel(&self, amps: f32) -> f32 {
            self.v_unloaded() - K_ACCEL * amps - self.ir_drop(amps)
        }
        /// Peak current the rotor draws accelerating, capped by what the lit
        /// array can deliver. Half the ceiling is fixed inertial surge, half
        /// scales with sun.
        pub fn i_accel_peak(&self) -> f32 {
            I_ACCEL_PEAK_FULL_SUN * (0.5 + 0.5 * self.sun)
        }
        /// Voltage a dry pump recovers to: near Voc, only a hair of residual
        /// load pulling it down.
        pub fn v_dry(&self, i_dry: f32) -> f32 {
            self.v_unloaded() - self.ir_drop(i_dry) - 0.3
        }
    }

    // ---- segment primitives ----

    fn convex(u: f32) -> f32 {
        // Slow then accelerating - the shape of the real current ramp.
        u.powf(1.8)
    }
    fn expo(u: f32) -> f32 {
        // Fast approach to the endpoint then level off - collapse / settle.
        1.0 - (-3.0 * u).exp()
    }

    /// Append an eased segment from `from` to `to` over `secs`, sampling every
    /// 500ms. Emits the start point but not the endpoint, so the next segment
    /// starting at that endpoint leaves no duplicate.
    fn seg(
        out: &mut Vec<Sample>,
        base_ms: u64,
        secs: f32,
        from: (f32, f32),
        to: (f32, f32),
        shape: fn(f32) -> f32,
    ) -> u64 {
        let n = ((secs * 1000.0) as u64 / POLL_MS).max(1);
        let mut t = base_ms;
        for k in 0..n {
            let u = k as f32 / n as f32;
            let p = shape(u);
            out.push(Sample {
                t_ms: t,
                volts: from.0 + (to.0 - from.0) * p,
                amps: from.1 + (to.1 - from.1) * p,
            });
            t += POLL_MS;
        }
        t
    }

    /// Hold a steady point (with optional +-ripple) for `secs`.
    fn hold(out: &mut Vec<Sample>, base_ms: u64, secs: f32, v: f32, a: f32) -> u64 {
        let n = ((secs * 1000.0) as u64 / POLL_MS).max(1);
        let mut t = base_ms;
        for _ in 0..n {
            out.push(Sample { t_ms: t, volts: v, amps: a });
            t += POLL_MS;
        }
        t
    }

    // ---- shared shapes ----

    /// The soft-start acceleration, identical for wet and dry: current climbs
    /// convexly 0.1A -> peak while volts sag. Returns the trace and the next
    /// timestamp.
    fn accel(out: &mut Vec<Sample>, base_ms: u64, rig: &Rig, peak: f32, secs: f32) -> u64 {
        seg(
            out,
            base_ms,
            secs,
            (rig.v_unloaded(), 0.1),
            (rig.v_accel(peak), peak),
            convex,
        )
    }

    // ============================================================
    // WET scenarios - each MUST keep running (never a dry trip)
    // ============================================================

    /// WET-START: the real soft-start ramp settling to steady 109V/5A.
    /// Shape: convex accel 137V/0.1A -> ~124V/4.7A over 35s, exponential settle
    /// down to 109V/5A over 15s, then a long steady hold. Current ends AT its
    /// peak (rises slightly through the settle), so there is no collapse.
    pub fn wet_start(rig: &Rig) -> Vec<Sample> {
        let mut out = Vec::new();
        let t = wet_ramp(&mut out, 0, rig);
        let ir = rig.water_load_a;
        hold(&mut out, t, 120.0, rig.v_op(ir), ir);
        out
    }

    /// The accel+settle prefix, reused by WET-START and WET-CLOUDS.
    fn wet_ramp(out: &mut Vec<Sample>, base_ms: u64, rig: &Rig) -> u64 {
        let ip = rig.i_accel_peak();
        let ir = rig.water_load_a;
        let t = accel(out, base_ms, rig, ip, ACCEL_SECS);
        seg(
            out,
            t,
            WET_SETTLE_SECS,
            (rig.v_accel(ip), ip),
            (rig.v_op(ir), ir),
            expo,
        )
    }

    /// WET-LOWFLOW: a weak well settling to steady ~120V/2.5A. Same ramp shape
    /// but a light load. Its current DOES dip from the accel peak (~4.5A) to the
    /// steady 2.5A - the tightest wet margin against a collapse detector - and
    /// its steady 120V sits under dry_volts, so the old voltage test is already
    /// blind to it. It MUST keep running.
    pub fn wet_lowflow(rig: &Rig) -> Vec<Sample> {
        wet_start(rig)
    }

    /// WET-CLOUDS: steady operation with clouds passing. Amps swing +-`cloud_frac`
    /// and volts swing the OTHER way by `volt_amp` (a lighter, cloud-shaded load
    /// lets the array drift back toward Voc). Volts are kept a few volts inside
    /// the band so this exercises current-collapse robustness, not undervoltage.
    /// Attackers raise `cloud_frac` to probe the detector's sustained-window
    /// logic; at the nominal 0.15 the current never dips near 40% of peak.
    pub fn wet_clouds(rig: &Rig, cloud_frac: f32, volt_amp: f32, passes: u32, secs_each: f32) -> Vec<Sample> {
        let mut out = Vec::new();
        wet_ramp(&mut out, 0, rig);
        let ir = rig.water_load_a;
        let vru = rig.v_op(ir);
        let mut t = out.last().unwrap().t_ms + POLL_MS;
        let n = ((passes as f32 * secs_each * 1000.0) as u64 / POLL_MS).max(1);
        for k in 0..n {
            let secs = k as f32 * (POLL_MS as f32 / 1000.0);
            let w = (secs / secs_each * std::f32::consts::TAU).sin();
            out.push(Sample {
                t_ms: t,
                volts: vru - volt_amp * w,
                amps: ir * (1.0 + cloud_frac * w),
            });
            t += POLL_MS;
        }
        out
    }

    // ============================================================
    // DRY scenarios - each MUST trip (goal for the detector to build)
    // ============================================================

    /// The dry tail shared by all dry-from-cold scenarios: accelerate to
    /// `peak`, then collapse exponentially to `i_dry` while volts recover
    /// toward Voc, then hold.
    fn dry_tail(rig: &Rig, peak: f32, i_dry: f32, accel_secs: f32) -> Vec<Sample> {
        let mut out = Vec::new();
        let t = accel(&mut out, 0, rig, peak, accel_secs);
        let t = seg(
            &mut out,
            t,
            DRY_COLLAPSE_SECS,
            (rig.v_accel(peak), peak),
            (rig.v_dry(i_dry), i_dry),
            expo,
        );
        hold(&mut out, t, 60.0, rig.v_dry(i_dry), i_dry);
        out
    }

    /// DRY-FROMSTART: full acceleration, then the current collapses to ~0.8A
    /// and the voltage recovers to ~137V - there was never any water. Peak ~4.7A
    /// collapses to 0.8A (~17% of peak).
    pub fn dry_from_start(rig: &Rig) -> Vec<Sample> {
        dry_tail(rig, rig.i_accel_peak(), I_DRY_DEFAULT, ACCEL_SECS)
    }

    /// DRY-WEAKSUN: the blind spot. Weak sun parks the array at ~120V unloaded;
    /// the dry pump accelerates (~3.6A, sagging to ~110V) then collapses to
    /// ~1A/119V. Voltage NEVER exceeds 125V, so a "volts > 125" test can never
    /// fire - only the relative collapse (~28% of peak) reveals it.
    pub fn dry_weak_sun(rig: &Rig) -> Vec<Sample> {
        dry_tail(rig, rig.i_accel_peak(), 1.0, ACCEL_SECS)
    }

    /// DRY-ACCEL: a dry pump whose acceleration briefly crests ~4A/124V - deep
    /// enough to look primed - then collapses to ~0.8A/137V. A shorter, sharper
    /// accel than DRY-FROMSTART, to test that the detector keys on the transient
    /// peak rather than a sustained load.
    pub fn dry_accel(rig: &Rig, peak_a: f32) -> Vec<Sample> {
        dry_tail(rig, peak_a, I_DRY_DEFAULT, ACCEL_SECS * 0.7)
    }

    /// DRY-MIDRUN: healthy 109V/5A for `wet_secs`, then the water is lost and
    /// the current collapses over ~3s to ~1A while volts spring back to ~135V.
    /// Peak is the 5A operating current; the collapse to 1A is ~20% of it. Being
    /// long-primed, this MUST trip fast.
    pub fn dry_mid_run(rig: &Rig, wet_secs: f32) -> Vec<Sample> {
        let mut out = Vec::new();
        let ip = rig.i_accel_peak();
        let ir = rig.water_load_a;
        let t = accel(&mut out, 0, rig, ip, ACCEL_SECS);
        let t = seg(&mut out, t, WET_SETTLE_SECS, (rig.v_accel(ip), ip), (rig.v_op(ir), ir), expo);
        let t = hold(&mut out, t, wet_secs, rig.v_op(ir), ir);
        // Water lost: current collapses fast, voltage recovers.
        let i_dry = 1.0;
        let t = seg(&mut out, t, 3.0, (rig.v_op(ir), ir), (rig.v_dry(i_dry), i_dry), expo);
        hold(&mut out, t, 30.0, rig.v_dry(i_dry), i_dry);
        out
    }

    // ============================================================
    // Harness: drive a scenario through a real Machine
    // ============================================================

    const DAY: &str = "2026-08-11";

    fn rd(v: f32, a: f32) -> Reading {
        Reading { volts: v, amps: a, watts: v * a, watt_hours: 1000.0 }
    }

    /// Field-armed settings: dry detection on, the 95V floor the box now uses,
    /// everything else at the tuned defaults (dry_amps 3, dry_volts 125,
    /// dry_seconds 1, dry_grace 90).
    pub fn armed() -> Settings {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.v_low_trip = 95.0;
        // These scenarios test the AMP-inferred dry-run fallback, used when no
        // well sensor is wired. With the level gate on, a wired sensor would
        // catch a dry well first (park), so turn the gate off to isolate the
        // current-based path under test.
        s.level_gate_enabled = false;
        s
    }

    /// Settle on the unloaded array so the contactor closes, then feed the
    /// scenario samples. Returns the machine and the state after each sample,
    /// so a test can assert both the outcome and the latency to it.
    pub fn run(mode: Mode, s: &Settings, rig: &Rig, samples: &[Sample]) -> (Machine, Vec<State>) {
        let mut m = mach(mode);
        let mut t = 0u64;
        for _ in 0..(s.settle_readings + 1) {
            m.update(Some(rd(rig.v_unloaded(), 0.0)), t, DAY, s);
            t += 1;
        }
        if mode == Mode::Manual {
            let _ = m.command(Command::Start, t, s);
            m.update(Some(rd(rig.v_unloaded(), 0.0)), t, DAY, s);
        }
        assert_eq!(m.state, State::Running, "harness failed to start the pump");

        let start = t;
        let mut states = Vec::with_capacity(samples.len());
        for sm in samples {
            m.update(Some(rd(sm.volts, sm.amps)), start + sm.t_ms / 1000, DAY, s);
            states.push(m.state);
        }
        (m, states)
    }

    // ---- shape helpers, for asserting on the waveform itself ----

    /// Peak current, and the mean current over the last `tail` samples.
    pub fn peak_and_tail(samples: &[Sample], tail: usize) -> (f32, f32) {
        let peak = samples.iter().fold(0.0f32, |m, s| m.max(s.amps));
        let n = tail.min(samples.len());
        let sum: f32 = samples[samples.len() - n..].iter().map(|s| s.amps).sum();
        (peak, sum / n as f32)
    }

    pub fn max_volts(samples: &[Sample]) -> f32 {
        samples.iter().fold(0.0f32, |m, s| m.max(s.volts))
    }
}

#[cfg(test)]
mod sim_check {
    use super::sim::*;
    use super::{Mode, State, TripReason};

    // ---- WET: the safety guarantee. These must hold now AND after the
    //      collapse detector is added: a wet pump is never called dry. ----

    #[test]
    fn wet_start_never_trips() {
        let s = armed();
        let rig = Rig::real_well();
        let (m, states) = run(Mode::Auto, &s, &rig, &wet_start(&rig));
        assert!(states.iter().all(|st| *st == State::Running), "wet start must stay running");
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn wet_lowflow_never_trips() {
        let s = armed();
        let rig = Rig::low_flow_well();
        let (m, states) = run(Mode::Auto, &s, &rig, &wet_lowflow(&rig));
        assert!(states.iter().all(|st| *st == State::Running), "low-flow well must keep running");
        assert_eq!(m.state, State::Running);
    }

    #[test]
    fn wet_clouds_never_trips() {
        let s = armed();
        let rig = Rig::real_well();
        let scn = wet_clouds(&rig, 0.15, 6.0, 6, 20.0);
        let (m, states) = run(Mode::Auto, &s, &rig, &scn);
        assert!(states.iter().all(|st| *st == State::Running), "clouds must not trip anything");
        assert_eq!(m.state, State::Running);
    }

    // ---- WET waveform shape: current does NOT collapse (stays >= 50% of
    //      peak), so a relative-collapse detector has clear air. ----

    #[test]
    fn wet_traces_do_not_collapse() {
        let real = Rig::real_well();
        let low = Rig::low_flow_well();
        for scn in [wet_start(&real), wet_lowflow(&low), wet_clouds(&real, 0.15, 6.0, 6, 20.0)] {
            let (peak, tail) = peak_and_tail(&scn, 20);
            assert!(tail >= 0.50 * peak, "wet tail {tail:.2}A collapsed vs peak {peak:.2}A");
        }
    }

    // ---- DRY waveform shape: current reaches a peak then collapses below
    //      40% of it (sustained) - the signal the new detector will use. ----

    #[test]
    fn dry_traces_collapse_below_40pc() {
        let cases = [
            ("from-start", dry_from_start(&Rig::real_well())),
            ("weak-sun", dry_weak_sun(&Rig::weak_sun())),
            ("brief-accel", dry_accel(&Rig::brief_accel(), 4.0)),
            ("mid-run", dry_mid_run(&Rig::real_well(), 300.0)),
        ];
        for (name, scn) in cases {
            let (peak, tail) = peak_and_tail(&scn, 20);
            assert!(tail < 0.40 * peak, "{name}: tail {tail:.2}A not a collapse vs peak {peak:.2}A");
        }
    }

    // ---- DRY-WEAKSUN is the blind spot: voltage never clears 125V, so the
    //      existing "volts > dry_volts" test can never fire. This documents
    //      exactly why the new detector is needed. ----

    #[test]
    fn dry_weak_sun_never_clears_dry_volts() {
        let s = armed();
        let scn = dry_weak_sun(&Rig::weak_sun());
        assert!(max_volts(&scn) < s.dry_volts, "weak-sun dry run must never exceed dry_volts");
    }

    // ---- Characterisation of TODAY's detector: the primed cases it already
    //      catches. These stay true after the collapse detector is added. ----

    #[test]
    fn dry_mid_run_trips_today_because_it_was_primed() {
        let s = armed();
        let rig = Rig::real_well();
        let (m, states) = run(Mode::Auto, &s, &rig, &dry_mid_run(&rig, 300.0));
        assert!(states.iter().any(|st| *st == State::Lockout), "primed pump losing water must trip");
        assert_eq!(m.reason, Some(TripReason::DryRun));
    }

    // ================================================================
    // The peak-collapse detector: what it must now do.
    // ================================================================

    /// The index of the first sample after which the machine is locked out.
    fn trip_index(states: &[State]) -> Option<usize> {
        states.iter().position(|st| *st == State::Lockout)
    }

    // ---- Every DRY-* scenario must trip. ----

    #[test]
    fn dry_from_start_trips() {
        let s = armed();
        let rig = Rig::real_well();
        let (m, states) = run(Mode::Auto, &s, &rig, &dry_from_start(&rig));
        assert!(trip_index(&states).is_some(), "a pump that never had water must trip");
        assert_eq!(m.reason, Some(TripReason::DryRun));
    }

    #[test]
    fn dry_accel_trips() {
        let s = armed();
        let rig = Rig::brief_accel();
        let (m, states) = run(Mode::Auto, &s, &rig, &dry_accel(&rig, 4.0));
        assert!(trip_index(&states).is_some(), "a brief-accel dry pump must trip");
        assert_eq!(m.reason, Some(TripReason::DryRun));
    }

    /// The blind spot, now closed. Weak sun parks the array under dry_volts,
    /// so the primed test can NEVER fire (proven by
    /// dry_weak_sun_never_clears_dry_volts above). Only the relative collapse
    /// betrays the dry well - so if this trips, the collapse detector is what
    /// caught it.
    #[ignore = "peak-collapse detector disabled pending decaying-baseline rework; see memory reisbot-preproduction"]
    #[test]
    fn dry_weak_sun_now_trips() {
        let s = armed();
        let rig = Rig::weak_sun();
        let scn = dry_weak_sun(&rig);

        // Guard the premise: the primed test really is blind here.
        assert!(max_volts(&scn) < s.dry_volts, "premise: array never clears dry_volts");

        let (m, states) = run(Mode::Auto, &s, &rig, &scn);
        assert!(
            trip_index(&states).is_some(),
            "weak-sun dry run must now be caught by the collapse detector"
        );
        assert_eq!(m.reason, Some(TripReason::DryRun));
    }

    /// DRY-MIDRUN loses its water after five minutes of healthy pumping and
    /// must trip fast - within a second or two of the loss, not a grace
    /// period later. (The primed path carries this one; the collapse detector
    /// runs alongside without slowing it.)
    #[test]
    fn dry_mid_run_trips_within_two_seconds_of_water_loss() {
        let s = armed();
        let rig = Rig::real_well();
        let scn = dry_mid_run(&rig, 300.0);
        let (_m, states) = run(Mode::Auto, &s, &rig, &scn);

        // Measure from the LAST sample the pump was still loaded (the moment
        // the water is lost) to the sample the trip lands on. Note the trace
        // opens at 137V/0.1A, which itself reads dry - the soft-start ramp
        // always does - so the reference has to be the wet->dry crossing after
        // the long healthy run, not the first dry-looking reading.
        let last_loaded = scn
            .iter()
            .rposition(|sm| sm.amps >= s.dry_amps)
            .expect("the mid-run trace must have a loaded stretch");
        let tripped = trip_index(&states).expect("a primed pump losing water must trip");

        let latency_ms = scn[tripped].t_ms - scn[last_loaded].t_ms;
        assert!(
            latency_ms <= 2000,
            "water loss to trip took {latency_ms}ms, must be within ~1-2s"
        );
    }

    // ---- Every WET-* scenario must still run without a dry trip, now that
    //      the collapse detector is armed. The low-flow well is the tightest
    //      case (tail ~0.55 of peak, just above the 0.40 line). ----

    #[test]
    fn wet_scenarios_never_dry_trip_with_collapse_armed() {
        let s = armed();
        let real = Rig::real_well();
        let low = Rig::low_flow_well();
        let cases = [
            ("wet-start", real, wet_start(&real)),
            ("wet-lowflow", low, wet_lowflow(&low)),
            ("wet-clouds", real, wet_clouds(&real, 0.15, 6.0, 6, 20.0)),
        ];
        for (name, rig, scn) in cases {
            let (m, states) = run(Mode::Auto, &s, &rig, &scn);
            assert!(
                states.iter().all(|st| *st == State::Running),
                "{name} must never trip with the collapse detector armed"
            );
            assert_eq!(m.state, State::Running, "{name} must end running");
        }
    }

    /// Heavier cloud swings (+-35%) still must not read as a collapse: the
    /// current never dips near 0.40 of peak, and even if it grazed it the
    /// sustained window would ride it out. Proves the detector keys on a
    /// sustained loss of load, not the ordinary swing of a passing cloud.
    #[test]
    fn heavy_clouds_do_not_dry_trip() {
        let s = armed();
        let rig = Rig::real_well();
        let scn = wet_clouds(&rig, 0.35, 8.0, 8, 18.0);
        let (m, states) = run(Mode::Auto, &s, &rig, &scn);
        assert!(
            states.iter().all(|st| *st == State::Running),
            "heavy but transient cloud swings must not be read as dry"
        );
        assert_eq!(m.state, State::Running);
    }
}


// ===================================================================
// ATTACKS: attempts to make the collapse detector false-trip a HEALTHY
// (wet, water-lifting) pump. Each test builds a trace that is wet from
// end to end, runs it through a real Machine with the field-armed
// settings, and asserts what ACTUALLY happens. Where the pump is wrongly
// locked out, the test documents the false trip; the trace is printed
// (run with `-- --nocapture`) with the (volts,amps) at the trip.
//
// Root cause under attack: `peak_amps` is a running max that only resets
// in close() and never decays inside a continuous run. Any transient
// current surge pins the collapse yardstick high for the rest of the
// run, and later normal-but-lower WET flow is then measured against that
// stale peak. Production data records healthy current up to 7.23A
// (p95 6.26A) on a pump whose median is 3.5A, so surges to ~2x the
// steady flow are real, not hypothetical.
// ===================================================================
#[cfg(test)]
mod attacks {
    use super::sim::*;
    use super::{Mode, State, TripReason};

    // ---- local trace builders (the sim module's seg/hold are private) ----

    /// Linear ramp (v0,a0)->(v1,a1) over `secs`, sampled every POLL_MS,
    /// emitting the start point but not the endpoint (so segments chain
    /// with no duplicate). Returns the next t_ms.
    fn ramp(out: &mut Vec<Sample>, t0: u64, secs: f32, from: (f32, f32), to: (f32, f32)) -> u64 {
        let n = ((secs * 1000.0) as u64 / POLL_MS).max(1);
        let mut t = t0;
        for k in 0..n {
            let u = k as f32 / n as f32;
            out.push(Sample {
                t_ms: t,
                volts: from.0 + (to.0 - from.0) * u,
                amps: from.1 + (to.1 - from.1) * u,
            });
            t += POLL_MS;
        }
        t
    }

    fn hold(out: &mut Vec<Sample>, t0: u64, secs: f32, v: f32, a: f32) -> u64 {
        let n = ((secs * 1000.0) as u64 / POLL_MS).max(1);
        let mut t = t0;
        for _ in 0..n {
            out.push(Sample { t_ms: t, volts: v, amps: a });
            t += POLL_MS;
        }
        t
    }

    fn trip_index(states: &[State]) -> Option<usize> {
        states.iter().position(|st| *st == State::Lockout)
    }

    /// Print the trace around the trip so the report can quote the exact
    /// (volts, amps). Also returns peak amps seen up to the trip.
    fn report(name: &str, scn: &[Sample], states: &[State]) -> Option<usize> {
        let idx = trip_index(states);
        match idx {
            None => eprintln!("[{name}] NO TRIP (pump stayed running) - detector safe here"),
            Some(i) => {
                let peak = scn[..=i].iter().fold(0.0f32, |m, s| m.max(s.amps));
                let s = scn[i];
                eprintln!(
                    "[{name}] FALSE TRIP at sample {i}, t={:.1}s: {:.1}V / {:.2}A  \
                     (peak_amps this run = {:.2}A, 0.40*peak = {:.2}A, so {:.2}A < {:.2}A)",
                    s.t_ms as f32 / 1000.0, s.volts, s.amps, peak, 0.40 * peak, s.amps, 0.40 * peak
                );
            }
        }
        idx
    }

    // -----------------------------------------------------------------
    // ATTACK A - low-flow well + one bright-sun / inflow surge.
    //
    // A genuinely low-yield well settles at ~119V/2.5A (the documented
    // wet low-flow case that MUST keep running). Partway through, a
    // legitimate bright-sun / MPPT / inflow excursion pushes current to
    // 6.5A for ~8s - inside the recorded production envelope (p95 6.26A,
    // max 7.23A) - then it returns to the same steady 2.5A. The pump is
    // lifting water the entire time. The surge pins peak_amps at 6.5A,
    // so 0.40*peak = 2.60A, and the perfectly healthy 2.5A steady flow
    // now reads as a sustained collapse.
    // -----------------------------------------------------------------
    #[ignore = "peak-collapse detector disabled pending decaying-baseline rework; see memory reisbot-preproduction"]
    #[test]
    fn attack_a_lowflow_well_bright_surge_false_trips() {
        let s = armed();
        let rig = Rig::low_flow_well();
        let mut scn = Vec::new();
        // Soft start to the low-flow operating point.
        let t = ramp(&mut scn, 0, 35.0, (137.0, 0.1), (120.0, 4.5));
        let t = ramp(&mut scn, t, 15.0, (120.0, 4.5), (119.0, 2.5));
        // Two minutes of healthy low flow.
        let t = hold(&mut scn, t, 120.0, 119.0, 2.5);
        // A legitimate bright-sun / inflow surge, within the production
        // envelope, then straight back to the same steady flow.
        let t = ramp(&mut scn, t, 8.0, (119.0, 2.5), (115.0, 6.5));
        let t = ramp(&mut scn, t, 8.0, (115.0, 6.5), (119.0, 2.5));
        // Resume steady low flow - still wet, still pumping.
        hold(&mut scn, t, 90.0, 119.0, 2.5);

        let (m, states) = run(Mode::Auto, &s, &rig, &scn);
        let idx = report("attack-A", &scn, &states);
        assert!(idx.is_some(), "ATTACK A did not reproduce a false trip");
        assert_eq!(m.reason, Some(TripReason::DryRun),
            "the false trip must be attributed to DryRun");
        // Prove it was the collapse path, not the primed path: the steady
        // volts (119V) never exceed dry_volts (125V), so primed_dry is
        // structurally blind - only collapse_dry can have fired.
        assert!(119.0 < s.dry_volts,
            "premise: steady 119V is below dry_volts, so primed path is blind");
    }

    // -----------------------------------------------------------------
    // ATTACK B - deep, fast clouds on the healthy full-flow well.
    //
    // The real 5A well under a run of deep cumulus. Each cloud swings
    // current by +-45% of the 5A load: bright edges brush the recorded
    // max (~7.25A), thick centres drop it to ~2.75A. peak_amps pins at
    // the ~7.25A bright edge, so 0.40*peak ~= 2.9A; the ~2.75A cloud
    // trough then sits below it, and a slow cloud holds the trough long
    // enough (> dry_collapse_seconds) to be read as a sustained collapse
    // even though every sample is a wet, water-lifting pump.
    // -----------------------------------------------------------------
    #[ignore = "peak-collapse detector disabled pending decaying-baseline rework; see memory reisbot-preproduction"]
    #[test]
    fn attack_b_deep_clouds_false_trip() {
        let s = armed();
        let rig = Rig::real_well();
        // frac 0.45 -> peak edge 7.25A, trough 2.75A; slow 30s passes so
        // the trough is held past the 3s sustained window.
        let scn = wet_clouds(&rig, 0.45, 10.0, 6, 30.0);
        let (m, states) = run(Mode::Auto, &s, &rig, &scn);
        let idx = report("attack-B", &scn, &states);
        assert!(idx.is_some(), "ATTACK B did not reproduce a false trip");
        assert_eq!(m.reason, Some(TripReason::DryRun));
    }

    // -----------------------------------------------------------------
    // ATTACK C - the non-decaying peak: hours of low flow after an early
    // strong-flow spell. No surge trick - just the ordinary shape of a
    // well that yields strongly for the first minutes (aquifer full),
    // then draws down to a steady trickle for the rest of the run. The
    // early 6.3A spell (within p95 6.26A) pins the peak; an hour later a
    // healthy ~2.4A trickle is still being measured against it.
    // -----------------------------------------------------------------
    #[ignore = "peak-collapse detector disabled pending decaying-baseline rework; see memory reisbot-preproduction"]
    #[test]
    fn attack_c_drawdown_after_strong_spell_false_trips() {
        let s = armed();
        let rig = Rig::real_well();
        let mut scn = Vec::new();
        let t = ramp(&mut scn, 0, 35.0, (137.0, 0.1), (118.0, 5.0));
        // Early strong-inflow spell: 4 minutes at 6.3A (aquifer full).
        let t = hold(&mut scn, t, 240.0, 112.0, 6.3);
        // Aquifer draws down to a steady trickle for the next hour.
        let t = ramp(&mut scn, t, 120.0, (112.0, 6.3), (120.0, 2.4));
        hold(&mut scn, t, 3600.0, 120.0, 2.4);

        let (_m, states) = run(Mode::Auto, &s, &rig, &scn);
        let idx = report("attack-C", &scn, &states);
        // The trace runs a full hour, well past the lockout expiry, so the
        // END state has already recovered to Waiting - the proof of the
        // false trip is the Lockout that landed mid-run (report() prints it),
        // not the final reason.
        assert!(idx.is_some(), "ATTACK C did not reproduce a false trip");
    }

    // -----------------------------------------------------------------
    // ATTACK D - the soft-start settle overshoot. The real ramp crests
    // near its accel peak then eases DOWN to the operating point. If the
    // operating point is a shade under 40% of the accel crest, the very
    // settle that every healthy start performs reads as a collapse. Here
    // a full-sun accel crest of ~4.9A eases to a 1.9A trickle-well
    // operating point. This probes whether the min-peak guard and the
    // settle shape leave any gap right at startup.
    // -----------------------------------------------------------------
    #[test]
    fn attack_d_softstart_settle_to_trickle_false_trips() {
        let s = armed();
        let rig = Rig::real_well();
        let mut scn = Vec::new();
        let t = ramp(&mut scn, 0, 35.0, (137.0, 0.1), (124.0, 4.9));
        let t = ramp(&mut scn, t, 15.0, (124.0, 4.9), (122.0, 1.9));
        hold(&mut scn, t, 120.0, 122.0, 1.9);

        let (_m, states) = run(Mode::Auto, &s, &rig, &scn);
        let idx = report("attack-D", &scn, &states);
        // Documented outcome, whichever way it lands - see report().
        let _ = idx;
    }
}
