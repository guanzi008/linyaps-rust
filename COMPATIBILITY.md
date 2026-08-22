# Compatibility Baseline

## 冻结目标

- 上游仓库：[`OpenAtom-Linyaps/linyaps`](https://github.com/OpenAtom-Linyaps/linyaps)
- 冻结提交：`9a258eeefd848122669f24c3d79703136b483d7a`
- 对外版本：`1.14.0-dev`
- 配套运行时提交：`OpenAtom-Linyaps/linyaps-box@2f6023b609f500b756b558bf0b87be4e504c53f5`

兼容判断以冻结提交的实际可执行行为、输出、磁盘格式和安装布局为准，而不是后续上游分支。

## 与上游的有意分歧

本项目采用**单用户、每用户仓库**模型，因此**不提供**以下上游接口：

- `ll-package-manager` 守护进程及 `org.deepin.linglong.PackageManager1`
  D-Bus 系统服务（含 Task1、peer 服务、任务排队、交互、polkit 鉴权）。
- `deepin-linglong` 系统用户、sysusers/tmpfiles 集成。

对应的功能（安装、卸载、升级、搜索、清理、仓库配置）由 `ll-cli` 直接调用
`linyaps-repository` 的 `operations` 模块完成，CLI 行为保持与冻结版本兼容。
仓库默认位于 `$XDG_DATA_HOME/linglong`（`LINGLONG_ROOT` 可覆盖）。

## 已覆盖接口

- 冻结可执行名：`ll-cli`、`llpkg`、`ll-builder`、`ll-builder-export`、`ll-driver-detect`、`ll-init`、`ll-system-helper`、`uab-header`、`uab-loader`。
- `ll-cli` 和 `ll-builder` 的命令、别名、短参数、错误码、帮助、版本、表格/JSON 输出及本地化文本。
- 本地和远端 OSTree 对象、引用、提交、checkout、缓存、迁移、删除恢复、prune、上传及下载。
- layer 文件、UAB ELF section、签名数据、纯 Rust EROFS 读写、校验和压缩格式。
- builder 项目、依赖、源码、Debian source、模块、容器、检查、导入、导出和推送流程。
- OCI 运行配置、CDI、扩展、Wayland、XDG、字体/动态链接缓存、应用配置和进程状态。
- systemd 环境生成器、MIME、desktop、图标、补全、profile 和 97 份 locale 安装布局。

没有保留调用冻结 C++ 程序的后备路径。Debian source 对内建格式使用 Rust 实现；冻结实现本来就委托 `dpkg-source` 的其他格式仍按相同协议调用系统工具。

## 冻结语义

- 支持架构名及 triplet：`x86_64`、`arm64`、`loongarch64`、`loong64`、`sw64`、`riscv64`、`mips64`。
- `ll-init` 必须为静态 Linux ELF；安装前会检查 `PT_INTERP` 和动态依赖。
- OCI 运行时仅接受冻结版本实际支持的 disabled cgroup manager。
- 冻结 `linyaps-box` 只解析 seccomp 配置且运行路径留有 TODO；重构保持这一实际行为，不额外安装过滤器。
- TLS、OSTree、EROFS、MO 编码和 OCI 控制路径均为 Rust 实现，不依赖 OpenSSL、libgit2、libcurl 或原项目库；builder 仍按冻结协议调用 Git、tar、dpkg 等系统构建工具作为兼容后备。

## 验证门

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
