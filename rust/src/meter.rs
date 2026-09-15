//! PZEM-017 DC energy meter, Modbus RTU over RS485.
//!
//! Hand-rolled rather than pulling in a Modbus crate: this is four register
//! reads and the framing is trivial, so a dependency would be more surface
//! area than code.
//!
//! Works with either the USB-RS485 cable from the PZEM kit (the transceiver
//! is in the plug and switches direction itself) or a MAX485 on the GPIO
//! UART, in which case we drive DE/RE ourselves.

use std::io::Write;
use std::time::Duration;

pub const SHUNT_100A: u16 = 0;
pub const SHUNT_50A: u16 = 1;
pub const SHUNT_200A: u16 = 2;
pub const SHUNT_300A: u16 = 3;

pub fn shunt_name(v: u16) -> &'static str {
    match v {
        SHUNT_100A => "100A",
        SHUNT_50A => "50A",
        SHUNT_200A => "200A",
        SHUNT_300A => "300A",
        _ => "unknown",
    }
}

/// Full-scale current (amps) the PZEM can read on each shunt. The meter
/// saturates here, so an overcurrent threshold set at or above this value can
/// never be exceeded by a reading - overcurrent would be silently defeated.
pub fn shunt_full_scale(v: u16) -> Option<f32> {
    match v {
        SHUNT_100A => Some(100.0),
        SHUNT_50A => Some(50.0),
        SHUNT_200A => Some(200.0),
        SHUNT_300A => Some(300.0),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Reading {
    pub volts: f32,
    pub amps: f32,
    pub watts: f32,
    pub watt_hours: f32,
}

#[derive(Debug)]
pub enum MeterError {
    /// No reply at all. Usually means the meter has no power - it draws
    /// from IN+/IN- and needs roughly 7V before it will talk.
    Silent,
    Short { got: usize, want: usize },
    Crc,
    Address(u8),
    Device(u8),
    Io(std::io::Error),
}

impl std::fmt::Display for MeterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MeterError::Silent => write!(f, "no reply (is the meter powered?)"),
            MeterError::Short { got, want } => write!(f, "short reply {got}/{want}"),
            MeterError::Crc => write!(f, "CRC mismatch"),
            MeterError::Address(a) => write!(f, "reply from address {a:#04x}"),
            MeterError::Device(c) => write!(f, "device error {c:#04x}"),
            MeterError::Io(e) => write!(f, "io: {e}"),
        }
    }
}

impl From<std::io::Error> for MeterError {
    fn from(e: std::io::Error) -> Self {
        MeterError::Io(e)
    }
}

/// Read up to `buf.len()` bytes, bounded by BOTH the reader's own per-read
/// timeout AND `deadline`. Returns how many bytes were read; the caller decides
/// whether that is a whole frame. The deadline is what makes a single
/// transaction bounded in time even against a meter that trickles bytes.
fn read_frame<R: std::io::Read + ?Sized>(
    port: &mut R,
    buf: &mut [u8],
    deadline: std::time::Instant,
) -> Result<usize, MeterError> {
    let want = buf.len();
    let mut got = 0;
    while got < want {
        if std::time::Instant::now() >= deadline {
            break;
        }
        match port.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(MeterError::Io(e)),
        }
    }
    Ok(got)
}

fn crc16(data: &[u8]) -> u16 {
    let mut crc: u16 = 0xFFFF;
    for &b in data {
        crc ^= b as u16;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xA001;
            } else {
                crc >>= 1;
            }
        }
    }
    crc
}

fn frame(payload: &[u8]) -> Vec<u8> {
    let mut out = payload.to_vec();
    out.extend_from_slice(&crc16(payload).to_le_bytes());
    out
}

pub struct Meter {
    port: Box<dyn serialport::SerialPort>,
    address: u8,
    /// GPIO driving DE+RE on a MAX485. None when the USB cable handles it.
    direction: Option<rppal::gpio::OutputPin>,
}

impl Meter {
    pub fn open(path: &str, address: u8, de_pin: Option<u8>) -> Result<Self, String> {
        let port = serialport::new(path, 9600)
            .data_bits(serialport::DataBits::Eight)
            .parity(serialport::Parity::None)
            // The PZEM uses two stop bits. One stop bit reads as framing
            // errors that look like random CRC failures.
            .stop_bits(serialport::StopBits::Two)
            .timeout(Duration::from_millis(600))
            .open()
            .map_err(|e| format!("cannot open {path}: {e}"))?;

        let direction = match de_pin {
            Some(pin) => {
                let gpio = rppal::gpio::Gpio::new().map_err(|e| e.to_string())?;
                let mut p = gpio.get(pin).map_err(|e| e.to_string())?.into_output();
                p.set_low();
                Some(p)
            }
            None => None,
        };

        Ok(Meter { port, address, direction })
    }

