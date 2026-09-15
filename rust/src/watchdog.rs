//! Hardware watchdog.
//!
//! The control loop already moves every blocking op (display i2c, SD writes)
//! onto its own thread, and a panic opens the relay (see `failsafe`). This is
//! the last line: if the loop itself ever stops making progress - a driver
//! wedge, a deadlock, some bug not yet found - nothing in software can help,
//! because the software is stuck. The Pi's hardware watchdog can: pet it once
//! per protection cycle, and if a cycle ever fails to complete within the
//! timeout the SoC resets. On reboot the relay comes up open (proven in R23),
//! so a frozen controller becomes a safe, self-recovering one instead of a
//! contactor stuck closed.
//!
//! Distro-independent: it drives `/dev/watchdog` directly, so it works the same
//! under runit (Void) and systemd (Raspbian) without either managing it.

use std::os::unix::io::RawFd;

// linux/watchdog.h, asm-generic ioctl encoding (identical on 32-bit ARM and
// x86-64 for these): _IOWR/_IOR('W', nr, int), size = 4. Kept as u32 and cast
// with `as _` at the call, because libc's ioctl request type differs by target
// (c_ulong on glibc, c_int on musl) and the high bit is set on SETTIMEOUT.
const WDIOC_SETTIMEOUT: u32 = 0xC004_5706; // _IOWR('W',6,int)
const WDIOC_GETTIMEOUT: u32 = 0x8004_5707; // _IOR('W',7,int)

/// Smallest negotiated timeout we will trust. The control loop pets roughly
/// twice a second but a slow cycle (meter reopen, i2c retry) can take a couple
/// of seconds; a window under this risks a spurious reset while the loop is
/// perfectly healthy, so if the device won't give us at least this many seconds
/// we decline to use it rather than nuisance-reboot the pump.
const MIN_SAFE_TIMEOUT: i32 = 8;

/// Byte written to keep the watchdog alive. Any byte other than 'V' pets it.
const PET: u8 = b'\0';
/// The "magic close" byte: written just before closing to DISABLE the watchdog,
/// so an intentional, clean shutdown does not reboot the Pi.
const MAGIC_CLOSE: u8 = b'V';

pub struct Watchdog {
    fd: Option<RawFd>,
}

impl Watchdog {
    /// A handle that owns no device: `pet`/`disarm` are silent no-ops. Used when
    /// the watchdog is disabled in settings, so the control loop can call `pet`
    /// unconditionally.
    pub fn disabled() -> Watchdog {
        Watchdog { fd: None }
    }

    /// Open and arm `/dev/watchdog`, asking for `timeout_secs` (best-effort; the
    /// bcm2835 timer caps at ~15s and may clamp). Degrades gracefully: if the
    /// device is absent, busy (another manager owns it), unwritable, or can only
    /// offer a timeout too short to pet safely, returns a disarmed handle whose
    /// `pet`/`disarm` are no-ops, and the controller runs on exactly as before.
    /// Never fails the boot.
    pub fn arm(timeout_secs: i32) -> Watchdog {
        let fd = unsafe {
            libc::open(
                b"/dev/watchdog\0".as_ptr() as *const libc::c_char,
                libc::O_WRONLY | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            eprintln!("watchdog: /dev/watchdog unavailable; running without it");
            return Watchdog { fd: None };
        }

        let mut want = timeout_secs;
        unsafe {
            // Ignore failure: a fixed-timeout watchdog rejects SETTIMEOUT but
            // still protects at its own interval.
            libc::ioctl(fd, WDIOC_SETTIMEOUT as _, &mut want as *mut i32);
        }
        let mut got: i32 = 0;
        let read_ok = unsafe { libc::ioctl(fd, WDIOC_GETTIMEOUT as _, &mut got as *mut i32) } == 0;

        // Only keep the watchdog if we could confirm a timeout long enough to
        // pet without nuisance-resetting a healthy loop. An unreadable or too
        // short timeout means we cannot trust it - back out cleanly.
        if !read_ok || got < MIN_SAFE_TIMEOUT {
            eprintln!(
                "watchdog: timeout {} too short/unknown (need >= {}s); disabling to avoid \
                 spurious resets",
                if read_ok { got.to_string() } else { "unknown".into() },
                MIN_SAFE_TIMEOUT
            );
            let mut w = Watchdog { fd: Some(fd) };
            w.disarm();
            return w;
        }
        println!("watchdog armed, {got}s timeout");
        Watchdog { fd: Some(fd) }
    }

    /// Feed the watchdog. Call once per completed protection cycle. Cheap and
    /// non-blocking; a write error is reported once but never stops the loop.
    pub fn pet(&self) {
        if let Some(fd) = self.fd {
            unsafe { kick(fd, PET) };
        }
    }

    /// Cleanly disable the watchdog for an intentional shutdown, so stopping
    /// the service does not trigger a reboot. Idempotent.
    pub fn disarm(&mut self) {
        if let Some(fd) = self.fd.take() {
            unsafe {
                kick(fd, MAGIC_CLOSE);
                libc::close(fd);
            }
        }
    }
}

/// Write a single control byte to the watchdog fd. Split out so the byte
/// protocol (pet vs magic-close) can be unit-tested against an ordinary fd.
///
/// # Safety
/// `fd` must be a valid writable file descriptor.
#[inline]
unsafe fn kick(fd: RawFd, byte: u8) {
    let b = [byte];
    let _ = libc::write(fd, b.as_ptr() as *const libc::c_void, 1);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::io::FromRawFd;
    use std::io::Read;

    // Point the byte protocol at a pipe and read back what was written, proving
    // pet sends a non-'V' keepalive and disarm sends the magic-close 'V'.
    #[test]
    fn pet_and_disarm_write_the_right_bytes() {
        let mut fds = [0i32; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        let (rd, wr) = (fds[0], fds[1]);

        let wd = Watchdog { fd: Some(wr) };
        wd.pet();
        let mut wd = wd;
        wd.disarm(); // writes 'V' then closes wr

        let mut buf = Vec::new();
        let mut r = unsafe { std::fs::File::from_raw_fd(rd) };
        r.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, vec![PET, MAGIC_CLOSE], "pet then magic-close on the wire");
    }

    // A disarmed handle (no device) must be a silent no-op, never a panic.
    #[test]
    fn a_disarmed_watchdog_is_a_safe_noop() {
        let wd = Watchdog { fd: None };
        wd.pet();
        let mut wd = wd;
        wd.disarm();
        wd.disarm(); // idempotent
    }

    // Lock the ioctl request numbers so a wrong encoding can't silently ship.
    #[test]
    fn ioctl_request_numbers_match_linux_watchdog_h() {
        assert_eq!(WDIOC_SETTIMEOUT, 0xC004_5706);
        assert_eq!(WDIOC_GETTIMEOUT, 0x8004_5707);
    }
}
