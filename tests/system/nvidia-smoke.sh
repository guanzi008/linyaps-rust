#!/usr/bin/env bash

set -euo pipefail

app=${1:?Usage: bash tests/system/nvidia-smoke.sh APPID}
ll_cli=${LINYAPS_CLI_BINARY:-ll-cli}
nvidia_smi=${LINYAPS_NVIDIA_SMI:-nvidia-smi}

expected=$("$nvidia_smi" --query-gpu=uuid,driver_version --format=csv,noheader | sort)
if [[ -z $expected ]]; then
    printf 'No working host NVIDIA GPU found\n' >&2
    exit 1
fi

for pass in first repeat; do
    actual=$("$ll_cli" run "$app" -- bash -c '
        set -eu
        if [[ ${NVIDIA_CTK_LIBCUDA_DIR:-} != /opt/extensions/org.deepin.driver.display.nvidia.*/orig ]]; then
            printf "Host fallback is not active; check the NVIDIA extension declaration and installed extensions\n" >&2
            exit 1
        fi
        test -r "$NVIDIA_CTK_LIBCUDA_DIR/libcuda.so.1"
        executable=${NVIDIA_CTK_LIBCUDA_DIR%/orig}/bin/nvidia-smi
        test -x "$executable"
        "$executable" --query-gpu=uuid,driver_version --format=csv,noheader
    ' | sort)
    if [[ $actual != "$expected" ]]; then
        printf '%s run differs from host\nhost:\n%s\ncontainer:\n%s\n' "$pass" "$expected" "$actual" >&2
        exit 1
    fi
    printf 'NVIDIA %s run: host/container UUID and driver version match\n' "$pass"
done

if [[ ${LINYAPS_NVIDIA_GRAPHICS:-0} == 1 ]]; then
    glx_output=$("$ll_cli" run --env __NV_PRIME_RENDER_OFFLOAD=1 --env __GLX_VENDOR_LIBRARY_NAME=nvidia "$app" -- glxinfo -B)
    printf '%s\n' "$glx_output"
    if ! grep -qi 'OpenGL renderer string:.*NVIDIA' <<<"$glx_output"; then
        printf 'OpenGL renderer is not NVIDIA\n' >&2
        exit 1
    fi
    vulkan_output=$("$ll_cli" run "$app" -- vulkaninfo --summary)
    printf '%s\n' "$vulkan_output"
    if ! grep -qi 'driverName.*NVIDIA\|deviceName.*NVIDIA' <<<"$vulkan_output"; then
        printf 'Vulkan did not enumerate an NVIDIA device\n' >&2
        exit 1
    fi
fi
