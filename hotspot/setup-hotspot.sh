#!/usr/bin/env bash
# Turn the Pi into its own WiFi access point, so the dashboard is reachable
# from a phone at a site with no router, no WiFi and no mobile data.
#
#   sudo bash setup-hotspot.sh                configure / re-converge
#   sudo bash setup-hotspot.sh --status       report, change nothing
#   sudo bash setup-hotspot.sh --undo         remove everything this installed
#   sudo bash setup-hotspot.sh --no-activate  configure, apply on next reboot
#
# Settings live in hotspot.conf next to this script. Idempotent: safe to
# re-run at any time - it converges the machine to the state that file
# describes rather than assuming a clean one.
#
# WHY NetworkManager and not hostapd+dnsmasq: Bookworm made NetworkManager
# the only network stack on all images including Lite. dhcpcd and
# /etc/network/interfaces are gone, so the static-IP half of every older
# tutorial silently does nothing, and NM keeps managing wlan0 and fights
# hostapd for the interface. NM's own AP mode drives the same wpa_supplicant
# code path and spawns its own scoped dnsmasq for DHCP. One profile, no
# second daemon stack, which matters on a 512MB single core.
#
# WHY nothing here can stop the pump: this script never touches
# pump-protection.service, and none of the units it installs name it in any
# Wants=/Requires=/Before=/After=. That is asserted at the end of every run.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CONF="$HERE/hotspot.conf"

say()  { printf '\n\033[1m==> %s\033[0m\n' "$*"; }
did()  { printf '    \033[32m+\033[0m %s\n' "$*"; }
same() { printf '    \033[2m=\033[0m %s\n' "$*"; }
warn() { printf '    \033[33m!\033[0m %s\n' "$*"; }
bad()  { printf '    \033[31mx\033[0m %s\n' "$*"; }

MODE="apply"
ACTIVATE="yes"
for arg in "$@"; do
  case "$arg" in
    --status)      MODE="status" ;;
    --undo)        MODE="undo" ;;
    --no-activate) ACTIVATE="no" ;;         # configure now, take effect on reboot
    -h|--help)     sed -n '2,12p' "$0" | cut -c3-; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

[ -f "$CONF" ] || { echo "missing $CONF" >&2; exit 1; }
# shellcheck source=hotspot.conf
. "$CONF"

AP_URL="http://${AP_ADDR}:${WEB_PORT}/"
UNIT_DIR=/etc/systemd/system
# Units we own, in the order they should be removed by --undo.
OUR_UNITS=(pump-ap-watchdog.timer pump-ap-watchdog.service
           pump-ap-diagnostics.service pump-ap-regdom.service
           pump-portal.service)

# ------------------------------------------------------------------ status

status_report() {
  say "Configuration"
  echo "    SSID        $SSID"
  echo "    security    $([ -n "$PSK" ] && echo "WPA2-PSK (pmf=$PMF, strict-ciphers=$STRICT_CIPHERS)" || echo "OPEN - no password")"
  echo "    dashboard   $AP_URL"

  say "Radio"
  rfkill list wifi 2>/dev/null | sed 's/^/    /' || warn "rfkill unavailable"
  # "country 00: DFS-UNSET" here means the regulatory domain never took and
  # the AP will transmit nothing, however healthy everything else looks.
  iw reg get 2>/dev/null | grep -m1 '^country' | sed 's/^/    /' || warn "iw unavailable"

  say "NetworkManager profiles"
  nmcli -f NAME,TYPE,AUTOCONNECT,AUTOCONNECT-PRIORITY connection show 2>/dev/null | sed 's/^/    /' \
    || warn "cannot talk to NetworkManager"

  say "Active on $WIFI_IFACE"
  local active
  active="$(nmcli -t -f NAME,DEVICE connection show --active 2>/dev/null | awk -F: -v d="$WIFI_IFACE" '$2==d{print $1}')"
  [ -n "$active" ] && echo "    $active" || bad "nothing active - the AP is DOWN"
  ip -4 -o addr show "$WIFI_IFACE" 2>/dev/null | sed 's/^/    /' || true

  say "Attached phones (DHCP leases)"
  # NM's shared mode writes leases here. Globbed rather than hardcoded
  # because the filename carries the interface name.
  local n=0 f
  for f in /var/lib/NetworkManager/dnsmasq-*.leases; do
    [ -f "$f" ] || continue
    n=$(( n + $(grep -c . "$f" || true) ))
  done
  echo "    $n"

  say "Units"
  # systemctl prints its answer on stdout AND exits non-zero for
  # not-found/inactive, so `|| echo ...` would print two lines. Capture.
  local u st
  for u in "${OUR_UNITS[@]}"; do
    st="$(systemctl is-enabled "$u" 2>/dev/null || true)"
    printf '    %-30s %s\n' "$u" "${st:-not-installed}"
  done
  st="$(systemctl is-active pump-protection 2>/dev/null || true)"
  printf '    %-30s %s\n' "pump-protection.service" "${st:-unknown}"
}

