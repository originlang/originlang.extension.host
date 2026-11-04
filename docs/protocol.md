# 协议、清单与 Host API

## 一、协议分层

| 层 | 内容 | 位置 |
| --- | --- | --- |
| 传输层 | stdio/TCP 使用 NDJSON；HTTP 使用 POST 请求体 | `bootidea.cli` 的 `src/serve` |
| 信封层 | JSON-RPC 2.0：request / notification / response | `crates/protocol` |
| 方法层 | 三个命名空间：`extension.*`、`host.*`（管理）、`host.*`（能力面） | `crates/protocol` |
| 语义层 | 能力标识、错误码、版本 | `crates/permission`、`crates/protocol` |
| 清单层 | `extension.json`，`apiVersion = "1"` | `crates/manifest` |

约束：

- stdio/TCP 单行上限 `MAX_MESSAGE_BYTES = 8 MiB`。上限作用在**读取过程**（`take(8 MiB + 1)`），不是读完再判断，否则对端只要永不发换行就能逼宿主分配任意内存。
- stdio/TCP 空行忽略；任何协议错误都写回一个 `id: null` 的错误响应，然后关闭连接（流已不可信，不猜下一条消息从哪开始）。
- 版本字段彼此独立：协议 `2.0`、清单 `apiVersion 1`、策略/注册表文档 `version 1`。

## 二、信封

```json
{"jsonrpc":"2.0","id":1,"method":"host.extension.call","params":{"id":"hello","method":"greet","params":{"name":"Origin"}}}
{"jsonrpc":"2.0","id":1,"result":{"message":"Hello, Origin"}}
{"jsonrpc":"2.0","id":2,"error":{"code":-32021,"message":"extension 'hello' lacks capability 'log.write'"}}
```

无 `id` 即为通知，不写回响应；`"id": null` 与缺省同等对待（JSON-RPC 本身不推荐该写法）。

### 2.1 入站校验（顺序即优先级）

| 条件 | 结果 |
| --- | --- |
| 不是合法 UTF-8 / 不是合法 JSON | `-32700 PARSE_ERROR` |
| `jsonrpc` 缺失或不为 `"2.0"` | `-32600`，报文里点明实际版本 |
| 顶层是数组（批量请求） | `-32600`，明确说明不支持批量，而不是伪装成解析失败 |
| 同时有 `method` 与 `result`/`error` | `-32600` |
| 无 `method` 且无 `id` | `-32600` |
| 响应同时带 `result` 与 `error` | `-32600` |

两类失败刻意分开：`-32700` 表示"你的序列化器坏了"，`-32600` 表示"JSON 没问题但信封不合规"。混成一个码，调用方会去查错层。错误信息由手工分类产生——`#[serde(untagged)]` 只会回"没有匹配任何变体"，说不清到底缺了哪个字段。

## 三、方法表

方法名常量集中在 `protocol::method`，并按调用面分成三张表（`CLIENT` / `HOST_API` / `RUNTIME`）。表是可遍历的数据，因此有测试保证：两两不相交、总数与常量数一致、`CLIENT` 中每个方法都能落到真实处理器、`RUNTIME` 中每个方法从客户端调用必须是 `METHOD_NOT_FOUND`。加常量却忘记接线会直接测试失败，而不是等客户端踩到。

### 3.1 `extension.*`：宿主 → 实例（运行时边界，不由客户端直接调用）

| 方法 | 说明 |
| --- | --- |
| `extension.load` | **仅沙箱链路**：启动帧，`{source, limits{memoryBytes, stackBytes, timeoutMs}}`；应答到达即表示入口脚本求值完成 |
| `extension.invoke` | **仅沙箱链路**：`{method, params, chain?}`，调用扩展的一个方法 |
| `extension.register` | 上报 id/version/能力/方法列表；加载成功的标志 |
| `extension.ping` | 存活探测 |
| `extension.activate` | 首次激活（懒执行，`bootstrap.js` 内部完成） |
| `extension.event` | 投递事件，参数 `{topic, payload, from?}` |
| `extension.deactivate` / `extension.shutdown` | 清理钩子；shutdown 后实例退役 |

