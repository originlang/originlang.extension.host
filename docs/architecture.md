# 架构与设计决策

## 定位

宿主是**编排层**：发现包、校验完整性、检查授权、启动实例、路由 Host API、持久化状态、调度定时器、分发事件。它不实现运行时，也不实现网络协议之上的业务语义。

非目标（当前版本明确不做）：

- 不做容器级或系统调用级隔离（Seccomp、AppContainer、受限令牌）。
- 不做扩展市场与远程分发。
- 不做多租户：一份安装目录对应一个用户。
- 不做协议网关：HTTP/WebSocket、流式调用、负载均衡。

> 进程外隔离已实现：默认后端把每个实例放进独立的 `oext-sandbox` 进程。见下文"两级隔离"。

## 依赖图

```
protocol ── permission
    │           │
    ├── manifest┘
    │      │
    │      └── signing
    │
    ├── storage   scheduler   session
    │      │          │          │
    └──── runtime ────┴──────────┘
              │
      ┌───────┴────────┐
      │                │
runtime-quickjs   runtime-process
      │                │
apps/sandbox           │
      └───────┬────────┘
            host
              │
   bootidea.cli（独立仓库）
```

依赖方向严格单向：`host` 只经 `runtime` trait 触达执行后端，不认识任何具体引擎。`apps/sandbox` 的执行体则是这条依赖链的**反向证据**：它只依赖 `protocol`、`runtime`、`runtime-quickjs`，不链接 `host`、`storage`、`permission`——沙箱进程里根本没有授权代码、没有存储实现、没有安装目录，因此它除了向父进程发 JSON-RPC 之外无路可走。

## 两级隔离

| 后端 | crate | 边界 | 用途 |
| --- | --- | --- | --- |
| `sandbox`（默认） | `runtime-process` + `apps/sandbox` | 操作系统进程 + Job Object | 不可信扩展 |
| `quickjs` | `runtime-quickjs` | 同进程工作线程 | 可信包、调试、无子进程环境 |

两者的 `Runtime` 实现完全等价，`host` 无从区分。区别只在"越界的后果"：同进程后端里，引擎的越界写就是宿主的越界写；进程后端里，那是一次 `oext-sandbox` 崩溃，宿主只会看到管道关闭。

## 一次调用的完整链路（沙箱后端）

```
客户端 ──NDJSON──> serve::dispatch
                     │  host.extension.call {id, method, params}
                     ▼
                Host::call ──> 未加载则 load（哈希校验 → 能力授权 → 启动子进程 → register）
                     │
                     ▼
              Slot.runtime(Mutex)  ── 串行化同一扩展的所有调用
                     │
                     ▼
        SandboxRuntime::call ──写出 extension.invoke──> oext-sandbox 的 stdin
                     │                                        │
                     │                              __oextInvoke(...)
                     │                                        │
                     │                       扩展代码调用 Origin.host.storage.get()
                     │                                        │
                     │                    写出 host.storage.get ──> 父进程读线程
                     │                                        │
                     │                    父进程按策略裁决：能力 → 存储 / 日志 / 定时器
                     │                                        │
                     │<──── 应答帧（result 或 error）──────────┘
                     ▼
              响应沿原路返回，Promise 轮询在超时点中断
```

关键点：**授权决策没有跟着代码搬到子进程**。子进程只有"请求"这一种能力，裁决永远发生在父进程；因此进程外隔离不削弱能力模型，只是把不可信代码挪到了更容易被限制的位置。

## 线程模型

| 线程 | 职责 | 同步方式 |
| --- | --- | --- |
| 主线程（stdio） | 读帧、分发、写回 | 单线程顺序处理 |
| 连接线程（TCP） | 每连接一个，鉴权后分发 | `Host` 为 `Arc` 共享 |
| 扩展工作线程（quickjs 后端） | 每实例一个，独占 Runtime/Context | `mpsc` 通道 + `SyncSender` 回执 |
| 管道读线程（sandbox 后端） | 每实例一个，读子进程输出、就地服务 Host API 调用 | 管道 + 待答表（id → 回执通道） |
| 调度线程 | 串行执行到期任务 | `Mutex + Condvar` |

锁顺序：`slots(RwLock)` → `Slot.runtime(Mutex)` → 运行时内部通道。定时器与事件投递使用 `try_lock`，实例繁忙时跳过并记日志，不阻塞调度线程，也就不存在跨线程锁倒置。

**跨线程调用链**是进程后端引入的唯一新问题：`a → b` 之后，`b` 的回调在**另一个**读线程上执行，线程本地栈是空的，`b → a` 就不会被识别为重入，而是阻塞在 `a` 的实例锁上。解决办法是把调用链随调用一起传下去（`CallChain`），由 `host` 在派生线程上安装回线程本地，`ExtensionBridge` 再按扩展 id 归一化。`crates/host/tests/sandbox.rs::a_chain_of_calls_across_processes_cannot_deadlock` 专门验证这条路径。
## 生命周期

