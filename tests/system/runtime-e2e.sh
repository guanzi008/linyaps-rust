#!/usr/bin/env bash

set -euo pipefail

project_root=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/../.." && pwd)
workspace_root=$(dirname -- "$project_root")
box_project=${LINYAPS_BOX_PROJECT:-"$workspace_root/linyaps-box-rust"}
keep_work=${LINYAPS_E2E_KEEP_WORK:-0}

work=$(mktemp -d "${TMPDIR:-/tmp}/linyaps-runtime-e2e.XXXXXX")
cleanup() {
    local status=$?
    if [[ $status -eq 0 && $keep_work != 1 ]]; then
        rm -rf -- "$work"
    else
        printf 'runtime E2E work directory: %s\n' "$work" >&2
    fi
}
trap cleanup EXIT

if [[ ${LINYAPS_E2E_SKIP_BUILD:-0} != 1 ]]; then
    cargo build --locked --bins --manifest-path "$project_root/Cargo.toml"
    cargo build --locked --manifest-path "$box_project/Cargo.toml"
fi

ll_builder=${LINYAPS_BUILDER_BINARY:-"$project_root/target/debug/ll-builder"}
ll_cli=${LINYAPS_CLI_BINARY:-"$project_root/target/debug/ll-cli"}
ll_init=${LINYAPS_INIT_BINARY:-"$project_root/target/debug/ll-init"}
package_manager=${LINYAPS_PACKAGE_MANAGER_BINARY:-"$project_root/target/debug/ll-package-manager"}
ll_box=${LINYAPS_BOX_BINARY:-"$box_project/target/debug/ll-box"}

for binary in "$ll_builder" "$ll_cli" "$ll_init" "$package_manager" "$ll_box"; do
    if [[ ! -x $binary ]]; then
        printf 'required executable is missing: %s\n' "$binary" >&2
        exit 1
    fi
done

case $(uname -m) in
    x86_64 | amd64) architecture=x86_64 ;;
    aarch64 | arm64) architecture=arm64 ;;
    loongarch64) architecture=loong64 ;;
    riscv64) architecture=riscv64 ;;
    mips64 | mips64el) architecture=mips64 ;;
    *)
        printf 'unsupported test architecture: %s\n' "$(uname -m)" >&2
        exit 1
        ;;
esac

base=$work/base
app=$work/app
rootfs=$base/files
repository=$work/repository
runtime=$work/runtime
home=$work/home
repo_lock=$work/repository.lock
mkdir -p "$rootfs" "$app/files/bin" "$runtime" "$home"
touch "$repo_lock"
chmod 700 "$runtime" "$home"

copy_file() {
    local source=$1
    local destination=$2
    local resolved mode
    resolved=$(readlink -f -- "$source")
    mode=$(stat -c '%a' -- "$resolved")
    install -D -m "$mode" -- "$resolved" "$rootfs$destination"
}

copy_dependencies() {
    local executable=$1
    local dependency
    while IFS= read -r dependency; do
        [[ -n $dependency ]] && copy_file "$dependency" "$dependency"
    done < <(
        ldd "$executable" 2>/dev/null | awk '
            /=> \/[^ ]+/ {
                for (field = 1; field <= NF; field++) {
                    if ($field ~ /^\//) {
                        print $field
                        break
                    }
                }
                next
            }
            /^[[:space:]]*\// { print $1 }
        '
    )
}

if [[ -n ${LINYAPS_E2E_ROOTFS:-} ]]; then
    if [[ ! -d $LINYAPS_E2E_ROOTFS ]]; then
        printf 'LINYAPS_E2E_ROOTFS is not a directory: %s\n' "$LINYAPS_E2E_ROOTFS" >&2
        exit 1
    fi
    cp -a -- "$LINYAPS_E2E_ROOTFS/." "$rootfs/"
else
    if [[ ! -x /bin/bash ]]; then
        printf 'required host executable is missing: /bin/bash\n' >&2
        exit 1
    fi
    copy_file /bin/bash /bin/bash
    copy_dependencies "$(readlink -f -- /bin/bash)"

    ldconfig=/sbin/ldconfig
    if [[ -x /sbin/ldconfig.real ]]; then
        ldconfig=/sbin/ldconfig.real
    fi
    if [[ ! -x $ldconfig ]]; then
        printf 'required host executable is missing: %s\n' "$ldconfig" >&2
        exit 1
    fi
    copy_file "$ldconfig" /sbin/ldconfig
    copy_dependencies "$(readlink -f -- "$ldconfig")"
    copy_dependencies "$ll_init"