    fn transact(&mut self, payload: &[u8], want: usize) -> Result<Vec<u8>, MeterError> {
        let msg = frame(payload);

        if let Some(p) = &mut self.direction {
            p.set_high();
        }
        let _ = self.port.clear(serialport::ClearBuffer::Input);
        self.port.write_all(&msg)?;
        self.port.flush()?;

        if let Some(p) = &mut self.direction {
            // Hold the driver enabled until the last bit is physically out,
            // or the tail of the frame is truncated. 11 bits per byte.
            std::thread::sleep(Duration::from_micros(
                (msg.len() as u64 * 11 * 1_000_000) / 9600 + 1000,
            ));
            p.set_low();
        }

        // Bound the WHOLE read, not just each read() call. The port's per-read
        // timeout caps a single read, but this loop repeats until `want` bytes
        // arrive - so a meter dribbling one byte just under each timeout keeps
        // every read() returning Ok(1) and stretches one transaction to
        // want * timeout (~15s), freezing the control loop that whole time. A
        // total deadline caps a sick meter at about one timeout; the resulting
        // Short/Silent is then handled as a normal missed reading (fail-safe).
        let mut buf = vec![0u8; want];
        let deadline = std::time::Instant::now() + Duration::from_millis(600);
        let got = read_frame(&mut *self.port, &mut buf, deadline)?;

        if got == 0 {
            return Err(MeterError::Silent);
        }
        if got < want {
            return Err(MeterError::Short { got, want });
        }
        if buf[0] != self.address {
            return Err(MeterError::Address(buf[0]));
        }
        if buf[1] & 0x80 != 0 {
            return Err(MeterError::Device(buf[2]));
        }

        let given = u16::from_le_bytes([buf[want - 2], buf[want - 1]]);
        if crc16(&buf[..want - 2]) != given {
            return Err(MeterError::Crc);
        }

        Ok(buf)
    }

    pub fn read(&mut self) -> Result<Reading, MeterError> {
        let mut req = vec![self.address, 0x04];
        req.extend_from_slice(&0u16.to_be_bytes());
        req.extend_from_slice(&8u16.to_be_bytes());

        let r = self.transact(&req, 21)?;
        Ok(decode_measurement(&r))
    }

    pub fn shunt(&mut self) -> Result<u16, MeterError> {
        let mut req = vec![self.address, 0x03];
        req.extend_from_slice(&3u16.to_be_bytes());
        req.extend_from_slice(&1u16.to_be_bytes());
        let r = self.transact(&req, 7)?;
        Ok(u16::from_be_bytes([r[3], r[4]]))
    }

    /// Persisted in the meter. Wrong value scales every current reading -
    /// a 50A shunt reported as 100A doubles the amps.
    pub fn set_shunt(&mut self, shunt: u16) -> Result<(), MeterError> {
        let mut req = vec![self.address, 0x06];
        req.extend_from_slice(&3u16.to_be_bytes());
        req.extend_from_slice(&shunt.to_be_bytes());
        self.transact(&req, 8)?;
        Ok(())
    }
}