```
未安装 ──install──> 已安装(enabled)
                       │  ▲
             disable   │  │ enable
                       ▼  │
                    已禁用
                       │
        load（哈希+授权）│  ▲ unload / 崩溃退役
                       ▼  │
                     已加载 ──call/事件──> 运行中
```

- 加载失败的实例不留残骸：`register` 失败会先 `shutdown` 再返回错误。
- 运行时报告 `is_running=false` 或返回超时，宿主剔除该实例（retire），下次调用重新加载。
- 卸载时先取消该扩展的全部定时器，再清空订阅，最后执行清理钩子。

## 设计决策

| 编号 | 决策 | 理由 | 代价 |
| --- | --- | --- | --- |
| D1 | 运行时抽象为 `RuntimeProvider` | 宿主与引擎解耦，便于引入 WASM 或进程后端 | 多一层 trait 与 `Box<dyn Runtime>` |
| D2 | 能力"声明 + 授予"双门 | 声明是包的意图，授予是人的同意；单靠任一都不足以表达授权 | 管理员需在安装后显式授权 |
| D3 | 同时提供进程内与进程外两个后端，默认进程外 | 进程内后端隔离只到"崩溃不传染"，引擎缺陷等于宿主缺陷；进程外后端把同一份引擎放进子进程，能力检查留在父进程，因此不需要新的协议或新的授权逻辑，只需一条管道 | 每实例一个进程，冷启动与内存成本更高（见 `docs/security.md`） |
| D4 | 沙箱子进程只链接 `protocol`/`runtime`/`runtime-quickjs` | 隔离强度来自"进程里没有可用的越界手段"，而不是运行时检查；依赖树本身就是约束，可被 `cargo tree -p originlang-ext-sandbox` 直接审计 | 沙箱不能自行读包目录，源码由父进程下发 |
| D4 | 事件投递不排队 | 无界队列会被慢消费者拖垮内存；有界队列会静默丢事件且难以观测 | 繁忙实例会跳过事件，仅记日志 |
| D5 | 安装后校验内容哈希 | 磁盘上的包是攻击面；不校验则"安装时审查过的内容"与"实际运行的代码"可能不同 | 原地修改需重新安装 |
| D6 | CLI 迁出到独立仓库 `bootidea.cli` | 入口与库的发布节奏不同：入口要独立发版与分发，而库消费者不该被迫引入 clap、config 与 tiny_http | 宿主 crate 以 git rev 依赖，升级需显式改 rev；彻底不依赖网络则要先发布到 registry |

### CLI 归属

命令入口已迁到独立仓库 `bootidea.cli`。判断标准是**消费者边界**，不是"是否可执行文件"：

- 库消费者不该为一个命令行引入 clap、config、tiny_http；
- 入口需要独立发版与独立分发渠道；
- 宿主协议与入口适配可以各自演进，不必互相绑版本。

集成测试不跟着走：验证库的留在本仓库 `crates/host/tests/`（`host.rs`、`sandbox.rs`、`signing.rs`），验证真实进程与传输的跟着入口走（`bootidea.cli/tests/transports.rs`）。

## 安装与签名校验

```
bootidea ext install <dir>
        │
        ├─ 复制到 .staging-<id>-<pid>-<ts>
        ├─ 重读清单、计算内容哈希（extension.json + 入口脚本）
        ├─ 读 extension.sig ──有──> 验签(发布者, id, 版本, 哈希)
        │                        ├─ 失败 ──> 拒绝（即使签名非必需）
        │                        └─ 成功 ──> 记 publisher，查信任库得 trusted
        ├─ 无 ──> require_signature ? 拒绝 : 记"未签名"
        └─ 通过 ──> rename 到 extensions/<id>，写 registry.json
```

校验发生在**暂存副本**上，用的是暂存副本自己的哈希：先验源目录再复制，中间有一个"验的和拷的不是同一份"的窗口，这里刻意不留。被拒绝的安装不会触碰已有版本。

## 扩展点

| 扩展点 | 做法 |
| --- | --- |
| 新执行后端 | 实现 `Runtime` + `RuntimeProvider`；`bootidea --backend` 增加一个取值即可 |
| 新 Host API | 在 `protocol::method` 增常量、`api::required_capability` 增映射、`dispatch_host_call` 增分支；三处改动即可，缺任一处会被测试捕获 |
| 新传输 | 复用 `protocol::read_message/write_line`，实现自己的帧循环，调用 `serve::dispatch` 同级逻辑 |
| 新存储后端 | 实现 `storage::Storage`，`ScopedStorage` 自动获得命名空间隔离 |
