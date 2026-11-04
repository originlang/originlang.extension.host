# OriginLang Extension Host

以 Rust 实现的扩展宿主平台：负责扩展的**安装、授权、隔离执行、状态持久化、定时任务与事件分发**，并可经 JSON-RPC 对外提供服务。

本仓库只产出**库与执行体**：宿主是 `originlang-ext-host` crate，执行侧是 `oext-sandbox` 可执行文件。命令入口不在这里，而在独立仓库 `bootidea.cli`——同一个宿主只有一个消费者，入口不该有第二份实现。

当前版本 `0.1.0`，提供三个执行后端：默认的**进程外沙箱**（每实例一个 `oext-sandbox` 进程）、**进程内 QuickJS 工作线程**，以及给一等方插件用的**动态库**。`bootidea` 默认按清单里的 `runtime` 字段逐个路由，一份进程里可以同时跑脚本插件与原生插件；取舍见 [docs/backends.md](docs/backends.md)。

## 特性

| 能力 | 现状 |
| --- | --- |
| 扩展格式 | `extension.json` + `main.js`（或 `runtime: dylib` 时的 `main.dll`），安装时校验并记录 SHA-256 |
| 执行隔离 | 默认每扩展独立进程（Job Object / PDEATHSIG 收尸），可切回进程内线程；堆/栈/墙钟上限按清单配置。`dylib` 后端不隔离，仅限一等方插件 |
| 授权模型 | 清单声明能力，运维显式授予；加载与每次调用双重校验，裁决永远在父进程 |
| 状态存储 | 按扩展 id 命名空间隔离，文件后端，原子写入 |
| 定时任务 | 一次性/重复任务，归属扩展卸载时自动取消 |
| 事件总线 | 主题订阅 + `onEvent:<topic>` 声明式激活，扩展间可通信（跨进程亦成立） |
| 传输 | stdio（嵌入式）、TCP 与 HTTP（网络传输均需令牌鉴权，写发现文件与单例锁） |
| 完整性 | 安装后文件被修改会拒绝加载；签名覆盖清单、版本与代码哈希 |
| 来源可信 | Ed25519 包签名 + 发布者信任库；可要求"必须由受信发布者签名" |

## 快速开始

```sh
cargo build --release
export PATH="$PWD/target/release:$PATH"   # 让 bootidea 找到 oext-sandbox
export OEXT_HOME="$PWD/tmp/home"          # 可选：默认走平台数据目录

# 以下命令由 bootidea.cli 提供
# 1. 生成模板
bootidea ext init demo

# 2. 安装并授予清单声明的能力
bootidea ext install ./demo --grant log.write

# 3. 直接运行一次（临时安装，不污染正式目录）
bootidea run ./demo --method hello --params '{"name":"OriginLang"}' --grant-all

# 4. 以 stdio 方式提供服务
printf '%s\n' \
  '{"jsonrpc":"2.0","id":1,"method":"host.info"}' \
  '{"jsonrpc":"2.0","id":2,"method":"host.extension.call","params":{"id":"demo","method":"hello"}}' \
  '{"jsonrpc":"2.0","id":3,"method":"host.shutdown"}' | bootidea serve
```

`bootidea run` / `bootidea serve` 默认使用进程外沙箱，需要 `oext-sandbox` 在 `PATH` 上（或用 `OEXT_SANDBOX_BIN` 指定完整路径）。调试时可用 `--backend quickjs` 回到同进程执行：

```sh
bootidea --backend quickjs run ./demo --method hello
```

TCP 方式：

```sh
bootidea serve --transport tcp --port 7311
# 输出 pairing token，客户端首个请求必须是 host.authenticate
```

HTTP 方式（`std` 是 `stdio` 的别名）：

```sh
bootidea serve --transport http --port 7311
# 从启动日志或 host.json 获取 token
curl http://127.0.0.1:7311/rpc \
  -H "Authorization: Bearer <token>" \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"host.info"}'
```

HTTP 每次请求都需携带令牌；请求体是一个完整 JSON-RPC 对象，支持格式化换行。
通知返回 HTTP 204；普通调用返回 HTTP 200 和 JSON-RPC 结果或错误。


## 包签名

签名把"这包是谁发的"变成可校验的事实；哈希只证明"与安装时一致"。默认策略是**校验已有签名、放行未签名包**，用 `--require-signature` 收紧。