#### 3.1.1 沙箱链路

宿主 ↔ `oext-sandbox` 复用同一套 JSON-RPC 信封，因此全项目只有一种线格式。方向与角色恰好相反：

```
父进程（宿主）    ── extension.load / invoke / event / shutdown ─▶   子进程（沙箱）
父进程（宿主）    ◀── host.*（Host API 调用）───────────────────────   子进程（沙箱）
```

- 子进程只发 `host.*`，**不发** `extension.*`；父进程只发 `extension.*`，不主动发 `host.*`。
- 子进程不认识 `Policy`、`Storage`、安装目录——这些 crate 不在它的依赖树里，所以"自己去授权"在编译期就不成立。
- 调用期间若收到别的帧（父进程理论上不会发），子进程以 `REJECTED` 直接拒绝，不重入引擎。
- 子进程在超时后自行退出，父进程据此把实例判为不可用并退役；父进程另有 `timeout + 2s` 的兜底，到时直接杀进程。

### 3.2 `host.*`：管理接口（客户端 → 宿主）

| 方法 | 参数 | 结果 |
| --- | --- | --- |
| `host.info` | – | 版本、后端、根目录、扩展计数 |
| `host.extension.list` | – | `{extensions:[...]}` |
| `host.extension.load` | `{id}` | 扩展摘要 |
| `host.extension.unload` | `{id}` | `{unloaded}` |
| `host.extension.reload` | `{id}` | 扩展摘要 |
| `host.extension.call` | `{id, method, params?}` | 方法返回值（按需自动加载） |
| `host.extension.logs` | `{id?, limit?}` | `{logs:[...]}`，limit 1..1000 |
| `host.event.emit` | `{topic, payload?}` | `{delivered:n}` |
| `host.authenticate` | `{token}` | 仅 TCP，必须是首个请求 |
| `host.shutdown` | – | `{stopping:true}`，随后优雅退出 |

### 3.3 `host.*`：能力面（扩展 → 宿主，即 Host API）

| 方法 | 参数 | 所需能力 |
| --- | --- | --- |
| `host.log` | `{message, level?}` | `log.write` |
| `host.storage.get` | `{key}` | `storage.read` |
| `host.storage.list` | `{prefix?}` | `storage.read` |
| `host.storage.set` | `{key, value}`（≤1 MiB） | `storage.write` |
| `host.storage.delete` | `{key}` | `storage.write` |
| `host.timer.set` | `{delayMs, repeat?}` | `timer.schedule` |
| `host.timer.clear` | `{id}` | `timer.schedule` |
| `host.events.emit` | `{topic, payload?}` | `events.emit` |
| `host.events.subscribe` | `{topic}` | `events.subscribe` |
| `host.extension.call` | `{id, method, params?}` | `extension.call` |
| `host.self.info` | – | 无 |

键与值均为 UTF-8 字符串；`get` 对缺失键返回 `null` 而不是错误。

## 四、错误码

| 码 | 含义 |
| --- | --- |
| `-32700` / `-32600` / `-32601` / `-32602` / `-32603` | JSON-RPC 标准：解析、非法请求、方法不存在、参数非法、内部错误 |
| `-32001` | 扩展未安装 |
| `-32002` | 扩展未加载 |
| `-32003` | 超时 |
| `-32004` | 清单/运行时/激活失败 |
| `-32005` | 通用执行失败 |
| `-32012` | 已加载 |
| `-32014` | 已被禁用 |
| `-32020` | 令牌无效 |
| `-32021` | 能力未声明或未授予 |
| `-32022` | 策略拒绝（重入、繁忙、关闭中） |

运行时的自有错误码原样透传，宿主不改写。

## 五、传输

### stdio

- 无鉴权：进程边界即信任边界（父进程创建）。
- **stdout 只承载协议帧**；所有人类可读输出走 stderr。扩展无法写 stdout（QuickJS 无 `print`）。

