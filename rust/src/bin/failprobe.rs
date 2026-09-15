//! End-to-end proof for the panic failsafe (NOT shipped in any image).
//!
//! Exercises the REAL `failsafe::arm` hook against real GPIO registers, so we
//! can observe that a genuine panic actually drives the relay pin low.
//!
//! Run in the QEMU Void box (which provides /dev/mem GPIO), one mode per
//! invocation; the pin level persists in hardware between processes:
//!   failprobe sethigh   -> drive GPIO17 high, exit WITHOUT resetting it
//!   failprobe read      -> print the live level of GPIO17
//!   failprobe armpanic  -> drive GPIO17 high, arm the failsafe, then panic
//! Expected: after `sethigh` a `read` sees 1 (high persists across processes);
//! after `armpanic` a `read` sees 0 (the panic hook opened the contactor).

#[path = "../failsafe.rs"]
mod failsafe;

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();
    let pin: u8 = 17;
    let base = match failsafe::map_for_probe() {
        Some(b) => b,
        None => {
            eprintln!("failprobe: cannot map GPIO (need /dev/gpiomem or /dev/mem as root)");
            std::process::exit(3);
        }
    };

    match mode.as_str() {
        "sethigh" => {
            unsafe { failsafe::set_output_high(base, pin) };
            println!("SETHIGH level={}", unsafe { failsafe::read_level(base, pin) });
            // Leave the pin driven high: exit without running any Drop, so the
            // next process can confirm an output level persists in hardware.
            unsafe { libc::_exit(0) };
        }
        "read" => {
            println!("PIN{}={}", pin, unsafe { failsafe::read_level(base, pin) });
        }
        "armpanic" => {
            unsafe { failsafe::set_output_high(base, pin) };
            println!("ARMPANIC before={}", unsafe { failsafe::read_level(base, pin) });
            failsafe::arm(pin); // the production hook under test
            println!("ARMPANIC panicking now");
            panic!("failprobe induced panic - the hook must open the relay");
        }
        "softkill" => {
            // Exactly what the software watchdog does on a wedged loop: relay
            // is CLOSED (high), then open_relay() drives it low and abort()
            // ends the process (NOT a panic - the panic hook does not run). The
            // pin must read low afterward, proving the watchdog opens the relay.
            failsafe::arm(pin); // maps the GPIO block for open_relay
            unsafe { failsafe::set_output_high(base, pin) };
            println!("SOFTKILL before={}", unsafe { failsafe::read_level(base, pin) });
            failsafe::open_relay(); // the watchdog's action
            println!("SOFTKILL opened={}", unsafe { failsafe::read_level(base, pin) });
            std::process::abort();
        }
        _ => {
            eprintln!("usage: failprobe sethigh|read|armpanic|softkill");
            std::process::exit(2);
        }
    }
}