/// Decode a validated PZEM-017 function-0x04 measurement frame - address,
/// function, byte count, then eight big-endian registers - into real units.
/// Split out from the I/O so the scaling is testable without a serial port: a
/// wrong divisor here would mis-report volts/amps and skew EVERY protection
/// threshold (over/under-voltage, over-current, amp dry-run). PZEM-017 register
/// map: voltage 0.01 V, current 0.01 A, power 0.1 W (32-bit), energy 1 Wh.
fn decode_measurement(r: &[u8]) -> Reading {
    let reg = |i: usize| u16::from_be_bytes([r[3 + i * 2], r[4 + i * 2]]);
    Reading {
        volts: reg(0) as f32 / 100.0,
        amps: reg(1) as f32 / 100.0,
        watts: ((reg(2) as u32) | ((reg(3) as u32) << 16)) as f32 / 10.0,
        watt_hours: ((reg(4) as u32) | ((reg(5) as u32) << 16)) as f32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read-input-registers request for address 1, registers 0-7. Value
    /// cross-checked against an independent CRC-16/MODBUS implementation.
    const READ_REQ: [u8; 6] = [0x01, 0x04, 0x00, 0x00, 0x00, 0x08];
    const READ_REQ_CRC: u16 = 0xCCF1;

    #[test]
    fn crc_matches_known_frame() {
        assert_eq!(crc16(&READ_REQ), READ_REQ_CRC);
    }

    use std::time::{Duration, Instant};

    // A meter that hands back one byte per read, slowly. Without a total
    // deadline this trickle would keep read() returning Ok(1) and stretch a
    // single frame read to want * per-read-timeout.
    struct Dribble {
        per_call: Duration,
    }
    impl std::io::Read for Dribble {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            std::thread::sleep(self.per_call);
            if buf.is_empty() {
                return Ok(0);
            }
            buf[0] = 0xAB;
            Ok(1)
        }
    }

    #[test]
    fn a_dribbling_meter_cannot_stall_the_read_past_the_deadline() {
        // 100 bytes at 20ms each would be ~2s unbounded; the deadline must cut
        // it off near 200ms so the control loop is never frozen by a sick meter.
        let mut d = Dribble { per_call: Duration::from_millis(20) };
        let want = 100usize;
        let mut buf = vec![0u8; want];
        let start = Instant::now();
        let got = read_frame(&mut d, &mut buf, start + Duration::from_millis(200)).unwrap();
        let elapsed = start.elapsed();
        assert!(got < want, "deadline must cut the read off (got {got}/{want})");
        assert!(
            elapsed < Duration::from_millis(600),
            "read must return near the deadline, took {elapsed:?}"
        );
    }

    // A healthy meter that returns the whole frame at once still reads fully.
    struct OneShot(Vec<u8>);
    impl std::io::Read for OneShot {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let n = self.0.len().min(buf.len());
            buf[..n].copy_from_slice(&self.0[..n]);
            self.0.drain(..n);
            Ok(n)
        }
    }

    #[test]
    fn a_prompt_full_frame_reads_completely_before_the_deadline() {
        let want = 25usize;
        let mut src = OneShot(vec![0x11; want]);
        let mut buf = vec![0u8; want];
        let got = read_frame(&mut src, &mut buf, Instant::now() + Duration::from_secs(5)).unwrap();
        assert_eq!(got, want, "a prompt full frame must read completely");
        assert!(buf.iter().all(|&b| b == 0x11));
    }

    #[test]
    fn frame_appends_crc_little_endian() {
        let f = frame(&READ_REQ);
        assert_eq!(f.len(), 8);
        assert_eq!(&f[6..], &READ_REQ_CRC.to_le_bytes());
    }

    #[test]
    fn crc_detects_a_flipped_bit() {
        let mut corrupt = READ_REQ;
        corrupt[3] ^= 0x01;
        assert_ne!(crc16(&corrupt), READ_REQ_CRC);
    }

    #[test]
    fn decodes_a_measurement_frame_to_real_units() {
        // A nominal running frame: 138.00 V, 5.00 A, 690.0 W, 1234 Wh.
        // Registers are big-endian; power and energy are lo|hi<<16.
        let mut r = vec![0x01, 0x04, 16];
        for reg in [13800u16, 500, 6900, 0, 1234, 0, 0, 0] {
            r.extend_from_slice(&reg.to_be_bytes());
        }
        let m = decode_measurement(&r);
        assert_eq!(m.volts, 138.0, "voltage scales at 0.01V/count");
        assert_eq!(m.amps, 5.0, "current scales at 0.01A/count");
        assert_eq!(m.watts, 690.0, "power scales at 0.1W/count");
        assert_eq!(m.watt_hours, 1234.0, "energy is 1Wh/count");
    }

    #[test]
    fn decodes_a_32bit_power_across_both_registers() {
        // Power over 6553.5 W needs the high register: 0x0001_0000 counts =
        // 65536 * 0.1 = 6553.6 W. Guards the lo|hi<<16 assembly + divisor.
        let mut r = vec![0x01, 0x04, 16];
        for reg in [13000u16, 4800, 0x0000, 0x0001, 0, 0, 0, 0] {
            r.extend_from_slice(&reg.to_be_bytes());
        }
        let m = decode_measurement(&r);
        assert_eq!(m.watts, 6553.6, "high power register must contribute");
    }
}
