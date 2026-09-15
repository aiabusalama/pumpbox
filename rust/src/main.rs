//! Solar pump protection controller.
//!
//! Holds the contactor open unless the array voltage is inside a safe band,
//! so the pump's controller card never sees the open-circuit voltage the
//! panels produce before any current is drawn - which is what destroyed the
//! previous pump on a cold morning.
//!
//! Everything fails toward "pump off". The contactor is normally open and
//! only closes while this process actively holds the relay. Losing power,
//! the process, the meter, or the relay all drop the pump.

mod display;
mod failsafe;
mod meter;
mod panel;
mod settings;
mod sound;
mod state;
mod watchdog;
mod web;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::SyncSender;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use meter::Meter;
use panel::Panel;
use settings::Settings;
use state::{Command, Machine, Mode, State};

const DATA_DIR: &str = "/var/lib/pump-protection";

/// How often the meter is read and the protection re-evaluated.
const POLL: Duration = Duration::from_millis(500);
/// Buttons are scanned far more often than the meter, so a press acts
/// immediately instead of waiting up to half a second for the next poll.
const BUTTON_SCAN: Duration = Duration::from_millis(20);

// GPIO assignments, BCM numbering. None disables that control, so the
// controller runs with any subset of the panel wired.
const RELAY_PIN: u8 = 17;
const PANEL: panel::Pins = panel::Pins {
    profile_jumper: Some(12), // physical 32 - ground it for BENCH testing
    // Swapped to match how the switch is actually wired: the pin that ends
    // up grounded in the left position is 6, not 5.
    switch_left: Some(6),   // physical 31 - grounded = OFF
    switch_right: Some(5),  // physical 29 - grounded = AUTO
    start: Some(16),        // physical 36 - green button
    stop: Some(13),         // physical 33 - red button
    led_green: Some(19),    // physical 35
    led_red: Some(26),      // physical 37
    // Hacked SSRC-04 level relays. Isolated dry contact: wire C to GND and
    // NO (or NC, chosen for fail-safe) to the pin. Both free on the header.
    tank_relay: Some(20),   // physical 38
    well_relay: Some(21),   // physical 40
};

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn now_string() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

fn today() -> String {
    chrono::Local::now().format("%Y-%m-%d").to_string()
}

fn data(name: &str) -> PathBuf {
    PathBuf::from(DATA_DIR).join(name)
}

/// Mode survives restart - a power cut must not silently re-arm a pump
/// someone deliberately switched off.
fn load_mode() -> Mode {
    match std::fs::read_to_string(data("mode")) {
        Ok(s) => match s.trim() {
            "auto" => Mode::Auto,
            "off" => Mode::Off,
            _ => Mode::Manual,
        },
        // First run: Manual is the cautious default. The operator opts in
        // to autonomy rather than discovering it.
        Err(_) => Mode::Manual,
    }
}

// ---- persistence writer thread -------------------------------------------
// A dying SD card can make a file write BLOCK for seconds (the kernel retries
// bad blocks). Doing that in the control loop would freeze the protection with
// the contactor possibly CLOSED - the field's LCD freeze was this same class of
// bug (a blocking peripheral op in the hot loop). So all disk persistence goes
// through a bounded channel to a writer thread; the loop only ever does a
// non-blocking try_send and DROPS the job if the card cannot keep up. Live
// protection never waits on the disk. Persistence is best-effort - and a lost
// dry-run lockout is still caught by the well sensor on the next run.
enum Job {
    Mode(&'static str),
    Lockout(u64, u32),
    History(String),
    Heartbeat(u64),
}

static PERSIST: OnceLock<SyncSender<Job>> = OnceLock::new();

fn start_persister() {
    let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(128);
    let _ = PERSIST.set(tx);
    std::thread::spawn(move || {
        // A blocking write here stalls ONLY this thread, never the control loop.
        for job in rx {
            match job {
                Job::Mode(s) => {
                    let _ = std::fs::write(data("mode"), s);
                }
                Job::Lockout(until, strikes) => {
                    let _ = std::fs::write(data("lockout"), format!("{until},{strikes}"));
                }
                Job::History(line) => {
                    use std::io::Write;
                    let path = data("history.csv");
                    // Rotate at a cap so months of 5s samples can never fill the
                    // card, and /history.csv can never read a huge file into the
                    // Pi Zero's 512MB RAM. One old file is kept: bounded to ~2x.
                    const HISTORY_CAP: u64 = 4 * 1024 * 1024;
                    if std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) >= HISTORY_CAP {
                        let _ = std::fs::rename(&path, data("history.csv.old"));
                    }
                    let new = !path.exists();
                    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                        if new {
                            let _ = f.write_all(b"timestamp,volts,amps,watts,kwh,state,mode,dry_flag\n");
                        }
                        let _ = f.write_all(line.as_bytes());
                    }
                }
                Job::Heartbeat(t) => {
                    let _ = std::fs::write(data("heartbeat"), t.to_string());
                }
            }
        }
    });
}

fn persist(job: Job) {
    if let Some(tx) = PERSIST.get() {
        // Non-blocking: drop the job if the writer is backed up on a slow card.
        let _ = tx.try_send(job);
    }
}