### TCP

- 默认 `127.0.0.1:0`；非回环绑定同样必须鉴权（无 `--no-auth` 开关）。
- 首个请求必须是 `host.authenticate`，令牌为 256 位十六进制，`getrandom` 生成，常量时间比较；失败立即断开。
- 启动时写 `host.json`（发现文件，含 pid/host/port/token）并持有 `host.lock` 单例锁；退出时清理。
- 连接空闲 300 秒断开；每连接一个线程；同一扩展的调用在实例互斥锁上串行。

### HTTP

- `bootidea serve --transport http --host 127.0.0.1 --port 7311`，入口 `POST /rpc`。
- 每次请求必须发送 `Authorization: Bearer <token>`，复用会话令牌与单例锁；不使用 `host.authenticate` 握手。
- `Content-Type: application/json`（可带 charset）；请求体是一个完整 JSON-RPC 对象，可格式化换行，不是 NDJSON。
- 请求体上限 8 MiB，检查 Content-Length，并对实际读取施加上限（包括 chunked 请求）。
- 调用返回 HTTP 200，包含 JSON-RPC `result` 或 `error`；合法通知执行后返回 HTTP 204，无响应体。
- JSON 解析/信封错误返回 HTTP 400 和 `id: null` 的 JSON-RPC 错误；不支持批量或将响应报文作为请求提交。
- HTTP 状态：401 鉴权失败、404 路径错误、405 非 POST、413 请求体过大、415 媒体类型错误、503 宿主关闭中。
- 发现文件增加 `transport: "http" | "tcp"`；旧文件未提供此字段时按 TCP 解析。HTTP 路径固定为 `/rpc`。
- 当前提供明文 HTTP，不提供内建 TLS、CORS 或 SSE。stdio 保持默认，另接受 `--transport std` 别名。


## 六、清单

```json
{
  "id": "hello",
  "name": "Hello",
  "version": "0.1.0",
  "apiVersion": "1",
  "main": "main.js",
  "description": "...",
  "capabilities": ["log.write"],
  "activationEvents": ["onStartup", "onEvent:greeting"],
  "limits": {"memoryBytes": 67108864, "stackBytes": 524288, "timeoutMs": 10000},
  "contributes": {}
}
```

校验规则（任一不满足即拒绝安装/加载）：

| 字段 | 规则 |
| --- | --- |
| `id` | `[a-z0-9.-]`，1..=64，不以 `.` 开头，无 `..` |
| `version` | `major.minor.patch`，无前导零，可带 `-pre`/`+build` |
| `apiVersion` | 必须为 `"1"` |
| `main` | 相对路径，无 `.`/`..` 组件，`.js`/`.mjs`，必须落在包目录内 |
| `capabilities` | 点分段 ASCII，禁止通配符，禁止重复 |
| `activationEvents` | `onStartup` 或 `onEvent:<topic>` |
| `limits` | 越界不报错，按宿主区间夹取（内存 1–512 MiB，栈 64 KiB–8 MiB，超时 0.1–600 s） |
| 未知字段 | 拒绝（`deny_unknown_fields`），避免拼写错误静默失效 |

`contributes` 为自由结构，宿主不解释，原样透传给客户端。

## 七、未实现

无 WebSocket、无双向流式、无额外二进制分帧、无压缩、无协议版本协商握手（`apiVersion` 在清单层校验）、无远程注册表。包签名已实现。

批量请求按 JSON-RPC 2.0 本应支持，本平台**显式不支持**：顶层数组返回 `-32600` 并说明原因，不做"部分处理"。理由是收益为零——单连接是顺序处理的，批量省不掉往返；而代价是每条回复都要多一层数组包装，错误语义也要重新定义（批内某个失败算整体失败还是部分成功）。请求取消（`$/cancelRequest`）同理：当前一次调用占住连接直到返回，后续请求排队，超时由宿主侧统一下发，客户端无法主动打断。
