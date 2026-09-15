# REISBOT Pump Guard

A small box that keeps a solar-powered water pump alive. Rust on a Raspberry Pi Zero W.


## The problem

The pump is a 138 V, 1.5 kW DC brushless submersible (110–150 V range, 11 A max). Its own controller card
is destroyed above 150 V. The array is three GSE-HC455 panels in series: 150.9 V open-circuit at 25 °C, and
climbing toward 165 V on a cold morning — exactly the voltage present at sunrise, before any current flows.
That killed the first controller card. The second time it could not be repaired.

## What the box does

- Watches the array voltage and current every few milliseconds (PZEM-017 over RS-485).
- Keeps the contactor open until the array has settled inside the safe window (95–145 V), and drops it
  again on over-voltage, under-voltage, over-current, a dry well, or loss of contact with the meter.
- Waits, re-checks that the voltage is steady (about 15 s), and restarts by itself.
- Runs the pump from the water levels: a level relay in the well, another in the tank.
- Shows everything on a 20×4 LCD, one status light (blinking = ready, solid = running, two flashes =
  too high, three = too low) and a phone dashboard over its own Wi-Fi hotspot.

## Parts

| Part | Role |
|---|---|
| Raspberry Pi Zero W | the brain, runs `pumpd` |
| PZEM-017 + 50 A shunt | volts, amps, watts on the pump line |
| Schneider contactor | switches the 150 V DC pump line |
| 5 V relay module | drives the contactor coil from the Pi |
| HLK-10M24 | 143 V → 24 V for the contactor coil |
| 5 V / 3 W supply | the Pi |
| 12 V converter | feeds the two level relays |
| 2 × SSRC-04 level relays | well and tank levels — sold for 220 V AC, bypassed to run on 12 V DC (two jumper wires) |
| 20×4 I²C LCD, mode switch, START / STOP buttons | the front panel |

## Repo layout

- `rust/` — `pumpd`, the controller: state machine, PZEM-017 meter, front panel, LCD, watchdog, fail-safe,
  web dashboard. `rust/src/bin/` holds two bench tools (`failprobe`, `wdtest`).
- `kicad/level/` — the level-sensor board (schematic, PCB, etch transfers, drill file, wire-link map); `kicad/render/`
  shows the finished board; `kicad/BOM.txt` is the sourcing list.
- `hotspot/` — the Pi's Wi-Fi hotspot: setup script, captive-portal responder, watchdog units.
- `fritzing/`, `qet/` — wiring sketch and electrical diagram.
- `docs/` — enclosure layout and wiring pages; `DESIGN.md` and `BUILD.md` — design notes and build guide.

## Safety

This box switches 150 V DC. It is a personal project shared as-is; read `BUILD.md` before copying any of it.

## Licence

MIT