fn save_mode(m: Mode) {
    persist(Job::Mode(match m {
        Mode::Auto => "auto",
        Mode::Manual => "manual",
        Mode::Off => "off",
    }));
}

/// Dry-run lockouts persist too, so a restart cannot be used to bypass the
/// rest a drawn-down well needs.
fn load_lockout() -> (u64, u32) {
    std::fs::read_to_string(data("lockout"))
        .ok()
        .and_then(|s| {
            let mut it = s.trim().split(',');
            Some((it.next()?.parse().ok()?, it.next()?.parse().ok()?))
        })
        .unwrap_or((0, 0))
}

fn save_lockout(until: u64, strikes: u32) {
    persist(Job::Lockout(until, strikes));
}

fn local_ip() -> Option<String> {
    // Client mode: ask the kernel which source address it would use to reach a
    // public address. Nothing is actually sent.
    if let Ok(sock) = std::net::UdpSocket::bind("0.0.0.0:0") {
        if sock.connect("192.0.2.1:1").is_ok() {
            // TEST-NET-1, never routed
            if let Ok(addr) = sock.local_addr() {
                return Some(addr.ip().to_string());
            }
        }
    }
    // AP / hotspot mode has NO default route, so the connect above fails with
    // "network unreachable" - read the first real IPv4 straight off the
    // interfaces instead. This is what puts the 192.168.4.1 hotspot address on
    // the LCD, the case the operator most needs to see.
    first_iface_ipv4()
}

/// First non-loopback, non-link-local IPv4 assigned to any interface, via
/// getifaddrs. Covers the AP case (wlan0 = 192.168.4.1) where route lookup fails.
fn first_iface_ipv4() -> Option<String> {
    unsafe {
        let mut ifap: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut ifap) != 0 {
            return None;
        }
        let mut cur = ifap;
        let mut found = None;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family as i32 == libc::AF_INET {
                let sin = ifa.ifa_addr as *const libc::sockaddr_in;
                // s_addr is network byte order; its in-memory bytes are the
                // octets in order on any host.
                let o = (*sin).sin_addr.s_addr.to_ne_bytes();
                let loopback = o[0] == 127;
                let link_local = o[0] == 169 && o[1] == 254;
                if !loopback && !link_local {
                    found = Some(format!("{}.{}.{}.{}", o[0], o[1], o[2], o[3]));
                    break;
                }
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(ifap);
        found
    }
}

/// Find the meter's serial port, resolved fresh every time it is opened.
///
/// A USB serial adapter does NOT come back on the same path. When the CH341
/// on this box glitched and re-enumerated, the kernel attached it to
/// /dev/ttyUSB1 while pumpd went on reopening /dev/ttyUSB0 every ten seconds
/// for ever. The meter is the only source of voltage truth, so that left the
/// guard permanently blind: it failed safe - Fault, contactor open - but it
/// could never recover, and no unattended box comes back from that without a
/// person walking to it and restarting the service.
///
/// /dev/serial/by-id is the stable name. It is built from the adapter's own
/// vendor and serial identifiers rather than from the order things were
/// plugged in, so it follows the device across a re-enumeration.
fn meter_port(pinned: Option<&str>) -> Option<String> {
    // An explicitly pinned PZEM_PORT wins while it exists. If it has
    // vanished, fall through and discover rather than fail for ever - a
    // pinned path that no longer resolves is exactly the situation that
    // stranded the box.
    if let Some(p) = pinned {
        if std::path::Path::new(p).exists() {
            return Some(p.to_string());
        }
        eprintln!("PZEM_PORT {p} is not present, looking for the meter");
    }

    let mut by_id: Vec<PathBuf> = std::fs::read_dir("/dev/serial/by-id")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .collect();
    by_id.sort();
    // 1a86 is the CH340/CH341 bridge the PZEM ships behind. Prefer it when
    // several adapters are present, but take any serial device over none.
    if let Some(p) = by_id
        .iter()
        .find(|p| p.to_string_lossy().contains("1a86"))
        .or_else(|| by_id.first())
    {
        return Some(p.to_string_lossy().into_owned());
    }

    // Last resort, for a kernel with no by-id links: the lowest ttyUSB.
    let mut tty: Vec<PathBuf> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("ttyUSB"))
        })
        .collect();
    tty.sort();
    tty.first().map(|p| p.to_string_lossy().into_owned())
}

fn append_history(r: &meter::Reading, st: State, mode: Mode, dry_flag: bool) {
    // Format here (cheap, no I/O), hand the line to the writer thread.
    let line = format!(
        "{},{:.1},{:.2},{:.0},{:.3},{},{},{}\n",
        now_string(),
        r.volts,
        r.amps,
        r.watts,
        r.watt_hours / 1000.0,
        format!("{st:?}").to_lowercase(),
        mode.label().to_lowercase(),
        if dry_flag { 1 } else { 0 }
    );
    persist(Job::History(line));
}

struct Relay {
    pin: Option<rppal::gpio::OutputPin>,
}

