//! Dashboard, control API and Prometheus metrics.
//!
//! Plain tiny_http on a background thread - no async runtime, no framework.
//! The whole surface is a handful of routes and the page is one self-
//! contained file, so the controller stays a single binary with nothing to
//! serve from disk.

use std::sync::{Arc, Mutex};

use crate::settings::{Profile, Settings};
use crate::state::{Command, Machine, Mode, State};

pub struct Shared {
    pub machine: Machine,
    pub settings: Settings,
    pub meter_ok: bool,
    pub lcd_ok: bool,
    pub started_at: u64,
    /// Commands from the dashboard, drained by the control loop so that all
    /// state changes happen in one place.
    pub pending: Vec<Command>,
    /// Result of the last command, surfaced back to whoever sent it.
    pub last_refusal: Option<String>,
    /// Current dashboard address, so the /api LCD mirror matches the physical
    /// screen (which rotates the address onto the bottom line).
    pub ip: Option<String>,
}

/// Read a request body with a hard cap. The dashboard AP is open-ish, so an
/// unbounded read_to_string would let a buggy or hostile client OOM the Pi.
/// 64KB dwarfs any real settings payload.
fn read_body(request: &mut tiny_http::Request) -> String {
    let mut raw = String::new();
    let mut limited = std::io::Read::take(request.as_reader(), 64 * 1024);
    let _ = std::io::Read::read_to_string(&mut limited, &mut raw);
    raw
}

pub fn serve(port: u16, shared: Arc<Mutex<Shared>>) {
    let server = match tiny_http::Server::http(("0.0.0.0", port)) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("dashboard unavailable on port {port}: {e}");
            return;
        }
    };

    for request in server.incoming_requests() {
        let url = request.url().split('?').next().unwrap_or("/").to_string();
        let method = request.method().clone();

        let mut request = request;
        let (code, body, mime) = match (method.as_str(), url.as_str()) {
            ("GET", "/") => (200, PAGE.to_string(), "text/html; charset=utf-8"),
            ("GET", "/api") => (200, api_json(&shared), "application/json"),
            ("GET", "/metrics") => (200, metrics(&shared), "text/plain; version=0.0.4"),
            ("GET", "/health") => health(&shared),
            ("GET", "/history.csv") => history_csv(),
            ("POST", "/settings") => save_settings(&read_body(&mut request), &shared, false),
            ("POST", "/settings/measured") => save_settings(&read_body(&mut request), &shared, true),
            ("POST", "/profile") => apply_profile_request(&read_body(&mut request), &shared),
            ("POST", path) => handle_post(path, &shared),
            _ => (404, "not found".into(), "text/plain"),
        };

        let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], mime.as_bytes())
            .expect("static header is valid");
        let no_cache =
            tiny_http::Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..])
                .expect("static header is valid");

        let response = tiny_http::Response::from_string(body)
            .with_status_code(code)
            .with_header(header)
            .with_header(no_cache);

        let _ = request.respond(response);
    }
}

