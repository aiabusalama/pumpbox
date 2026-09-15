#!/usr/bin/env bash
# Unblock the radio and assert the wireless regulatory domain, before
# NetworkManager gets a chance to start the access point.
#
# Installed as /usr/local/sbin/pump-ap-regdom by setup-hotspot.sh.
#
# WHY this exists at all: Raspberry Pi OS ships WLAN rfkill soft-blocked
# until a wireless country is set. With no country the AP profile activates
# "successfully" and no SSID is ever transmitted - a green status line and a
# dead box. /etc/modprobe.d/pump-ap-regdom.conf is the primary mechanism and
# is deterministic when cfg80211 is a loadable module; this covers the case
# where it is built into the kernel, and re-asserts if anything resets it.

set -uo pipefail        # not -e: never let this block boot

COUNTRY="${1:?country code required}"

rfkill unblock wifi 2>/dev/null || true

# cfg80211 may not have finished loading when this runs. Retry briefly
# rather than fail - but cap it hard, because this unit is ordered BEFORE
# NetworkManager and every second here is a second of blank LCD.
for _ in $(seq 1 10); do
  if iw reg set "$COUNTRY" 2>/dev/null; then
    break
  fi
  sleep 1
done

reg="$(iw reg get 2>/dev/null | grep -m1 '^country' || echo 'country ?: unavailable')"
echo "regulatory domain: $reg"

# "country 00: DFS-UNSET" here is the failure that matters. Say so plainly
# so the boot diagnostics log carries an unambiguous line.
case "$reg" in
  "country $COUNTRY:"*) exit 0 ;;
  *) echo "WARNING: expected country $COUNTRY - the AP may transmit nothing" >&2
     exit 0 ;;   # still exit 0: a wrong regdom must not fail the boot
esac
