# 安全模型与边界

本文说明**实际生效**的控制，以及明确不承诺的部分。结论先行：当前模型能约束"扩展通过 Host API 做什么"，并在默认后端下把"扩展代码本身能对宿主做什么"压缩到一次进程边界之内；它仍不是容器，也不防范同用户的本机进程。

## 信任边界

| 主体 | 信任级别 | 依据 |
| --- | --- | --- |
| 宿主进程 | 可信 | 自己 |
| 扩展作者 | **不可信** | 代码来自外部，可能被篡改 |
| 扩展代码（QuickJS） | 不可信，受引擎与进程双重限制 | 无文件/网络/进程绑定；默认跑在独立进程中 |
| `oext-sandbox` 子进程 | **半可信**：可崩溃、可被攻陷，但不能自行授权 | 依赖树不含 `permission`/`storage`/`host` |
| 本机其他进程 | 不可信 | 可访问回环端口 |
| 客户端（stdio 父进程 / TCP 客户端） | stdio 视为可信；TCP 需令牌 | 见 `serve.rs` |

## 已实施的控制

| 控制 | 机制 | 位置 |
| --- | --- | --- |
| 能力声明与授予双门 | 清单声明 + 策略授予，加载与每次调用都检查 | `host::load`、`api::dispatch_host_call` |
| 拒绝默认 | 空策略不授予任何能力 | `permission::Policy` |
| 无通配符 | 能力为精确点分段字符串，`fs.*` 被拒 | `permission::validate_capability` |
| 身份不可伪造 | 调用身份由宿主注入，参数中不接受扩展 id | `ExtensionBridge` |
| 命名空间隔离 | 存储键前缀由宿主固定为 `<id>/` | `storage::ScopedStorage` |
| 完整性校验 | 安装时记录 SHA-256，每次加载前重算比对 | `manifest::content_hash`、`host::load` |
| 包签名 | Ed25519 覆盖 `发布者 + id + 版本 + 内容哈希`；安装时在暂存副本上校验，签名不符必拒 | `signing::verify_package`、`registry::check_provenance` |
| 发布者信任 | `state/publishers.json` 记录受信公钥；`--require-signature` 时未签名或未受信一律拒绝 | `signing::TrustStore`、`host::trust_publisher` |
| 公钥有效性 | 入库前解析校验，避免"信任了一个用不了的密钥"直到安装时才暴露 | `host::trust_publisher` |
| 路径逃逸防护 | `main` 禁止 `.`/`..`/绝对路径，规范化后必须在包目录内 | `manifest::validate_main`、`entry_path` |
| 安装包限制 | 禁止符号链接与非普通文件；≤1024 文件、≤64 MiB | `registry::copy_dir` |
| 资源上限 | 堆 1–512 MiB、栈 64 KiB–8 MiB、单次调用 0.1–600 s | `manifest::Limits`、QuickJS `set_memory_limit` / `set_max_stack_size` / 中断处理器 |
| 超时即退役 | 超时被视为引擎状态不可信，实例退出，后续调用重新加载 | `runtime-quickjs::worker`、`apps/sandbox` |
| **进程外执行**（默认） | 每实例一个 `oext-sandbox` 进程，只能通过管道发 Host API 请求 | `runtime-process`、`apps/sandbox` |
| **授权留在父进程** | 子进程没有 `Policy`/`Storage` 的任何副本，无法自我授权，也无法伪造身份 | `ExtensionBridge`、`dispatch_host_call` |
| **越界收尸** | Windows Job Object `KILL_ON_JOB_CLOSE`；Linux `PR_SET_PDEATHSIG`。宿主被强杀也不会留下孤儿沙箱 | `runtime-process::job` |
| **进程级内存兜底** | Job Object `PROCESS_MEMORY` 上限 = 引擎堆上限 × 2 + 128 MiB | `runtime-process::job` |
| **崩溃隔离** | 子进程原生崩溃只关闭一条管道，读线程失败待答请求，实例退役，宿主继续服务 | `runtime-process::Reader` |
| 重入防护 | 调用链随调用跨线程/跨进程传递，`a → b → a` 被拒绝而不是死锁 | `host::ChainGuard`、`CallChain` |
| 强杀残留清理 | 启动与发现时清理死进程留下的 `host.json` / `host.lock`（仅当锁持有者 pid 已死） | `session::prune_stale` |
| 事件回环防护 | 发布者不接收自己发出的事件 | `host::emit` |
| 日志滥用上限 | 单条 4096 字符，环形缓冲 2000 条 | `logs::LogBuffer` |
| TCP 鉴权 | 256 位 CSPRNG 令牌，首请求校验，常量时间比较 | `session::generate_token`、`serve::handle_connection` |
| 单例与发现 | `host.lock` 独占；死进程残留锁可清理 | `session::acquire_lock` |
| 凭据权限 | 发现文件在 Unix 下 0600 | `session::restrict_permissions` |

