# Field hotspot

The pump box has to be commissioned at a site with **no router, no WiFi and no
usable mobile data**, with **only a phone**. Nothing in the rest of this repo
gets you a network there: `BUILD.md` assumes the Pi already joined a WiFi you
control and tells you to browse to `http://<pi-address>:8080`. At the field
site there is no such address and no way to discover one.

This directory makes the Pi **its own access point**, so the phone joins the
box directly.

```
    SSID       PumpBox
    PASSWORD   ReisLandPump2026
    DASHBOARD  http://192.168.4.1:8080/
```

Change any of that in [`hotspot.conf`](hotspot.conf) and re-run the script.

---

## Run it

**At home, over SSH, before the trip.** It needs internet once, for
`dnsmasq-base`.

```bash
cd ~/automations/pump/hotspot     # wherever deploy.sh put the repo
sudo bash setup-hotspot.sh
```

Then the only test that counts:

```bash
sudo reboot
# ...with your home router powered OFF...
sudo bash setup-hotspot.sh --status
```

| Command | Effect |
|---|---|
| `sudo bash setup-hotspot.sh` | configure and activate |
| `sudo bash setup-hotspot.sh --status` | report only, changes nothing, no root needed |
| `sudo bash setup-hotspot.sh --no-activate` | configure, apply on next reboot |
| `sudo bash setup-hotspot.sh --undo` | remove everything it installed |

It is idempotent. Re-running converges the machine rather than stacking
changes, so it is safe to run after editing `hotspot.conf`, and safe to run on
a box you are not sure about.

If you run it **while connected over the hotspot itself**, it notices (your
`SSH_CONNECTION` source is in the AP subnet) and defers activation rather than
cutting the session out from under you. Reboot to apply.

---

## What it does

**One NetworkManager profile.** Not hostapd + dnsmasq. Bookworm made
NetworkManager the only network stack on every image including Lite — `dhcpcd`
and `/etc/network/interfaces` are gone, so the static-IP half of every older
tutorial silently does nothing, and NM keeps managing `wlan0` and fights
hostapd for it. NM's AP mode drives the same wpa_supplicant code path and
spawns its own scoped dnsmasq for DHCP. One profile, no second daemon stack,
which matters on a 512 MB single core.

Specifically:

| | |
|---|---|
| **Packages** | Installs `dnsmasq-base`, `iw`, `rfkill`. Holds and disables the full `dnsmasq` **daemon** — it binds `:53` system-wide and collides with the private instance NM spawns for shared mode. |
| **Regulatory domain** | Three ways: `/etc/modprobe.d/pump-ap-regdom.conf`, `raspi-config nonint do_wifi_country`, and `pump-ap-regdom.service` re-asserting at boot before NM. Pi OS keeps WLAN rfkill soft-blocked until a country is set, and the failure is silent — the profile activates "successfully" and no SSID is ever transmitted. |
| **Competing profiles** | Sets `autoconnect no` on every *other* WiFi profile, by enumerating real names. Raspberry Pi Imager's WiFi customisation creates one called **`preconfigured`**, so guides that say "set your home profile to autoconnect no" name something that does not exist — and the bench rehearsal then quietly runs on home WiFi, which is the one thing it exists to disprove. |
| **AP profile** | `pump-ap`: AP mode, band bg, fixed channel, `ipv4.method shared`, `192.168.4.1/24`, IPv6 off, `autoconnect-priority 100`, `autoconnect-retries 0` (retry forever). |
| **Watchdog** | `pump-ap-watchdog.timer` every 2 min. Brings the AP up **only if `wlan0` has no active connection**, so it recovers a dead AP without yanking you off home WiFi mid-`apt` at the bench. |
| **Boot diagnostics** | `pump-ap-diagnostics.service` appends the network state to `/boot/firmware/pumpnet.log` on every boot. |
| **Captive portal** | Answers Android/Apple connectivity probes so the phone stops nagging and stops routing your request out the cellular interface. Own process, port 80. |
| **Boot speed** | Disables `NetworkManager-wait-online.service`, which can stall boot 45 s waiting for a network that will never exist at the site. |

### Why `192.168.4.1` and not NM's default `10.42.0.1`

