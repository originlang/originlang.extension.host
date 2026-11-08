# 项目范围与协作边界

本文件约束本仓库内的开发工作。修改对外契约时同步说明消费者需要的适配。

## 职责与交付

- Host 是扩展运行基础设施，负责安装、校验、授权、生命周期、后端路由、扩展存储、事件与定时任务。
- 本仓库优先交付可独立下载和运行的 Host 服务 `bin/ol-host`（Windows 为 `bin/ol-host.exe`），并维护内部 Rust 库。`oext-sandbox` 的源码当前位于本仓库，是否由本仓库交付待定，不作为当前 Host 交付的前置条件。服务启动参数属于本项目，通用用户 CLI 属于 `bootidea.cli`。
- `ol-host` 是唯一正式服务入口；旧名 `oext-host` 已取消且不提供别名。更名只涉及可执行文件与交付物：`host.*` 协议方法、Rust crate 名称（`originlang-ext-*`）、`OEXT_*` 环境变量与安装根目录布局都不随之调整。
- 执行后端包括进程外 sandbox、进程内 QuickJS 和原生 dylib；明确区分资源限制、进程隔离与操作系统权限隔离。不含沙箱的发布包必须报告 `sandbox: null`，不得暗示具备进程隔离。
- dylib 插件共享宿主地址空间，仅用于可信代码，能力授权不能强制约束其直接文件或网络访问。

## 向其他项目提供能力

```text
bootidea.cli（独立运行）--可选下载与进程通信--> Host 独立服务
Host --运行时加载--> harness.dll / 其他扩展
HomeWorld（独立运行）--可选下载与进程通信--> Host 独立服务
Host --按需启动--> oext-sandbox
```

- Host 拥有服务进程入口和服务端通信协议；CLI 与 HomeWorld 通过版本化协议调用，不将宿主运行引擎编译进自身。
- Harness 是独立发布的动态库插件。Host 不编译依赖 harness，也不内置其模型循环。
- HomeWorld 不直接链接 Host 库，不加载 Rust 动态库，不通过 FFI 调用宿主。
- CLI 与 HomeWorld 各自可下载 Host 发布包、启动或连接 Host；两者的基础功能均不得依赖 Host 的安装与运行。
- Host 独立发布平台产物、版本及完整性校验信息，明确协议、插件 ABI 和沙箱兼容性。服务可执行文件名固定为 `ol-host`，发布清单的 `entry` 必须与之一致，不沿用 CLI 名称作为架构约定。
- `oext-sandbox` 是执行体，不是完整宿主服务；Host 发布包是否包含它必须按真实产物说明。当前包不含沙箱，清单写 `sandbox: null`。
- stdio 协议输出不得混入日志；诊断写 stderr，stdin 关闭应触发清理退出。网络传输必须保留鉴权与明确的监听配置。

## 不属于本项目的工作

- 模型供应商适配、agent 循环和上下文策略属于 harness。
- 用户命令与终端体验属于 CLI；宿主服务入口与服务端传输属于本项目。
- 家庭账号、成员权限、资料库、网页和家庭业务数据库属于 HomeWorld。
- 对 ABI、工具协议、能力名称和包格式的变更应保留兼容性或明确迁移方案；各仓库保持可独立构建。

## 交付与校验

发布包固定为 `oext-<version>-<target>/`（`.tar.gz` / `.zip`），包内 `bin/ol-host`、`manifest.json`、`SHA256SUMS`、`docs/`、`examples/`，nightly 另有 `build-info.json`。

- 打包与验收只能用 `.github/scripts/` 下的脚本：`package_release.py` 生成包，`verify_package.py` 校验收链，`smoke_host.py` 从解压后的包做冒烟验收。不要在 CI 里手写清单或校验和。
- 校验链条是单向的：归档哈希（旁挂 `<archive>.sha256`，消费者从发布页获取）→ `SHA256SUMS`（覆盖含 `manifest.json` 在内的全部文件）→ `manifest.files`（覆盖除 `manifest.json`、`SHA256SUMS` 外的载荷）。任何文件都不得包含自身哈希；`manifest.files` 出现 `manifest.json` 或 `SHA256SUMS` 即视为包非法。
- 正式版与 nightly 共用同一条构建/打包/验收路径（`.github/workflows/build.yml`），发布阶段只做传输完整性检查，不重新打包。

## 迁移说明

以上是目标架构约束。独立服务入口 `ol-host`（`apps/host`，原名 `oext-host`，2026-11-08 更名）已落地，承载 stdio/tcp/http 传输、`host.hello` 握手、协议版本协商、按根目录单例锁与退出码区分；改名不改变协议、配置与数据目录，消费者迁移步骤见 `docs/service-contract.md` 第 1.2 节。CLI/HomeWorld 迁移为协议客户端的后续工作（C01—C03、F01—F02）仍在推进，本文不代表消费端迁移已经完成。
