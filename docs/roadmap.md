# Host 实施路线图

更新日期：2026-11-08。本文是计划与状态记录，区分「已实现」「已验证」与「待办」；职责以仓库根 `AGENTS.md` 为准。

## 目标

交付可独立下载和运行的扩展宿主服务 `ol-host`。CLI 与 HomeWorld 均为可选消费者，不将 Host 运行引擎编译进自身；harness 由本服务动态加载。

## 实施顺序

1. 编写服务协议与发布清单规范：入口名称、启动参数、配置与数据目录、协议协商、就绪检查、错误、能力查询、平台产物、校验及 ABI 兼容范围。
   - 服务契约：[docs/service-contract.md](service-contract.md)（H01，修订 `1.1`）。固定入口 `ol-host`、参数与配置优先级、按根目录隔离的实例、`host.hello` 版本协商、错误码与退出码；第 12 节列出实现缺口与 2026-11-08 复核状态。
   - 发布契约：[docs/release-contract.md](release-contract.md)（H02，版本 `2`）。固定包结构、`manifest.json`（`schemaVersion: 2`）、`SHA256SUMS` 与无环校验链。
2. 将 CLI 中的宿主装配、服务入口与服务端传输迁入本项目；本地优先 stdio，保留现有网络鉴权。
3. 交付 `bin/ol-host`（Windows 为 `bin/ol-host.exe`）；`oext-sandbox` 是否由本仓库交付待定，不作为当前交付前置条件。harness 保持单独发布，不作为服务启动前提。
4. 提供扩展安装、授权、启停、状态与日志的稳定协议，供两种消费者适配。
5. 为 harness 的任务事件与取消提供所需宿主能力，避免将 agent 循环引入宿主。

## 验收标准

- 不安装 CLI、HomeWorld、harness 或沙箱即可启动、报告版本及能力、接受请求并退出。
- stdio 协议不混入日志；EOF 和正常关闭触发清理，异常有明确错误。
- 发布清单准确描述平台、校验、协议、插件 ABI 与沙箱版本关系，且校验链可实际生成与验证。
- 客户端只能管理自己拥有的实例，连接已有实例不隐含停止或升级权限。

## 状态（2026-11-08）

### 已实现并本地验证

| 项 | 证据 |
| --- | --- |
| 独立入口命名 `ol-host`（Cargo bin 目标、帮助文本、诊断前缀、文档、脚本） | `cargo build -p originlang-ext-host-bin`；包内 `bin/ol-host --version` 输出 `0.1.0` |
| `host.hello` 握手、`host.info`、`-32023` 版本拒绝 | stdio 冒烟脚本；`cargo test -p originlang-ext-host-bin` |
| stdio EOF 退出 `0`，`run/` 无残留，stdout 只有协议帧 | `smoke_host.py`（macOS aarch64 包） |
| 按根目录单例：第二实例退出 `3` 并报持有者 pid；`host.json` 权限 `0600` | TCP 实测（同 root 不同端口） |
| TCP 鉴权（`-32020`、错误令牌断开、正确令牌后握手）、`host.shutdown` 退出 `0` | TCP 实测 |
| 缺少沙箱时 `--backend sandbox` 失败、`auto` 只报 `quickjs`/`dylib`、`sandbox: null` | stdio 冒烟 + 启动实测 |
| 非回环绑定在 stderr 提示「已鉴权但未加密、无 TLS」 | 绑定 `0.0.0.0` 实测 |
| 打包与校验（`bin/ol-host`、`manifest.json`、`SHA256SUMS`、`<archive>.sha256`） | `package_release.py` + `verify_package.py`；`test_package.py` 21 项通过 |
| 包内不含沙箱，清单 `sandbox: null` | 解包检查 + `verify_package.py` |

### 已接入但未在本地执行

| 项 | 原因 |
| --- | --- |
| Windows x64 构建、打包（ZIP）与冒烟 | 本机为 macOS，无 Windows 环境；CI（`windows-2022`）首跑后确认 |
| Linux x64 构建、打包（tar.gz）与冒烟 | 本机无 Docker/act，无法在 Ubuntu 容器内执行 |
| 冒烟脚本在 Windows 上的可执行位/PowerShell 调用路径 | 同上，需 CI 首跑确认 |

### 待办

- 服务契约 M-05：关闭优雅期（5 秒）、超时强杀兜底、stdio/tcp 关闭中的 `-32022`（`stopping`）。
- 服务契约 M-10：绑定通配地址时发现文件写 `127.0.0.1`，远程消费者需自行使用实际接口地址。
- 服务契约 M-08：任务事件推送与 `taskId`/`seq`（A02）。
- 服务契约 M-07：CLI 侧配置迁移到 `OEXT_*` + TOML（C01/C03，属 CLI 仓库）。
- `oext-sandbox` 是否纳入发布包（待定）；`build-info.json` 是否加入签名。
- 正式发布：本仓库本次不建标签、不推送、不发布远端版本。

## 当前差距与不做事项

消费端（CLI/HomeWorld）迁移为协议客户端的后续工作仍在推进；服务名称与新命令格式由契约确定，不在消费者里猜测，迁移步骤见服务契约第 1.2 节。

暂不引入自动服务发现、跨机器调度或自动共享实例。本地验收以 macOS 单平台为准，跨平台结论必须等 CI 实跑。