Turkish carriers hand out CGNAT addresses inside RFC1918 `10.0.0.0/8`. A phone
holding a `10.x` cellular route can send `10.42.0.1` out the LTE interface
instead of the WiFi one, and the symptom is indistinguishable from a dead Pi.
`192.168.4.0/24` has no such overlap.

If you have seen `10.42.0.1` written on an earlier version of the field card or
the lid label, **it is wrong** — fix it before printing.

---

## On the phone

**In this order. The first step is not optional.**

1. **Airplane mode ON, then WiFi back ON.**
2. Join `PumpBox`.
3. Browser: **`http://192.168.4.1:8080/`** — type the `http://`.

Airplane mode first is the guarantee, not a superstition. Android probes
`http://connectivitycheck.gstatic.com/generate_204` over a candidate network;
with no upstream that fails, the network is marked unvalidated, and the failure
mode is usually **not** "WiFi disconnects" — it is "WiFi stays connected but
the browser's request to the Pi leaves via LTE and dies." With the cellular
radio off there is no alternative route and the whole class of problem is gone.
There is no usable data at the site anyway.

Also set once, at home:

- **Private DNS → Off.** A DoT hostname makes the phone bypass the Pi's dnsmasq
  entirely.
- **Adaptive Connectivity / "switch to mobile data automatically" → Off.** Name
  varies by vendor. This is the setting that actively drops a no-internet WiFi.
- When "This network has no internet access. Stay connected?" appears, tick
  **"Don't ask again for this network"** and tap **Yes**.

Type the `http://`. Chrome hands a bare `192.168.4.1:8080` to the search engine
often enough to matter, and the search fails because there is no internet.

### Do not rely on `raspberrypi.local`

`BUILD.md` offers it "if your phone supports mDNS". Android has never reliably
resolved `.local` from a browser. The AP address is static by construction, so
mDNS buys nothing and can only produce a confusing failure at the worst moment.

### Print this for inside the lid

```
SSID       PumpBox
PASSWORD   ReisLandPump2026
DASHBOARD  http://192.168.4.1:8080/
```

Add two QR codes on the same A4 sheet — one
`WIFI:T:WPA;S:PumpBox;P:ReisLandPump2026;;` (the Android camera joins from it,
no typing) and one `http://192.168.4.1:8080/`. Typing a 16-character password
on a phone in field sunlight eats twenty minutes.

---

## Verifying the AP is up

### From the box, with a shell

```bash
sudo bash setup-hotspot.sh --status
```

The four lines that matter:

| Line | Good | Bad |
|---|---|---|
| regulatory domain | `country TR:` | `country 00: DFS-UNSET` → **AP transmits nothing** |
| rfkill | both blocks `no` | `Soft blocked: yes` → no country set |
| Active on wlan0 | `pump-ap` | *nothing active* → AP is down |
| Attached phones | `1` after you join | `0` → association or DHCP failed |

### From the phone, with no app and no shell

**Look at the phone's own IP.** Settings → WiFi → `PumpBox` → details.

- **`192.168.4.x`** — radio *and* DHCP both work. Any remaining problem is the
  web service or Android routing (go back and do airplane mode).
- **`169.254.x.x` or no address** — association succeeded, **DHCP failed**.
  That is `dnsmasq-base` missing or NM's dnsmasq refusing to start. This is the
  only way to see that failure without a shell, and it is the single most
  likely silent one.
- **Not in the WiFi list at all** — the regulatory domain, almost certainly.

### From the LCD

Currently: **you cannot.** See the known-issues section below — the LCD will
show a blank address line even when the AP is perfectly healthy. That is a bug
in `pump_control.py`, not in this directory, and it is not fixed here.

---

## Undo

```bash
sudo bash setup-hotspot.sh --undo
```

Removes the units, the AP profile, the dnsmasq drop-in, the modprobe file and
the installed helpers, and restarts NetworkManager.

It deliberately does **not** re-enable your other WiFi profiles — it has no way
to know which of them you wanted autoconnecting. Put your home network back
with:

```bash
nmcli connection modify preconfigured connection.autoconnect yes
```

### Getting on home WiFi temporarily without undoing anything

At the bench, to run `apt` or `deploy.sh`:

```bash
sudo touch /run/pump-ap-inhibit          # stop the watchdog reclaiming the radio
sudo nmcli connection up preconfigured   # or whatever `nmcli con show` calls it
```

`/run` is tmpfs, so **any reboot clears the inhibit and returns to the AP.**
There is no state in which you can strand yourself. To come back without
rebooting:

```bash
sudo rm /run/pump-ap-inhibit
sudo nmcli connection up pump-ap
```

---

## When the phone will not associate

Work down this list. Each step is one edit to `hotspot.conf` and a re-run.

1. **`PMF="1"`** (the shipped default). NM's default resolves to
   PMF-*optional* for `wpa-psk`, so the AP advertises 802.11w-capable.
   brcmfmac's AP path handles 802.11w poorly and a failed negotiation surfaces
   on Android as **"incorrect password"**. This is the leading suspect.
2. **`STRICT_CIPHERS="no"`** (the shipped default) leaves `proto`, `pairwise`
   and `group` unset, which is what NetworkManager's own documentation
   recommends for maximum compatibility. Try `"yes"` (WPA2-AES pinned) if
   loose fails — the field evidence is genuinely contradictory, one Raspberry
   Pi forum thread reports Android working only after *removing* the pinning
   and an Arch thread reports the exact opposite.
3. **`CHANNEL`** → try `1` or `11`.
4. **`PSK=""`** — an open, unencrypted AP. Last resort, and a legitimate one:
   the box is on private family land and its only exposed control sits behind a
   143 V DC contactor. An unencrypted dashboard beats an unreachable one.

> There is a published report of a **Zero W on 32-bit Bookworm** whose
> NetworkManager AP refuses Android with "password incorrect" while Windows and
> Linux connect fine. **Test with the actual phone you are taking.** A laptop
> associating proves nothing about this failure.

## Do not run Trixie on this board