impl Relay {
    fn new(bcm: u8) -> Self {
        match rppal::gpio::Gpio::new().and_then(|g| g.get(bcm)) {
            Ok(p) => {
                let mut pin = p.into_output();
                pin.set_low(); // open on startup, always
                Relay { pin: Some(pin) }
            }
            Err(e) => {
                eprintln!("relay gpio unavailable: {e}");
                Relay { pin: None }
            }
        }
    }

    fn set(&mut self, closed: bool) {
        if let Some(p) = &mut self.pin {
            if closed {
                p.set_high()
            } else {
                p.set_low()
            }
        }
    }
}

/// Green button: start, or clear a fault and arm if one is standing.
///
/// Shared by the fast button scan and the main loop so a press is handled
/// identically wherever it is noticed.
/// Green button: start, or clear a fault and arm if one is standing.
///
/// Returns a short message when the request was refused, for the display to
/// latch. Nothing sleeps here - this runs inside the protection loop, and
/// blocking it would stop the overvoltage check for as long as the message
/// was on screen.
fn handle_start(machine: &mut Machine, settings: &Settings) -> Option<String> {
    let now = now_secs();

    if matches!(machine.state, State::Tripped | State::Fault | State::Lockout) {
        let _ = machine.command(Command::ClearLockout, now, settings);
        let _ = machine.command(Command::Reset, now, settings);
        save_lockout(0, 0);
        println!("green - cleared fault, arming");
        return None;
    }

    match machine.command(Command::Start, now, settings) {
        Ok(()) => None,
        Err(refusal) => {
            let msg = refusal.message();
            println!("refused: {msg}");
            Some(msg)
        }
    }
}

/// What the LED should show, including why a start is being refused.
///
/// Waiting carries no reason of its own, so the band is checked here and
/// the same two patterns that mean "tripped on volts" are reused for
/// "will not start on volts". One vocabulary, whichever side it happens on.
fn led_want(machine: &state::Machine, s: &settings::Settings) -> panel::Led {
    let blocked = if machine.state == State::Waiting {
        machine.last.and_then(|r| {
            if r.volts > s.v_high_reset {
                Some(state::TripReason::OverVoltage)
            } else if r.volts < s.v_low_reset {
                Some(state::TripReason::UnderVoltage)
            } else {
                None
            }
        })
    } else {
        None
    };
    panel::led_for(machine.state, machine.reason, blocked)
}

/// Turn a panel event into a machine command, via whatever screen is up.
///
/// Returns true if anything was acted on, so the caller knows to re-evaluate
/// and repaint immediately rather than waiting for the next poll.
fn handle_input(
    input: panel::Input,
    machine: &mut Machine,
    settings: &Settings,
    refusal: &mut Option<String>,
    refusal_until: &mut u64,
) -> bool {
    let now = now_secs();
    let mut acted = false;

    // The handle is the mode. Nothing else can set it, so it can never
    // disagree with what the panel shows.
    if let Some(m) = input.mode {
        println!("mode switch moved to {}", m.label());
        let _ = machine.command(Command::SetMode(m), now, settings);
        save_mode(m);
        acted = true;
        // The far-left OFF position is a hardware "stop everything": open the
        // pump (SetMode(Off) above did) and, past the boot grace, power the Pi
        // down cleanly. The main loop performs it (it holds the relay + syncs).
        let uptime = BOOT_AT.get().map(|b| b.elapsed()).unwrap_or_default();
        if should_power_off(input.mode, uptime) {
            POWER_OFF_REQUESTED.store(true, Ordering::SeqCst);
        }
    }

    // Red first, and unconditionally. If both arrive in the same drain,
    // stopping is the one that must win.
    if input.stop.is_some() {
        let _ = machine.command(Command::Stop, now, settings);
        acted = true;
    }

    if input.start.is_some() {
        if let Some(msg) = handle_start(machine, settings) {
            *refusal = Some(msg);
            *refusal_until = now + 4;
        }
        acted = true;
    }

    acted
}


