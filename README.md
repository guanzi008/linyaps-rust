# linyaps-rust

`OpenAtom-Linyaps/linyaps` 的纯 Rust、命令兼容重构。兼容目标同步到上游提交
[`fcd650e8f7cc289ecb66392673edf7cec44a7df3`](https://github.com/OpenAtom-Linyaps/linyaps/commit/fcd650e8f7cc289ecb66392673edf7cec44a7df3)，版本输出保持 `1.15.0-dev`。

仓库不包含 C、C++、Go、Python 或汇编实现源码，也不调用原项目二进制。构建器按协议执行用户构建命令和系统工具；`misc` 中的 shell 文件是桌面/系统集成资源，不是替代实现。

## 功能范围

- `ll-cli` / `llpkg`：运行、进程管理、安装、卸载、升级、搜索、列表、仓库、信息、内容、清理和分析。
- `ll-builder`：项目校验、源码获取、依赖解析、容器构建、模块拆分、导入、导出、推送及 UAB 生成。
- `ll-package-manager`：完整 D-Bus/peer 服务、任务、交互、策略、安装、升级、卸载、清理和运行上下文。
- `linyaps-repository`：纯 Rust OSTree 兼容存储、远端传输、缓存、迁移、layer/UAB 导入导出及 EROFS。
- 运行时：OCI 配置、CDI、扩展、Wayland/XDG、进程状态和 `ll-box` 调用链。
- 系统组件：`ll-init`、驱动检测、systemd、D-Bus、polkit、sysusers、tmpfiles、shell 补全及 97 份本地化目录。

完整兼容范围、冻结行为和验证清单见 [`COMPATIBILITY.md`](COMPATIBILITY.md)。配套 OCI 运行时位于独立仓库 `linyaps-box-rust`。

## NVIDIA 宿主驱动透传分支

`feat/host-nvidia-passthrough` 在声明的 NVIDIA 扩展没有安装时，将与已加载内核驱动版本一致的宿主 NVIDIA 用户态库以只读方式挂进应用容器；已安装的扩展仍优先。这里是容器共享宿主 GPU，不是虚拟机 PCI/VFIO 直通，也不负责安装或加载内核驱动。

实现包括 SONAME 别名、x86 32/64 位库隔离、EGL/Wayland/GBM/Vulkan 配置、NVML/CUDA 库及 `nvidia-smi`。宿主文件指纹参与启动缓存 ID，避免升级驱动后继续使用旧缓存。显式 NVIDIA `--device` 优先使用 CDI、不再自动注入宿主驱动；宿主回退启用时也不自动叠加 NVIDIA CDI。

详细的源码分工、启用方法、参考分支和验证边界见 [`COMPATIBILITY.md`](COMPATIBILITY.md#nvidia-宿主回退实现分支)。本分支的自动化测试不等于 NVIDIA 硬件实测；发布前需要在有 NVIDIA 的机器上执行：

```sh
bash tests/system/nvidia-smoke.sh <已安装的应用ID>
LINYAPS_NVIDIA_GRAPHICS=1 bash tests/system/nvidia-smoke.sh <含glxinfo和vulkaninfo的应用ID>
```

运行应用要求内核启用非特权用户命名空间。系统仓库层通常由
`deepin-linglong` 服务账号持有，此时 `ll-cli` 会自动选择
`fuse-overlayfs` 并将层内属主映射到当前用户；本地用户持有的层可直接使用
内核 OverlayFS。Debian 包已声明 `fuse-overlayfs` 为运行依赖。

## 构建测试

要求 Linux 和 Rust 1.92 或更高版本：

```sh
cargo test --workspace --all-targets --all-features --locked
cargo clippy --workspace --all-targets --all-features --locked -- -D warnings
cargo build --workspace --release --locked
```

上游要求 `ll-init` 静态链接。官方 musl 目标可直接生成静态产物：

```sh
rustup target add x86_64-unknown-linux-musl
cargo build -p ll-init --release --locked --target x86_64-unknown-linux-musl
cp target/x86_64-unknown-linux-musl/release/ll-init target/release/ll-init
```

`aarch64-unknown-linux-musl`、`loongarch64-unknown-linux-musl` 和 `riscv64gc-unknown-linux-musl` 同样可用。其他架构可在安装了静态 libc 的 GNU 工具链上使用 `cargo rustc -p ll-init --release --locked -- -C target-feature=+crt-static`。安装器会拒绝动态链接的 `ll-init`。

## 安装布局

先构建本仓库和配套 `ll-box`，再生成可打包的系统根目录：

```sh
target/release/ll-system-helper install \
  --destdir ./package-root \
  --prefix /usr \
  --binary-dir target/release \
  --ll-box ../linyaps-box-rust/target/release/ll-box
```

## Debian 包

仓库包含标准 Debian 源码包目录 `debian/`，并通过 `dh-rust` 离线使用
Debian 提供的 `librust-*-dev` crate。先启用包含这些构建依赖的 Debian
仓库，然后执行：

```sh
sudo apt build-dep ./
dpkg-buildpackage --build=binary --no-sign
```

构建结果按照 Debian 惯例写入源码目录的上一级，包括 `linglong-bin`、
`linglong-builder`、`.changes` 和 `.buildinfo`。版本、架构、依赖计算、调试
符号拆分及校验和均由 Debian 工具链管理；发布新版本时应先更新
`debian/changelog`。`ll-init` 由打包规则使用目标架构的静态 GNU libc
重新链接，安装器仍会验证它没有动态解释器或动态依赖。

`linglong-builder` 是可选的开发工具包；运行应用只需 `linglong-bin` 与
配套仓库发布的 `linglong-box`。

## Arch Linux 包

`packaging/arch/` 包含可直接同步到 AUR 的 VCS 包配方。配方通过
`ll-system-helper install` 生成完整系统布局，不再手工复制部分文件；因此
会同时安装 X11 会话脚本、systemd 环境生成器、helper 入口和本地化目录。

许可证为 `LGPL-3.0-or-later`；随仓库保留了对应上游的完整 REUSE 许可证集合。
