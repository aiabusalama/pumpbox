//! Operator-adjustable settings, persisted as JSON.
//!
//! Validation runs before anything is written, so a bad value from the
//! dashboard is rejected with an explanation rather than saved and acted on.
//! A corrupt file falls back to defaults - the pump keeps its protection
//! even if the disk is damaged.

use serde::{Deserialize, Serialize};
use std::path::Path;

/// Which set of thresholds is in force.
///
/// The bench profile lets the whole system be exercised from a small DC
/// supply. The pump profile carries the real numbers. Keeping both in the
/// binary means commissioning is a single flag, not eight values retyped
/// correctly in a shed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Profile {
    /// Low voltage bench supply, 10-17V working range.
    Bench,
    /// The real array: 3x GSE-HC455 in series feeding a 138V 1500W pump.
    Pump,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Profile::Bench => "BENCH",
            Profile::Pump => "PUMP",
        }
    }

    /// Thresholds for this profile. Everything voltage- or current-related
    /// comes from here; timing and dry-run tuning carry across.
    pub fn thresholds(self) -> Thresholds {
        match self {
            Profile::Bench => Thresholds {
                v_high_trip: 18.0,
                v_low_trip: 9.0,
                v_high_reset: 17.0,
                v_low_reset: 10.0,
                i_max: 5.0,
                dry_amps: 0.5,
                dry_volts: 16.0,
            },
            // The pump's controller card is destroyed above 150V, and the
            // array reaches 150.9V open-circuit at 25C - higher when cold.
            // It self-protects below 110V, so we stop just above that.
            // Straight off the pump's own listing: "Ürün 110V-150V arasında
            // çalışmaktadır", below 110V its card self-protects, above 150V
            // that card is damaged. So 110-150 is the hard envelope and
            // everything here is derived from it.
            //
            // v_high_reset was 138 and that was a real flaw, found in the
            // field. The array sits near 150.9V open circuit, so after any
            // trip it could not fall back under 138V until evening - one trip
            // cost a whole day of pumping. 142 is still 8V under the damage
            // point, and the pump only sees it for the instant before 11A
            // drags the array down to its operating point.
            //
            // The low trip stays 2V above the pump's own 110V cutout on
            // purpose: this controller should act first, so a stop is our
            // decision and is logged, rather than the pump quietly
            // protecting itself with nobody knowing why it stopped.
            Profile::Pump => Thresholds {
                // v_low_trip 95, not 112: 1417 minutes of field data show the
                // pump operates at a median 109V down to 100.4V under load,
                // and its own protection does not engage even at 100V. 112
                // was tripping it mid-run - start at 137V, sag under load,
                // undervoltage trip, array recovers to 137V, restart: exactly
                // the start/stop cycling that wears a motor. 95 sits below the
                // real operating floor with margin. The pump reads a little
                // less than the box at the far end of the cable.
                //
                // dry_amps 3.0 / dry_volts 125: the boundary between the wet
                // operating region (median 109V, 3.5-6A) and a dry pump (high
                // volts, low amps). A loaded pump is always well under 125V;
                // only high-volts AND low-amps together read as dry.
                v_high_trip: 145.0,
                v_low_trip: 95.0,
                v_high_reset: 142.0,
                v_low_reset: 110.0,
                i_max: 13.0,
                dry_amps: 3.0,
                dry_volts: 125.0,
            },
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Thresholds {
    pub v_high_trip: f32,
    pub v_low_trip: f32,
    pub v_high_reset: f32,
    pub v_low_reset: f32,
    pub i_max: f32,
    pub dry_amps: f32,
    pub dry_volts: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// Bench or pump. Changing this rewrites every threshold below.
    #[serde(default = "default_profile")]
    pub profile: Profile,

    // --- Voltage protection -------------------------------------------
    /// The pump's controller card is destroyed above 150V. This sits below
    /// that with margin.
    pub v_high_trip: f32,
    /// The pump self-protects below 110V; trip just above so we stop first.
    pub v_low_trip: f32,
    /// Restart band, deliberately inside the trip band so a marginal array
    /// cannot chatter the contactor.
    pub v_high_reset: f32,
    pub v_low_reset: f32,

    // --- Overcurrent ---------------------------------------------------
    pub i_max: f32,
    pub i_max_seconds: u64,

    // --- Dry running ---------------------------------------------------
    /// Off until tuned against real data - see BUILD.md.
    pub dry_enabled: bool,
    pub dry_amps: f32,
    pub dry_volts: f32,
    pub dry_seconds: u64,
    pub dry_grace_seconds: u64,
    pub dry_lockout_1: u64,
    pub dry_lockout_2: u64,
    pub dry_lockout_3: u64,
    /// How long the pump must run without going dry before a strike is
    /// forgiven. Without this the escalation is permanent: one bad week in
    /// summer would still be handing out 8 hour lockouts months later.
    pub dry_forgive_minutes: u64,

    // --- Dry running: peak-collapse detector ---------------------------
    /// The relative-collapse dry detector, alongside the primed one.
    ///
    /// A pump accelerating its own rotor draws a surge whether or not there
    /// is water, so a dry start looks like a wet start while it spins up. At
    /// speed the difference shows: a dry pump's current COLLAPSES back toward
    /// zero and the array recovers toward Voc, while a wet pump's current
    /// HOLDS. Keying on the collapse RELATIVE to the peak reached this run
    /// catches a dry well even in weak sun, where the array never rises above
    /// dry_volts and the absolute-voltage primed test is blind.
    ///
    /// Defaulted through serde so a settings.json written before these fields
    /// existed still loads, and so arming dry_enabled arms this too.
    ///
    /// Fraction of the run's peak current below which the pump is treated as
    /// having lost (or never had) its load. Tuned from production data: a
    /// genuinely low-flow wet well settles no lower than ~0.55 of its peak,
    /// while every dry tail falls under ~0.3, so 0.40 separates them with
    /// margin on both sides even under passing cloud.
    #[serde(default = "default_collapse_frac")]
    pub dry_collapse_frac: f32,
    /// The peak must have passed this before a collapse counts, so a pump
    /// still climbing a slow ramp (peak near zero) cannot "collapse" from
    /// nothing. Set to the loaded-current threshold: the pump must have drawn
    /// a real acceleration surge to be judged on losing it.
    #[serde(default = "default_collapse_min_peak")]
    pub dry_collapse_min_peak: f32,
    /// The collapse must hold this long to trip, so a cloud that briefly dips
    /// the current does not cut off a low-flow well. A dry motor overheats
    /// over minutes, so a few seconds here is fully protective.
    #[serde(default = "default_collapse_seconds")]
    pub dry_collapse_seconds: u64,

    // --- Meter ---------------------------------------------------------
    /// Which shunt is fitted to the PZEM, as the meter itself encodes it:
    /// 0 = 100A, 1 = 50A, 2 = 200A, 3 = 300A. Checked against the meter at
    /// startup and corrected, because it lives in the meter's own memory
    /// and a replacement unit arrives set to whatever the last owner used.
    #[serde(default = "default_shunt")]
    pub shunt: u16,

    // --- Timing --------------------------------------------------------
    pub settle_readings: u32,
    pub retry_seconds: u64,
    pub meter_error_limit: u32,
    /// Longest a hand-started (Manual) run may last before the controller
    /// ends it. 0 disables the limit.
    ///
    /// Auto runs are not capped - see Machine::ran_too_long for why. This
    /// exists because a healthy pump has no fault to trip on, so a run that
    /// nobody ends never ends. The setup wizard leaves exactly that: it
    /// starts the pump and then waits on a human tap.
    ///
    /// Defaulted through serde so a settings.json written before this field
    /// existed still loads. Without that the whole file would fail to parse
    /// and every tuned threshold would silently revert to defaults.
    #[serde(default = "default_max_run")]
    pub max_run_minutes: u64,

    /// Arm the two level relays as a pump gate. Off by default: the relays
    /// are telemetry until this is set, so a fresh install and every test
    /// behave exactly as before. Wiring polarity (confirmed 2026-08-18, and
    /// re-confirmed in the field 2026-08-31: the LCD read tank/well exactly
    /// right): a CLOSED well contact = water present, a CLOSED tank contact =
    /// full. ON by default now that polarity is proven - a dry well must not
    /// pump and a full tank must stop in Auto. Fail-safe: an open/broken well
    /// wire reads no-water and blocks.
    #[serde(default = "default_level_gate")]
    pub level_gate_enabled: bool,

    /// Arm the hardware watchdog (/dev/watchdog) so a wedged control loop resets
    /// the Pi into a safe, relay-open boot. Opt-in (default OFF): on a kernel
    /// whose watchdog misbehaves it could boot-loop, and that can only be
    /// confirmed on the real board - so enable it once verified on hardware,
    /// per FIELD_INSTALL. When off, pumpd never opens the device.
    #[serde(default = "default_watchdog")]
    pub watchdog_enabled: bool,

    // --- Sunset / weak-supply anti-cycling -----------------------------
    // At dusk the supply can read fine UNLOADED (e.g. 138 V) but collapse
    // below the run threshold the instant the motor loads it, so the pump
    // trips on under-voltage seconds after every start. Retrying every
    // `retry_seconds` then hammers the motor dozens of times an evening.
    // These bound that: a run that trips on under-voltage in under
    // `sag_min_run_seconds` is a "sag collapse"; after `sag_strike_limit`
    // consecutive ones the retry wait jumps to `sag_backoff_seconds` (a long
    // rest) instead of `retry_seconds`. A run that HOLDS past
    // `sag_min_run_seconds` proves the supply carries the load today and
    // clears the count - which is exactly what separates a marginal dusk from
    // a passing daytime cloud (whose restart runs on and resets).
    /// A run tripping on under-voltage sooner than this (s) counts as a sag
    /// collapse. A run lasting longer proves the supply holds under load.
    #[serde(default = "default_sag_min_run")]
    pub sag_min_run_seconds: u64,
    /// Consecutive sag collapses before the long backoff engages. 0 disables
    /// the anti-cycling entirely (retries always use retry_seconds).
    #[serde(default = "default_sag_strike_limit")]
    pub sag_strike_limit: u32,
    /// The long rest (s) used for retries once sag_strike_limit is reached,
    /// so a marginal supply rests the motor instead of restarting it in a loop.
    #[serde(default = "default_sag_backoff")]
    pub sag_backoff_seconds: u64,
}

fn default_level_gate() -> bool {
    true
}

fn default_watchdog() -> bool {
    false
}

fn default_sag_min_run() -> u64 {
    // A weak-supply collapse trips within a few seconds of loading the motor;
    // a genuine run lasts minutes. 60 s cleanly separates the two.
    60
}

fn default_sag_strike_limit() -> u32 {
    // Allow a couple of honest retries (a passing cloud clears on the 2nd), then
    // back off. On by default - this is the reported dusk motor-cycling fix.
    3
}

fn default_sag_backoff() -> u64 {
    // Rest the motor ~20 min before probing the supply again, instead of every
    // retry_seconds. Long enough to stop the 50-starts-an-evening cycling, short
    // enough to resume within the same session once the supply firms up.
    1200
}

fn default_shunt() -> u16 {
    crate::meter::SHUNT_50A
}

fn default_collapse_frac() -> f32 {
    0.40
}

fn default_collapse_min_peak() -> f32 {
    // DISABLED in production. Adversarial testing (2026-08-18) proved the
    // peak-collapse detector false-trips ordinary aquifer drawdown, because
    // peak_amps never decays. Until it is reworked to a decaying baseline,
    // an unreachable min-peak keeps it inert: peak_amps >= 1e9 is never true.
    // Tests that exercise the detector set this field explicitly and are
    // unaffected. The primed+fast dry path remains the live protection.
    1.0e9
}

fn default_collapse_seconds() -> u64 {
    3
}

fn default_max_run() -> u64 {
    // Long enough that a deliberate irrigation run by hand is not
    // interrupted, short enough that a forgotten one is not an afternoon.
    // The wizard needs three minutes of it.
    60
}

fn default_profile() -> Profile {
    // The test jumper is the authority and is read at startup, so this only
    // applies before a panel exists. Bench is the cautious choice: its
    // limits cannot close a contactor on a real 143V array.
    Profile::Bench
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            profile: Profile::Bench,
            v_high_trip: 145.0,
            v_low_trip: 112.0,
            v_high_reset: 138.0,
            v_low_reset: 118.0,

            i_max: 13.0,
            i_max_seconds: 5,

            // false by DEFAULT so a fresh Settings and the whole test suite
            // behave as before. The field box arms dry explicitly via its
            // settings.json - the tuned values below are what it uses.
            dry_enabled: false,
            dry_amps: 3.0,
            dry_volts: 125.0,
            // 1s hold: once a primed pump loses water it trips in about a
            // second, floored by the 500ms meter poll. A dry motor overheats
            // over minutes, so 1s is fully protective - the old 30 was slow
            // for no benefit.
            dry_seconds: 1,
            // 90s grace applies ONLY to a pump that has not yet primed, to
            // clear the soft-start ramp. A primed pump ignores it entirely.
            // The well level relay (SSRC-04) is the real never-primed guard;
            // this current-based path is the fallback.
            dry_grace_seconds: 90,
            dry_lockout_1: 45,
            dry_lockout_2: 120,
            dry_lockout_3: 480,
            dry_forgive_minutes: 120,

            dry_collapse_frac: default_collapse_frac(),
            dry_collapse_min_peak: default_collapse_min_peak(),
            dry_collapse_seconds: default_collapse_seconds(),

            shunt: default_shunt(),

            settle_readings: 30,
            retry_seconds: 10,
            meter_error_limit: 6,
            max_run_minutes: default_max_run(),
            level_gate_enabled: default_level_gate(),
            watchdog_enabled: default_watchdog(),
            sag_min_run_seconds: default_sag_min_run(),
            sag_strike_limit: default_sag_strike_limit(),
            sag_backoff_seconds: default_sag_backoff(),
        }
    }
}