fn main() {
    println!("pumpd starting");
    let _ = BOOT_AT.set(std::time::Instant::now());

    // Tones only, through the panel speaker. Failure in here is swallowed:
    // the controller must come up whether or not the speaker works.
    sound::start();
    sound::play("boot");

    if let Err(e) = std::fs::create_dir_all(DATA_DIR) {
        eprintln!("cannot create {DATA_DIR}: {e}");
    }

    let settings = Settings::load(&data("settings.json"));
    let mode = load_mode();
    let (lockout_until, dry_strikes) = load_lockout();

    let mut machine = Machine::new(mode);
    machine.lockout_until = lockout_until;
    machine.dry_strikes = dry_strikes;
    if lockout_until > now_secs() {
        machine.state = State::Lockout;
        machine.detail = "lockout resumed after restart".into();
        println!("resuming dry-run lockout, {}s remaining", lockout_until - now_secs());
    }

    let mut relay = Relay::new(RELAY_PIN);
    // With panic = "abort" a panic skips Drop, so the relay pin is never
    // released. Arm a panic hook that drives it low directly, so a panic while
    // the pump is running still opens the contactor instead of holding it
    // closed until the service restarts. Must come after the pin is an output.
    failsafe::arm(RELAY_PIN);
    // Logging must NEVER be able to freeze the control loop. pumpd's stdout is
    // piped to svlogd (/var/log/pumpd); if that consumer ever wedges - svlogd
    // stuck on a FULL SD card - a blocking write would fill the pipe and hang the
    // next println! mid-loop, potentially with the contactor closed (the R28/R29
    // freeze class). Making stdout/stderr non-blocking turns a stuck sink into a
    // fast write error instead of an unbounded block: the print macro then panics
    // and the failsafe hook armed just above opens the relay and aborts for a
    // restart - fail-safe. On a healthy box the pipe drains and this never fires.
    make_stdio_nonblocking();
    let mut panel = Panel::new(PANEL);

    // A spring-return selector has no position to read at boot, so the mode
    // loaded from disk is the only truth there is.
    println!("mode restored as {}", machine.mode.label());

    // The commissioning jumper is the authority on which thresholds apply.
    // Read once at startup, before the first meter reading, so the pump can
    // never briefly run on the wrong limits.
    let mut settings = settings;
    if let Some(want) = panel.profile() {
        if want != settings.profile || !settings.matches_profile() {
            println!(
                "profile jumper selects {}, applying its thresholds",
                want.label()
            );
            settings.apply_profile(want);
            let _ = settings.save(&data("settings.json"));
        }
    }
    println!(
        "thresholds: {} - start {:.0}-{:.0}V, trip below {:.0} or above {:.0}",
        settings.profile.label(),
        settings.v_low_reset,
        settings.v_high_reset,
        settings.v_low_trip,
        settings.v_high_trip
    );

    // The display runs on its own thread: a wedged i2c bus can then only block
    // that thread, never the control loop. A missing or flaky screen never
    // stops the protection - the pump keeps running headless.
    let display = display::Display::spawn(&display::LCD_ADDRESSES);
    let probed: Vec<String> = display::LCD_ADDRESSES.iter().map(|a| format!("{a:#04x}")).collect();
    println!("display thread started (probing {})", probed.join(", "));

    // All disk persistence runs on its own thread so a dying SD card can never
    // block the control loop (see start_persister).
    start_persister();

    // Resolved, not hardcoded - see meter_port. Held as the operator's
    // preference; the actual device is looked up again on every open.
    let pinned_port = std::env::var("PZEM_PORT").ok();
    let config_address = 1u8;
    // The shunt is stored in the meter, not here, and a wrong value scales
    // every current reading - a 50A shunt read as 100A doubles the amps and
    // quietly ruins both overcurrent tripping and the dry-run data.
    fn check_shunt(m: &mut Meter, want: u16) {
        match m.shunt() {
            Ok(got) if got == want => {
                println!("meter shunt {}, correct", meter::shunt_name(got))
            }
            Ok(got) => {
                println!(
                    "meter shunt is {} but should be {} - every amp reading is \
                     wrong by that ratio, correcting",
                    meter::shunt_name(got),
                    meter::shunt_name(want)
                );
                match m.set_shunt(want) {
                    Ok(()) => match m.shunt() {
                        Ok(now) if now == want => {
                            println!("meter shunt now {}", meter::shunt_name(now))
                        }
                        Ok(now) => println!(
                            "meter refused the change, still {}",
                            meter::shunt_name(now)
                        ),
                        Err(e) => println!("shunt set but could not confirm: {e}"),
                    },
                    Err(e) => println!("could not set shunt: {e}"),
                }
            }
            Err(e) => println!("could not read meter shunt: {e}"),
        }
    }

    let mut meter = match meter_port(pinned_port.as_deref()) {
        Some(p) => match Meter::open(&p, config_address, None) {
            Ok(mut m) => {
                println!("meter open on {p}");
                check_shunt(&mut m, settings.shunt);
                Some(m)
            }
            Err(e) => {
                eprintln!("meter unavailable: {e}");
                None
            }
        },
        None => {
            eprintln!("meter unavailable: no serial adapter found");
            None
        }
    };

    // Re-resolved periodically in the loop (below): the network usually comes up
    // AFTER pumpd starts - wifi takes seconds to join, or the AP is raised later
    // - so a once-at-boot lookup would leave the LCD showing no address forever.
    let mut ip = local_ip();
    let mut last_ip_check = now_secs();
    if let Some(a) = &ip {
        println!("dashboard on http://{a}:8080");
    }

    let shared = Arc::new(Mutex::new(web::Shared {
        machine: Machine::new(mode),
        settings: settings.clone(),
        meter_ok: false,
        lcd_ok: display.healthy(),
        started_at: now_secs(),
        pending: Vec::new(),
        last_refusal: None,
        ip: None,
    }));

    {
        let s = Arc::clone(&shared);
        std::thread::spawn(move || web::serve(8080, s));
    }

    install_signal_handlers();
    let running = Arc::new(AtomicBool::new(true));

    let mut last_state = machine.state;
    let mut last_mode = machine.mode;
    let mut last_log = 0u64;
    let mut last_beat = 0u64;
    let mut tick = 0u64;
    let mut meter_retry = 0u32;
    // A refusal is shown for a few seconds by swapping the headline,
    // rather than blanking the screen and sleeping.
    let mut refusal_until = 0u64;
    let mut refusal_msg: Option<String> = None;

    // The display thread claims the screen with a splash on its own. Give it a
    // moment so the logo is up before the loop starts, but keep it short - the
    // protection is watching within two seconds of power-on.
    std::thread::sleep(Duration::from_millis(1500));

    // Last-resort liveness: the hardware watchdog, fed once per completed
    // protection cycle below. If a cycle ever stops finishing in time the SoC
    // resets and the relay comes back up open. Opt-in (see settings): armed here
    // just before the loop so a slow startup can't trip it.
    let mut watchdog = if settings.watchdog_enabled {
        watchdog::Watchdog::arm(15)
    } else {
        println!("watchdog disabled in settings");
        watchdog::Watchdog::disabled()
    };

    // Always-on software backstop: restart if the loop ever wedges (e.g. a
    // serial flush stuck in tcdrain, which no per-op timeout bounds).
    start_loop_watchdog();

    println!("ready, mode {}", mode.label());

    while running.load(Ordering::SeqCst) && !SHUTDOWN.load(Ordering::SeqCst) {
        let now = now_secs();
        // Prove the loop is alive for the software watchdog.
        LOOP_TICKS.fetch_add(1, Ordering::Relaxed);

        // Refresh the dashboard address every ~15s so the LCD shows the current
        // one once the network is up (a UDP-connect probe; nothing is sent).
        if now.saturating_sub(last_ip_check) >= 15 {
            last_ip_check = now;
            ip = local_ip();
        }

        // ---- physical controls ----------------------------------------
        let input = panel.poll();
        // Telemetry only for now: record the two level-relay contacts so the
        // Pi and dashboard can show tank/well state. These do NOT gate the
        // contactor yet - that step waits until the wiring polarity is
        // confirmed and verify_logic proves the interlock.
        let (tank_lvl, well_lvl) = panel.levels();
        machine.tank_contact = tank_lvl;
        machine.well_contact = well_lvl;

        handle_input(input, &mut machine, &settings, &mut refusal_msg, &mut refusal_until);

        // ---- panel OFF -> clean power-down -----------------------------
        // Set by the physical OFF selector (past the boot grace). Stop everything
        // in the safe order: contactor OPEN first (so the motor is dead before we
        // go), flush the SD (its journal matters - see the field boot-loop), then
        // ask the init system for a clean halt. Hold the relay open while it tears
        // down; if that stalls, sync + fall back to the kernel power-off after a
        // few seconds so the box always ends up off with the pump open.
        if POWER_OFF_REQUESTED.load(Ordering::SeqCst) {
            relay.set(false);
            let _ = machine.command(Command::SetMode(Mode::Off), now, &settings);
            display.set(display::render(&machine, &settings, now_secs(), ip.as_deref(), 0, Some("powering off")));
            println!("panel OFF selected - opening contactor and powering down");
            unsafe { libc::sync() };
            let _ = std::process::Command::new("poweroff").spawn();
            for _ in 0..12 {
                relay.set(false); // keep the contactor open through teardown
                std::thread::sleep(Duration::from_millis(500));
            }
            // init did not take us down in time - guarantee it, pump still open.
            unsafe {
                libc::sync();
                libc::reboot(libc::LINUX_REBOOT_CMD_POWER_OFF);
            }
        }

        // ---- commands from the dashboard -------------------------------
        let queued: Vec<Command> = {
            let mut s = shared.lock().expect("shared state poisoned");
            settings = s.settings.clone();
            std::mem::take(&mut s.pending)
        };
        for cmd in queued {
            let result = machine.command(cmd, now, &settings);
            if let Command::SetMode(m) = cmd {
                save_mode(m);
            }
            if let Command::ClearLockout = cmd {
                save_lockout(0, 0);
            }
            if let Err(refusal) = result {
                let msg = refusal.message();
                println!("refused: {msg}");
                shared.lock().expect("shared state poisoned").last_refusal = Some(msg);
            }
        }

        // ---- wait out the poll interval, scanning buttons throughout ----
        // The meter only needs reading twice a second, but a press must feel
        // instant. The panel is scanned every BUTTON_SCAN, commands apply
        // immediately, and the display is redrawn on the spot so the screen
        // never lags behind the contactor.
        let deadline = std::time::Instant::now() + POLL;
        while std::time::Instant::now() < deadline {
            std::thread::sleep(BUTTON_SCAN);

            let acted = handle_input(
                panel.poll(),
                &mut machine,
                &settings,
                &mut refusal_msg,
                &mut refusal_until,
            );

            if acted {
                // Re-evaluate against the reading already in hand, drive the
                // contactor, and repaint - all before the operator's finger
                // has left the button.
                machine.update(machine.last, now_secs(), &today(), &settings);
                relay.set(machine.state.contactor_closed());
                panel.set_led(led_want(&machine, &settings));

                let refusal = if now_secs() < refusal_until {
                    refusal_msg.as_deref()
                } else {
                    None
                };
                display.set(display::render(
                    &machine, &settings, now_secs(), ip.as_deref(), tick, refusal,
                ));
            }
        }

        // ---- read the meter --------------------------------------------
        let reading = match &mut meter {
            Some(m) => match m.read() {
                Ok(r) => Some(r),
                Err(e) => {
                    if machine.meter_errors == 0 {
                        eprintln!("meter read failed: {e}");
                    }
                    None
                }
            },
            None => None,
        };

        // If the meter has been silent for a while, look the port up again
        // and reopen it. The old file handle is dead after a re-enumeration,
        // and the device may well have moved to a different node - so the
        // path is re-resolved here rather than reused, which is what turns
        // "blind until a human restarts the service" into "blind for ten
        // seconds".
        if reading.is_none() {
            meter_retry = meter_retry.saturating_add(1);
            if meter_retry % 20 == 0 {
                match meter_port(pinned_port.as_deref()) {
                    Some(p) => {
                        eprintln!("meter silent, reopening {p}");
                        meter = match Meter::open(&p, config_address, None) {
                            Ok(mut m) => {
                                eprintln!("meter port reopened on {p}");
                                // A replacement or re-enumerated meter comes
                                // up with whatever shunt is in its own
                                // memory. Unchecked, every amp reading after
                                // a swap is wrong by that ratio.
                                check_shunt(&mut m, settings.shunt);
                                Some(m)
                            }
                            Err(e) => {
                                eprintln!("could not reopen meter: {e}");
                                None
                            }
                        };
                    }
                    None => eprintln!("meter silent, no serial adapter present"),
                }
            }
        } else {
            meter_retry = 0;
        }


        machine.update(reading, now, &today(), &settings);

        // ---- drive the contactor ---------------------------------------
        relay.set(machine.state.contactor_closed());
        panel.set_led(led_want(&machine, &settings));

        // A full sense -> decide -> actuate cycle just completed, so the loop
        // is provably alive: feed the watchdog. If the loop ever wedges, this
        // is skipped and the SoC resets into a safe, relay-open boot.
        watchdog.pet();

        // ---- report transitions ----------------------------------------
        if machine.state != last_state || machine.mode != last_mode {
            let r = machine.last;
            println!(
                "{} -> {} [{}] {:.1}V {:.2}A {}",
                format!("{last_state:?}").to_lowercase(),
                format!("{:?}", machine.state).to_lowercase(),
                machine.mode.label(),
                r.map_or(0.0, |x| x.volts),
                r.map_or(0.0, |x| x.amps),
                machine.detail
            );
            sound::announce(last_state, machine.state, last_mode, machine.mode);

            last_state = machine.state;
            last_mode = machine.mode;

            if machine.state == State::Lockout {
                save_lockout(machine.lockout_until, machine.dry_strikes);
            }
        }

        // ---- display ----------------------------------------------------
        // Just hand the rendered frame to the display thread. The thread owns
        // the i2c bus, retries a missing screen, and resyncs a corrupted one -
        // none of which can ever block this control loop.
        {
            let refusal = if now < refusal_until {
                refusal_msg.as_deref()
            } else {
                refusal_msg = None;
                None
            };
            display.set(display::render(&machine, &settings, now, ip.as_deref(), tick, refusal));
        }

        // ---- history ----------------------------------------------------
        // Sample every 5s while the pump runs and every 60s otherwise. Dry
        // running is a transition over tens of seconds, so a once-a-minute
        // log would miss the shape of it entirely - and that shape is what
        // the thresholds have to be tuned against later.
        if let Some(r) = reading {
            let interval = if machine.state.contactor_closed() { 5 } else { 60 };
            if now.saturating_sub(last_log) >= interval {
                last_log = now;
                append_history(&r, machine.state, machine.mode, machine.dry_would_fire);
            }
        }

        // ---- heartbeat ---------------------------------------------------
        // A forensic breadcrumb: the wall-clock time of the last completed
        // cycle, written async so it can never block the loop. The hardware
        // watchdog above is what actually catches a wedge; this just lets a
        // human (or a post-mortem of the SD card) see when the loop last ran.
        if now != last_beat {
            last_beat = now;
            persist(Job::Heartbeat(now));
        }

        // ---- publish -----------------------------------------------------
        {
            let mut s = shared.lock().expect("shared state poisoned");
            s.machine.mode = machine.mode;
            s.machine.state = machine.state;
            s.machine.last = machine.last;
            s.machine.stable = machine.stable;
            s.machine.reason = machine.reason;
            s.machine.detail = machine.detail.clone();
            s.machine.lockout_until = machine.lockout_until;
            s.machine.dry_strikes = machine.dry_strikes;
            s.machine.events = machine.events.clone();
            s.machine.starts_today = machine.starts_today;
            s.machine.run_seconds_today = machine.run_seconds_today;
            s.machine.energy_at_midnight = machine.energy_at_midnight;
            s.machine.tank_contact = machine.tank_contact;
            s.machine.well_contact = machine.well_contact;
            s.meter_ok = reading.is_some();
            s.lcd_ok = display.healthy();
            s.ip = ip.clone();
        }

        tick = tick.wrapping_add(1);
    }

    // ---- shutdown --------------------------------------------------------
    relay.set(false);
    // Intentional stop (SIGTERM from the service manager, a deploy): disable the
    // watchdog so the clean exit doesn't reboot the Pi. A crash/panic never
    // reaches here, leaving the watchdog armed - which is exactly what we want.
    watchdog.disarm();
    panel.set_led(panel::Led::Off);
    display.stop_with([
        display::LOGO.to_string(),
        "  controller stopped".to_string(),
        "  pump disconnected".to_string(),
        "  safe - not running".to_string(),
    ]);
    println!("stopped, contactor open");
}