if [ "$MODE" = "status" ]; then
  status_report
  exit 0
fi

if [ "$(id -u)" -ne 0 ]; then
  echo "Run with sudo." >&2
  exit 1
fi

# -------------------------------------------------------------------- undo

if [ "$MODE" = "undo" ]; then
  say "Removing units"
  for u in "${OUR_UNITS[@]}"; do
    if [ -f "$UNIT_DIR/$u" ]; then
      systemctl disable --now "$u" >/dev/null 2>&1 || true
      rm -f "$UNIT_DIR/$u"
      did "$u"
    fi
  done
  systemctl daemon-reload

  say "Removing AP profile"
  if nmcli -t -f NAME connection show 2>/dev/null | grep -qx "$AP_PROFILE"; then
    nmcli connection delete "$AP_PROFILE" >/dev/null \
      && did "$AP_PROFILE deleted" \
      || warn "could not delete $AP_PROFILE - remove it with nmcli by hand"
  else
    same "$AP_PROFILE not present"
  fi

  say "Removing config drop-ins"
  for f in /etc/NetworkManager/dnsmasq-shared.d/pump-portal.conf \
           /etc/modprobe.d/pump-ap-regdom.conf; do
    [ -f "$f" ] && { rm -f "$f"; did "$f"; } || true
  done
  rm -f /usr/local/sbin/pump-ap-ensure /usr/local/sbin/pump-ap-diagnostics \
        /usr/local/sbin/pump-ap-regdom /usr/local/sbin/pump-portal-responder

  # Undo means undo: put back the two system-wide settings we changed, so
  # this box is not left subtly different from one that never ran us.
  apt-mark unhold dnsmasq >/dev/null 2>&1 && did "unheld dnsmasq" || true
  systemctl enable NetworkManager-wait-online.service >/dev/null 2>&1 \
    && did "re-enabled NetworkManager-wait-online" || true

  systemctl restart NetworkManager || true

  warn "Other WiFi profiles were left with autoconnect as this script set it."
  warn "Re-enable your home network with: nmcli con modify <name> autoconnect yes"
  say "Done. The pump protection service was not touched."
  exit 0
fi

# ------------------------------------------------------------- sanity gates

say "Checks"

# The Zero W is ARMv6 and cannot run a 64-bit image at all, so the only
# thing worth pinning is the release. Pi OS 13 (Trixie) cannot bring up a
# WPA-protected nmcli hotspot on an original Zero W - activation dies with
# "802.1X supplicant took too long to authenticate" and the kernel logs
# "key setting validation failed" from brcmfmac. Upstream issue
# raspberrypi/linux#7247, open, no workaround. An OPEN hotspot still comes
# up on Trixie, which is why this warns rather than refuses.
# Read in a subshell: sourcing os-release into this scope would overwrite
# whatever hotspot.conf set if the two ever share a variable name.
os_ver="$(. /etc/os-release 2>/dev/null; echo "${VERSION_ID:-}")"
os_code="$(. /etc/os-release 2>/dev/null; echo "${VERSION_CODENAME:-}")"
if [ "$os_ver" = "12" ]; then
  same "Raspberry Pi OS 12 (bookworm) - the supported release"
else
  warn "os-release says VERSION_ID=${os_ver:-unknown} (${os_code:-?})."
  warn "WPA hotspots are broken on Zero W from Pi OS 13 (Trixie) onward."
  warn "If the phone cannot associate, reflash Bookworm 32-bit. See README."
