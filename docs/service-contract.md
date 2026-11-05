# Host 服务契约

契约版本：`1`（草案）。编写日期：2026-11-05。状态：待验证。对应工作包 H01。

本文定义独立 Host 服务对外**可观察的行为**：入口、参数、配置优先级、目录、发现与就绪、握手与版本、帧规则、鉴权、错误关联、关闭与退出、实例归属。消费者（CLI、HomeWorld、Desktop）应按本文实现，不应从当前实现代码反推约定。

约定：

- 契约要求与当前实现的差距集中列在[第 12 节](#12-实现缺口清单)，正文不因尚未实现而放宽要求。
- 「必须 / 不得」为强制项；「建议」为不参与验收的实现建议。
- 版本号只描述本文规定的行为集合。新增可选字段、可选错误 `data` 字段属于次版本；删除字段、改变字段语义、改变错误码属于主版本。

## 1. 服务入口

| 项目 | 规定 |
| --- | --- |
| 可执行文件 | `oext-host`（Windows 为 `oext-host.exe`） |
| 子命令 | 无。行为完全由启动参数决定 |
| 同包执行体 | `oext-sandbox`，由 Host 按第 3.4 节发现，不单独作为服务入口 |
| 版本输出 | `--version` 打印 `<hostVersion>` 一行到 stdout，退出码 `0` |
| 帮助 | `--help` 打印用法到 stdout，退出码 `0` |
| 未知参数 | 打印用法到 stderr，退出码 `2` |

消费者不得硬编码其他二进制名称，也不得把 `oext-sandbox` 当作 Host 服务启动。

### 1.1 参数

| 参数 | 取值 | 默认 | 说明 |
| --- | --- | --- | --- |
| `--transport` | `stdio` \| `tcp` \| `http` | `stdio` | 别名 `std` 等价于 `stdio` |
| `--host` | IP 或主机名 | `127.0.0.1` | 仅 `tcp`/`http` |
| `--port` | `0..=65535` | `0` | `0` 表示由系统分配，实际端口写入发现文件；仅 `tcp`/`http` |
| `--home` | 路径 | 见 3.1 | 安装根目录 |
| `--config` | 路径 | 见 2.2 | 配置文件 |
| `--backend` | `auto` \| `sandbox` \| `quickjs` \| `dylib` | `auto` | 扩展执行后端 |
| `--require-signature` | 标志 | 关 | 拒绝未签名或签名不可信的包 |
| `--log-level` | `error` \| `warn` \| `info` \| `debug` \| `trace` | `info` | 诊断输出级别 |
| `--no-startup` | 标志 | 关 | 不加载声明 `onStartup` 的扩展 |

强制规则：

- `--port` 与 `--host` 在 `stdio` 下被忽略；显式指定不报错，避免同一启动脚本跨传输复用。
- 网络传输**没有**关闭鉴权的开关。绑定非回环地址时鉴权照常生效，且发现文件写入实际地址。
- 传输方式不接受配置文件设置，只能由参数决定：监听行为必须是每次启动的显式意图。

## 2. 配置

### 2.1 优先级

高 → 低：命令行参数 > 环境变量 > 配置文件 > 内置默认值。冲突时不报错，按优先级取胜；需要诊断时以 `--log-level debug` 输出最终生效值。

### 2.2 配置文件

| 项目 | 规定 |
| --- | --- |
| 显式路径 | `--config`,其次 `OEXT_CONFIG`；文件不存在即退出码 `2` |
| 默认路径 | `$XDG_CONFIG_HOME/oext/config.toml`，否则 `~/.config/oext/config.toml`；不存在不算错误 |
| 格式 | 仅 TOML |
| 允许键 | `home`、`backend`、`require_signature`、`log_level` |
| 未知键 | 退出码 `2`，指明键名（拼写错误必须失败，不能静默失效） |

不支持 YAML/JSON 是有意收敛：宿主只需要一层扁平键值，减少解析面与格式歧义。

### 2.3 环境变量

| 变量 | 映射 | 备注 |
| --- | --- | --- |
| `OEXT_HOME` | `home` | 安装根目录 |
| `OEXT_CONFIG` | 配置文件路径 | 低于 `--config` |
| `OEXT_STATE_DIR` | 发现文件与锁目录 | 测试与便携场景；见 3.2 |
| `OEXT_SANDBOX_BIN` | 沙箱执行体路径 | 必须存在，否则启动失败 |
| `OEXT_BACKEND` | `backend` | |
| `OEXT_REQUIRE_SIGNATURE` | `require_signature` | `1`/`true` 为真 |
| `OEXT_LOG_LEVEL` | `log_level` | |

Host 不读取 `BOOTIDEA_*`。CLI 在迁移期可继续接受 `BOOTIDEA_HOME` 等旧名称并映射到上述变量，映射发生在消费者内部，不属于本契约。

## 3. 目录与实例

### 3.1 根目录

`--home` > `OEXT_HOME` > 平台默认：

| 平台 | 默认根目录 |
| --- | --- |
| Windows | `%APPDATA%\originlang-ext`（缺 `APPDATA` 时用 `%LOCALAPPDATA%`） |
| macOS | `~/Library/Application Support/originlang-ext` |
| Linux/其他 Unix | `$XDG_DATA_HOME/originlang-ext`，否则 `~/.local/share/originlang-ext` |

### 3.2 布局

| 路径 | 内容 | 是否可删除 |
| --- | --- | --- |
| `<root>/extensions/` | 已安装扩展包 | 可删，等同卸载 |
| `<root>/data/storage/` | 扩展键值存储 | 删除即清空扩展数据 |
| `<root>/state/` | `policy.json`、`registry.json`、`publishers.json` | 删除即丢失授权与注册信息 |
| `<root>/logs/host.log` | 滚动日志 | 可删 |
| `<root>/run/host.json` | 发现文件 | 仅进程运行期间存在 |
| `<root>/run/host.lock` | 单例锁 | 仅进程运行期间存在 |

`OEXT_STATE_DIR` 只覆盖 `run/` 的位置，不覆盖其他目录。

### 3.3 单实例与实例归属

- 单例范围是**一个根目录**，不是整台机器。不同根目录的实例并行运行，互不发现、互不停止。
- 同一根目录启动第二个实例时，若锁持有者仍存活：stderr 说明持有者 pid，退出码 `3`。
- 锁持有者已退出时，新实例可接管，并先清理残留的 `host.json`、`host.lock`。
- 发现文件写入是原子的（临时文件 + 重命名），读取方不会看到半份文档。
- Unix 上 `run/` 权限 `0700`、`host.json` 权限 `0600`；Windows 依赖用户目录 ACL。

### 3.4 沙箱发现

按顺序：`OEXT_SANDBOX_BIN` 指向的文件 > 与 Host 可执行文件同目录的 `oext-sandbox`（Windows `oext-sandbox.exe`）。**不从 `PATH` 查找**：隔离能力不能由环境路径顺序决定。找不到时 `--backend sandbox` 启动失败；`--backend auto` 跳过沙箱后端，其余后端可用，并在 `host.hello` 的 `sandbox` 字段报告 `null`。

## 4. 就绪、握手与版本

### 4.1 就绪判定

| 传输 | 就绪信号 | 客户端行为 |
| --- | --- | --- |
| `stdio` | 无外部信号；stderr 启动横幅仅为人类可读提示 | 发送 `host.hello`，收到成功响应即为就绪 |
| `tcp`/`http` | 发现文件已写入且端口可连接 | 读取发现文件 → 建连 → 鉴权 → `host.hello` |

启动横幅必须写 stderr，且不得作为协议判据。stdio 的 stdout 从第一个字节起只承载协议帧。

### 4.2 握手

任何连接上的首个业务调用必须是 `host.hello`；`tcp` 上首个帧必须是 `host.authenticate`，随后是 `host.hello`。

```json
{"jsonrpc":"2.0","id":1,"method":"host.hello",
 "params":{"protocolVersion":"1","client":{"name":"bootidea-cli","version":"0.1.0"}}}
```

成功响应：

```json
{"jsonrpc":"2.0","id":1,"result":{
  "hostVersion":"0.1.0",
  "protocolVersion":"1",
  "apiVersion":"1",
  "abiVersion":1,
  "runtime":"sandbox|quickjs|dylib|router",
  "backends":["sandbox","quickjs","dylib"],
  "sandbox":{"version":"0.1.0","path":"..."},
  "root":"/path/to/root",
  "startedAt":1762300000
}}
```

强制规则：

- `protocolVersion`、`apiVersion` 为字符串，`abiVersion` 为整数，`sandbox` 在沙箱不可用时为 `null`。
- 缺失 `params.protocolVersion` 视为 `0`，按不兼容处理。
- 兼容判定只看 `protocolVersion` 的主版本：客户端与宿主主版本相同即兼容。
- `host.info` 保留为诊断方法，返回与 `host.hello` 相同的对象；它**不**承担协商，客户端不得用它替代握手。
- 未握手的连接上调用其他方法，返回 `-32022`，`data.reason = "handshake_required"`。
- `stdio` 与 `tcp` 在版本不兼容后关闭连接；`http` 返回 HTTP 400。

### 4.3 版本拒绝

主版本不匹配时，`host.hello` 返回 `-32023`：

```json
{"jsonrpc":"2.0","id":1,"error":{"code":-32023,
 "message":"incompatible host protocol: host requires 1, client sent 2",
 "data":{"reason":"incompatible_protocol","required":"1","received":"2"}}}
```

客户端必须把该错误呈现为「需要升级/降级」而不是网络故障。宿主不得在拒绝后继续接受该连接上的任何方法。

### 4.4 插件 ABI 协商

`dylib` 插件在加载前由宿主比对 `abiVersion`；不匹配时拒绝加载，返回 `-32004`，`data` 含 `{reason:"incompatible_abi", required, received}`。扩展进程内的能力授权不能弥补 ABI 不兼容。

## 5. 帧与请求

| 传输 | 帧规则 |
| --- | --- |
| `stdio`/`tcp` | NDJSON：一行一个 JSON-RPC 信封，UTF-8；空行与首尾空白忽略 |
| `http` | 一个请求体一个信封；`Content-Type: application/json`（可带 `charset`）；允许格式化换行 |

- 单帧上限 8 MiB，限制施加在读取过程而非结果；超限即 `-32600` 并关闭 `stdio`/`tcp` 连接，HTTP 返回 `413`。
- 顶层数组（批量请求）明确不支持：`-32600`，说明原因，不做部分处理。
- 非法 UTF-8 或非法 JSON：`-32700`，写回 `id: null` 的错误响应后关闭连接——错误帧之后流已不可信，不猜测下一条消息的起点。
- 通知（无 `id`）：执行但不回响应；`http` 返回 `204`。
- 客户端发送响应帧（无 `method` 而有 `result`/`error`）：忽略，不报错。
- 并发模型：`stdio` 顺序处理，响应顺序与请求顺序一致；`tcp` 每连接一线程，同一扩展实例的调用在实例锁上串行；`http` 每请求一线程，可并发。

### 5.1 超时

| 场景 | 规定 |
| --- | --- |
| 宿主级 RPC 超时 | 无。宿主不主动中断已接受的调用 |
| 扩展调用 | 受清单 `limits.timeoutMs` 约束；沙箱另加 2 秒收尸预算，超时后实例退役 |
| `tcp` 连接空闲 | 300 秒读超时后关闭该连接 |
| 探活 | 客户端连接与首个响应各自不超过 1 秒 |
| 客户端 | 必须自设截止时间；管理类方法建议 30 秒，调用扩展按清单超时另加 5 秒 |

### 5.2 取消

首版不实现 `$/cancelRequest`。单次管理调用不可中途取消；任务级取消由 A01 定义（取消请求针对任务 id，不是 RPC id）。客户端不得以关闭连接代替取消并宣称副作用已撤销。

## 6. 鉴权

| 项目 | 规定 |
| --- | --- |
| 令牌 | 32 字节 CSPRNG 生成，十六进制 64 字符 |
| 传递 | `tcp` 首帧 `host.authenticate {"token":"..."}`；`http` 每请求 `Authorization: Bearer <token>` |
| 比较 | 常量时间；长度不同直接失败 |
| 失败 | `tcp` 返回 `-32020` 后立即断开；`http` 返回 `401` 加 `WWW-Authenticate: Bearer` |
| 输出 | 令牌在启动时写 stderr 一次（配对通道），并写入 `0600` 的发现文件；不得出现在 stdout、日志文件与错误消息中 |
| 非回环绑定 | 允许，但必须鉴权；文档需提示风险；无内建 TLS |

`host.authenticate` 在 `stdio` 与 `http` 上是未知方法（`-32601`）；`stdio` 无鉴权，进程边界即信任边界。

## 7. 错误与关联

错误对象固定为 `{code, message, data?}`。

| 码 | 含义 | 关联要求 |
| --- | --- | --- |
| `-32700` | 解析失败 | `id: null`，随后关闭连接 |
| `-32600` | 信封非法（含批量、超限） | 回显 `id`（可解析时） |
| `-32601` | 方法不存在 | 回显 `id` |
| `-32602` | 参数非法 | 回显 `id`，`data.reason = "invalid_params"` |
| `-32603` | 内部错误 | 回显 `id` |
| `-32001` | 扩展未安装 | `data.extensionId` |
| `-32002` | 扩展未加载 | `data.extensionId` |
| `-32003` | 调用超时 | `data.extensionId`、`data.method` |
| `-32004` | 清单/运行时/激活失败、ABI 不兼容 | `data.reason` |
| `-32005` | 通用执行失败 | 原样透传执行体错误 |
| `-32012` | 已存在 | `data.extensionId` |
| `-32014` | 已禁用 | `data.extensionId` |
| `-32020` | 令牌无效 | 不泄露期望值 |
| `-32021` | 能力未声明或未授予 | `data.capability` |
| `-32022` | 策略拒绝（未握手、关闭中、重入） | `data.reason` |
| `-32023` | 协议主版本不兼容 | `data.required`、`data.received` |

强制规则：

- 每个响应必须回显请求 `id`；不得复用 `id` 表示不同请求。
- 新增方法必须提供稳定的 `data.reason`；`message` 是给人看的，不得作为客户端分支依据。
- 错误不得包含令牌、绝对路径之外的敏感环境信息以外的密钥材料。

## 8. 事件（预留，随 A01/A02 启用）

任务事件尚未实现。为免客户端后续改写，信封与关联字段在此固定：

```json
{"jsonrpc":"2.0","method":"host.event","params":{
  "taskId":"...","seq":7,"type":"tool.call","at":1762300000,"payload":{}}}
```

- `seq` 在每个 `taskId` 内从 1 单调递增，不复用；终态事件之后不再发送。
- 事件是通知，不要求响应；不重放。断线重连后客户端以任务查询补齐（方法由 A01 定义）。
- 仅 `stdio`/`tcp` 推送。`http` 无服务端推送，客户端按 A01 的查询方法轮询。
- 未实现前，任何消费者不得对外宣称可观察任务事件。

## 9. 关闭与退出

### 9.1 触发

`host.shutdown` 请求、`stdio` 的 stdin EOF、`SIGINT`/`SIGTERM`（Windows 控制台中断）、致命协议错误、初始化失败。

### 9.2 顺序

1. 停止接受新请求：`http` 返回 `503`，`stdio`/`tcp` 返回 `-32022`（`data.reason = "stopping"`）。
2. 停止定时任务调度。
3. 各扩展实例 `deactivate`/`shutdown`。
4. 终止残留沙箱子进程（Job Object / 进程组）。
5. 删除本实例的 `host.json`，只释放自己持有的锁。
6. flush stdout 后退出。

优雅期 5 秒；超时则强杀子进程并以 `1` 退出。

### 9.3 退出码

| 码 | 含义 |
| --- | --- |
| `0` | 正常关闭（请求、EOF、信号） |
| `1` | 未分类失败，或优雅期超时后强杀 |
| `2` | 参数或配置错误（含未知键、`--config` 不存在） |
| `3` | 同根目录实例冲突，或监听地址绑定失败 |
| `4` | `stdio` 上的致命协议错误（帧不可恢复） |

`tcp`/`http` 的协议错误只关闭连接，进程继续服务，不产生退出码 `4`。

## 10. 客户端义务

- 按根目录计算发现文件，不扫描用户主目录猜测实例。
- 记录启动时得到的 `pid`、`startedAt` 与令牌，作为「自己启动的实例」的凭据；pid 会被操作系统复用，不能单独作为身份。
- 只对自己启动的实例发送 `host.shutdown`。停止外部实例必须是用户显式操作。
- 连接失败、握手失败、版本不兼容分别呈现为「未启动/不兼容/需要升级」，不得统一显示为「Host 未响应」。
- 基础功能不得因 Host 缺失而不可用。

## 11. 消费者适配要点

| 消费者 | 需要改动 |
| --- | --- |
| CLI | 移除宿主装配与 `serve` 服务端实现，改为下载/启动/连接 `oext-host`；`serve` 命令迁移或删除（C01/C03） |
| HomeWorld | 下载源与启动参数改为 `oext-host`；发现文件按根目录计算；不再下载 CLI（F01） |
| Desktop | 按状态机区分未安装、未连接、连接中、已连接、失败（DT2/DT3） |
| 发布清单 | 入口固定为 `oext-host`，由 H02 定义包内布局与校验 |

## 12. 实现缺口清单

以下为按本文核对当前实现（`bootidea serve` 与 `crates/*`）得到的差距，均需在 M1/M2 内关闭。

| 编号 | 契约要求 | 当前状态 | 承接 |
| --- | --- | --- | --- |
| M-01 | 独立入口 `oext-host` 承载服务 | 服务端仍在 CLI 的 `serve` 模块 | H03 |
| M-02 | 单例与发现文件按根目录隔离（`<root>/run/`） | 全局 `~/.originlang-ext`，不同 `--home` 互相抢锁 | H03 |
| M-03 | `host.hello` 握手与 `protocolVersion`/`abiVersion`/`sandbox` 字段 | 无协商；`host.info` 缺这些字段；文档明确「无版本协商」 | H03 |
| M-04 | 版本不兼容返回 `-32023` 并拒绝连接 | 无此错误码与判定 | H03 |
| M-05 | 关闭中返回 `-32022` 且顺序固定、5 秒优雅期 | 有停止标志；无截止时间，顺序未固定 | H03 |
| M-06 | 退出码区分配置/实例冲突/协议致命 | 全部为 `1`；stdio 帧错误后仍 `0` | H03 |
| M-07 | 配置统一为 `OEXT_*` 与 TOML 单格式 | CLI 使用 `BOOTIDEA_*`，接受 TOML/YAML | C01/C03 |
| M-08 | 任务事件通知与 `taskId`/`seq` 关联 | 无推送通道，仅有客户端→宿主的 `host.event.emit` | A02 |
| M-09 | 非回环绑定风险提示与文档一致 | 已强制鉴权，未在启动输出中提示风险 | H03 |

## 13. 验收映射

| H01 验收项 | 本文位置 | 判定方式 |
| --- | --- | --- |
| 明确版本拒绝 | 4.2、4.3、4.4 | 用不兼容 `protocolVersion` 握手，收到 `-32023` 且连接关闭 |
| 明确 EOF 行为 | 9.1、9.2 | 关闭 stdio 的 stdin，进程清理子进程、删除发现文件、退出 `0` |
| 明确超时行为 | 5.1 | 扩展超时返回 `-32003`；空闲 `tcp` 连接 300 秒后被关闭 |
| 明确退出语义 | 9.3 | 分别构造配置错误、实例冲突、绑定失败、帧错误，核对退出码 |
| 明确网络鉴权行为 | 6 | 无令牌/错误令牌被拒绝且不泄露令牌；`0600` 发现文件；令牌不进日志 |
