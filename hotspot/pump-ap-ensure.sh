#!/usr/bin/env bash
# Bring the field hotspot up if - and only if - nothing else is using the
# radio. Run from pump-ap-watchdog.timer every two minutes.
#
# Installed as /usr/local/sbin/pump-ap-ensure by setup-hotspot.sh.
#
# WHY this is conditional rather than an unconditional `nmcli con up`:
# at the bench you sometimes want the Pi on home WiFi to run apt or
# deploy.sh. An unconditional watchdog would yank it back within two
# minutes, mid-download. Acting only when the interface has NO active
# connection means the watchdog recovers a dead AP and stays out of the
# way the rest of the time. At the field site nothing else is in range,
# so "no active connection" and "the AP died" are the same event.

set -uo pipefail        # deliberately not -e: this must run to the end

PROFILE="${1:-pump-ap}"
IFACE="${2:-wlan0}"
INHIBIT=/run/pump-ap-inhibit

# /run is tmpfs, so the inhibit clears itself on reboot. There is no state
# in which you can strand yourself: a power cycle always returns to the AP.
if [ -e "$INHIBIT" ]; then
  echo "inhibited by $INHIBIT - leaving the radio alone"
  exit 0
fi

active="$(nmcli -t -f NAME,DEVICE connection show --active 2>/dev/null \
          | awk -F: -v d="$IFACE" '$2==d{print $1}')"

if [ "$active" = "$PROFILE" ]; then
  exit 0                                     # already up, nothing to say
fi

if [ -n "$active" ]; then
  echo "$IFACE is on '$active' - not overriding it"
  exit 0
fi

echo "$IFACE has no active connection - bringing up $PROFILE"
rc=0
nmcli -w 30 connection up "$PROFILE" >/dev/null 2>&1 || rc=$?

case "$rc" in
  0) echo "$PROFILE activated" ; exit 0 ;;
  # 3 is nmcli's timeout, not a failure - a slow cold board can exceed the
  # wait while still completing. Do not red-flag the unit for it; the next
  # timer tick will confirm.
  3) echo "activation still in progress after 30s (nmcli timeout)" ; exit 0 ;;
  # 4 is "connection activation failed" - the actual thing this watchdog
  # exists to surface. Exit non-zero so `systemctl status` and the boot
  # diagnostics log both show it.
  4) echo "ACTIVATION FAILED - AP is down. Check: iw reg get / rfkill list wifi" >&2 ; exit 1 ;;
  *) echo "nmcli exited $rc" >&2 ; exit 1 ;;
esac