/// SIGINT/SIGTERM handling without pulling in a crate.
///
/// The handler does exactly one thing: store into a static atomic. That is
/// the only operation guaranteed async-signal-safe, and it is all we need -
/// the main loop notices on its next tick and shuts down in its own context,
/// where opening the contactor is safe to do.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Set when the PHYSICAL mode selector is turned to OFF: the panel's far-left
/// position is a "stop everything" that cleanly powers the Pi down. Only the
/// hardware selector sets this - a web/API mode-off just parks the pump.
static POWER_OFF_REQUESTED: AtomicBool = AtomicBool::new(false);

/// When pumpd's control loop started, for the power-off grace period below.
static BOOT_AT: OnceLock<std::time::Instant> = OnceLock::new();

/// The OFF selector only powers the Pi down after this long. Boot takes far
/// longer than the first panel poll, so a switch left sitting at OFF at power-on
/// reports its position while uptime is still tiny - and must NOT trigger a
/// power-off, or the Pi would boot then halt then boot then halt for ever. A
/// deliberate flip to OFF while running is always well past this.
const POWER_OFF_GRACE: Duration = Duration::from_secs(20);

/// Pure, hardware-free decision: the physical OFF position powers the Pi down
/// only once the box has been up past the grace period (so a boot-time OFF read
/// cannot loop us). Extracted so it is unit-testable.
fn should_power_off(selector: Option<Mode>, uptime: Duration) -> bool {
    selector == Some(Mode::Off) && uptime >= POWER_OFF_GRACE
}

