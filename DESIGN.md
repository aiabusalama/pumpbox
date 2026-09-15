# Pump Controller — Design

## What the user actually needs

Someone standing at the shed door wants to know three things in under a second:

1. **Is water flowing?**
2. **If not, why not, and will it fix itself?**
3. **Can I do something about it right now?**

Everything below serves those three questions.

---

## Modes

The operator chooses how much autonomy the controller has.

| Mode | Behaviour | When you'd use it |
|------|-----------|-------------------|
| **AUTO** | Starts the pump whenever conditions are safe. Restarts after transient faults. | Normal running. Set once, forget. |
| **MANUAL** | Waits at READY. Only a button press or dashboard command starts it. | Filling a specific tank, testing, working on the well. |
| **OFF** | Contactor held open. Nothing starts it. | Maintenance. Physically safe to work. |

Mode persists across reboots — a power cut must not silently re-arm a pump
you deliberately stopped.

---

## States

```
                    ┌──────────────────────────────────────┐
                    │                                      │
   power on         ▼                                      │
      │        ┌─────────┐   conditions bad          ┌──────────┐
      └───────▶│ WAITING │◀──────────────────────────│ RUNNING  │
               └────┬────┘                           └──────────┘
                    │ conditions good, stable              ▲
                    ▼                                      │
               ┌─────────┐    AUTO: automatic              │
               │  READY  │────────────────────────────────-┘
               └────┬────┘    MANUAL: button or dashboard
                    │
      any fault     │
      ┌─────────────┴──────────────┐
      ▼                            ▼
 ┌─────────┐                 ┌──────────┐
 │ TRIPPED │                 │ LOCKOUT  │
 └─────────┘                 └──────────┘
  recovers when               dry well - waits
  conditions return           45min / 2h / 8h
```

| State | Meaning to the operator | Contactor |
|-------|-------------------------|-----------|
| `WAITING` | Conditions not safe yet. Voltage out of band. | open |
| `READY` | Everything is fine. In MANUAL, waiting for you. | open |
| `STARTING` | Confirming stability before closing. ~15 s. | open |
| `RUNNING` | Water is flowing. | **closed** |
| `TRIPPED` | Something went wrong. Will retry when safe. | open |
| `LOCKOUT` | Well is dry. Resting so it can recover. | open |
| `FAULT` | Cannot read the meter. Refusing to run blind. | open |
| `OFF` | You switched it off. | open |

`READY` exists so MANUAL mode has somewhere to sit: everything is safe, the
pump could start, it is waiting for a human. Without it, MANUAL and WAITING
would be indistinguishable on the display.

---

## Physical controls

Three buttons and an LED. The shed is not always in phone range, and gloves
do not work on touchscreens.

| Control | Short press | Long press (2 s) |
|---------|-------------|------------------|
| **START / STOP** | MANUAL: start the pump. RUNNING: stop it. | — |
| **MODE** | Cycle AUTO → MANUAL → OFF | — |
| **RESET** | Clear a trip and retry now | Clear a dry-run lockout |

**Status LED** — visible from the doorway:

| Pattern | Meaning |
|---------|---------|
| Solid green | Running |
| Slow green pulse | Ready, waiting for you |
| Slow amber pulse | Waiting for conditions |
| Fast amber blink | Dry-run lockout |
| Solid red | Tripped |
| Fast red blink | Meter fault |
| Off | Mode is OFF |

---

## Display

Four lines, 20 characters. Line 1 is the answer; lines 2–4 are the detail.

**Running** — the good case:
```
RUNNING        AUTO
  126.4V   10.8A
  1367W    4h 12m
today 8.4kWh  ▓▓▓▓
```

**Ready, manual** — the pump is waiting for you:
```
READY - PRESS START
  128.1V    0.0A
all conditions ok
mode MANUAL
```

**Waiting** — tell them what for, and how close:
```
WAITING
  152.3V   too high
need under 138V
warming up  ▓▓▓░░░░
```

**Dry lockout** — the one that needs explaining:
```
WELL RECOVERING
dry run detected
retry in 38 min
strike 1 of 3
```

**Tripped** — what happened and what now:
```
STOPPED - OVERVOLT
was 147.2V at 09:14
now 131.8V  ok
retrying in 12s
```

Every screen answers "why" without the operator needing the manual.

---

## Dashboard

Same information, more of it, from a phone.

- **Live tile** — state, volts, amps, watts, updating every second
- **Mode switch** — AUTO / MANUAL / OFF, one tap
- **Start / Stop** — big, only enabled when the action is valid
- **Today** — energy, run time, number of starts
- **History** — 24 h chart of voltage and current
- **Events** — what tripped, when, at what reading
- **Settings** — thresholds, validated before saving

---

## Safety invariants

These hold regardless of mode, settings, or dashboard state:

1. The contactor is **normally open**. Losing power, the process, or the
   relay drops the pump.
2. Voltage limits are **never** overridden by mode. MANUAL start is refused
   if conditions are unsafe — the button does nothing and says why.
3. Losing the meter for more than 3 seconds **stops the pump**. Running
   blind is never safe.
4. A dry-run lockout survives restart. It is written to disk.
5. Mode survives restart. A power cut does not re-arm a pump someone
   deliberately switched off.

---

## Observability

Designed to be diagnosed from a phone, over a slow link, weeks later.

- **`/metrics`** — Prometheus format: state, volts, amps, watts, energy,
  trip counts by reason, uptime, meter error rate
- **Structured logs** to journald, one line per state change with the
  readings that caused it
- **`history.csv`** — one row per minute, rotated weekly
- **Heartbeat file** touched every loop; a systemd timer restarts the
  service if it goes stale, catching a wedged process that systemd would
  otherwise consider healthy