```sh
# 1. 生成密钥（私钥留在包目录之外）
bootidea keygen --out ~/keys/acme.key --publisher acme

# 2. 给包签名，生成 extension.sig
bootidea sign ./demo --key ~/keys/acme.key

# 3. 在该宿主上信任这个公钥
bootidea trust add <publicKey> --name acme     # keygen 会直接打印这条命令
bootidea trust list

# 4. 只接受受信发布者签名的包
bootidea --require-signature ext install ./demo --grant log.write
```

签名负载为 `oext-signature-v1\n<publisher>\n<id>\n<version>\n<content hash>\n`，因此发布者名、扩展身份、版本与代码四者被绑定在一起：换名字、换版本、改代码都会让签名失效。`bootidea ext list` / `bootidea ext info` 会显示 `publisher` 与 `trusted`。

## 目录结构

```
crates/protocol          JSON-RPC 信封、错误码、方法名
crates/permission        能力标识校验与授予策略（拒绝默认）
crates/manifest          清单 schema、路径安全、内容哈希
crates/storage           命名空间字节存储（内存 / 文件）
crates/scheduler         单线程串行调度器
crates/session           令牌、发现文件、存活探测、单例锁
crates/runtime           运行时后端 trait（宿主唯一依赖的执行抽象）
crates/runtime-quickjs   QuickJS 引擎与进程内后端
crates/runtime-process   进程外后端：拉起沙箱、管道协议、Job Object
crates/runtime-dylib     动态库后端：加载一等方原生插件
crates/plugin-abi        宿主与插件共用的 C ABI、信封与 panic 守卫
crates/signing           Ed25519 包签名、密钥文件、发布者信任库
crates/host              注册表、生命周期、Host API、事件、定时器
apps/sandbox             oext-sandbox：只链接 protocol/runtime/quickjs 的执行体
examples                 hello / timers / notes
docs                     架构、协议、后端、安全
```

命令行入口在独立仓库 `bootidea.cli`：它以 git 依赖（锁定到具体 rev）链接本仓库的 crate，装配后端并实现全部子命令。库的契约在这里演进，入口的适配在那里演进，两者独立发版。

## 能力模型

扩展在清单中**声明**能力，运维在宿主机上**授予**能力，两者同时满足才放行。

| Host API | 能力 |
| --- | --- |
| `host.log` | `log.write` |
| `host.storage.get` / `list` | `storage.read` |
| `host.storage.set` / `delete` | `storage.write` |
| `host.timer.set` / `clear` | `timer.schedule` |
| `host.events.emit` | `events.emit` |
| `host.events.subscribe` | `events.subscribe` |
| `host.extension.call` | `extension.call` |
| `host.self.info` | 无 |

加载是授权门：声明了但未授予的能力会导致扩展根本无法加载；运行期撤销能力则立即对后续调用生效，无需卸载。

## 文档

- [架构与设计决策](docs/architecture.md)
- [执行后端与插件形态](docs/backends.md)
- [协议、清单与 Host API](docs/protocol.md)
- [安全模型与边界](docs/security.md)

## 已知边界

- 沙箱进程与宿主**同一用户**：能读该用户可读的一切。要收紧需受限令牌 / AppContainer / seccomp，见安全文档。`--backend quickjs` 会退回同进程，此时引擎缺陷等同宿主缺陷。
- 无 CPU 配额计量，只有墙钟超时（引擎中断 + 父进程杀进程兜底）；无文件与网络能力（方向见安全文档）。
- 定时器不持久化，重启后不恢复；事件投递在实例繁忙时跳过并记日志，不排队。
- 签名覆盖 `extension.json` 与入口脚本，不覆盖包内其他文件；未实现远程仓库与发布者撤销列表。

## 开发

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

## CI/CD 与自动版本发布

每个阶段一个独立 YAML，由入口文件编排：

| 文件 | 职责 |
| --- | --- |
| `.github/workflows/pipeline.yml` | 分支推送、PR、标签推送和手动 CI |
| `.github/workflows/quality.yml` | 格式、Clippy、全仓版本一致性与标签校验 |
| `.github/workflows/test.yml` | Windows / Linux 全部测试 |
| `.github/workflows/build.yml` | Release 构建、沙箱冒烟测试、打包和 SHA-256 |
| `.github/workflows/release-please.yml` | 正式发布分支上管理 Release PR；创建 Release 后编排验证与构建 |
| `.github/workflows/nightly.yml` | feature/dev 检查、测试和打包通过后发布 nightly 预发布 |
| `.github/workflows/release.yml` | 将本次流水线的已校验产物上传到 Release Please 创建的 Release |