/// Incremented once per control-loop iteration; the software watchdog watches it
/// for progress.
/// Make stdout/stderr non-blocking so a wedged log consumer can never block a
/// write and hang the control loop. A full pipe then makes the write fail fast
/// (the print macro panics -> failsafe hook opens the relay -> restart) rather
/// than blocking indefinitely with the contactor possibly closed.
fn make_stdio_nonblocking() {
    // SAFETY: toggles O_NONBLOCK on this process's own stdout/stderr fds only.
    unsafe {
        for fd in [libc::STDOUT_FILENO, libc::STDERR_FILENO] {
            let flags = libc::fcntl(fd, libc::F_GETFL);
            if flags >= 0 {
                let _ = libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
            }
        }
    }
}

static LOOP_TICKS: AtomicU64 = AtomicU64::new(0);

/// The software watchdog polls every STALL_CHECK; STALL_LIMIT consecutive checks
/// with no loop progress (~24s) mean the loop is wedged. The loop iterates about
/// once a second, so this never fires on normal slowness.
const STALL_CHECK: Duration = Duration::from_secs(4);
const STALL_LIMIT: u32 = 6;

/// One pure step of the watchdog decision, factored out so it can be tested
/// without threads or timing. Given the previously seen tick count, the current
/// one, and the running stall count, returns the new (last, stalls, abort?).
fn watchdog_step(last: u64, cur: u64, stalls: u32, limit: u32) -> (u64, u32, bool) {
    if cur == last {
        let s = stalls + 1;
        (last, s, s >= limit)
    } else {
        (cur, 0, false)
    }
}

