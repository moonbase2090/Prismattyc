#!/bin/sh
set -eu
# All keys and writes live inside this disposable container, never on the host.
mkdir -p /run/sshd /root/.ssh
chmod 700 /root/.ssh
ssh-keygen -q -t ed25519 -N '' -f /root/.ssh/id_ed25519
cp /root/.ssh/id_ed25519.pub /root/.ssh/authorized_keys
chmod 600 /root/.ssh/authorized_keys
/usr/sbin/sshd -o ListenAddress=127.0.0.1 -o PasswordAuthentication=no -o PermitRootLogin=prohibit-password
ssh-keyscan -t ed25519 127.0.0.1 > /root/.ssh/known_hosts
printf 'Host terminfo-test\n  HostName 127.0.0.1\n  User root\n  BatchMode yes\n' > /root/.ssh/config
PRISMATTYC_TERMINFO_SSH_TEST=1 python3 scripts/terminfo_test.py
