#!/usr/bin/env bash
# Append a snapshot of the network state to the FAT boot partition on every
# boot, then stop.
#
# Installed as /usr/local/sbin/pump-ap-diagnostics by setup-hotspot.sh.
#
# WHY the boot partition: if the AP does not come up there is no dashboard,
# no SSH and no console. Android can mount a FAT32 partition over an OTG
# card reader; it cannot see the ext4 root at all. So this file is the only
# channel that survives total network failure without a laptop.
#
# WHY read-only and one-shot, with no companion "edit this to fix it" file:
# reading the card once is a plausible field action. Editing and retrying is
# not - each attempt costs opening the DC breaker, unscrewing an IP65 lid,
# unplugging the Pi, and extracting a microSD that protrudes about a
# millimetre from a friction slot. The recovery mechanism for a dead AP is
# the SPARE SD CARD, not an editor. This file only tells you which card to
# blame. Note also that some Android builds offer to "fix" a
# multi-partition card by formatting it, so every extra trip to the card
# reader is a chance to destroy the thing you are rescuing.

set -uo pipefail

IFACE="${1:-wlan0}"
PROFILE="${2:-pump-ap}"

LOG=/boot/firmware/pumpnet.log
[ -d /boot/firmware ] || LOG=/boot/pumpnet.log
[ -d "$(dirname "$LOG")" ] || exit 0

# Overlay FS or a read-only boot mount makes this a no-op rather than a
# boot failure.
if ! touch "$LOG" 2>/dev/null; then
  echo "$(dirname "$LOG") is not writable - skipping" >&2
  exit 0
fi

{
  echo "===== $(date -Is) boot ====="
  # Clock is unreliable: the Zero W has no RTC and there is no NTP at the
  # site, so treat the date above as ordering information only, not truth.
  echo "-- os"
  grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release 2>/dev/null
  echo "nmcli $(nmcli --version 2>/dev/null | awk '{print $NF}')  wpa_supplicant $(wpa_supplicant -v 2>/dev/null | head -1)"
  echo "-- rfkill"
  rfkill list wifi 2>&1
  echo "-- regdom"
  iw reg get 2>&1 | grep -m1 '^country'
  echo "-- interface"
  iw dev "$IFACE" info 2>&1 | grep -E 'type|channel|ssid' || echo "no $IFACE"
  ip -4 -o addr show "$IFACE" 2>&1 || true
  echo "-- connections"
  nmcli -t -f NAME,TYPE,DEVICE,STATE connection show --active 2>&1
  echo "-- services"
  # systemctl writes its answer to stdout AND exits non-zero when the unit
  # is inactive or absent, so `|| echo n/a` emits two values. Capture it.
  for s in NetworkManager pump-ap-regdom pump-portal pump-protection; do
    st="$(systemctl is-active "$s" 2>/dev/null || true)"
    printf '%s=%s ' "$s" "${st:-n/a}"
  done
  echo
  echo "-- dnsmasq leases"
  cat /var/lib/NetworkManager/dnsmasq-*.leases 2>/dev/null || echo "(none)"
  echo
} >> "$LOG" 2>&1

# Keep the file small. It lives on a FAT partition that also holds the
# kernel and the boot config; unbounded growth there is not acceptable.
if [ "$(wc -l < "$LOG")" -gt 400 ]; then
  tmp="$(mktemp)"
  tail -n 300 "$LOG" > "$tmp" && cat "$tmp" > "$LOG"
  rm -f "$tmp"
fi

# Flush to the card. Pulling the power on a Pi is the normal way this box
# is switched off, and an unflushed FAT write is a lost diagnostic.
sync
exit 0