/// Software watchdog: if the control loop stops making progress, drive the relay
/// open and abort so the service manager restarts us (relay comes up open on
/// boot). Needs no hardware and catches ANY loop hang - including a serial flush
/// wedged in tcdrain, which no per-operation timeout covers. Complements the
/// opt-in hardware watchdog; this one is always on because aborting a genuinely
/// wedged process is always the right move and carries no boot-loop risk.
fn start_loop_watchdog() {
    std::thread::spawn(|| {
        let mut last = LOOP_TICKS.load(Ordering::SeqCst);
        let mut stalls = 0u32;
        loop {
            std::thread::sleep(STALL_CHECK);
            let cur = LOOP_TICKS.load(Ordering::SeqCst);
            let abort;
            (last, stalls, abort) = watchdog_step(last, cur, stalls, STALL_LIMIT);
            if abort {
                // Open the relay FIRST, before anything that could itself block
                // or fail (a wedged log sink may be the very reason we stalled).
                // open_relay() only writes the GPIO register - it never logs or
                // allocates - so the contactor opens even if logging cannot.
                failsafe::open_relay();
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr(),
                    "control loop made no progress for ~{}s - opened relay, aborting for restart",
                    STALL_LIMIT * STALL_CHECK.as_secs() as u32
                );
                std::process::abort();
            }
        }
    });
}

