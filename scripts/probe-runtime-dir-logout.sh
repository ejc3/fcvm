#!/usr/bin/env bash
# What a logout does to a user's files on tmpfs, and why fcvm keeps VM states under
# /run/fcvm-<uid> or in the data directory and never in /dev/shm or /run/user/<uid>
# (src/runtime_dir.rs).
#
# logind removes a user's System V and POSIX IPC objects, which is every file the user owns
# in /dev/shm, when the user's last login session ends (RemoveIPC=yes, the default), unless
# the user is root, a system user (uid at or below SYS_UID_MAX) or lingers. A service of
# the user has no login session, so an unrelated ssh login coming and going is enough.
#
# For an ordinary user (with and without lingering), a system user and root, each as:
#   service   a system service running as the user holds a file in /dev/shm and one in a
#             root-made /run/fcvm-probe-<uid>, and keeps running; one ssh login of that
#             user opens and closes; are the files there 40 s later?
#   detached  the file's maker is started from an ssh session that then ends, and keeps
#             running; is its /dev/shm file there 40 s later?
# and whether /run/user/<uid> exists before, during and after the login.
#
# Measured 2026-11-26, Ubuntu 24.04, systemd 255.4, RemoveIPC=yes:
#   ordinary user, no lingering, service:  /dev/shm file GONE after 12 s, /run file there,
#                                          /run/user/<uid> absent before and after the login
#   ordinary user, lingering, service:     both files there
#   system user, service; root, service:   both files there
#   detached, every user:                  file there (logind keeps the session "closing"
#                                          while a process of it runs)
#
# Not a test of any lane: it makes two user accounts, installs an ssh key for them and
# for root, and needs sshd and logind, which the container lane does not have. Run it as
# root on a host you can change. It removes what it made.
set -uo pipefail
[ "$(id -u)" = 0 ] || { echo "run as root"; exit 2; }
for tool in ssh ssh-keygen loginctl systemd-run busctl useradd; do
  command -v "$tool" > /dev/null || { echo "BLOCKED: $tool is missing"; exit 2; }
done
work=$(mktemp -d)
key=$work/key
ssh-keygen -q -t ed25519 -N '' -f "$key"
login() { ssh -i "$key" -o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null -o LogLevel=ERROR -o BatchMode=yes -o ConnectTimeout=10 "$1@localhost" "$2"; }
made_users=()
cleanup() {
  for u in ripc ripcsys root; do
    systemctl stop "ripc-probe-$u" 2> /dev/null
    rm -rf "/dev/shm/ripc-$u" "/run/fcvm-probe-$(id -u "$u" 2> /dev/null)"
    home=$(getent passwd "$u" | cut -d: -f6)
    [ -n "$home" ] && [ -f "$home/.ssh/authorized_keys" ] && sed -i "\|$(cut -d' ' -f2 "$key.pub")|d" "$home/.ssh/authorized_keys"
  done
  for u in "${made_users[@]}"; do loginctl disable-linger "$u" 2> /dev/null; pkill -u "$u" 2> /dev/null; userdel -r "$u" 2> /dev/null; done
  rm -rf "$work"
}
trap cleanup EXIT
echo "logind RemoveIPC: $(busctl get-property org.freedesktop.login1 /org/freedesktop/login1 org.freedesktop.login1.Manager RemoveIPC); $(systemctl --version | head -1)"
id ripc > /dev/null 2>&1 || { useradd -m -s /bin/bash ripc && made_users+=(ripc); }
id ripcsys > /dev/null 2>&1 || { useradd -r -m -s /bin/bash ripcsys && made_users+=(ripcsys); }
for u in ripc ripcsys root; do
  home=$(getent passwd "$u" | cut -d: -f6)
  install -d -m 700 -o "$u" "$home/.ssh"
  cat "$key.pub" >> "$home/.ssh/authorized_keys"
  chown "$u" "$home/.ssh/authorized_keys"; chmod 600 "$home/.ssh/authorized_keys"
done
there() { [ -e "$1" ] && echo there || echo GONE; }
after_logout() {  # FILE: its state up to 40 s after the login ended
  local i
  for i in $(seq 1 20); do sleep 2; [ -e "$1" ] || { echo "GONE after $((i * 2)) s"; return; }; done
  echo "there after 40 s"
}
in_session='echo "session $(cat /proc/self/sessionid), XDG_RUNTIME_DIR ${XDG_RUNTIME_DIR:-unset}"'
row() {  # USER LABEL
  local u=$1 label=$2 uid shm run before seen
  uid=$(id -u "$u"); shm=/dev/shm/ripc-$u/d/f; run=/run/fcvm-probe-$uid/d/f
  systemctl stop "ripc-probe-$u" 2> /dev/null; loginctl terminate-user "$u" 2> /dev/null; sleep 12
  rm -rf "/dev/shm/ripc-$u" "/run/fcvm-probe-$uid"; install -d -m 0700 -o "$u" "/run/fcvm-probe-$uid"
  before="sessions $(loginctl list-sessions --no-legend | grep -c " $u "), /run/user/$uid $([ -d "/run/user/$uid" ] && echo there || echo absent)"
  systemd-run --quiet --unit="ripc-probe-$u" --collect -p User="$u" sh -c "mkdir -p /dev/shm/ripc-$u/d /run/fcvm-probe-$uid/d && echo x > $shm && echo x > $run && sleep 600"
  sleep 2
  seen=$(login "$u" "$in_session" 2>&1 | tail -1)
  echo "ROW $label service: before the login [$before; /dev/shm file $(there "$shm"), /run file $(there "$run")]; in it [$seen]; after it: /dev/shm file $(after_logout "$shm"), /run file $(there "$run"), service $(systemctl is-active "ripc-probe-$u"), /run/user/$uid $([ -d "/run/user/$uid" ] && echo there || echo absent)"
  systemctl stop "ripc-probe-$u" 2> /dev/null; rm -rf "/dev/shm/ripc-$u" "/run/fcvm-probe-$uid"; sleep 12
  seen=$(login "$u" "mkdir -p /dev/shm/ripc-$u/d && echo x > $shm && (nohup setsid sleep 300.7 > /dev/null 2>&1 < /dev/null &); $in_session" 2>&1 | tail -1)
  echo "ROW $label detached: in the login [$seen]; after it: /dev/shm file $(after_logout "$shm"), maker running $(pgrep -u "$u" -fc 'sleep 300[.]7'), user state $(loginctl show-user "$u" -p State --value 2>&1 | head -1)"
  # Only this probe's maker: root has other sleeps.
  pkill -u "$u" -f 'sleep 300[.]7' 2> /dev/null; rm -rf "/dev/shm/ripc-$u"
}
row ripc ordinary-user-no-lingering
loginctl enable-linger ripc; sleep 2
row ripc ordinary-user-lingering
loginctl disable-linger ripc
row ripcsys system-user
row root root
