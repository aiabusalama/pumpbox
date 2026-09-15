//! Panic failsafe for the contactor.
//!
//! `Cargo.toml` sets `panic = "abort"` (a wedged controller must die and be
//! restarted by the service manager, not limp on). Abort SKIPS unwinding, so
//! `Drop` does NOT run - the `Relay`'s pin is never released, and a panic while
//! the pump is RUNNING would leave the contactor CLOSED until the service
//! restarts (seconds of unprotected running - the burn scenario).
//!
//! `std::panic::set_hook` still runs under `panic = "abort"` (the hook fires,
//! then the process aborts). So we install a hook that drives the relay pin LOW
//! directly, through our own tiny GPIO mapping, before the process dies.
//!
//! The write is a single volatile store to the write-1-to-clear GPCLR0 register
//! for exactly the relay bit: no read-modify-write, no allocation, no lock, so
//! it is safe to run from inside a panic on any thread. It only affects the
//! relay pin, and only drives it low if the pin is still an output - which it is
//! whenever the pump is running, the one moment this matters.

use std::sync::atomic::{AtomicPtr, Ordering};

/// Word offset of GPCLR0 (0x28) in the GPIO register block. Writing `1 << pin`
/// here drives that pin low without disturbing any other pin.
const GPCLR0: usize = 0x28 / 4;
/// GPSET0 (0x1c): write `1 << pin` to drive a pin high. Only the failprobe
/// end-to-end test binary uses this; pumpd drives the relay through rppal.
#[allow(dead_code)]
pub const GPSET0: usize = 0x1c / 4;
/// GPLEV0 (0x34): reading bit `pin` gives that pin's live level. Used by the
/// failprobe test binary to observe the pin across a panic.
#[allow(dead_code)]
pub const GPLEV0: usize = 0x34 / 4;
/// Bytes to map: enough to cover GPCLR0. rppal maps 61 registers; match it.
const MAP_WORDS: usize = 61;

/// Base of our GPIO mapping, or null until armed. Loaded by the panic hook.
static GPIO_BASE: AtomicPtr<u32> = AtomicPtr::new(std::ptr::null_mut());
/// Relay bit mask, stored as a word so the hook needs no second atomic type.
static RELAY_MASK: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

/// Drive the relay bit low on a live GPIO block. Pure given `base`: writes
/// `mask` to GPCLR0. Separated out so it can be unit-tested against an ordinary
/// buffer with no hardware.
///
/// # Safety
/// `base` must point to at least `MAP_WORDS` writable `u32`s (a real GPIO
/// mapping or a test buffer of that size).
#[inline(always)]
unsafe fn drive_low(base: *mut u32, mask: u32) {
    std::ptr::write_volatile(base.add(GPCLR0), mask);
}

/// The peripheral base for `/dev/mem`, by SoC. `/dev/gpiomem` maps at offset 0
/// and needs none of this; this is only the fallback when gpiomem is absent
/// (e.g. under QEMU). Mirrors rppal's own table so our mapping lands on the
/// exact registers pumpd's rppal is already driving.
fn devmem_gpio_base() -> Option<libc::off_t> {
    use rppal::system::{DeviceInfo, SoC};
    // Only the deployment target (Pi Zero W, BCM2835) and the QEMU raspi2b test
    // rig (BCM2836/2837) need the /dev/mem fallback; on the real Pi, gpiomem is
    // used and this is never reached. Pi4/Pi5 bases don't fit a 32-bit off_t on
    // armhf and aren't targets, so decline rather than map a wrong address.
    let peripheral: libc::off_t = match DeviceInfo::new().ok()?.soc() {
        SoC::Bcm2835 => 0x2000_0000,
        SoC::Bcm2836 | SoC::Bcm2837A1 | SoC::Bcm2837B0 => 0x3f00_0000,
        _ => return None,
    };
    Some(peripheral + 0x20_0000) // GPIO_OFFSET
}

/// mmap the GPIO block: `/dev/gpiomem` at offset 0 (the field Pi, as root),
/// else `/dev/mem` at the SoC's GPIO base (QEMU fallback). Returns the base.
fn map_gpio() -> Option<*mut u32> {
    use std::os::unix::io::AsRawFd;
    let size = MAP_WORDS * std::mem::size_of::<u32>();

    let try_map = |path: &str, off: libc::off_t| -> Option<*mut u32> {
        let f = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .ok()?;
        let p = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED,
                f.as_raw_fd(),
                off,
            )
        };
        if p == libc::MAP_FAILED {
            None
        } else {
            Some(p as *mut u32)
        }
    };

    if let Some(p) = try_map("/dev/gpiomem", 0) {
        return Some(p);
    }
    let base = devmem_gpio_base()?;
    try_map("/dev/mem", base)
}

