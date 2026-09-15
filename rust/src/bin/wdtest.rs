//! Watchdog behaviour probe for the QEMU/hardware environment (NOT shipped).
//! Modes: starve | disarm | peek (instrumented, raw ioctls, localises timing).

#[path = "../watchdog.rs"]
mod watchdog;

use std::io::Write;
use std::time::Duration;

fn say(s: &str) {
    println!("{s}");
    let _ = std::io::stdout().flush();
}

const WDIOC_SETTIMEOUT: u32 = 0xC004_5706;
const WDIOC_GETTIMEOUT: u32 = 0x8004_5707;

fn get_to(fd: i32) -> i32 {
    let mut v: i32 = -1;
    unsafe { libc::ioctl(fd, WDIOC_GETTIMEOUT as _, &mut v as *mut i32) };
    v
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_default();

    if mode == "peek" {
        say("PEEK opening /dev/watchdog");
        let fd = unsafe {
            libc::open(b"/dev/watchdog\0".as_ptr() as *const libc::c_char, libc::O_WRONLY | libc::O_CLOEXEC)
        };
        say(&format!("PEEK fd={fd}"));
        if fd < 0 { return; }
        say(&format!("PEEK default timeout={}s", get_to(fd)));
        let mut want: i32 = 15;
        let sr = unsafe { libc::ioctl(fd, WDIOC_SETTIMEOUT as _, &mut want as *mut i32) };
        say(&format!("PEEK set(15) rc={sr}, now timeout={}s", get_to(fd)));
        for i in 0..24 {
            std::thread::sleep(Duration::from_millis(500));
            let b = [b'\0'];
            unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1); }
            say(&format!("PEEK pet {i} (t={}ms)", (i + 1) * 500));
        }
        say("PEEK survived 12s of 0.5s petting -> pet works, timeout holds");
        let v = [b'V'];
        unsafe { libc::write(fd, v.as_ptr() as *const libc::c_void, 1); libc::close(fd); }
        say("PEEK magic-closed");
        for t in 0..8 { std::thread::sleep(Duration::from_millis(1000)); say(&format!("PEEK alive-after-close t={t}s")); }
        say("PEEK DISARM-OK");
        return;
    }

    say(&format!("WDTEST mode={mode} arming (want 2s)"));
    let mut wd = watchdog::Watchdog::arm(2);
    for i in 0..3 {
        std::thread::sleep(Duration::from_millis(700));
        wd.pet();
        say(&format!("WDTEST pet {i}"));
    }
    match mode.as_str() {
        "starve" => {
            say("WDTEST starving now");
            for t in 0..25 { std::thread::sleep(Duration::from_millis(1000)); say(&format!("WDTEST unpetted t={t}s")); }
            say("WDTEST NO-RESET-FAIL");
            std::process::exit(1);
        }
        "disarm" => {
            wd.disarm();
            say("WDTEST disarmed - staying alive");
            for t in 0..8 { std::thread::sleep(Duration::from_millis(1000)); say(&format!("WDTEST alive t={t}s")); }
            say("WDTEST DISARM-OK");
        }
        _ => {
            let _ = watchdog::Watchdog::disabled(); // exercise the no-op path
            eprintln!("usage: wdtest starve|disarm|peek");
        }
    }
}