impl Settings {
    /// Overwrite every threshold from the chosen profile.
    ///
    /// This is the commissioning switch: flip to Pump and the real limits
    /// take effect, with no chance of a bench value being left behind.
    pub fn apply_profile(&mut self, profile: Profile) {
        let t = profile.thresholds();
        self.profile = profile;
        self.v_high_trip = t.v_high_trip;
        self.v_low_trip = t.v_low_trip;
        self.v_high_reset = t.v_high_reset;
        self.v_low_reset = t.v_low_reset;
        self.i_max = t.i_max;
        self.dry_amps = t.dry_amps;
        self.dry_volts = t.dry_volts;
    }

    /// True when the stored thresholds no longer match the named profile,
    /// which happens after hand-editing a single value. Worth surfacing so
    /// nobody believes they are on the pump profile when they are not.
    pub fn matches_profile(&self) -> bool {
        let t = self.profile.thresholds();
        let near = |a: f32, b: f32| (a - b).abs() < 0.05;
        near(self.v_high_trip, t.v_high_trip)
            && near(self.v_low_trip, t.v_low_trip)
            && near(self.v_high_reset, t.v_high_reset)
            && near(self.v_low_reset, t.v_low_reset)
            && near(self.i_max, t.i_max)
    }

    /// Returns human-readable problems. Empty means the settings are sane.
    pub fn problems(&self) -> Vec<String> {
        let mut p = Vec::new();

        // A non-finite threshold defeats every comparison silently (amps > NaN
        // is always false), so reject it before any range check runs. This also
        // fails a loaded settings.json into safe defaults if one ever carried a
        // bad value. Comparisons below are safe for finite floats only.
        for (name, val) in [
            ("v_high_trip", self.v_high_trip),
            ("v_low_trip", self.v_low_trip),
            ("v_high_reset", self.v_high_reset),
            ("v_low_reset", self.v_low_reset),
            ("i_max", self.i_max),
            ("dry_amps", self.dry_amps),
            ("dry_volts", self.dry_volts),
            ("dry_collapse_frac", self.dry_collapse_frac),
            ("dry_collapse_min_peak", self.dry_collapse_min_peak),
        ] {
            if !val.is_finite() {
                p.push(format!("{name} must be a finite number"));
            }
        }
        if !p.is_empty() {
            return p;
        }

        if self.v_high_trip > 150.0 {
            p.push("Trip voltage above 150V exceeds the pump's damage threshold".into());
        }
        if self.v_high_reset >= self.v_high_trip {
            p.push("Restart voltage must be below the trip voltage, or the pump will chatter".into());
        }
        if self.v_low_reset <= self.v_low_trip {
            p.push("Low restart must be above the low trip, or the pump will chatter".into());
        }
        if self.v_low_reset >= self.v_high_reset {
            p.push("The restart band is inverted".into());
        }
        if self.i_max <= 0.0 {
            p.push("Maximum current must be positive".into());
        }
        // Upper bound is a SAFETY limit, mirroring meter_error_limit below: the
        // PZEM saturates at its shunt's full scale, so an i_max at or above that
        // (a fat-fingered 130 on a 50A shunt, or a bad value over the open AP)
        // can NEVER be exceeded by a reading - overcurrent would silently never
        // trip, and a locked-rotor/jammed pump would draw fault current and cook
        // unseen. Require it to sit below the shunt's readable range.
        if let Some(full_scale) = crate::meter::shunt_full_scale(self.shunt) {
            if self.i_max >= full_scale {
                p.push(format!(
                    "Maximum current {:.0}A is at or above the {} shunt's full scale, so overcurrent can never trip",
                    self.i_max,
                    crate::meter::shunt_name(self.shunt)
                ));
            }
        }
        if self.dry_enabled && self.dry_volts >= self.v_high_trip {
            p.push("Dry-run voltage is above the trip voltage, so it can never fire".into());
        }
        if self.dry_enabled && self.dry_amps <= 0.0 {
            p.push("Dry-run current must be positive".into());
        }
        if !(self.dry_collapse_frac > 0.0 && self.dry_collapse_frac < 1.0) {
            p.push("Dry collapse fraction must be between 0 and 1".into());
        }
        if self.dry_collapse_min_peak <= 0.0 {
            p.push("Dry collapse minimum peak current must be positive".into());
        }
        if self.shunt > 3 {
            p.push("Shunt must be 0 (100A), 1 (50A), 2 (200A) or 3 (300A)".into());
        }
        if self.settle_readings < 1 {
            p.push("Stability count must be at least 1".into());
        }
        if self.meter_error_limit < 1 {
            p.push("Meter error limit must be at least 1, or a single missed reading is a fault".into());
        }
        // Upper bound is a SAFETY limit, not a nicety: at the ~500ms poll each
        // count is about half a second of running with no voltage check. A huge
        // value (set by mistake or over the open AP) would let the pump run
        // BLIND for minutes-to-days after the meter dies - long enough for an
        // overvoltage to burn it unseen. 30 (~15s) is the most we ever tolerate.
        if self.meter_error_limit > 30 {
            p.push("Meter error limit above 30 (~15s) would let the pump run blind too long after the meter dies".into());
        }
        if self.settle_readings > 600 {
            p.push("Stability count above 600 (~5 min) would never let the pump start".into());
        }
        // The setup wizard runs the pump for three minutes before it can ask
        // anything. A shorter cap would cut it off mid-measurement and the
        // operator would never learn why.
        if self.max_run_minutes > 0 && self.max_run_minutes < 4 {
            p.push("Maximum run time must be 0 (no limit) or at least 4 minutes, or the setup wizard cannot finish".into());
        }

        p
    }

