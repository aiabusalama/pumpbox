# REISBOT Level Board — build notes

Tank and well level sensing with autonomous pump and valve control.
113 × 226 mm, 90 parts, **single-sided**, all through-hole.

## Built for your process

Toner transfer, acid etch, hand placement, **soldered from one side only**.
Everything below follows from that last constraint.

All the copper is on one face. The parts sit on the other face, their legs
come through the holes, and every joint — parts, terminals, wire links — is
made from the copper side. There is nothing to solder on the far face, and
nothing that needs reaching under a component body.

That cost 75 wire links on the previous revision. **The link count and the board size below have not been re-derived for this revision** — `place2.py` → `route_ss.py` → `jumpers.py` → `check_solderable.py` must all run to completion first. Link count is not linear in part count.

> **This replaced a two-layer version, and that version was unbuildable
> by you.** It had 110 pads whose only connection was on the far face, and
> every one of them sat under a component body — under the DIP switch,
> under U3 to U8, inside J2's own housing. Once the part is in the hole
> that copper is unreachable forever. Neither ERC nor DRC reports this,
> because both assume plated holes. `check_solderable.py` is the test that
> does; run it after any layout change.

## What it does without anything else attached

Probes → resistor ladder → one analog voltage → LM3914 → five thresholds.
A DIP switch picks the fill target, a CD4013 latches, and a Darlington
drives the pump contactor coil: **the pump runs only when the tank wants
water AND the well has water AND the excitation is alive.** No firmware,
no Pi, no network is involved in that path.

Five tank levels, not eight. Level resolution is 20% instead of 12.5%, and
in exchange the calibration window widens from 11% to 16.7% — which is
1/(N+1), the widest it can be. Hang the five probes non-uniformly
(5/20/45/70/95% of depth); probe height costs nothing.

The Raspberry Pi is optional. It reads levels over three wires and gives
you a dashboard; if it dies, the pump keeps working.

## Making it

Print at **100% scale, no fit-to-page**. The board fits one A4 sheet.

| File | What it is |
|------|-----------|
| `etch/1-COPPER-mirrored-iron-this-on.pdf` | the artwork — already mirrored for ironing |
| `etch/2-wire-links-map.pdf` | the 75 wire links, seen from the component side |
| `etch/3-component-placement.pdf` | what goes where |
| `etch/board2.drl` | hole sizes |

Build order:

1. Etch and drill.
2. **Fit the wire links first.** Several run under IC bodies and cannot be
   fitted afterwards. Work through `WIRE-LINKS.md` and tick them off.
3. Fit the low parts, then the ICs, then the connectors.
4. Solder everything from the copper side.

Copper is 0.8 mm track, 0.5 mm clearance, and stays 1.5 mm inside the cut
line — the router was fenced out of the margin so nothing gets nicked when
you saw the board out.

## Connectors — one row along the bottom edge

Each one sits directly under the circuit it feeds. That is not cosmetic:
on a single layer, a connector placed away from its own circuit turns
straight into hand-soldered wire. An earlier arrangement cost 3.7 m of it.

| Ref | Part | Ways | Purpose |
|-----|------|------|---------|
| J1  | RJ45 | 8 | CAT5 to the control box: 5 V in and 3 data wires |
| J4  | 5.08 mm pluggable | 4 | pump contactor coil + watering valve |
| J11 | 3.5 mm pluggable | 4 | well: EXC common + W1..W3 |
| J2  | 3.5 mm pluggable | 6 | tank: EXC common + P1..P5 |

The two pitches are deliberate: a 5.08 mm pump plug will not fit a 3.5 mm
probe header, so they cannot be crossed.

### J1 must NOT be a magjack

Fit a **plain 8P8C receptacle** (Amphenol 54602 or equivalent). A magjack
has Ethernet transformers inside it. Nothing on this cable is Ethernet —
it carries DC and three logic lines — and **a transformer passes no DC**,
so the supply and all three signals would die inside the connector. The
footprint looks identical and the board routes and passes DRC either way.
Check the part description before you buy.

| Contact | Signal | Contact | Signal |
|---------|--------|---------|--------|
| 1, 2 | +5 V (doubled — 24 AWG drops volts) | 4 | SLOAD |
| 3, 7 | GND | 5 | SCLK |
| 8 | spare | 6 | SDATA |

Contacts 1+2 and 3+7 are joined on the board on purpose — two conductors
each for power and ground, halving the cable's resistance.

At the Pi end use an RJ45 breakout (~25 TL) and five jumpers to the GPIO.

## Probe cable — read this before wiring, it is not optional

**The EXC return must not share an unscreened multicore with the probe
cores.** Use screened cable with the screen earthed to the board's GND, or
run EXC as a separate conductor.