/// Map the GPIO block the same way `arm` does. Exposed for the failprobe
/// end-to-end test binary so it observes the exact registers the failsafe
/// drives; pumpd itself never calls this.
#[allow(dead_code)]
pub fn map_for_probe() -> Option<*mut u32> {
    map_gpio()
}

/// Configure `pin` as an output and drive it high (relay CLOSED). Test-only.
///
/// # Safety
/// `base` must be a live GPIO mapping of at least `MAP_WORDS` words.
#[allow(dead_code)]
pub unsafe fn set_output_high(base: *mut u32, pin: u8) {
    let fsel = base.add((pin / 10) as usize);
    let shift = ((pin % 10) * 3) as u32;
    let v = std::ptr::read_volatile(fsel);
    std::ptr::write_volatile(fsel, (v & !(0b111 << shift)) | (0b001 << shift)); // output
    std::ptr::write_volatile(base.add(GPSET0), 1 << pin);
}

/// Read the live level of `pin` (0 or 1). Test-only.
///
/// # Safety
/// `base` must be a live GPIO mapping of at least `MAP_WORDS` words.
#[allow(dead_code)]
pub unsafe fn read_level(base: *mut u32, pin: u8) -> u32 {
    (std::ptr::read_volatile(base.add(GPLEV0)) >> pin) & 1
}

/// Drive the relay pin low NOW, through the armed mapping, if armed (else a
/// no-op). One volatile store, no allocation or lock, so it is safe to call from
/// a panic hook OR a watchdog thread that is about to abort a wedged process.
pub fn open_relay() {
    let base = GPIO_BASE.load(Ordering::SeqCst);
    if !base.is_null() {
        let mask = RELAY_MASK.load(Ordering::SeqCst);
        unsafe { drive_low(base, mask) };
    }
}

/// Arm the failsafe for `pin` and install the panic hook. Best-effort: if the
/// GPIO can't be mapped (no hardware, no permission) the hook is still set but
/// becomes a no-op, so normal operation is never affected. Call once at startup
/// AFTER the relay pin is configured as an output.
pub fn arm(pin: u8) {
    if let Some(base) = map_gpio() {
        RELAY_MASK.store(1u32 << pin, Ordering::SeqCst);
        GPIO_BASE.store(base, Ordering::SeqCst);
        println!("panic failsafe armed on GPIO{pin}");
    } else {
        eprintln!("panic failsafe: GPIO map unavailable; relay will rely on service restart");
    }

    let prev = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Open the contactor FIRST, before anything that might itself fail.
        open_relay();
        // Then let the normal panic message print for the logs.
        prev(info);
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    // The register math: GPCLR0 lands at word 10 (0x28/4) and only the relay
    // bit is written, nothing else in the block is touched.
    #[test]
    fn drive_low_writes_only_gpclr0_with_only_the_relay_bit() {
        let mut regs = vec![0u32; MAP_WORDS];
        let pin = 17u8;
        unsafe { drive_low(regs.as_mut_ptr(), 1 << pin) };

        assert_eq!(regs[GPCLR0], 1 << 17, "GPCLR0 must clear exactly pin 17");
        assert_eq!(GPCLR0, 10, "GPCLR0 is byte 0x28 = word 10");
        for (i, w) in regs.iter().enumerate() {
            if i != GPCLR0 {
                assert_eq!(*w, 0, "register word {i} must be untouched");
            }
        }
    }

    // A different pin clears a different bit, never bit 17.
    #[test]
    fn drive_low_targets_the_given_pin() {
        let mut regs = vec![0u32; MAP_WORDS];
        unsafe { drive_low(regs.as_mut_ptr(), 1 << 21) };
        assert_eq!(regs[GPCLR0], 1 << 21);
        assert_eq!(regs[GPCLR0] & (1 << 17), 0);
    }

    // The armed hook, when it fires, drives the relay bit into the mapped
    // block. Proves the static plumbing (base + mask -> GPCLR0), i.e. that a
    // panic would actually open the contactor. Uses a local buffer as the
    // "GPIO block" so no hardware is needed.
    #[test]
    fn the_hook_clears_the_relay_bit_when_it_fires() {
        let mut regs = vec![0u32; MAP_WORDS];
        RELAY_MASK.store(1 << 17, Ordering::SeqCst);
        GPIO_BASE.store(regs.as_mut_ptr(), Ordering::SeqCst);

        // Exercise the public path the panic hook AND the watchdog both use.
        open_relay();

        assert_eq!(regs[GPCLR0], 1 << 17, "open_relay must clear the relay pin");

        // Leave the statics disarmed so a real panic elsewhere in the test
        // binary can't scribble on freed test memory.
        GPIO_BASE.store(std::ptr::null_mut(), Ordering::SeqCst);
    }
}