fi

command -v nmcli >/dev/null || { bad "nmcli not found - this needs NetworkManager"; exit 1; }
systemctl is-active --quiet NetworkManager || { bad "NetworkManager is not running"; exit 1; }
same "NetworkManager $(nmcli --version | awk '{print $NF}')"

ip link show "$WIFI_IFACE" >/dev/null 2>&1 || { bad "no interface $WIFI_IFACE"; exit 1; }

if [ -n "$PSK" ] && { [ ${#PSK} -lt 8 ] || [ ${#PSK} -gt 63 ]; }; then
  bad "PSK must be 8-63 characters (or empty for an open AP)"; exit 1
fi

case "$CHANNEL" in 1|6|11) ;; *) warn "channel $CHANNEL is not 1/6/11 - overlapping, but legal" ;; esac

# If this is being run over the AP itself, re-activating the profile drops
# the very SSH session issuing the command. Detect and defer.
if [ "$ACTIVATE" = "yes" ] && [ -n "${SSH_CONNECTION:-}" ]; then
  client="${SSH_CONNECTION%% *}"
  if [ "${client%.*}" = "${AP_ADDR%.*}" ]; then
    ACTIVATE="no"
    warn "You are connected over the hotspot itself ($client)."
    warn "Deferring activation so this session survives. Reboot to apply."
  fi
fi

# --------------------------------------------------------------- packages

say "Packages"
# dnsmasq-base is the load-bearing one: NM's ipv4.method=shared uses it for
# DHCP. Without it the phone associates and never gets an address, which
# looks exactly like a broken AP and is invisible without a shell.
need_pkgs=()
dpkg -s dnsmasq-base >/dev/null 2>&1 || need_pkgs+=(dnsmasq-base)
command -v iw      >/dev/null 2>&1 || need_pkgs+=(iw)
command -v rfkill  >/dev/null 2>&1 || need_pkgs+=(rfkill)

if [ ${#need_pkgs[@]} -gt 0 ]; then
  warn "installing: ${need_pkgs[*]}  (this step needs internet - run it at home)"
  apt-get update -qq
  apt-get install -y -qq "${need_pkgs[@]}"
  did "installed ${need_pkgs[*]}"
else
  same "dnsmasq-base, iw, rfkill all present"
fi
dpkg -s dnsmasq-base >/dev/null 2>&1 || { bad "dnsmasq-base still missing - DHCP will not work"; exit 1; }

# The standalone dnsmasq DAEMON is actively harmful here: it binds :53
# system-wide and collides with the private instance NM spawns for shared
# mode. dnsmasq-base is the binary without the daemon.
if dpkg -s dnsmasq >/dev/null 2>&1 && [ "$(dpkg-query -W -f='${Status}' dnsmasq 2>/dev/null)" = "install ok installed" ]; then
  warn "the full dnsmasq daemon is installed and will fight NetworkManager"
  systemctl disable --now dnsmasq >/dev/null 2>&1 && did "disabled dnsmasq.service" || true
fi
apt-mark hold dnsmasq >/dev/null 2>&1 && same "dnsmasq held (cannot arrive as a dependency)" || true

# ------------------------------------------------------- regulatory domain

say "Regulatory domain ($COUNTRY)"
# Three independent mechanisms, because this is the highest-consequence
# setting on the box and each one has a failure mode the others cover.

# 1. modprobe option - applied when cfg80211 loads, before anything else
#    can care. Deterministic when cfg80211 is a module, which it is on
#    Raspberry Pi OS. No effect if it is built into the kernel.
regdom_conf=/etc/modprobe.d/pump-ap-regdom.conf
regdom_line="options cfg80211 ieee80211_regdom=$COUNTRY"
if [ "$(cat "$regdom_conf" 2>/dev/null)" = "$regdom_line" ]; then
  same "$regdom_conf"
else
  echo "$regdom_line" > "$regdom_conf"
  did "$regdom_conf"
fi

# 2. The vendor path, if raspi-config is present. Best effort - the exact
#    mechanism it uses has changed across releases, so it is not trusted
#    on its own.
if command -v raspi-config >/dev/null 2>&1; then
  raspi-config nonint do_wifi_country "$COUNTRY" >/dev/null 2>&1 \
    && did "raspi-config do_wifi_country $COUNTRY" \
    || warn "raspi-config do_wifi_country failed (non-fatal)"
fi

# 3. A boot-time re-assert ordered before NetworkManager. Covers the
#    built-in-cfg80211 case and anything that resets the domain later.
install -m 755 "$HERE/pump-ap-regdom.sh" /usr/local/sbin/pump-ap-regdom
sed "s|__COUNTRY__|$COUNTRY|g" "$HERE/pump-ap-regdom.service" > "$UNIT_DIR/pump-ap-regdom.service"
did "pump-ap-regdom.service"

rfkill unblock wifi || true
iw reg set "$COUNTRY" 2>/dev/null || warn "iw reg set failed now - the boot service will retry"

reg_now="$(iw reg get 2>/dev/null | grep -m1 '^country' || true)"
case "$reg_now" in
  "country $COUNTRY:"*) same "$reg_now" ;;
  *)  warn "${reg_now:-no regulatory domain reported}"
      warn "expected 'country $COUNTRY:'. Re-check with 'iw reg get' AFTER a reboot -"
      warn "if it still says 00 the AP will transmit nothing." ;;
esac

# ------------------------------------------------------- competing profiles

say "Competing WiFi profiles"
# Raspberry Pi Imager's WiFi customisation creates a profile literally named
# "preconfigured" with autoconnect=yes. Every guide that says "set your home
# profile to autoconnect no" names a profile that does not exist, so the
# bench rehearsal silently runs on home WiFi instead of the AP - which is
# the one thing the rehearsal is supposed to prove. Enumerate real names.
while IFS=: read -r name ctype; do
  [ "$ctype" = "802-11-wireless" ] || continue
  [ "$name" = "$AP_PROFILE" ] && continue
  if [ "$(nmcli -g connection.autoconnect connection show "$name" 2>/dev/null)" = "yes" ]; then
    nmcli connection modify "$name" connection.autoconnect no connection.autoconnect-priority 0
    did "$name -> autoconnect no (bring it up by hand when you need it)"
  else
    same "$name already autoconnect no"
  fi
done < <(nmcli -t -f NAME,TYPE connection show)

# -------------------------------------------------------------- AP profile

say "Access point profile '$AP_PROFILE'"

if ! nmcli -t -f NAME connection show | grep -qx "$AP_PROFILE"; then
  # Create bare, then converge below. A rejected property aborts the whole
  # 'add', so keeping 'add' minimal means a syntax problem surfaces on one
  # named property in the modify loop instead of as a blanket failure.
  nmcli connection add type wifi ifname "$WIFI_IFACE" con-name "$AP_PROFILE" \
        ssid "$SSID" autoconnect yes >/dev/null
  did "created"
else
  same "exists - converging"
fi

props=(
  802-11-wireless.ssid            "$SSID"
  802-11-wireless.mode            ap
  # 2.4GHz b/g/n only radio; fixing the channel avoids an ACS pass the
  # brcmfmac AP path does poorly.
  802-11-wireless.band            bg
  802-11-wireless.channel         "$CHANNEL"
  802-11-wireless.hidden          no
  # 2 = disabled. Station-side power save is a STA concept and NM does not
  # document AP-mode behaviour, so do NOT treat this as the reason response
  # stalls will not happen - it is free insurance, not a guarantee.
  802-11-wireless.powersave       2
  # NM's shared mode assigns 10.42.x.1/24 unless told otherwise, and every
  # label and QR code printed for this box says 192.168.4.1. Set it.
  ipv4.method                     shared
  ipv4.addresses                  "$AP_ADDR/$AP_PREFIX"
  # Nothing here needs v6; removes a source of activation delay.
  ipv6.method                     disabled
  connection.autoconnect          yes
  # An AP-mode profile is always "available" - unlike a client profile it
  # needs no matching SSID in a scan - so with nothing else in range it is
  # the only candidate. Priority 100 makes that deterministic rather than
  # incidental.
  connection.autoconnect-priority 100
  # 0 = retry forever. On a cold, slow board the first activation can lose
  # a race; without this NM gives up permanently.
  connection.autoconnect-retries  0
)

if [ -n "$PSK" ]; then
  props+=( wifi-sec.key-mgmt wpa-psk wifi-sec.psk "$PSK" wifi-sec.pmf "$PMF" )
  if [ "$STRICT_CIPHERS" = "yes" ]; then
    props+=( wifi-sec.proto rsn wifi-sec.pairwise ccmp wifi-sec.group ccmp )
  else
    # Empty clears them. NetworkManager's own settings documentation says
    # to leave pairwise/group empty for maximum client compatibility.
    props+=( wifi-sec.proto "" wifi-sec.pairwise "" wifi-sec.group "" )
  fi
fi
# Open AP (empty PSK): add NO wifi-sec props. NetworkManager 1.52 (Trixie)
# rejects wifi-sec.key-mgmt="" with "property is missing"; the security
# setting is stripped entirely after the loop instead.

# Apply one at a time so a property this NM version rejects names itself
# instead of failing the whole batch anonymously.
i=0
while [ $i -lt ${#props[@]} ]; do
  k="${props[$i]}"; v="${props[$((i+1))]}"; i=$((i+2))
  nmcli connection modify "$AP_PROFILE" "$k" "$v" \
    || { bad "NetworkManager rejected: $k = '$v'"; exit 1; }
done
# Open AP: strip any security setting so NM treats it as unsecured. Blanking
# key-mgmt is rejected on NM 1.52; removing the whole setting is accepted.
if [ -z "$PSK" ]; then
  nmcli connection modify "$AP_PROFILE" remove 802-11-wireless-security 2>/dev/null || true
fi
did "$( [ -n "$PSK" ] && echo "WPA2-PSK, pmf=$PMF, strict-ciphers=$STRICT_CIPHERS" || echo 'OPEN - no password' )"
did "$AP_ADDR/$AP_PREFIX on $WIFI_IFACE, channel $CHANNEL, autoconnect priority 100"

# ------------------------------------------------------------ captive portal

say "Captive portal probe answers"
portal_dir=/etc/NetworkManager/dnsmasq-shared.d
if [ "$ENABLE_PORTAL" = "yes" ]; then
  install -d -m 755 "$portal_dir"
  sed "s|__AP_ADDR__|$AP_ADDR|g" "$HERE/pump-portal.conf" > "$portal_dir/pump-portal.conf"
  did "$portal_dir/pump-portal.conf"

  # Substitute, do not copy raw. This line used to be a plain install and the
  # responder shipped with literal __AP_ADDR__ in it, so every captive-portal
  # redirect pointed at a hostname that does not exist.
  sed -e "s|__AP_ADDR__|$AP_ADDR|g" -e "s|__WEB_PORT__|$WEB_PORT|g" \
      "$HERE/portal-responder.py" > /usr/local/sbin/pump-portal-responder
  chmod 755 /usr/local/sbin/pump-portal-responder
  sed -e "s|__AP_ADDR__|$AP_ADDR|g" -e "s|__WEB_PORT__|$WEB_PORT|g" \
      "$HERE/pump-portal.service" > "$UNIT_DIR/pump-portal.service"
  did "pump-portal.service (own process, port 80, never inside the control loop)"
else
  rm -f "$portal_dir/pump-portal.conf" "$UNIT_DIR/pump-portal.service"
  same "disabled"
fi

# ------------------------------------------------------------------- units

say "Watchdog and diagnostics"
install -m 755 "$HERE/pump-ap-ensure.sh" /usr/local/sbin/pump-ap-ensure
install -m 755 "$HERE/pump-ap-diagnostics.sh" /usr/local/sbin/pump-ap-diagnostics
for u in pump-ap-watchdog.service pump-ap-watchdog.timer pump-ap-diagnostics.service; do
  sed -e "s|__PROFILE__|$AP_PROFILE|g" -e "s|__IFACE__|$WIFI_IFACE|g" \
      "$HERE/$u" > "$UNIT_DIR/$u"
  did "$u"
done

systemctl daemon-reload
systemctl enable pump-ap-regdom.service pump-ap-diagnostics.service pump-ap-watchdog.timer >/dev/null
[ "$ENABLE_PORTAL" = "yes" ] && systemctl enable pump-portal.service >/dev/null || true
did "enabled at boot"

# This service can stall boot up to 45s waiting for a network that will
# never be "online" at the field site. pump-protection.service is
# After=network.target, which waits for nothing, so this is purely about
# not staring at a blank LCD for 45 seconds.
if systemctl is-enabled --quiet NetworkManager-wait-online.service 2>/dev/null; then
  systemctl disable NetworkManager-wait-online.service >/dev/null 2>&1
  did "disabled NetworkManager-wait-online (saves up to 45s of boot)"
fi

# ------------------------------------------------------- the safety assertion

say "Asserting networking cannot stop the pump"
fail=0
# Match systemd DIRECTIVES, not the word anywhere in the file - these units
# carry comments that say "no relationship with pump-protection.service",
# and a plain text search would flag its own documentation.
dep_re='^[[:space:]]*(After|Before|Wants|Requires|Requisite|BindsTo|PartOf|Conflicts|Upholds)=.*pump-protection'
for u in "${OUR_UNITS[@]}"; do
  [ -f "$UNIT_DIR/$u" ] || continue
  if grep -Eqi "$dep_re" "$UNIT_DIR/$u"; then
    bad "$u declares a dependency on pump-protection.service"; fail=1
  fi
done
# And the converse: the pump unit must not have grown a dependency on the
# network. It ships After=network.target, which orders but does not wait.
# Anything stronger means a dead radio can hold the contactor open.
pp="$UNIT_DIR/pump-protection.service"
if [ -f "$pp" ]; then
  if grep -Eqi '^(Wants|Requires|BindsTo)=.*(NetworkManager|network-online)' "$pp"; then
    bad "pump-protection.service depends on the network - remove that line"; fail=1
  else
    same "pump-protection.service has no hard network dependency"
  fi
else
  warn "pump-protection.service not installed yet (run ../install.sh)"
fi
[ "$fail" -eq 0 ] || { bad "refusing to finish with the above unresolved"; exit 1; }
same "no unit installed here names pump-protection"

# ---------------------------------------------------------------- activate

if [ "$ACTIVATE" = "yes" ]; then
  say "Activating"
  systemctl restart NetworkManager      # picks up the dnsmasq-shared drop-in
  sleep 2
  # nmcli exit 3 = timeout, 4 = activation failed. Widely published recipes
  # have these two swapped, and get it exactly backwards: they mark a real
  # activation failure as success, which is the one thing a watchdog exists
  # to catch. Checked against `man nmcli`, EXIT STATUS.
  rc=0
  nmcli -w 30 connection up "$AP_PROFILE" >/dev/null 2>&1 || rc=$?
  if [ "$rc" -eq 0 ]; then
    did "$AP_PROFILE is up"
  else
    case $rc in
      3) warn "activation timed out - may still complete, check --status in a minute" ;;
      4) bad  "activation FAILED. Check: iw reg get, rfkill list wifi, journalctl -u NetworkManager -b" ;;
      *) bad  "nmcli exited $rc" ;;
    esac
  fi
  [ "$ENABLE_PORTAL" = "yes" ] && systemctl restart pump-portal.service || true
  systemctl start pump-ap-watchdog.timer
else
  warn "not activating now - reboot to apply"
fi

# ------------------------------------------------------------------ summary

cat <<SUMMARY

$(printf '\033[1m--- write this inside the enclosure lid ---\033[0m')

    SSID       $SSID
    PASSWORD   $( [ -n "$PSK" ] && echo "$PSK" || echo '(none - open network)' )
    DASHBOARD  $AP_URL

On the phone, in this order:
    1. Airplane mode ON, then WiFi back ON.   <- not optional, see README
    2. Join $SSID
    3. Browser: $AP_URL   (type the http:// )

Verify from here:  sudo bash setup-hotspot.sh --status
Cold-boot test:    sudo reboot     (then --status again)

The only test that counts is a cold boot with no other network in range,
and an association from the actual phone you are taking. Neither has
happened yet.

SUMMARY
