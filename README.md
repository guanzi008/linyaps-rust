# linyaps-rust

`OpenAtom-Linyaps/linyaps` 的纯 Rust、命令兼容重构。兼容目标冻结在上游提交
[`9a258eeefd848122669f24c3d79703136b483d7a`](https://github.com/OpenAtom-Linyaps/linyaps/commit/9a258eeefd848122669f24c3d79703136b483d7a)，版本输出保持 `1.14.0-dev`。

仓库不包含 C、C++、Go、Python 或汇编实现源码，也不调用原项目二进制。构建器按协议执行用户构建命令和系统工具；`misc` 中的 shell 文件是桌面/系统集成资源，不是替代实现。

## 功能范围

- `ll-cli` / `llpkg`：运行、进程管理、安装、卸载、升级、搜索、列表、仓库、信息、内容、清理和分析。
- `ll-builder`：项目校验、源码获取、依赖解析、容器构建、模块拆分、导入、导出、推送及 UAB 生成。
- `ll-package-manager`：完整 D-Bus/peer 服务、任务、交互、策略、安装、升级、卸载、清理和运行上下文。
- `linyaps-repository`：纯 Rust OSTree 兼容存储、远端传输、缓存、迁移、layer/UAB 导入导出及 EROFS。
- 运行时：OCI 配置、CDI、扩展、Wayland/XDG、进程状态和 `ll-box` 调用链。
- 系统组件：`ll-init`、驱动检测、systemd、D-Bus、polkit、sysusers、tmpfiles、shell 补全及 97 份本地化目录。

完整兼容范围、冻结行为和验证清单见 [`COMPATIBILITY.md`](COMPATIBILITY.md)。配套 OCI 运行时位于独立仓库 `linyaps-box-rust`。

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

许可证为 `LGPL-3.0-or-later`；随仓库保留了冻结上游的完整 REUSE 许可证集合。
