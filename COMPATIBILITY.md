# Compatibility Baseline

## 冻结目标

- 上游仓库：[`OpenAtom-Linyaps/linyaps`](https://github.com/OpenAtom-Linyaps/linyaps)
- 冻结提交：`fcd650e8f7cc289ecb66392673edf7cec44a7df3`
- 对外版本：`1.15.0-dev`
- 配套运行时提交：`OpenAtom-Linyaps/linyaps-box@ca5e503ea72553f65b3e4e4bfe86c707eeeb63ea`

兼容判断以冻结提交的实际可执行行为、输出、D-Bus 接口、磁盘格式和安装布局为准，而不是后续上游分支。

## 已覆盖接口

- 所有冻结可执行名：`ll-cli`、`llpkg`、`ll-builder`、`ll-builder-export`、`ll-driver-detect`、`ll-init`、`ll-package-manager`、`ll-system-helper`、`uab-header`、`uab-loader`。
- `ll-cli` 和 `ll-builder` 的命令、别名、短参数、错误码、帮助、版本、表格/JSON 输出及本地化文本。
- PackageManager1、Task1、peer 服务、属性、信号、任务排队、交互、polkit 与直接模式。
- 本地和远端 OSTree 对象、引用、提交、checkout、缓存、迁移、删除恢复、prune、上传及下载。
- layer 文件、UAB ELF section、签名数据、纯 Rust EROFS 读写、校验和压缩格式。
- UABX 分发包、目标架构 builder-utils、模块化 entries 导出和 ELF 签名数据生命周期。
- builder 项目、依赖、源码、Debian source、模块、容器、检查、导入、导出和推送流程。
- OCI 运行配置、CDI、扩展、Wayland、XDG、字体/动态链接缓存、应用配置和进程状态。
- systemd、D-Bus、polkit、sysusers、tmpfiles、MIME、desktop、图标、补全、profile 和 97 份 locale 安装布局。

没有保留调用冻结 C++ 程序的后备路径。Debian source 对内建格式使用 Rust 实现；冻结实现本来就委托 `dpkg-source` 的其他格式仍按相同协议调用系统工具。

## 冻结语义

- 跟进 `32793c63` / `fcd650e8`：binary 模块生成 appid 命令包装器，支持参数转义、更新、模块/升级卸载保留和最终卸载清理；导出版本为 `1.0.0.5`，systemd 环境生成器在存在时将 `entries/bin` 去重追加到 PATH。
- 支持架构名及 triplet：`x86_64`、`arm64`、`loongarch64`、`loong64`、`sw64`、`riscv64`、`mips64`。
- `ll-init` 必须为静态 Linux ELF；安装前会检查 `PT_INTERP` 和动态依赖。
- OCI 运行时仅接受冻结版本实际支持的 disabled cgroup manager。
- 冻结 `linyaps-box` 只解析 seccomp 配置且运行路径留有 TODO；重构保持这一实际行为，不额外安装过滤器。
- TLS、OSTree、EROFS、MO 编码和 OCI 控制路径均为 Rust 实现，不依赖 OpenSSL、libgit2、libcurl 或原项目库；builder 仍按冻结协议调用 Git、tar、dpkg 等系统构建工具作为兼容后备。

## 验证门

### NVIDIA 宿主回退实现分支

源码重新梳理后的职责划分：

- `apps/ll-cli/src/runtime.rs`：解析应用/base/runtime 扩展，已安装扩展优先；缺少 NVIDIA 扩展时选择宿主回退，协调显式/自动 CDI、环境覆盖和最终 OCI 挂载顺序。
- `crates/linyaps-core/src/host_nvidia.rs`：共用驱动版本检测、受限目录扫描、ELF/SONAME 读取、32/64 位分离、图形 JSON 改写、只读挂载和文件指纹。只读取 ELF 头、程序头、动态段及 SONAME，不整份读取大型驱动库，不执行 `ldd`、`nvidia-ctk` 或原 C++ 程序。
- `crates/linyaps-api/src/lib.rs`：可选 `hostNvidiaExtension` 与 `hostNvidiaFingerprint` 字段；未启用时不影响旧上下文 JSON。
- `apps/ll-package-manager/src/run_context.rs`：使用同一发现/挂载实现初始化 ldconfig 缓存；库文件、SONAME、manifest 或同版本驱动替换导致新缓存 ID，过期上下文被拒绝并提示重试。
- `linyaps-box-rust`：继续使用已有 OCI bind mount/设备挂载机制，本次没有新上游提交，也不需要复制一份 NVIDIA 发现代码进去。

参考的是用户 C++ 分支 [`feat/host-nvidia-fallback-mainline@28384f3e`](https://github.com/guanzi008/linyaps/tree/28384f3e51a7a8b7395a00c6c9854170e4976732) 及 `split/host-nvidia-fallback`，不是将整段 C++ 翻译成 Rust。与参考实现有意不同：只挂载选定的 NVIDIA 文件，不把整个宿主 `/usr`、通用 GLVND、libc 覆盖进容器；拒绝与内核版本不符的核心 NVIDIA 库。独立版本的 EGL 平台库（包括 egl-wayland2）单独识别。JSON 中宿主绝对库路径改写成已验证的 SONAME/文件名，防止沿用容器中不存在的宿主路径。

使用原有应用/base/runtime 的 `org.deepin.driver.display.nvidia` 扩展声明即可自动启用回退。没有此声明的应用，可以在 `~/.config/linglong/apps/<APPID>/config.d/90-host-nvidia.json` 添加以下内容（将两处 `<APPID>` 替换为实际应用 ID）：

```json
{
  "ext_defs": {
    "<APPID>": [{
      "name": "org.deepin.driver.display.nvidia",
      "version": "",
      "directory": "/opt/extensions/nvidia"
    }]
  }
}
```

不自动卸载已安装的驱动扩展。自动路径的优先级是：已安装扩展优先；缺失时宿主回退；未回退时维持原自动 CDI 选择。用户显式指定的 CDI 始终保留；显式 NVIDIA CDI 禁用自动宿主回退，避免 CDI 使用不同驱动根目录时被宿主扫描错误阻断。对于外部传入且同时包含宿主驱动和 NVIDIA CDI 的上下文，不再追加宿主 Vulkan manifest。运行配置和 `--env` 仍可覆盖自动生成环境。已有 `/dev/nvidia*`、`/dev/dri` 透传机制继续复用，不将此功能当作 GPU/MIG 的设备访问控制边界。

Vulkan 使用 [Khronos 的追加型发现变量](https://github.com/KhronosGroup/Vulkan-Loader/blob/main/docs/LoaderInterfaceArchitecture.md)，保留 Mesa 的默认 ICD，而非强制 `VK_ICD_FILENAMES`；旧版 Vulkan loader 可能不支持这些变量，需要更新容器 runtime。[egl-wayland2](https://github.com/NVIDIA/egl-wayland2) 也要求匹配的宿主驱动和容器 Wayland/GBM 依赖。本实现不会向容器移植宿主 libc，也不启动宿主 Xorg、加载内核固件或自动安装 OpenCL loader。

当前自动化覆盖：无驱动路径、proc/sys 版本解析、Arch/Debian 布局、SONAME、32/64 位隔离、错误版本/架构、越界链接和畸形 ELF/JSON、只读 OCI 顺序、显式 CDI、安装扩展优先、同版本文件替换和配置变更的缓存失效。`tests/system/nvidia-smoke.sh` 对比宿主/容器 GPU UUID 与驱动版本，两次运行检查重复启动，可选检查 OpenGL renderer 和 Vulkan 枚举；它要求实际 NVIDIA 硬件与已安装应用，不会安装驱动或更改系统配置。

本次开发环境没有检测到 NVIDIA 硬件驱动，因此尚不能宣称 WeChat 图形加速、CUDA 内核执行、32 位 GPU 程序和所有发行版驱动组合已完成硬件验证；应在 NVIDIA 机器完成这些测试后再合入发布。

已在本机通过非 GPU 的完整 `ll-cli → package-manager → ll-box → ll-init` 启动回归，另在真实 rootless OCI 容器内验证了模拟 NVIDIA ELF 库的 SONAME 链接、只读目录/文件和 ldconfig 缓存路径。后者默认忽略（依赖配套 `ll-box` 和用户命名空间），x86_64 上可单独执行：

```sh
LINYAPS_TEST_OCI_RUNTIME="$(realpath ../linyaps-box-rust/target/debug/ll-box)" \
  cargo test -p linyaps-core host_nvidia::tests::oci_readonly_mounts_and_ldconfig_use_the_same_layout \
  --locked -- --ignored
```

### 通用验证

持续集成执行：

```sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo test --workspace --all-targets --all-features --locked
cargo build --workspace --release --locked
```

同时执行静态 `ll-init` 构建与安装审计、首方/vendored 原生源码审计、原生 TLS/Git 依赖审计。跨仓运行链可在两个项目同级目录执行：

```sh
tests/system/runtime-e2e.sh
```

发布验证还在 Deepin 25 虚拟机中使用系统仓库的服务账号属主层实际启动
`org.deepin.calculator`，检查窗口、`ll-cli ps`、`ll-cli kill`、退出状态、
`ll-box list` 及挂载清理。该路径覆盖用户/挂载命名空间、FUSE OverlayFS、
图形会话透传和真实 OCI 容器生命周期，而不是仅运行合成单元测试。

实现期间还对冻结二进制完成了 CLI 非法/边界参数、确定性参数模糊测试、帮助/版本、本地化 MO、UAB、`ll-init` 和 `ll-box` 生命周期的差分矩阵。仓库内单元、集成和系统测试是发布前必须全部通过的最终门禁。