    pub fn load(path: &Path) -> Self {
        let parsed = std::fs::read_to_string(path)
            .ok()
            .and_then(|s| serde_json::from_str::<Settings>(&s).ok());

        match parsed {
            Some(s) if s.problems().is_empty() => s,
            Some(_) => {
                eprintln!("settings file failed validation, using defaults");
                Settings::default()
            }
            None => Settings::default(),
        }
    }

    /// Atomic AND durable write. A power cut at any point leaves either the old
    /// file or the complete new one - never a torn file. Durability matters here
    /// because the box browns out: without fsync the rename can be recorded while
    /// the data is still only in the page cache, so a power cut just after save()
    /// returns could revert wizard-calibrated settings (e.g. a measured i_max) to
    /// the generic profile defaults. We fsync the temp file before the rename, and
    /// fsync the directory after, so once save() returns the new settings are on
    /// disk. This is never called from the control loop (only at startup and from
    /// the web thread), so the fsync latency cannot stall live protection.
    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        use std::io::Write;
        let dir = path.parent();
        if let Some(d) = dir {
            std::fs::create_dir_all(d)?;
        }
        let tmp = path.with_extension("tmp");
        {
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(serde_json::to_string_pretty(self)?.as_bytes())?;
            f.sync_all()?; // data+metadata of the temp file hit disk before the rename
        }
        std::fs::rename(&tmp, path)?;
        // fsync the directory so the rename itself survives a power cut.
        if let Some(d) = dir {
            if let Ok(dfile) = std::fs::File::open(d) {
                let _ = dfile.sync_all();
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_path(tag: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "pumpd-settings-test-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn a_corrupt_settings_file_falls_back_to_safe_defaults() {
        // A brownout can truncate settings.json mid-write. It must load as
        // safe defaults (gate ON), never leave the pump unprotected.
        let dir = tmp_path("corrupt");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        std::fs::write(&path, b"{ \"profile\": \"pump\", \"v_low_tri").unwrap(); // torn JSON
        let s = Settings::load(&path);
        assert!(s.level_gate_enabled, "a corrupt file must load with the level gate ON");
        assert!(s.problems().is_empty(), "the fallback defaults must themselves be valid");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_settings_file_missing_the_level_gate_field_loads_with_the_gate_on() {
        // An old-format or partially-written-but-valid JSON that OMITS
        // level_gate_enabled must NOT silently disable the gate (the R4 dry-run
        // trap). This guards default_level_gate() against ever becoming false.
        let dir = tmp_path("nogate");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("settings.json");
        let mut full = Settings::default();
        full.apply_profile(Profile::Pump);
        let json = serde_json::to_string(&full).unwrap();
        // strip the level_gate_enabled field entirely, as an old file would lack it
        let stripped: String = json
            .split(',')
            .filter(|kv| !kv.contains("level_gate_enabled"))
            .collect::<Vec<_>>()
            .join(",");
        std::fs::write(&path, &stripped).unwrap();
        let s = Settings::load(&path);
        assert!(
            s.level_gate_enabled,
            "settings.json without the gate field must default the gate ON, not OFF"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_is_durable_and_round_trips_with_no_leftover_temp() {
        // Exercises the fsync'd atomic write: the file is complete + reloadable,
        // and the temp file is renamed away (no torn .tmp left behind).
        let dir = tmp_path("save");
        let path = dir.join("settings.json");
        let mut s = Settings::default();
        s.apply_profile(Profile::Pump);
        s.save(&path).expect("durable save must succeed");
        assert!(!path.with_extension("tmp").exists(), "the temp file must be renamed away");
        let loaded = Settings::load(&path);
        assert_eq!(loaded.profile, Profile::Pump);
        assert!(loaded.level_gate_enabled);
        assert!(loaded.problems().is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_i_max_at_or_above_the_shunt_full_scale_is_rejected() {
        // A huge/fat-fingered i_max the meter can never reach would silently
        // defeat overcurrent (amps > i_max is never true past shunt full scale).
        let mut s = Settings::default();
        s.apply_profile(Profile::Pump);
        s.shunt = crate::meter::SHUNT_50A; // reads up to 50A
        s.i_max = 130.0; // 130A on a 50A shunt: unreadable -> never trips
        assert!(
            s.problems().iter().any(|m| m.contains("full scale")),
            "i_max above the shunt full scale must be rejected: {:?}",
            s.problems()
        );
        // exactly at full scale is also rejected (no headroom to ever exceed it)
        s.i_max = 50.0;
        assert!(
            s.problems().iter().any(|m| m.contains("full scale")),
            "i_max == shunt full scale must be rejected (overcurrent could never fire)"
        );
        // a normal in-range i_max is accepted
        s.i_max = 13.0;
        assert!(
            !s.problems().iter().any(|m| m.contains("full scale")),
            "a normal i_max within the shunt range must be accepted: {:?}",
            s.problems()
        );
        // a bigger shunt raises the ceiling proportionally
        s.shunt = crate::meter::SHUNT_300A;
        s.i_max = 130.0;
        assert!(
            !s.problems().iter().any(|m| m.contains("full scale")),
            "130A is fine on a 300A shunt"
        );
    }

    #[test]
    fn both_profiles_are_valid() {
        for profile in [Profile::Bench, Profile::Pump] {
            let mut s = Settings::default();
            s.apply_profile(profile);
            assert!(
                s.problems().is_empty(),
                "{:?} profile is invalid: {:?}",
                profile,
                s.problems()
            );
            assert!(s.matches_profile());
        }
    }

    #[test]
    fn pump_profile_protects_the_real_pump() {
        let mut s = Settings::default();
        s.apply_profile(Profile::Pump);
        // The pump's controller card dies above 150V.
        assert!(s.v_high_trip < 150.0, "trip must be below the damage point");
        // Field data (1417 min) disproved the datasheet's 110V self-protect:
        // the pump runs to 100.4V and does not cut out even at 100V. So the
        // floor sits below the real operating range, but a typo to near-zero
        // must still fail this test.
        assert!(s.v_low_trip < 105.0, "must clear the real operating floor");
        assert!(s.v_low_trip > 85.0, "but not be absurdly low");
        // And the array's 150.9V open-circuit must be outside the start band.
        assert!(s.v_high_reset < 150.9, "sunrise Voc must not be startable");
    }

    // A non-finite threshold defeats every comparison silently (amps > NaN is
    // always false), so problems() must reject it - this is the guard that also
    // fails a bad settings.json into safe defaults on load.
    #[test]
    fn a_non_finite_threshold_is_rejected() {
        for bad in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            let mut s = Settings::default();
            s.apply_profile(Profile::Pump);
            s.i_max = bad;
            assert!(
                !s.problems().is_empty(),
                "i_max = {bad} must be flagged as a problem"
            );
        }
    }

    #[test]
    fn switching_profiles_replaces_every_threshold() {
        let mut s = Settings::default();
        s.apply_profile(Profile::Bench);
        let bench = s.v_high_trip;
        s.apply_profile(Profile::Pump);
        assert_ne!(s.v_high_trip, bench, "thresholds should change with profile");
        assert!(s.matches_profile());
    }

    #[test]
    fn a_hand_edited_value_is_detected() {
        let mut s = Settings::default();
        s.apply_profile(Profile::Pump);
        s.v_high_trip = 140.0;
        assert!(!s.matches_profile(), "should notice a manual override");
    }

    #[test]
    fn rejects_an_impossible_shunt() {
        let mut s = Settings::default();
        s.shunt = 7;
        assert!(s.problems().iter().any(|p| p.contains("Shunt")));
    }

    #[test]
    fn default_shunt_is_the_fitted_one() {
        // 50A. Reading it as 100A would double every current measurement.
        assert_eq!(Settings::default().shunt, crate::meter::SHUNT_50A);
    }

    #[test]
    fn defaults_are_valid() {
        assert!(Settings::default().problems().is_empty());
    }

    #[test]
    fn rejects_trip_above_pump_limit() {
        let mut s = Settings::default();
        s.v_high_trip = 155.0;
        assert!(s.problems().iter().any(|p| p.contains("150V")));
    }

    #[test]
    fn rejects_inverted_hysteresis() {
        let mut s = Settings::default();
        s.v_high_reset = 148.0;
        assert!(!s.problems().is_empty());
    }

    #[test]
    fn a_settings_file_written_before_the_run_limit_still_loads() {
        // The box in the field has a settings.json with no max_run_minutes
        // key. Without a serde default the whole file fails to parse and
        // every tuned threshold silently reverts to the built-in defaults -
        // a config change nobody asked for, on a pump guard.
        let old = r#"{
            "profile":"bench",
            "v_high_trip":18.0,"v_low_trip":9.0,
            "v_high_reset":17.0,"v_low_reset":10.0,
            "i_max":5.0,"i_max_seconds":5,
            "dry_enabled":true,"dry_amps":0.5,"dry_volts":16.0,
            "dry_seconds":30,"dry_grace_seconds":15,
            "dry_lockout_1":45,"dry_lockout_2":120,"dry_lockout_3":480,
            "dry_forgive_minutes":120,"shunt":1,
            "settle_readings":30,"retry_seconds":10,"meter_error_limit":6
        }"#;
        let s: Settings = serde_json::from_str(old).expect("old settings must still parse");
        assert_eq!(s.v_high_trip, 18.0, "existing thresholds must survive");
        assert!(s.dry_enabled, "existing dry-run choice must survive");
        assert_eq!(s.max_run_minutes, default_max_run());
        assert!(s.problems().is_empty());
    }

    #[test]
    fn the_run_limit_leaves_room_for_the_wizard() {
        // The wizard runs the pump 180s before it can ask anything.
        let mut s = Settings::default();
        assert!(s.max_run_minutes == 0 || s.max_run_minutes * 60 > 180);

        s.max_run_minutes = 2;
        assert!(s.problems().iter().any(|p| p.contains("setup wizard")));

        s.max_run_minutes = 0; // explicitly disabled is allowed
        assert!(s.problems().is_empty());
    }

    #[test]
    fn rejects_a_meter_error_limit_of_zero() {
        let mut s = Settings::default();
        s.meter_error_limit = 0;
        assert!(s.problems().iter().any(|p| p.contains("Meter error limit")));
    }

    #[test]
    fn rejects_a_meter_error_limit_that_runs_the_pump_blind() {
        // A huge limit (set by mistake or over the open AP) would defeat the
        // meter-loss protection - the pump would run for minutes/days with no
        // voltage check after the meter died. Must be rejected.
        let mut s = Settings::default();
        s.meter_error_limit = 100_000;
        assert!(
            s.problems().iter().any(|p| p.contains("run blind too long")),
            "a huge meter_error_limit must be rejected"
        );
        // a sane operating value is still accepted
        s.meter_error_limit = 12;
        assert!(!s.problems().iter().any(|p| p.contains("Meter error limit")));
    }

    #[test]
    fn rejects_unreachable_dry_threshold() {
        let mut s = Settings::default();
        s.dry_enabled = true;
        s.dry_volts = 149.0;
        assert!(s.problems().iter().any(|p| p.contains("never fire")));
    }
}
