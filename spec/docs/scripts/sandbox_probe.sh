#!/bin/sh
# Runs inside a sandbox and prints KEY=VALUE lines describing what the process can see and do.
# Non-destructive: it writes only inside its own directory (the workspace) and /tmp, and removes what it creates.
# The host side writes ref.env next to this script first: the host's namespace ids, a host PID, and a host /tmp marker.
here=$(cd "$(dirname "$0")" && pwd)
. "$here/ref.env"

for ns in user mnt pid net ipc uts; do
    inside=$(readlink "/proc/self/ns/$ns" 2>/dev/null)
    eval "host=\$HOST_NS_$ns"
    if [ -z "$inside" ]; then echo "ns_$ns=unknown"; elif [ "$inside" = "$host" ]; then echo "ns_$ns=shared"; else echo "ns_$ns=private"; fi
done
echo "uid=$(id -u)"
echo "cap_eff=$(awk '/^CapEff/ {print $2}' /proc/self/status)"
echo "no_new_privs=$(awk '/^NoNewPrivs/ {print $2}' /proc/self/status)"
echo "seccomp=$(awk '/^Seccomp:/ {print $2}' /proc/self/status)"
echo "visible_pids=$(ls /proc | grep -c '^[0-9][0-9]*$')"
if kill -0 "$HOST_PID" 2>/dev/null; then echo "signal_host=allowed"; else echo "signal_host=blocked"; fi

if touch "$here/written" 2>/dev/null; then echo "write_workspace=allowed"; rm -f "$here/written"; else echo "write_workspace=blocked"; fi
probe="$HOME/.fridica-probe-$$"
if touch "$probe" 2>/dev/null; then echo "write_home=allowed"; rm -f "$probe"; else echo "write_home=blocked"; fi
if [ -e "$HOME/.claude/settings.json" ]; then
    if [ -w "$HOME/.claude/settings.json" ]; then echo "write_settings=allowed"; else echo "write_settings=blocked"; fi
else echo "write_settings=absent"; fi
if [ -d "$HOME/.claude/hooks" ]; then
    if [ -w "$HOME/.claude/hooks" ]; then echo "write_hooks=allowed"; else echo "write_hooks=blocked"; fi
else echo "write_hooks=absent"; fi
if [ -e "$HOST_TMP_MARKER" ]; then echo "host_tmp=visible"; else echo "host_tmp=hidden"; fi
if touch "/tmp/fridica-probe-$$" 2>/dev/null; then echo "write_tmp=allowed"; rm -f "/tmp/fridica-probe-$$"; else echo "write_tmp=blocked"; fi

if [ -r /etc/os-release ]; then echo "read_system=allowed"; else echo "read_system=blocked"; fi
if ls "$HOME" >/dev/null 2>&1; then echo "read_home=allowed"; else echo "read_home=blocked"; fi
if [ -d "$HOME/.ssh" ]; then
    if ls "$HOME/.ssh" >/dev/null 2>&1; then echo "read_ssh=allowed"; else echo "read_ssh=blocked"; fi
else echo "read_ssh=absent"; fi
echo "dev_entries=$(ls /dev 2>/dev/null | wc -l)"
if [ -e /dev/nvidiactl ]; then echo "gpu_devices=visible"; else echo "gpu_devices=absent"; fi
if [ -S "/run/user/$(id -u)/bus" ]; then echo "session_bus=visible"; else echo "session_bus=hidden"; fi
if command -v busctl >/dev/null 2>&1 && busctl --user --no-pager get-property org.freedesktop.systemd1 \
        /org/freedesktop/systemd1 org.freedesktop.systemd1.Manager Version >/dev/null 2>&1; then
    echo "systemd_user_bus=usable"; else echo "systemd_user_bus=unusable"; fi
if [ -z "$SSH_AUTH_SOCK" ]; then echo "ssh_agent=not_inherited"
else ssh-add -l >/dev/null 2>&1; rc=$?; if [ $rc -le 1 ]; then echo "ssh_agent=reachable"; else echo "ssh_agent=unreachable"; fi; fi
if python3 -c "import socket; s=socket.socket(socket.AF_UNIX); s.close()" 2>/dev/null; then echo "unix_socket=allowed"; else echo "unix_socket=blocked"; fi
if python3 -c "import socket; s=socket.create_connection(('1.1.1.1', 443), timeout=3); s.close()" 2>/dev/null; then
    echo "direct_network=allowed"; else echo "direct_network=blocked"; fi
if [ -n "$HTTPS_PROXY$https_proxy" ]; then echo "proxy_env=set"; else echo "proxy_env=unset"; fi
