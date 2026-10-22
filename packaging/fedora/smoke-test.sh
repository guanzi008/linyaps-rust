#!/usr/bin/env bash
set -euo pipefail
test "$(id -u)" = 0
if [[ ! -f /.dockerenv && ! -f /run/.containerenv ]]; then
    printf 'Run this destructive package lifecycle test only in a disposable container\n' >&2
    exit 2
fi
packages=${1:-/packages}
grep -Eq '^ID="?fedora"?$' /etc/os-release
dnf -y --setopt=install_weak_deps=False install "$packages"/RPMS/*.rpm dbus-daemon findutils
rpm -q linyaps-rust linyaps-rust-builder linyaps-box-rust
rpm -V linyaps-rust linyaps-rust-builder linyaps-box-rust
dnf check
ll-cli --version
ll-builder --version
ll-box --version
ll-cli --help >/dev/null
ll-builder --help >/dev/null
ll-box --help >/dev/null
test "$(readlink -f /usr/libexec/linglong/fetch-file-source)" = /usr/bin/ll-builder
test "$(readlink -f /usr/libexec/linglong/app-conf-generator)" = /usr/libexec/linglong/ll-system-helper
mkdir -p /tmp/fedora-rpm-helper-probe
/usr/libexec/linglong/app-conf-generator org.example.RpmProbe /tmp/fedora-rpm-helper-probe
/usr/lib/systemd/system-environment-generators/61-linglong | grep -q 'XDG_DATA_DIRS='
helper_result=0
/usr/libexec/linglong/fetch-file-source >/tmp/fedora-rpm-fetch-usage.log 2>&1 || helper_result=$?
test "$helper_result" != 0
grep -q 'fetch-file-source OUTPUT URL SHA256 CACHE' /tmp/fedora-rpm-fetch-usage.log
test -x /etc/X11/xinit/xinitrc.d/50-linglong.sh
test -r /usr/share/zsh/site-functions/_ll-cli
test ! -e /etc/X11/Xsession.d/21linglong
test "$(stat -c %U /var/lib/linglong)" = deepin-linglong
test "$(stat -c %a /run/linglong)" = 1777
test -r /usr/share/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf
test -r /usr/share/polkit-1/actions/org.deepin.linglong.PackageManager1.policy
mkdir -p /run/dbus
dbus-daemon --system --fork
runuser -u deepin-linglong -- /usr/libexec/linglong/ll-package-manager >/tmp/ll-package-manager.log 2>&1 &
manager_pid=$!
trap 'kill "$manager_pid" 2>/dev/null || true' EXIT
ready=0
for ((attempt = 0; attempt < 30; attempt++)); do
    if dbus-send --system --print-reply --dest=org.freedesktop.DBus /org/freedesktop/DBus org.freedesktop.DBus.NameHasOwner string:org.deepin.linglong.PackageManager1 | grep -q 'boolean true'; then
        ready=1
        break
    fi
    sleep 1
done
if [[ $ready != 1 ]]; then
    cat /tmp/ll-package-manager.log >&2
    exit 1
fi
useradd --create-home validation-user
runuser -u validation-user -- dbus-send --system --print-reply --dest=org.deepin.linglong.PackageManager1 /org/deepin/linglong/PackageManager1 org.freedesktop.DBus.Properties.Get string:org.deepin.linglong.PackageManager1 string:Configuration
runuser -u validation-user -- ll-cli list
runuser -u validation-user -- ll-cli repo show
kill "$manager_pid"
wait "$manager_pid" || true
trap - EXIT
printf 'retain repository data\n' >/var/lib/linglong/fedora-rpm-smoke-sentinel
dnf -y reinstall "$packages"/RPMS/*.rpm
rpm -V linyaps-rust linyaps-rust-builder linyaps-box-rust
grep -q 'retain repository data' /var/lib/linglong/fedora-rpm-smoke-sentinel
dnf -y --setopt=clean_requirements_on_remove=False remove linyaps-rust linyaps-rust-builder linyaps-box-rust
test ! -e /usr/bin/ll-cli
test ! -e /usr/bin/ll-box
test ! -e /usr/bin/ll-builder
test ! -e /usr/share/dbus-1/system.d/org.deepin.linglong.PackageManager1.conf
grep -q 'retain repository data' /var/lib/linglong/fedora-rpm-smoke-sentinel
dnf -y install "$packages"/RPMS/*.rpm
rpm -V linyaps-rust linyaps-rust-builder linyaps-box-rust
dnf check
printf 'Fedora RPM installation, system D-Bus, reinstall and removal smoke tests passed\n'