Pi OS 13 (Trixie) **cannot bring up a WPA-protected nmcli hotspot on an
original Pi Zero W.** Activation dies with `802.1X supplicant took too long to
authenticate` and the kernel logs `key setting validation failed` from
brcmfmac. Upstream issue [raspberrypi/linux#7247][7247] is open with no
workaround. Open hotspots still come up; adding `wpa-psk` kills it.

Use **Raspberry Pi OS 12 (Bookworm) Lite, 32-bit**. The Zero W is ARMv6 and
cannot run a 64-bit image at all. Confirm with `cat /etc/os-release` →
`VERSION_ID="12"`. The script warns if it sees anything else.

Do not `apt full-upgrade` across a release before the trip. If you want to be
strict about it, Bookworm's own security pocket can still move the two packages
implicated in that bug:

```bash
sudo apt-mark hold network-manager wpasupplicant
```

The script does **not** do this for you — it blocks security updates, and that
is your call, not the script's.

## Do not run AP and home WiFi at the same time

One radio, one channel. The AP gets forced onto whatever channel the router
picked; throughput roughly halves; and brcmfmac's AP+STA combination on the
BCM43438 is the least-exercised path in the driver — on the same board that
already cannot complete a plain WPA hotspot on Trixie. You would be stacking
your only access on the flakiest available configuration, to gain nothing: at
the field site there is no other network for the client half to join.

Two profiles, AP always wins a reboot. That is what the script sets up.

---

## Phone-only recovery, when the AP is dead

**Triage on the LCD first**, because it splits the problem cleanly:

- **LCD blank** → power, boot or SD card. Not a network problem.
- **LCD shows STANDBY with live volts** → the controller is alive and
  protecting. This is *purely* a WiFi problem. **The pump is safe. You can stop
  here and tune the dashboard another day.** `BUILD.md` Stage 6 needs 2–3 days
  of history before dry-run tuning is meaningful, so the dashboard is genuinely
  not needed on day one. The real risk at this point is you poking at a working
  protection box.

Then, in order:

1. Wait a full 90 s. NM can be slow to bring up AP mode on this board.
2. Power-cycle once. The watchdog retries every 2 minutes after that.
3. Phone: forget the network, rescan. Confirm airplane mode is on.
4. **Swap to the spare SD card.** This is the recovery mechanism.
5. Only then, power down and read `/boot/firmware/pumpnet.log` on the phone via
   an OTG card reader, to learn *which* card to blame.

### Why the log is read-only and there is no editable config on the boot partition

An earlier plan for this box proposed a `/boot/firmware/pumpnet.conf` you could
edit from the phone to fix the AP in the field. **That is not a phone
procedure.** Executing it costs: open the DC breaker (pump stops), unscrew the
IP65 lid, unplug the Pi from the LM2596 and the USB-RS485 cable, extract a
microSD that protrudes about a millimetre from a Zero W's friction slot, plug
in an OTG reader, edit, reinsert, reassemble, re-energize — and it is
edit-reboot-repeat, so **every hypothesis costs a full disassembly cycle.**

Worse, a Pi OS card has a FAT32 boot partition and an ext4 root, and some
Android builds respond to that by offering to "fix" the **corrupted USB drive**
— tapping it formats the card you were trying to rescue.

So: reading the log **once** is a plausible field action and is supported. The
fix for a dead AP is **the spare SD card**, not an editor. Prepare two
identical cards and verify both boot before you leave. You cannot write a disk
image from stock Android; if both cards fail on site, the trip is over.

If you do want an in-field AP override, make it **physical** — a jumper on a
spare GPIO, read at boot, forcing an open no-password AP on channel 6. That
bypasses every failure a config file guards against (wrong PSK, wrong channel,
wrong regdomain) without opening anything. Not implemented here.

### The one thing on the boot partition you *can* usefully change from a phone

The regulatory domain is also settable as a kernel parameter, by appending this
to the single line in `/boot/firmware/cmdline.txt`:

```
cfg80211.ieee80211_regdom=TR
```

`cmdline.txt` is on the FAT partition, so it is reachable over an OTG reader,
and a missing country code is the highest-probability AP failure. Worth writing
on the lid label next to the SSID.

**Caveat, stated plainly:** this script does *not* write that line, because
whether a kernel-command-line module parameter is honoured for `cfg80211` when
it loads as a module is exactly the sort of thing that must be checked on the
hardware rather than assumed. The script uses `/etc/modprobe.d` and a boot-time
`iw reg set` instead, both of which are unambiguous. Treat the `cmdline.txt`
line as an unverified emergency lever: it will not hurt, and it may help.

---

## Known issues in the rest of the repo that this directory does not fix

These are real, they bite specifically in AP mode, and they are **not** touched
here because this task was scoped to the hotspot. Fix them before the trip.

**1. The LCD will never show you the address.** `pump_control.py:22-31`:

```python
s.connect(("192.0.2.1", 1))       # TEST-NET-1, never routed
ip = s.getsockname()[0]
```

That works by asking the kernel which source address a route to an arbitrary
destination would use. `ipv4.method=shared` installs **only** the on-link
`192.168.4.0/24` route and **no default route** — correctly, there is no
upstream. So `connect()` fails with `ENETUNREACH`, `except OSError` returns
`None`, and the LCD's address line renders as `""` or `waiting for band`. You
stand at the box with a working hotspot and a screen telling you nothing.

Read the interface instead of the routing table — `SIOCGIFADDR` via `fcntl`, or
`ip -4 -o addr show wlan0`. Note that `install.sh:70` already uses the AP-safe
incantation (`hostname -I`); the broken one is in `pump_control.py`. The fix is
already in this repo, in the wrong file.

**2. The address is read once, at startup.** `pump_control.py:44` calls
`local_ip()` in `App.__init__`. On a cold boot the protection service easily
wins the race against NetworkManager, caches `None`, and never re-checks for
the rest of the day. Re-read it on a slow cadence — every ~10 s from the main
loop, into a cached string, never raising into the loop.

**3. A dashboard that cannot bind permanently stops the pump.**
`pump_control.py:45` calls `web.start(self.controller)` unguarded, and
`web.py:285` binds `0.0.0.0:8080`. If that raises — a lingering process after a
crash-restart, say — it propagates out of `App()`, `main()` returns 1, and
systemd's `Restart=always` retries every 5 s forever with the contactor open.
Wrap it:

```python
try:
    self.server = web.start(self.controller)
except OSError as e:
    log(f"dashboard unavailable: {e}")
    self.server = None
```

and make `shutdown()` tolerate `self.server is None`.

**4. Do not add `iw dev wlan0 station dump` to the control loop.** If you add a
client count to the LCD, read the lease files
(`/var/lib/NetworkManager/dnsmasq-*.leases`) — O(1), non-blocking. A subprocess
inside a 0.5 s loop on a single-core 512 MB board stalls `controller.update()`
and blinds the protection for its duration. If you must shell out, do it on the
web thread.

**5. Leave `pump-protection.service` at `After=network.target`.** Do not add
`Wants=`/`Requires=NetworkManager.service` or `network-online.target`. The pump
must come up on a box whose radio is dead. The script asserts this on every
run and refuses to finish if something has added such a line.

---

## Nothing here has been run on hardware

The Pi is not wired yet. Be precise about what that means.

**Verified off-hardware, on the workstation:**

- Every property in the profile is accepted by `nmcli --offline connection add`
  (nmcli 1.50) and emits the intended keyfile. Every property used dates to
  nmcli ≤ 1.4, so Bookworm's 1.42 accepts it.
- Clearing `proto`/`pairwise`/`group` with `""` produces a keyfile with those
  keys absent; empty `key-mgmt` produces one with no `[wifi-security]` section
  at all. Both compatibility fallbacks are real.
- `man nmcli` EXIT STATUS: **3 = timeout, 4 = connection activation failed.**
  Widely copied recipes have these swapped and mark a real activation failure
  as success. `pump-ap-ensure.sh` maps them correctly.
- `portal-responder.py`: 204 on `/generate_204` and `/gen_204` with no
  `Content-Length` (RFC 7230), the exact Apple success body on
  `/hotspot-detect.html`, 302 to the dashboard for everything else, HTTP/1.1
  keep-alive across all three, and a second instance losing port 80 exits 0
  instead of restart-looping.
- `pump-ap-ensure.sh` decision paths: already-up, another-connection-owns-the
  interface, inhibit-file, activation-failure. All behave.
- `pump-ap-diagnostics.sh` produces a useful log and caps its own size.
- All shell files pass `bash -n`; the Python compiles.

**NOT verified, and only the hardware can settle it:**

| | |
|---|---|
| **That an Android phone associates at all** | The highest-risk item by a distance. There is a published report of exactly this stack — Zero W, 32-bit Bookworm, NM AP, WPA2 — refusing Android with "password incorrect". **Test with your phone, not a laptop.** Do this *before* the cold-boot rehearsal; everything else is downstream of it. |
| That `brcmfmac` starts AP mode on this Zero W at all | Widely reported working on Bookworm. Not observed by me on this board. |
| That NM autoconnects the AP on a cold boot | The whole reason the watchdog timer exists. |
| That `raspi-config nonint do_wifi_country` persists on Bookworm | Check `iw reg get` **after a reboot**, not before. The modprobe file and the boot service are there because this is uncertain. |
| That `cfg80211.ieee80211_regdom=` on the kernel command line is honoured for a loadable `cfg80211` | Documented above as an unverified emergency lever, deliberately not used by the script. |
| That `ipv4.method shared` serves DHCP with no upstream link | Reasoned from NM's shared-mode design. Confirm by joining and reading the phone's own IP. |
| Whether Android accepts a plain-HTTP 204 as validation | Modern Android runs an HTTPS probe alongside, which cannot be satisfied without a trusted certificate. Treat the portal as *reduces the nag*, not a guarantee. **Airplane mode is the guarantee.** |
| Exact DHCP pool bounds NM picks in `192.168.4.0/24` | Does not matter — you never need the phone's address, only its prefix. |
| Whether `/var/lib/NetworkManager/dnsmasq-wlan0.leases` is the exact path | Globbed rather than hardcoded, so a different filename still counts. |
| That the OTG card reader mounts this card on this phone | Test the exact reader/phone/card combination at home. |
| The AP's cost on a 512 MB single core already running the 0.5 s poll loop | Watch for control-loop lag with a phone attached. |

**Rehearse these two at the bench, in this order. Everything else in this
document is recoverable in the field; these are not:**

1. **Associate your actual Android phone**, in airplane mode, and load
   `http://192.168.4.1:8080/`.
2. **Cold-boot the box five times with the router powered off** and confirm
   `--status` reaches `192.168.4.1` every time.

[7247]: https://github.com/raspberrypi/linux/issues/7247
