# Changelog

本文件随版本发布手动维护（push tag 发布，不再使用 release-please 自动生成）。

## Unreleased（2026-11-08）：服务入口更名 `oext-host` → `ol-host`，交付链落地

版本号未变（仍为 `0.1.0`），本次是命名与交付链变更，协议未变。

### 重命名

- 服务可执行文件 `oext-host` 更名为 `ol-host`（Windows `ol-host.exe`），Cargo 二进制目标同步改为 `ol-host`；`bin/ol-host` 成为唯一正式服务入口。
- 旧名不提供别名或转发脚本。消费者需替换启动路径与下载清单里的 `entry`，迁移步骤见 `docs/service-contract.md` 第 1.2 节。
- 不随本次改名调整：`host.*` 协议方法、Rust crate 名称（`originlang-ext-*`）、`OEXT_*` 环境变量、安装根目录布局。
- `oext-sandbox` 名称与源码保持不变，但**不随本次发布**；包清单写 `sandbox: null`。

### 发布链

- 新增 `.github/scripts/package_release.py`：生成 `bin/ol-host` + `manifest.json` + `SHA256SUMS` + `docs/` + `examples/`（nightly 另含 `build-info.json`），并写出 `<archive>.sha256`。取代 `.github/scripts/package.ps1`（已删除）。
- 新增 `.github/scripts/verify_package.py`、`.github/scripts/smoke_host.py`、`.github/scripts/test_package.py`：分别做校验链验证、解压后冒烟验收、打包脚本自测。
- 修复发布契约的循环校验：清单不再记录 `SHA256SUMS` 自身哈希，`SHA256SUMS` 覆盖含 `manifest.json` 在内的全部文件，归档哈希由旁挂 `<archive>.sha256` 承担信任锚。清单 `schemaVersion` 升为 `2`，新增 `hostVersion` 字段，移除 `checksums` 字段；发布契约升为版本 `2`。
- `build.yml` 改为构建 → 入口冒烟 → 打包 → 从解压包校验与冒烟；正式版与 nightly 共用同一条路径。`.act/nightly.yml` 改用同一脚本，本地 nightly 与正式版包结构一致。
- 本地打包与 CI 同一脚本、同一包结构：`python3 .github/scripts/package_release.py --build --verify` 一条命令完成构建、打包、校验与解压冒烟。`--target` 省略时从 `rustc -vV` 读取 host 三元组，`target/release/` 与 `target/<triple>/release/` 两种 cargo 产物布局均被接受；显式 `--target` 时只用对应目录，避免把本机二进制标成其他平台。
- 质量检查新增 `test_package.py`；测试任务新增 `ol-host --version` 冒烟，防止改名回退。

### 修复

- 非回环绑定现在会在 stderr 提示「已鉴权但未加密、无 TLS」（服务契约 M-09）。
- 清理 `cargo fmt` 与 `clippy::collapsible_if` 的既有告警，使 `cargo fmt --check`、`cargo clippy -D warnings` 通过。
- 移除误提交的 `.github/scripts/__pycache__/check-release.cpython-313.pyc`，并忽略 `__pycache__/`。

### 未完成

- 关闭优雅期（5 秒）与关闭中 `-32022`、通配绑定的发现地址（服务契约 M-05、M-10）。
- Windows/Linux 的打包与冒烟尚未在 CI 实跑确认。