Cable capacitance between the EXC conductor and a probe conductor is a
current path that does not care whether the probe is wet. Ordinary
multicore is 60–120 pF/m, so 20 m puts ~1.2 nF beside each probe, and at
152 Hz that is 872 kΩ of reactance — the same order as a probe sitting in
rainwater. Simulated: with EXC run alongside the probe cores, **an empty
tank reads 1.2 V, which is four levels of water that is not there**, and
the pump never starts. With the same capacitance referenced to GND
instead, the empty tank reads 0.000 V at up to 2.4 nF.

This is the one failure mode that no amount of resistor value fixes, and
it is why the SSRC-04's 5–50 kΩ sensitivity ceiling is a feature rather
than a limitation: at a 50 kΩ threshold, cable reactance is 17× clear.
Reaching 300 kΩ rainwater is what forces the wiring rule.

## Commissioning

1. Power up with no probes connected. **EXC** (yellow) lights — if it is
   dark the oscillator is dead. That also now holds the pump off by
   itself, through D25, whatever SW2 is set to.
2. Fill the tank. **Wind RV1 up until the top LED goes out, then back off
   until it just lights again.**

   This is a self-checking test, and that is the whole reason exactly five
   LEDs are fitted for five probes. The point where the top LED
   extinguishes is exactly the top of the window in which probe *i* lights
   segment *i*; backing off from it can only move inward. Anywhere in the
   top 16.7% of that setting is correct, at every water resistance from
   1 kΩ to 300 kΩ.

   Wind too far up and the top LED stays dark — visibly wrong. Wind far
   too far down and the bar reads one level high; the bottom LED lighting
   with the tank near empty is the symptom.
3. Set the target level on **SW1**. The pump stops when the bar reaches
   that segment. **Every switch open now means "stop at the top probe"**
   rather than "never stop" — D26 ties the top segment in permanently.
4. Check the well: the three WELL LEDs should follow the well level, and
   **WELL** (green) means the interlock is satisfied.

## LED colours

Fitted, not driven — the gradient comes from which colour goes where.

- **Tank bar D10→D14** (bottom to top): 2 red, 1 yellow, 2 green
- **Well bar D22→D24**: red, yellow, green
- **Status**: D6 PUMP red, D7 EXC yellow

There is no PWR LED. D7 lighting already proves both that the rail is up
and that the oscillator on it is running, which is strictly more than a
power LED could say. D6 now hangs across the contactor coil, so it reports
the coil actually being energised rather than the latch's intention.

## Robustness — 1000 simulated installations

Water 1k (salty well) to 300k (rainwater in plastic), 3 to 5 probes
fitted, every ladder resistor and R19 drawn from ±5%, supply 4.75–5.25 V
**and drifting after commissioning**, 0–25 m of probe cable, RV1 wound by
hand off the visible top-LED edge.

**The pass criterion changed and is much stricter than the one that
produced the old 976/1000 figure.** It used to be that every probe
resolved to its own *distinct* segment. That is not what the control logic
assumes: `verify_logic.py` models `L[n] = (level < n)`, which needs probe
*i* to light segment *i* **exactly**. Distinctness passed happily on maps
like `[1,2,4,5,7,8,10,10]` — under which two DIP positions select the same
probe and one selects nothing at all. The two proofs were joined by an
assumption neither of them tested.

Scored against the criterion the logic actually needs:

| Board | Pass |
|---|---|
| the eight-probe ladder it replaced | **599 / 1000** |
| after the re-fit | **792 / 1000** |
| after RV1 made ratiometric | **809 / 1000** |

Of the remaining failures, roughly two thirds are **visible at
commissioning** — the top LED stays dark, or the bar lights on an empty
tank — because exactly N LEDs are fitted. The silent remainder is ~6%,
concentrated at 300 kΩ water with long cable, and is bottom-of-scale
compression: probes 1 and 2 landing on the same segment.

## Failure behaviour — verified over 576 states

Verified over **1536 states** — all 2^5 SW1 settings (not five of them),
every well state, filling and draining. The previous 576-state proof tested
one switch position per pass and so never tested the all-open state the DIP
switch ships in, which overfilled.

| Fault | What happens |
|-------|--------------|
| Oscillator dies | both vessels read dry, **pump stops** |
| Oscillator dies, SW2-1 open | **pump still stops** — D25 clamps the enable |
| SW1 all positions open | **stops at the top probe** — D26 |
| Well goes dry | **pump stops**, valve unaffected |
| Pi dies / WiFi down | telemetry lost, **pump unaffected** |
| Tank probe shorted | reads full, pump stops, tank can run empty |
| **Tank probe open (corroded)** | reads empty, **pump keeps running — overfill** |
| **Well probe shorted** | reads wet, **pump can run dry** |

The last two are why an **independent overflow float switch in series with
the pump contactor is mandatory**, not optional. The board cannot detect a
probe that has failed open.

## The output is a signal, not a switch

U7, a ULN2003, sinks a **DC-rated contactor coil**. It must not switch the
138 V DC pump directly: DC arcs do not self-extinguish at a zero crossing, and any
PCB relay will weld its contacts. The contactor itself must also be
DC-rated — an AC contactor will weld too.