加载门与调用门的区别值得强调：能力未全部授予时，扩展**根本不会被加载**；运行期撤销能力则立即影响后续调用，不必卸载。

## 不提供的能力（有意为之）

- **文本与网络**：Host API 没有文件读写与网络方法。`fs.read`、`network.*` 之类字符串既不合法也不存在实现，扩展无法通过合法途径触达。
- **进程与模块加载**：QuickJS 内 `process`、`require` 为 `undefined`（有测试断言）。宿主不提供 `eval` 之外的代码加载入口，`host.extension.load` 只接受已安装的 id，不接受路径。
- **任意入口**：`extension.json` 的 `main` 只能是包内相对路径；不存在"运行时指定脚本"的远程入口。

## 明确不承诺的部分

1. **进程边界 ≠ 系统调用边界。** 沙箱进程仍以宿主同一用户身份运行，能读该用户可读的一切（包括扩展自己的包文件与该用户的 `data/storage/storage.bin`）。要真正收紧，需要受限令牌 / AppContainer（Windows）或 seccomp + 新用户命名空间（Linux），当前未实现。`--backend quickjs` 会退回到同进程执行，此时连崩溃隔离都不存在。
2. **无 CPU 配额。** 只有墙钟中断（引擎中断处理器 + 父进程杀进程兜底），没有指令数计量。恶意扩展可以持续占满一个核心：每次调用超时后实例退役，但重新调用会重新拉起一个进程。
3. **无分层内存总量约束。** 引擎堆上限按实例计算，Job Object 上限按进程计算；N 个扩展各自可达上限，宿主总内存约为 N × limit。
4. **签名只解决"谁发的"，不解决"发了什么"。** 签名证明来源与未被篡改，但一个受信发布者仍然可以发布一个请求全部能力的扩展——授权仍是独立的一步。信任库没有撤销列表（CRL）与过期机制，移除信任只影响后续安装，已安装的包保持原状态。签名覆盖 `extension.json` 与入口脚本，包内其他文件不在哈希范围内（它们也不会被加载）。
5. **存储不加密、不隔离到操作系统用户。** 同机同用户的其他进程可读 `data/storage/storage.bin`；沙箱进程同样可读（见第 1 条）。
6. **TCP 只保护传输入口。** 令牌泄露（例如通过 stderr 日志）等于宿主被接管；令牌每次启动轮换，但同一进程生命周期内不变。
7. **事件与定时器无速率限制。** 扩展可高频发布事件或设置大量定时器，目前只受单次调用超时与调度线程串行执行的间接约束。
8. **日志可被伪造。** 扩展可写入任意级别的日志内容，日志不是审计证据。
9. **工作集共享开销。** 每实例一个进程意味着每实例至少一份引擎与二进制的私有内存；大量小扩展时这笔固定成本高于同进程后端。

## 滥用上限速查

| 资源 | 上限 | 超限行为 |
| --- | --- | --- |
| 存储单值 | 1 MiB | `INVALID_PARAMS` |
| 存储键 | 512 字节 | `INVALID_PARAMS` |
| 列表返回 | 512 条 | 截断 |
| 日志单条 | 4096 字符 | 截断 |
| 日志缓冲 | 2000 条 | 淘汰最旧 |
| 定时延迟 | 24 小时 | 夹取 |
| 安装包 | 1024 文件 / 64 MiB | 拒绝安装 |
| 消息帧 | 8 MiB | 协议错误并断开 |

## 加固路线（按收益排序）

1. **操作系统级限制**：受限令牌 / AppContainer（Windows）、seccomp + 独立用户与挂载命名空间（Linux），把"同用户可读"这条残余风险关掉。进程边界已经就位，这一步只是在边界上加策略。
2. **发布者信任补全**：撤销列表、密钥轮换与过期、多人共签（threshold），以及把信任库写入操作纳入审计。
3. **CPU 与调用速率配额**：指令计量（如 WASM fuel 或引擎指令计数）+ 每扩展令牌桶；进程后端已具备"超限即杀"的执行手段。
4. **审计日志分离**：宿主侧结构化事件日志与扩展可写日志分文件，前者不可被扩展污染。
5. **存储加密**：按用户密钥加密，或改用操作系统凭据存储。
6. **文件/网络能力的受控实现**：若确需，文件能力复用 `<id>/` 命名空间思路（永不接受插件提供的绝对路径），网络能力需独立的域名与地址白名单设计。