fi

mkdir -p \
    "$rootfs/etc/ld.so.conf.d" \
    "$rootfs/opt" \
    "$rootfs/proc" \
    "$rootfs/run" \
    "$rootfs/sys" \
    "$rootfs/tmp" \
    "$rootfs/usr" \
    "$rootfs/var"
chmod 1777 "$rootfs/tmp"
printf ':\n' >"$rootfs/etc/profile"
printf 'include /etc/ld.so.conf.d/*.conf\n' >"$rootfs/etc/ld.so.conf"

cat >"$base/info.json" <<EOF
{
  "arch": ["$architecture"],
  "base": "",
  "channel": "stable",
  "description": "Linyaps Rust runtime E2E base",
  "id": "org.deepin.base",
  "kind": "base",
  "module": "binary",
  "name": "Runtime E2E Base",
  "schema_version": "1.0",
  "size": 1,
  "version": "23.1.0.0"
}
EOF

cat >"$app/info.json" <<EOF
{
  "arch": ["$architecture"],
  "base": "org.deepin.base/23.1.0",
  "channel": "stable",
  "command": ["/opt/apps/org.example.RuntimeE2E/files/bin/demo", "default-arg"],
  "description": "Linyaps Rust full runtime E2E application",
  "id": "org.example.RuntimeE2E",
  "kind": "app",
  "module": "binary",
  "name": "Runtime E2E App",
  "schema_version": "1.0",
  "size": 1,
  "version": "1.0.0.0"
}
EOF

cat >"$app/files/bin/demo" <<'EOF'
#!/bin/bash
set -eu
printf 'APP_RUNTIME_OK:%s:%s:%s\n' "${LINGLONG_APPID:-missing}" "$UID" "${XDG_RUNTIME_DIR:-missing}"
printf 'APP_ARGS:%s\n' "$*"
EOF
chmod 755 "$app/files/bin/demo"

cat >"$work/builder.yaml" <<EOF
version: 1
repo: $repository
offline: true
EOF

LINGLONG_BUILDER_CONFIG="$work/builder.yaml" "$ll_builder" import-dir "$base"
LINGLONG_BUILDER_CONFIG="$work/builder.yaml" "$ll_builder" import-dir "$app"

uid=$(id -u)
expected=$(printf 'APP_RUNTIME_OK:org.example.RuntimeE2E:%s:/run/user/%s\nAPP_ARGS:default-arg' "$uid" "$uid")
stderr_log=$work/runtime.stderr.log
runtime_status=0
output=$(
    HOME="$home" \
    XDG_RUNTIME_DIR="$runtime" \
    DBUS_SESSION_BUS_ADDRESS="unix:path=$runtime/nonexistent-session-bus" \
    LINGLONG_ROOT="$repository" \
    LINGLONG_REPO_LOCK="$repo_lock" \
    LINGLONG_PACKAGE_MANAGER="$package_manager" \
    LINGLONG_PACKAGE_MANAGER_DIRECT=1 \
    LINGLONG_PEER_ALLOW_UNPRIVILEGED=1 \
    LINGLONG_OCI_RUNTIME="$ll_box" \
    LINYAPS_CONTAINER_INIT="$ll_init" \
    "$ll_cli" --no-dbus run org.example.RuntimeE2E 2>"$stderr_log"
) || runtime_status=$?

if ((runtime_status != 0)); then
    printf 'runtime invocation failed with status %s\nstderr:\n' "$runtime_status" >&2
    cat "$stderr_log" >&2
    exit "$runtime_status"
fi

if [[ $output != "$expected" ]]; then
    printf 'unexpected runtime output\nexpected:\n%s\nactual:\n%s\nstderr:\n' "$expected" "$output" >&2
    cat "$stderr_log" >&2
    exit 1
fi

container_list=$(XDG_RUNTIME_DIR="$runtime" "$ll_box" list --format json)
if [[ ${container_list//[[:space:]]/} != '[]' ]]; then
    printf 'container state leaked after application exit: %s\n' "$container_list" >&2
    exit 1
fi

process_state=$runtime/linglong/processes/$uid
if [[ -d $process_state ]] && find "$process_state" -type f -print -quit | grep -q .; then
    printf 'process state leaked after application exit:\n' >&2
    find "$process_state" -type f -print >&2
    exit 1
fi

printf '%s\n' "$output"
printf 'runtime E2E passed\n'
