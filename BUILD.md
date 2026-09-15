# Solar Pump Protection — Build Guide

## Why

**Pump:** 138 V 1500 W DC brushless submersible, 11 A max, 110–150 V range.
Its controller card is destroyed above 150 V. It already self-protects below 110 V.

**Array:** 3 × GSE-HC455 in series. Voc 50.30 V each → **150.9 V at 25 °C**,
climbing toward 165 V on a cold morning. That voltage is present whenever no
current is flowing — which is exactly the situation at sunrise, before the pump
starts. That is what killed the first pump.

The contactor stays open until the array has settled into a safe band, and
drops out again on overvoltage, undervoltage, overcurrent, a dry well, or loss
of contact with the meter.

---

## Shopping list

### Already have

| Item | Note |
|------|------|
| Raspberry Pi Zero W | brain, WiFi |
| PZEM-017 + 50 A shunt | volts, amps, watts |
| 20×4 I²C LCD | local display |
| 5 V relay module | drives the contactor coil |
| HLK-10M24 | 143 V → 24 V |
| HLK-PM01 | now spare |
| PA003-4 terminals ×3 | 143 V bus |
| LP1K0901BD contactor | the switch |
| Soldering iron | for the perfboard |

### To buy — motorobit (~₺343)

| Item | Link | ₺ |
|------|------|---|
| **MAX485 TTL-RS485** | [motorobit](https://www.motorobit.com/ttl-rs485-seri-donusturucu-karti-max485) | 33 |
| 5×7 cm perfboard ×2 | [motorobit](https://www.motorobit.com/urun/5x7cm-epoxy-cift-tarafli-delikli-plaket) | 50 |
| 40-pin F-F jumpers | [motorobit](https://www.motorobit.com/40pin-20cm-female-female-jumper-cable) | 40 |
| M6 ring lugs ×4 | [motorobit](https://www.motorobit.com/jg6-6-m6-skp-kablo-pabucu) | 40 |
| Ferrule set + crimper | [motorobit](https://www.motorobit.com/800-pieces-8-types-of-insulated-ferrule-set-cable-ferrule) | 180 |

### To buy — solardepo (~₺750)

| Item | Link | ₺ |
|------|------|---|
| DC fuse 16 A + 1000 V holder | [solardepo](https://www.solardepo.com/hegel-dc-1000v-20ka-kartus-yuvasi-hegel-dc-20a-1000v-kartus-sigorta) | 150 |
| DC breaker 16 A 550 V | [solardepo](https://www.solardepo.com/pantec-dc-16a-550v-devre-kesici) | 250 |
| DC SPD Type 2 | [solardepo](https://www.solardepo.com) | 350 |

> The SPD **must say DC**. An AC unit looks identical and fails dangerously
> on a DC line.

### To buy — local shops (~₺1,000)

| Item | Where | ₺ |
|------|-------|---|
| LM2596 buck converter | electronics shop | 60 |
| Stick-on heatsink for the Pi | electronics shop | 20 |
| SD card, endurance grade | anywhere | 150 |
| 4 mm² solar cable + MC4 pair | [mundasolar](https://mundasolar.com/urun/4mm-solar-kablo/) | 430 |
| IP65 enclosure, room for the contactor | electrical supply | 400 |

**Total to buy: ~₺2,100**

---

## Stage 0 — Rewire the array to 3S1P

Free, and the single biggest safety gain.

| | 3S2P (6 panels) | 3S1P (3 panels) |
|---|---|---|
| Running voltage | ~143 V | **~126 V** |
| Margin below 150 V | 7 V | **24 V** |

Voc is 150.9 V either way — the controller handles that. But 126 V running
gives real margin and stops nuisance trips on cool days.

---

## Stage 1 — Prepare the Pi

On a fresh Raspberry Pi OS **Lite** install, with WiFi configured:

```bash
git clone <or copy> pump-protection
cd pump-protection
sudo bash install.sh
sudo reboot
```

The script installs dependencies, moves Bluetooth off the good UART, frees
the serial port from the login console, and registers the service.

### Why the UART matters

The Zero W gives the good PL011 UART to Bluetooth by default and leaves GPIO
with the mini-UART, whose baud rate drifts with CPU clock. That produces
intermittent Modbus CRC errors that look exactly like a wiring fault. The
install script adds `dtoverlay=disable-bt` to fix it.

---

## Stage 2 — Solder the board

Two boards keep the high and low voltage sides apart.

### Board A — power (143 V)

Solder onto one perfboard:

- 3 × PA003-4 terminal blocks
- HLK-10M24 (four PCB pins)
- LM2596 module

Wiring on the board:

```
Terminal 1  ──→ 143 V in from the fuse
            ├─→ HLK-10M24 AC pin 1
            ├─→ PZEM IN+ (flying lead)
            └─→ Terminal 3 (to contactor)

Terminal 2  ──→ 143 V negative
            ├─→ HLK-10M24 AC pin 2
            └─→ PZEM IN−
```

> **The HLK's input pins are labelled AC and have no polarity mark.** That is
> correct — there is a full-wave bridge rectifier behind them, so either pin
> can be your DC positive. The **output** pins `+Vo` / `−Vo` are marked and
> polarity there does matter.

### Board B — logic (5 V)

Solder the MAX485 module onto the second perfboard, or mount it on headers so
it can be swapped.

### Set the LM2596 before connecting anything

1. Feed it 24 V from the HLK-10M24
2. **Leave the output disconnected**
3. Turn the blue trimmer while watching a multimeter
4. Set to **5.1 V** — the slight excess covers cable drop
5. Only then wire it to the Pi

These boards ship set anywhere between 1.2 V and 35 V. Connecting an
unadjusted one to the Pi's 5 V pin destroys it instantly.

---

## Stage 3 — Bench test

**No high voltage.** Power the Pi from its normal USB supply.

### Wiring

```
MAX485          Pi Zero W (physical pin)
  RO      →     10   GPIO15 / RX
  DI      →      8   GPIO14 / TX
  DE ┐
  RE ┴   →      12   GPIO18
  VCC     →      2   5 V
  GND     →      6
  A / B   →     PZEM 4-pin header, A and B

LCD             Pi Zero W
  GND     →      9
  VCC     →      4   5 V
  SDA     →      3
  SCL     →      5

Relay           Pi Zero W
  VCC     →     17   (5 V)
  GND     →     14
  IN      →     11   GPIO17
```

DE and RE are tied together and driven from one pin — high to transmit, low
to receive.

### Run the checks

```bash
i2cdetect -y 1                          # find the LCD, usually 0x27 or 0x3F
sudo python3 test_hardware.py lcd       # four rows of text
sudo python3 test_hardware.py relay     # clicks every 1.5 s
sudo python3 test_hardware.py shunt     # sets the meter to 50 A
sudo python3 test_hardware.py meter     # reads 0.0 V unconnected
```

If `i2cdetect` shows a different address, change `LCD_I2C_ADDRESS` in
`config.py`.

**The shunt step is not optional.** The meter defaults to a 100 A shunt.
Yours is 50 A. Skip this and every current reading is double the truth —
which would break dry-run detection completely.

---

## Stage 4 — Wire the power side

**Breaker open. Verify 0 V with a meter before touching anything.**

DC arcs do not self-extinguish. Never separate a live 143 V connection by hand.

```
Panels (3S1P)
    │
    ├── DC breaker 16 A      manual isolation
    ├── DC fuse 16 A         1.56 × Isc of 11.48 A
    ├── DC SPD               surge → earth
    │
    ▼
Board A terminal bus ── 143 V
    │
    ├──→ PZEM IN+ / IN−            voltage sense, self-powered
    ├──→ HLK-10M24 → 24 V ─┬─→ LM2596 → 5.1 V → Pi, LCD, relay
    │                       └─→ relay COM
    │
    └──→ contactor pole 1 → pole 2 → pole 3 → Pump (+)

Pump (−) ──→ 50 A shunt ──→ Panels (−)

Relay NO ──→ coil A1
24 V GND ──→ coil A2
```

### Contactor — all three poles in series

```
143 V+ ──[1]─[2]──[3]─[4]──[5]─[6]── Pump+
        pole 1    pole 2    pole 3
```

The LP1K0901BD is rated for AC only. Three contact gaps in series break a
143 V DC arc that a single gap cannot. Chain them; do not use one pole.

### Shunt — in the negative line

The shunt's two **fat M6 bolts** carry the full pump current and belong in
the negative return. The two **small screws** are the sense tap and go to
PZEM SHUNT+ / SHUNT−. Crimp M6 ring lugs onto the pump cable for the bolts.

Swapping those roles destroys the meter.

---

## Stage 5 — First power-up

1. Close the breaker.
2. The contactor must stay **open**.
3. Watch the LCD — it shows STANDBY with the live voltage and the Pi's address.
4. Open that address in a browser on your phone: `http://<pi-address>:8080`
5. After 30 stable readings in the 118–138 V band, the contactor closes.
6. Running voltage should drop as the pump loads the array.

### Test the trip

With the pump running, open the breaker. Voltage collapses, the controller
trips on undervoltage, contactor opens. Close it again and it re-arms after
the settle period.

---

## Stage 6 — Tune dry-run detection

**Dry-run detection ships disabled.** Enabling it with guessed numbers will
either miss a dry well or stop a healthy pump.

### How it works

A pump moving water is a heavy load and drags the array down toward Vmpp.
Spinning in air it draws far less, so voltage drifts back up toward Voc.

| Condition | Volts | Amps |
|-----------|-------|------|
| Pumping normally | ~126 V | ~11 A |
| Weak sun | ~115 V | ~6 A |
| **Running dry** | **~145 V ↑** | **~5 A ↓** |

Low current alone is ambiguous — weak sun looks similar. Dry running is the
only state where **voltage rises while current falls**, so both conditions
must hold together before it fires.

### The procedure

1. Run normally for **2–3 days** with dry detection off.
2. Download `history.csv` from the dashboard.
3. Find your real running numbers across the day — early morning, midday,
   late afternoon.
4. Set **Amps below** to about **half** your normal running current.
5. Set **While volts above** to about **10 V above** your normal running
   voltage.
6. Enable it and watch for a few days.

If it trips during a passing cloud, raise the voltage threshold or lengthen
the duration. If a genuinely dry well runs too long before tripping, lower
the current threshold.

### Escalating lockout

| Strike | Rest |
|--------|------|
| 1st | 45 min |
| 2nd | 2 h |
| 3rd | 8 h — effectively until tomorrow |

A drawn-down well needs hours, not minutes. Retrying every five minutes wears
the pump for nothing. The dashboard has an override button for when you know
there is water.

Lockouts survive a restart — the state is written to disk, so a power cut
does not reset the counter.

---

## The dashboard

`http://<pi-address>:8080` from any phone or laptop on the WiFi.

- Live volts, amps, watts and state, refreshing every 2 seconds
- Every threshold editable from the browser — no SSH, no site visit
- Recent trip history with the reading that caused each one
- `history.csv` download for tuning
- Lockout override

Settings are validated before saving. It will refuse a high trip above 150 V,
or hysteresis bands that would make the pump chatter, and tells you why.

### Fix the address

The Pi's IP may change. Either reserve it in your router's DHCP settings, or
use `http://raspberrypi.local:8080` if your phone supports mDNS.

---

## Failure behaviour

Everything fails toward *pump off*:

| Failure | Result |
|---------|--------|
| Pi loses power | Relay opens, contactor drops |
| Script crashes | systemd restarts it; contactor open meanwhile |
| Meter stops replying | Trips after 6 failed reads — never runs blind |
| SD card corrupts | Pi does not boot, contactor never closes |
| Relay fails open | Pump stays off |
| 24 V supply dies | Coil de-energizes |
| WiFi drops | Protection unaffected; only the dashboard is lost |

The contactor is normally-open. The only way the pump runs is if the
controller is alive and actively holding the relay closed.

---

## Maintenance

```bash
sudo systemctl status pump-protection    # is it running
journalctl -u pump-protection -f         # live log
journalctl -u pump-protection --since today | grep OPEN    # today's trips
```

History and settings live in `/var/lib/pump-protection/`.

### Reducing SD wear

Once things are stable, consider enabling the read-only overlay filesystem
in `raspi-config` (Performance Options → Overlay File System). Root then runs
from RAM and the card is never written, making power cuts harmless. Remount
read-write temporarily when you need to change settings that persist.