extern "C" fn on_signal(_sig: i32) {
    SHUTDOWN.store(true, Ordering::SeqCst);
}

fn install_signal_handlers() {
    extern "C" {
        fn signal(sig: i32, handler: extern "C" fn(i32)) -> usize;
    }
    // SAFETY: registering a handler that only performs an atomic store.
    unsafe {
        signal(2, on_signal); // SIGINT
        signal(15, on_signal); // SIGTERM
    }
}

#[cfg(test)]
mod tests {
    use super::watchdog_step;
    use super::{should_power_off, Mode, POWER_OFF_GRACE};
    use std::time::Duration;

    #[test]
    fn off_selector_powers_down_only_after_the_boot_grace() {
        // Boot-time OFF (switch sitting at OFF at power-on, uptime tiny) must NOT
        // power off - otherwise the Pi boots, halts, boots, halts for ever.
        assert!(!should_power_off(Some(Mode::Off), Duration::from_secs(0)));
        assert!(!should_power_off(Some(Mode::Off), POWER_OFF_GRACE - Duration::from_secs(1)));
        // A deliberate flip to OFF while running (past the grace) DOES power off.
        assert!(should_power_off(Some(Mode::Off), POWER_OFF_GRACE));
        assert!(should_power_off(Some(Mode::Off), Duration::from_secs(3600)));
        // Manual and Auto never power off; a no-change poll never powers off.
        assert!(!should_power_off(Some(Mode::Manual), Duration::from_secs(3600)));
        assert!(!should_power_off(Some(Mode::Auto), Duration::from_secs(3600)));
        assert!(!should_power_off(None, Duration::from_secs(3600)));
    }

    #[test]
    fn a_nonblocking_write_to_a_full_pipe_fails_fast_instead_of_blocking() {
        // The safety mechanism behind make_stdio_nonblocking(): once a log
        // consumer wedges and the pipe fills, a non-blocking write must return
        // EAGAIN immediately rather than block the caller (which, on the control
        // loop, would freeze protection). Proven on a private pipe so it never
        // touches the process's real stdout / the test harness.
        unsafe {
            let mut fds = [0i32; 2];
            assert_eq!(libc::pipe(fds.as_mut_ptr()), 0, "pipe() failed");
            let (r, w) = (fds[0], fds[1]);
            let flags = libc::fcntl(w, libc::F_GETFL);
            assert_eq!(
                libc::fcntl(w, libc::F_SETFL, flags | libc::O_NONBLOCK),
                0,
                "could not set O_NONBLOCK"
            );
            let buf = [b'x'; 4096];
            let mut hit_eagain = false;
            for _ in 0..100_000 {
                let n = libc::write(w, buf.as_ptr() as *const libc::c_void, buf.len());
                if n < 0 {
                    let e = std::io::Error::last_os_error().raw_os_error();
                    assert!(
                        e == Some(libc::EAGAIN) || e == Some(libc::EWOULDBLOCK),
                        "a full non-blocking pipe must fail with EAGAIN, got {e:?}"
                    );
                    hit_eagain = true;
                    break;
                }
            }
            assert!(
                hit_eagain,
                "the write must eventually fail fast on a full pipe, never block forever"
            );
            libc::close(r);
            libc::close(w);
        }
    }

    #[test]
    fn watchdog_resets_when_the_loop_makes_progress() {
        // cur advanced past last -> adopt cur, stalls cleared, no abort.
        let (last, stalls, abort) = watchdog_step(5, 6, 3, 6);
        assert_eq!((last, stalls, abort), (6, 0, false));
    }

    #[test]
    fn watchdog_accumulates_stalls_but_holds_until_the_limit() {
        // Same tick count seen again: stall count rises, no abort below limit.
        let (last, stalls, abort) = watchdog_step(5, 5, 0, 6);
        assert_eq!((last, stalls, abort), (5, 1, false));
        let (_last, stalls, abort) = watchdog_step(5, 5, 4, 6);
        assert_eq!((stalls, abort), (5, false), "one short of the limit must not abort");
    }

    #[test]
    fn watchdog_aborts_exactly_at_the_limit() {
        // The stall that reaches the limit triggers the abort.
        let (_last, stalls, abort) = watchdog_step(5, 5, 5, 6);
        assert_eq!((stalls, abort), (6, true), "the limit-th stall must abort");
    }

    #[test]
    fn first_iface_ipv4_returns_a_valid_non_loopback_address_if_any() {
        // Validates the getifaddrs FFI (byte order + formatting): whenever it
        // returns an address it must be a real, parseable IPv4 - never garbage,
        // loopback, or link-local. (None is correct on an isolated host.)
        if let Some(ip) = super::first_iface_ipv4() {
            let parsed: std::net::Ipv4Addr = ip.parse().expect("must be a valid IPv4");
            assert!(!parsed.is_loopback(), "must skip 127.x, got {ip}");
            assert!(!parsed.is_link_local(), "must skip 169.254.x, got {ip}");
        }
    }
}
