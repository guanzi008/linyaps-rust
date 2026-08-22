# linyaps-rust

`OpenAtom-Linyaps/linyaps` 的纯 Rust、命令兼容重构。兼容目标冻结在上游提交
[`9a258eeefd848122669f24c3d79703136b483d7a`](https://github.com/OpenAtom-Linyaps/linyaps/commit/9a258eeefd848122669f24c3d79703136b483d7a)，版本输出保持 `1.14.0-dev`。

仓库不包含 C、C++、Go、Python 或汇编实现源码，也不调用原项目二进制。构建器按协议执行用户构建命令和系统工具；`misc` 中的 shell 文件是桌面/系统集成资源，不是替代实现。

## 架构说明

本项目是**单用户、每用户仓库**模型，与上游的多用户共享仓库 + D-Bus 服务模型不同：

- 每个用户的已安装应用存放在自己的目录：`$XDG_DATA_HOME/linglong`
  （默认 `~/.local/share/linglong`），可用 `LINGLONG_ROOT` 覆盖。
- 不再有 `ll-package-manager` 常驻服务，不再需要 `deepin-linglong` 系统
  用户、D-Bus 系统服务、polkit 鉴权或 sysusers/tmpfiles 集成。
- 安装、卸载、升级、搜索、清理等操作由 `ll-cli` 通过
  `linyaps-repository` crate 直接完成，进程内并发由 `RepoLock` 文件锁保证。
- 保留 XDG Desktop Portal（session bus）客户端调用，用于图形应用的
  Documents 挂载，不依赖任何包管理守护进程。

## 功能范围

- `ll-cli` / `llpkg`：运行、进程管理、安装、卸载、升级、搜索、列表、仓库、信息、内容、清理和分析。
- `ll-builder`：项目校验、源码获取、依赖解析、容器构建、模块拆分、导入、导出、推送及 UAB 生成。
- `linyaps-repository`：纯 Rust OSTree 兼容存储、远端传输、缓存、迁移、layer/UAB 导入导出、EROFS 及安装/卸载/升级/清理操作。
- 运行时：OCI 配置、CDI、扩展、Wayland/XDG、进程状态和 `ll-box` 调用链。
- 系统组件：`ll-init`、驱动检测、systemd 环境生成器、shell 补全及 97 份本地化目录。

完整兼容范围、冻结行为和验证清单见 [`COMPATIBILITY.md`](COMPATIBILITY.md)。配套 OCI 运行时位于独立仓库 `linyaps-box-rust`。

运行应用要求内核启用非特权用户命名空间。单用户模式下层由当前用户持有，
可直接使用内核 OverlayFS；`fuse-overlayfs` 仍作为运行依赖声明，用于
需要属主映射的场景。

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

包不再安装 `ll-package-manager` 守护进程及其 D-Bus/polkit/sysusers/tmpfiles
集成；`linglong-bin` 只安装 CLI、运行时和系统集成资源。包内的维护者脚本
仍会刷新 desktop/mime 数据库并重载 systemd。

`linglong-builder` 是可选的开发工具包；运行应用只需 `linglong-bin` 与
配套仓库发布的 `linglong-box`。

## Arch Linux 包

`packaging/arch/` 包含可直接同步到 AUR 的 VCS 包配方。配方通过
`ll-system-helper install` 生成完整系统布局，不再手工复制部分文件；因此
会同时安装 X11 会话脚本、systemd 环境生成器、helper 入口和本地化目录。

许可证为 `LGPL-3.0-or-later`；随仓库保留了冻结上游的完整 REUSE 许可证集合。
