#!/usr/bin/env bash
# Prepare a disposable Ubuntu Actions runner for workspace-confined helpers.
set -euo pipefail
sudo apt-get update
sudo apt-get install -y bubblewrap "$@"
if [ "$(sysctl -n kernel.apparmor_restrict_unprivileged_userns 2>/dev/null)" = 1 ]; then
  printf '%s\n' 'abi <abi/4.0>,' 'include <tunables/global>' '' \
    'profile bwrap /usr/bin/bwrap flags=(unconfined) {' '  userns,' \
    '  include if exists <local/bwrap>' '}' | sudo tee /etc/apparmor.d/bwrap > /dev/null
  sudo apparmor_parser -r /etc/apparmor.d/bwrap
fi
bwrap --unshare-user --ro-bind / / -- /bin/true