### 发布流程

1. 使用 Conventional Commits，例如 `feat: add HTTP transport`、`fix: handle shutdown`。
2. 推送到正式发布分支（默认 `main`，可用仓库变量 `RELEASE_BRANCH` 配置）后，Release Please 创建或更新版本 PR。
3. 合并版本 PR 后，Release Please 创建 `vX.Y.Z` 标签和 GitHub Release。
4. 同一工作流从该标签检出代码，依次检查、测试、构建并上传程序包。无需手动打标签。

仅根发布生成 GitHub Release；成员 crate 不单独发布，也不会执行 `cargo publish`。
Release 页面创建早于二进制上传；若后续阶段失败，可在该次 Actions 运行中重跑失败任务。
其他分支、PR 和手工推送标签只检查和打包，不创建正式 Release。

### feature/dev nightly

每次推送 `feature/dev`，以及在该分支手动运行 CI/CD，都会在质量检查、Windows/Linux 测试和构建成功后发布 nightly。
PR 不发布。此处 nightly 表示开发渠道，未配置每日定时任务。

nightly 使用独立标签 `nightly-<run-id>-<sha前8位>`，标记为预发布且不设为 Latest；每次运行保留自己的发行记录。
压缩包名包含 `nightly.<run-id>.<sha前8位>`，包内 `build-info.json` 记录完整提交 SHA、运行编号和目标平台。
Cargo 版本和 Release Please manifest 不变，程序 `--version` 仍显示当前 Cargo 版本。
重跑同一次构建会更新同一 nightly 的附件；只重跑失败任务即可重试上传。
`feature/dev` 始终不运行正式 Release Please，即使它是 GitHub 默认分支或被误填到 `RELEASE_BRANCH`。

### Workspace 配置

`release-please-config.json` 使用 manifest 模式：成员使用 `rust` 策略与 `cargo-workspace` 插件，
`linked-versions` 将全部成员与根发布绑定到同一版本。`.release-please-manifest.json` 以当前 `0.1.0` 为基线，
后续根据提交计算新版本。根发布使用 `simple` 策略维护 `version.txt`、根 `CHANGELOG.md`，
并通过 `extra-files` 更新 `[workspace.package].version`。

这是为了兼容上游 Rust updater 不支持虚拟 workspace 根和继承版本的限制：
成员的 `version.workspace = true` 改为显式版本，由 Release Please 统一同步；其他 workspace 继承保持不变。
质量检查会核对根版本、全部成员、Cargo.lock 和发布 manifest，阻止版本漂移。
新增 crate 时需同时加入发布配置、manifest 与 linked-versions 组。

### GitHub 设置

在 Settings → Actions → General 中允许 GitHub Actions 创建 PR。
默认使用 `GITHUB_TOKEN`；它创建的 Release PR 不会自动触发另一条 PR CI。
如需自动运行 Release PR 的分支保护检查，配置仓库 secret `RELEASE_PLEASE_TOKEN`，
使用具备仓库 Contents、Pull requests、Issues 写权限的 token。工作流优先使用该 secret。
不配置额外 token 时，合并后的标签验证、构建和上传仍会在同一工作流运行。

工具链由 `rust-toolchain.toml` 指定，Cargo 使用 `--locked`，需提交 Cargo.lock。
产物包括 Windows x64 ZIP、Linux x64 tar.gz 和 SHA-256；每个包包含 `oext-sandbox`、
README、CHANGELOG、文档及示例。宿主库不在包内——它由 Cargo 依赖引入；命令行入口也不在包内，它由 `bootidea.cli` 独立发布。Linux 包在 Ubuntu 24.04 构建，需要兼容的 glibc；未提供 musl 包。
普通 CI 产物保留 14 天，发行产物保存到 GitHub Release。

参考：[Release Please manifest](https://github.com/googleapis/release-please/blob/main/docs/manifest-releaser.md)
与 [Action token 说明](https://github.com/googleapis/release-please-action#other-actions-on-release-please-prs)。