fn handle_post(path: &str, shared: &Arc<Mutex<Shared>>) -> (u16, String, &'static str) {
    let cmd = match path {
        "/start" => Some(Command::Start),
        "/stop" => Some(Command::Stop),
        "/reset" => Some(Command::Reset),
        "/clear-lockout" => Some(Command::ClearLockout),
        "/mode/auto" => Some(Command::SetMode(Mode::Auto)),
        "/mode/manual" => Some(Command::SetMode(Mode::Manual)),
        "/mode/off" => Some(Command::SetMode(Mode::Off)),
        _ => None,
    };

    match cmd {
        Some(c) => {
            let mut s = shared.lock().expect("shared state poisoned");
            s.last_refusal = None;
            s.pending.push(c);
            (200, r#"{"ok":true}"#.into(), "application/json")
        }
        None => (404, r#"{"error":"unknown command"}"#.into(), "application/json"),
    }
}

/// Apply settings from the dashboard.
///
/// Values arrive as JSON strings from a form, so each is parsed individually
/// and anything unparseable keeps its current value rather than resetting to
/// a default. Validation runs on the merged result: a rejected change leaves
/// the running configuration untouched.
fn save_settings(
    raw: &str,
    shared: &Arc<Mutex<Shared>>,
    from_wizard: bool,
) -> (u16, String, &'static str) {
    let incoming: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(e) => {
            return (
                400,
                format!(r#"{{"ok":false,"problems":["malformed request: {e}"]}}"#),
                "application/json",
            )
        }
    };

    let mut s = shared.lock().expect("shared state poisoned");
    let mut next = s.settings.clone();

    let num = |v: &serde_json::Value, key: &str| -> Option<f64> {
        let parsed = match v.get(key)? {
            serde_json::Value::Number(n) => n.as_f64(),
            // f64::from_str accepts "NaN", "inf" and "Infinity". Left unchecked,
            // a posted "i_max":"NaN" would set a threshold that no comparison
            // ever trips (amps > NaN is always false -> overcurrent silently
            // off), and "inf" on an integer field casts to u64::MAX (a timeout
            // that never elapses). Reject non-finite here so such a value is
            // treated as unparseable and the current setting is kept.
            serde_json::Value::String(t) => t.trim().parse().ok(),
            _ => None,
        }?;
        parsed.is_finite().then_some(parsed)
    };

    if let Some(v) = num(&incoming, "v_high_trip") { next.v_high_trip = v as f32 }
    if let Some(v) = num(&incoming, "v_low_trip") { next.v_low_trip = v as f32 }
    if let Some(v) = num(&incoming, "v_high_reset") { next.v_high_reset = v as f32 }
    if let Some(v) = num(&incoming, "v_low_reset") { next.v_low_reset = v as f32 }
    if let Some(v) = num(&incoming, "i_max") { next.i_max = v as f32 }
    if let Some(v) = num(&incoming, "i_max_seconds") { next.i_max_seconds = v as u64 }
    if let Some(v) = num(&incoming, "dry_amps") { next.dry_amps = v as f32 }
    if let Some(v) = num(&incoming, "dry_volts") { next.dry_volts = v as f32 }
    if let Some(v) = num(&incoming, "dry_seconds") { next.dry_seconds = v as u64 }
    if let Some(v) = num(&incoming, "dry_grace_seconds") { next.dry_grace_seconds = v as u64 }
    if let Some(v) = num(&incoming, "dry_lockout_1") { next.dry_lockout_1 = v as u64 }
    if let Some(v) = num(&incoming, "dry_lockout_2") { next.dry_lockout_2 = v as u64 }
    if let Some(v) = num(&incoming, "dry_lockout_3") { next.dry_lockout_3 = v as u64 }
    if let Some(v) = num(&incoming, "settle_readings") { next.settle_readings = v as u32 }
    if let Some(v) = num(&incoming, "retry_seconds") { next.retry_seconds = v as u64 }

    // These three were rendered or documented as settings, answered
    // {"ok":true}, and were then thrown away because nothing merged them.
    // dry_forgive_minutes decides when a dry-run strike decays and
    // meter_error_limit decides when a silent meter becomes a Fault - both
    // are live protection parameters, so "saved" meaning "ignored" was a
    // safety claim the controller was not honouring.
    if let Some(v) = num(&incoming, "dry_forgive_minutes") { next.dry_forgive_minutes = v as u64 }
    if let Some(v) = num(&incoming, "meter_error_limit") { next.meter_error_limit = v as u32 }
    // Read by the meter at startup only, so this takes effect on restart.
    // Persisting it is still the honest behaviour - the alternative is a
    // field that reports success and reverts on the next poll.
    if let Some(v) = num(&incoming, "shunt") { next.shunt = v as u16 }

    // A negative would cast to 0, and 0 means "no limit" - so an accidental
    // minus sign would quietly switch the run deadline off. Refuse instead.
    if let Some(v) = num(&incoming, "max_run_minutes") {
        if v < 0.0 {
            return (
                400,
                r#"{"ok":false,"problems":["Maximum run time cannot be negative"]}"#.into(),
                "application/json",
            );
        }
        next.max_run_minutes = v as u64;
    }

    // ---- production lock -------------------------------------------------
    //
    // On the production profile the voltage limits are constants, not
    // settings. They come from the pump's datasheet - the controller card is
    // destroyed above 150V and self-protects below 110V - so no number typed
    // into a phone should be able to move them. They are forced back here,
    // after the merge, so it makes no difference whether the field was
    // hidden, edited, or posted directly with curl.
    //
    // The current and dry-run thresholds are locked too, but differently:
    // they are properties of this installation rather than of the pump, so
    // the setup wizard may write them from what it measured. Nothing else
    // may. That is the whole reason /settings/measured exists as a separate
    // door - it is not about trust, it is about there being exactly one way
    // for those numbers to be set.
    //
    // Bench is left fully editable on purpose: it exists for experimenting.
    if next.profile == Profile::Pump {
        let locked = Profile::Pump.thresholds();
        next.v_high_trip = locked.v_high_trip;
        next.v_low_trip = locked.v_low_trip;
        next.v_high_reset = locked.v_high_reset;
        next.v_low_reset = locked.v_low_reset;

        if !from_wizard {
            next.i_max = s.settings.i_max;
            next.dry_amps = s.settings.dry_amps;
            next.dry_volts = s.settings.dry_volts;
        }
    }

    // Absent means "not mentioned", NOT "off".
    //
    // This used to default a missing key to false, which was written for a
    // checkbox - an unchecked box really is absent from a form. The control
    // is a <select>, which is always submitted, so nothing was gained and
    // one thing was badly lost: the setup wizard posts only i_max, dry_amps
    // and dry_volts, so every wizard run switched dry-run protection off and
    // then reported "the pump is now protected using its own measured
    // numbers". The one tool for tuning the dry-run thresholds was the one
    // thing that disabled them.
    //
    // A present value may still arrive as the string "true": the form is
    // serialised with FormData and everything comes through as text. Both
    // spellings are accepted so neither side can silently regress the other.
    if let Some(v) = incoming.get("level_gate_enabled") {
        next.level_gate_enabled = v
            .as_bool()
            .or_else(|| v.as_str().map(|t| t == "true"))
            .unwrap_or(next.level_gate_enabled);
    }
    if let Some(v) = incoming.get("dry_enabled") {
        next.dry_enabled = v
            .as_bool()
            .or_else(|| v.as_str().map(|t| t == "true"))
            .unwrap_or(next.dry_enabled);
    }

    let problems = next.problems();
    if !problems.is_empty() {
        let list: Vec<String> = problems.iter().map(|p| format!("\"{}\"", p.replace('"', "'"))).collect();
        return (
            400,
            format!(r#"{{"ok":false,"problems":[{}]}}"#, list.join(",")),
            "application/json",
        );
    }

    // Take the new settings live, then release the lock BEFORE touching the
    // disk.
    //
    // This used to write the file while still holding the shared mutex, and
    // the control loop takes that same mutex every tick. Root here is a
    // removable USB flash drive with a bad block-group checksum, where a
    // write+rename+sync measures around 650ms - so saving a setting stalled
    // the protection loop for as long as the stick took to answer. During
    // that stall the contactor is a plain GPIO still held HIGH and nothing
    // is comparing volts against the trip. On a 150.9V array feeding a card
    // that dies at 150V, that is the whole hazard in one line.
    //
    // Committing to memory first means a failed write leaves the running
    // config newer than the file: the operator gets a 500 saying exactly
    // that, and a reboot falls back to the last values that were known good
    // on disk. The reverse order would leave the file ahead of the pump.
    let saved = next.clone();
    s.settings = next;
    drop(s);

    let path = std::path::Path::new(crate::DATA_DIR).join("settings.json");
    if let Err(e) = saved.save(&path) {
        return (
            500,
            format!(
                r#"{{"ok":false,"problems":["applied, but could not be written to disk and will not survive a restart: {e}"]}}"#
            ),
            "application/json",
        );
    }

    (200, r#"{"ok":true}"#.into(), "application/json")
}

/// Switch every threshold between bench and production in one action.
///
/// This exists because the settings form can edit each threshold
/// individually, so there has to be one operation that puts the real pump
/// limits back with no chance of a typo - 145V trip, 112V cut-out, 13A.
///
/// It is gated on a password for one reason: to stop a stray tap on a phone
/// moving a live pump onto bench limits. It is not protecting against an
/// attacker; anyone on this access point is already standing at the box.
/// The password lives in its own file rather than settings.json, because
/// settings.json is served wholesale by /api and a password must never be.
///
/// Fails closed. No password file means no profile switching, and the
/// dashboard says how to create one - better than inventing a default that
/// everybody keeps.
fn apply_profile_request(raw: &str, shared: &Arc<Mutex<Shared>>) -> (u16, String, &'static str) {
    let incoming: serde_json::Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return (400, r#"{"ok":false,"error":"bad request"}"#.into(), "application/json"),
    };

    let pass_path = std::path::Path::new(crate::DATA_DIR).join("admin.pass");
    let expected = match std::fs::read_to_string(&pass_path) {
        Ok(t) => t.trim().to_string(),
        Err(_) => {
            return (
                503,
                format!(
                    r#"{{"ok":false,"error":"no admin password set - create {} with a password in it, then restart"}}"#,
                    pass_path.display()
                ),
                "application/json",
            )
        }
    };
    if expected.is_empty() {
        return (503, r#"{"ok":false,"error":"admin password file is empty"}"#.into(),
                "application/json");
    }

    let given = incoming.get("pass").and_then(|v| v.as_str()).unwrap_or("");
    if given != expected {
        return (403, r#"{"ok":false,"error":"wrong password"}"#.into(), "application/json");
    }

    let profile = match incoming.get("profile").and_then(|v| v.as_str()) {
        Some("pump") => Profile::Pump,
        Some("bench") => Profile::Bench,
        _ => return (400, r#"{"ok":false,"error":"profile must be pump or bench"}"#.into(),
                     "application/json"),
    };

    let mut s = shared.lock().expect("shared state poisoned");
    let was = s.settings.profile;
    let mut next = s.settings.clone();
    next.apply_profile(profile);

    let path = std::path::Path::new(crate::DATA_DIR).join("settings.json");
    if let Err(e) = next.save(&path) {
        return (500, format!(r#"{{"ok":false,"error":"could not save: {e}"}}"#),
                "application/json");
    }
    s.settings = next;

    // Loud on purpose: this rewrote every protection limit on the box.
    println!(
        "PROFILE {} -> {} from the dashboard: trip {}V/{}V, start {}V-{}V, {}A max",
        was.label(),
        profile.label(),
        s.settings.v_high_trip,
        s.settings.v_low_trip,
        s.settings.v_low_reset,
        s.settings.v_high_reset,
        s.settings.i_max
    );

    (200, format!(r#"{{"ok":true,"profile":"{}"}}"#, profile.label()), "application/json")
}

fn history_csv() -> (u16, String, &'static str) {
    let path = std::path::Path::new(crate::DATA_DIR).join("history.csv");
    match std::fs::read_to_string(path) {
        Ok(body) => (200, body, "text/csv"),
        Err(_) => (200, "timestamp,volts,amps,watts,kwh,state,mode\n".into(), "text/csv"),
    }
}

fn health(shared: &Arc<Mutex<Shared>>) -> (u16, String, &'static str) {
    let s = shared.lock().expect("shared state poisoned");
    let healthy = s.meter_ok;
    let body = format!(
        r#"{{"healthy":{},"meter":{},"display":{},"state":"{}"}}"#,
        healthy,
        s.meter_ok,
        s.lcd_ok,
        format!("{:?}", s.machine.state).to_lowercase()
    );
    (if healthy { 200 } else { 503 }, body, "application/json")
}

fn api_json(shared: &Arc<Mutex<Shared>>) -> String {
    let mut s = shared.lock().expect("shared state poisoned");
    // A refusal is delivered once, to whoever polls next, then cleared -
    // otherwise the dashboard would keep re-showing a stale error.
    let refusal = s.last_refusal.take();
    let s = &*s;
    let m = &s.machine;
    let now = crate::now_secs();
    let r = m.last;

    let events: Vec<String> = m
        .events
        .iter()
        .rev()
        .take(25)
        .map(|e| {
            format!(
                r#"{{"at":"{}","reason":"{}","detail":"{}","volts":{:.1},"amps":{:.2}}}"#,
                e.at, e.reason, escape(&e.detail), e.volts, e.amps
            )
        })
        .collect();

    // The exact four lines the physical LCD is showing, straight from the real
    // renderer - so a remote view (dashboard, panel) sees the box's own screen
    // rather than a second guess at it. Read-only; changes nothing.
    let lcd = crate::display::render(m, &s.settings, now, s.ip.as_deref(), now, refusal.as_deref());
    let lcd_json = lcd
        .iter()
        .map(|l| format!(r#""{}""#, escape(l)))
        .collect::<Vec<_>>()
        .join(",");

    format!(
        r#"{{
"state":"{state}","state_label":"{label}","mode":"{mode}",
"lcd":[{lcd}],
"volts":{volts:.1},"amps":{amps:.2},"watts":{watts:.0},
"uptime":{uptime},"stable":{stable},"settle":{settle},
"lockout_remaining":{lockout},"dry_strikes":{strikes},
"retry_in":{retry},
"reason":{reason},"detail":"{detail}",
"why_not_ready":"{why}",
"can_start":{can_start},"can_stop":{can_stop},
"energy_today":{energy:.0},"starts_today":{starts},"run_seconds_today":{runsec},
"meter_ok":{meter_ok},"display_ok":{lcd_ok},
"tank_contact":{tank_c},"well_contact":{well_c},
"service_uptime":{svc},
"last_refusal":{refusal},
"settings":{settings},
"events":[{events}]
}}"#,
        state = format!("{:?}", m.state).to_lowercase(),
        label = m.state.label(),
        mode = m.mode.label(),
        lcd = lcd_json,
        volts = r.map_or(0.0, |x| x.volts),
        amps = r.map_or(0.0, |x| x.amps),
        watts = r.map_or(0.0, |x| x.watts),
        uptime = m.uptime(now),
        stable = m.stable,
        settle = s.settings.settle_readings,
        lockout = m.lockout_remaining(now),
        strikes = m.dry_strikes,
        retry = m.retry_in(now).map_or("null".to_string(), |v| v.to_string()),
        reason = m
            .reason
            .map_or("null".to_string(), |x| format!(r#""{}""#, x.label())),
        detail = escape(&m.detail),
        why = escape(&m.why_not_ready(&s.settings)),
        can_start = matches!(m.state, State::Ready | State::Starting) && m.mode != Mode::Off,
        // Not contactor_closed(): that drives the relay and must stay exactly
        // "is the contactor shut". This is the UI question, which is
        // different - during Starting the contactor is still open, but the
        // owner has just pressed START and watching it count up with no way
        // to abort is the most alarming thing this page can do.
        can_stop = matches!(m.state, State::Running | State::Starting),
        energy = m.energy_today(),
        starts = m.starts_today,
        runsec = m.run_seconds_today,
        meter_ok = s.meter_ok,
        lcd_ok = s.lcd_ok,
        tank_c = m.tank_contact.map_or("null".into(), |v| v.to_string()),
        well_c = m.well_contact.map_or("null".into(), |v| v.to_string()),
        svc = now.saturating_sub(s.started_at),
        refusal = refusal
            .map(|r| format!(r#""{}""#, escape(&r)))
            .unwrap_or_else(|| "null".into()),
        settings = serde_json::to_string(&s.settings).unwrap_or_else(|_| "{}".into()),
        events = events.join(","),
    )
}

/// Escape a string for embedding in the hand-built /api JSON. `"` and `\` are
/// escaped; the whitespace controls collapse to a space (keeping the one-line
/// dashboard/LCD look the original replace('\n', " ") intended); and ANY other
/// C0 control character (U+0000..U+001F) is emitted as a \u escape. That last
/// clause is the point: an unescaped control char is illegal in JSON and would
/// make the whole document unparseable - blinding the dashboard exactly when a
/// fault detail most needs to be read.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' | '\r' | '\t' => out.push(' '),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn metrics(shared: &Arc<Mutex<Shared>>) -> String {
    let s = shared.lock().expect("shared state poisoned");
    let m = &s.machine;
    let now = crate::now_secs();
    let r = m.last;

    let mut out = String::new();

    let mut g = |name: &str, help: &str, value: String| {
        out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} gauge\n{name} {value}\n"));
    };

    g("pump_volts", "Array voltage", format!("{:.2}", r.map_or(0.0, |x| x.volts)));
    g("pump_amps", "Pump current", format!("{:.3}", r.map_or(0.0, |x| x.amps)));
    g("pump_watts", "Pump power", format!("{:.1}", r.map_or(0.0, |x| x.watts)));
    g("pump_energy_wh_total", "Lifetime energy", format!("{:.0}", r.map_or(0.0, |x| x.watt_hours)));
    g("pump_energy_wh_today", "Energy since midnight", format!("{:.0}", m.energy_today()));
    g("pump_running", "1 when the contactor is closed",
      if m.state.contactor_closed() { "1".into() } else { "0".into() });
    g("pump_uptime_seconds", "Current run duration", m.uptime(now).to_string());
    g("pump_run_seconds_today", "Run time since midnight", m.run_seconds_today.to_string());
    g("pump_starts_today", "Starts since midnight", m.starts_today.to_string());
    g("pump_dry_strikes", "Consecutive dry-run detections", m.dry_strikes.to_string());
    g("pump_lockout_seconds", "Remaining dry-run lockout", m.lockout_remaining(now).to_string());
    g("pump_meter_ok", "1 when the meter is responding",
      if s.meter_ok { "1".into() } else { "0".into() });
    g("pump_display_ok", "1 when the LCD is responding",
      if s.lcd_ok { "1".into() } else { "0".into() });
    g("pump_service_uptime_seconds", "Controller uptime",
      now.saturating_sub(s.started_at).to_string());

    // State and mode as labelled series, so a dashboard can graph transitions
    out.push_str("# HELP pump_state Current state, 1 for the active one\n# TYPE pump_state gauge\n");
    for st in [State::Off, State::Waiting, State::Ready, State::Starting,
               State::Running, State::Tripped, State::Lockout, State::Fault] {
        out.push_str(&format!(
            "pump_state{{state=\"{}\"}} {}\n",
            format!("{st:?}").to_lowercase(),
            if m.state == st { 1 } else { 0 }
        ));
    }

    out.push_str("# HELP pump_mode Selected mode, 1 for the active one\n# TYPE pump_mode gauge\n");
    for md in [Mode::Auto, Mode::Manual, Mode::Off] {
        out.push_str(&format!(
            "pump_mode{{mode=\"{}\"}} {}\n",
            md.label().to_lowercase(),
            if m.mode == md { 1 } else { 0 }
        ));
    }

    // Trip counts by reason, useful for alerting on a rising trend
    out.push_str("# HELP pump_trips_total Trips since start, by reason\n# TYPE pump_trips_total counter\n");
    for reason in ["OVERVOLT", "UNDERVOLT", "OVERCURRENT", "DRY RUN", "METER LOST", "STOPPED BY HAND"] {
        let n = m.events.iter().filter(|e| e.reason == reason).count();
        out.push_str(&format!(
            "pump_trips_total{{reason=\"{}\"}} {}\n",
            reason.to_lowercase().replace(' ', "_"),
            n
        ));
    }

    out
}

const PAGE: &str = include_str!("dashboard.html");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::{Profile, Settings};
    use crate::state::{Machine, Mode};

    fn pump_shared() -> Arc<Mutex<Shared>> {
        let mut settings = Settings::default();
        settings.apply_profile(Profile::Pump);
        Arc::new(Mutex::new(Shared {
            machine: Machine::new(Mode::Auto),
            settings,
            meter_ok: true,
            lcd_ok: true,
            started_at: 0,
            pending: Vec::new(),
            last_refusal: None,
            ip: None,
        }))
    }

    // A posted NaN threshold must not slip through: it defeats every comparison
    // (amps > NaN is always false), silently disabling overcurrent. The wizard
    // door is the only way i_max can be set on the pump profile, so that is
    // where the guard has to hold. (We assert the LIVE setting rather than the
    // HTTP code, which is 500 here only because the test can't write /var/lib.)
    #[test]
    fn a_nan_i_max_from_the_wizard_is_rejected_overcurrent_stays_armed() {
        let sh = pump_shared();
        let before = sh.lock().unwrap().settings.i_max;
        let _ = save_settings(r#"{"i_max":"NaN"}"#, &sh, true);
        let after = sh.lock().unwrap().settings.i_max;
        assert_eq!(after, before, "i_max must keep its finite value, not become NaN");
        assert!(after.is_finite(), "overcurrent threshold must stay finite");
    }

    // "inf" on an integer-cast field would become u64::MAX (a sustain window
    // that never elapses -> overcurrent never fires). Must be rejected too.
    #[test]
    fn an_infinite_i_max_seconds_is_rejected() {
        let sh = pump_shared();
        let before = sh.lock().unwrap().settings.i_max_seconds;
        let _ = save_settings(r#"{"i_max_seconds":"inf"}"#, &sh, true);
        let after = sh.lock().unwrap().settings.i_max_seconds;
        assert_eq!(after, before, "i_max_seconds must keep its value");
        assert_ne!(after, u64::MAX, "must never become an unreachable sustain window");
    }

    // The guard must not break the legitimate wizard write it exists to protect:
    // a finite measured i_max is applied to the live config.
    #[test]
    fn a_valid_measured_i_max_is_accepted() {
        let sh = pump_shared();
        let _ = save_settings(r#"{"i_max":11.5}"#, &sh, true);
        let after = sh.lock().unwrap().settings.i_max;
        assert!((after - 11.5).abs() < 0.001, "finite i_max should apply, got {after}");
    }

    // A huge measured i_max the meter can never reach (9999A exceeds even a 300A
    // shunt) would silently defeat overcurrent. The wizard must reject it and
    // keep the live threshold at its safe, in-range value.
    #[test]
    fn an_unreachable_i_max_from_the_wizard_is_rejected_overcurrent_stays_armed() {
        let sh = pump_shared();
        let before = sh.lock().unwrap().settings.i_max;
        let _ = save_settings(r#"{"i_max":9999.0}"#, &sh, true);
        let after = sh.lock().unwrap().settings.i_max;
        assert_eq!(after, before, "an i_max past the shunt full scale must be rejected");
    }

    // Whatever ends up in a detail/LCD/refusal string, the /api document must
    // stay parseable - control characters and quotes included - or the operator
    // loses the dashboard exactly when a fault needs reading.
    #[test]
    fn escape_keeps_the_api_document_valid_json() {
        let nasty = "well \"dry\"\nline2\r\ttab \u{0007}bell \u{0000}nul \\slash end";
        let doc = format!(r#"{{"detail":"{}"}}"#, escape(nasty));
        let parsed: serde_json::Value =
            serde_json::from_str(&doc).expect("escaped output must be valid JSON");
        let out = parsed["detail"].as_str().unwrap();
        assert!(out.contains("dry") && out.contains("slash"), "content preserved");
        assert!(!out.contains('\n') && !out.contains('\r') && !out.contains('\t'));
    }
}
