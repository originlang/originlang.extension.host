# Host 发布契约

契约版本：`2`。编写日期：2026-11-06。最近修订：2026-11-08。状态：已冻结（版本 2）。对应工作包 H02。

本文定义独立 Host 服务**发布产物的可观察约定**：平台与架构、入口名称、包内文件布局、完整性校验、协议/插件 ABI/沙箱兼容矩阵，并附完整清单样例与不兼容样例。消费者（CLI、HomeWorld、Desktop）按本文下载、校验、安装、启动 Host，不硬编码未约定的二进制名称，也不从当前实现反推包结构。

约定：

- 「必须 / 不得」为强制项；「建议」为不参与验收的实现建议。
- 版本号只描述本文规定的发布行为集合。新增可选字段、新增平台属于次版本；删除产物、改入口名称、改校验算法属于主版本。

## 变更记录

| 版本 | 日期 | 变更 |
| --- | --- | --- |
| `1` | 2026-11-06 | 首次冻结。入口 `oext-host`，清单 `schemaVersion` 为 `1`。 |
| `2` | 2026-11-08 | 服务入口更名为 `ol-host`；清单升为 `schemaVersion: 2` 并取消循环校验（详见第 4、5 节）；明确本渠道包不附带沙箱（`sandbox: null`）；新增 `hostVersion` 字段与冒烟验收章节（第 10 节）。 |

版本 `1` 的 `checksums` 字段要求清单记录 `SHA256SUMS` 自身的哈希，而 `SHA256SUMS` 又需覆盖包内全部文件（含 `manifest.json`），两者互相包含哈希，任何实现都无法生成或验证。版本 `2` 以「归档哈希 → `SHA256SUMS` → 清单 → 载荷」的单向链取代它，因此该字段被移除而非改名。

## 1. 平台与架构

| 目标三元组 | 说明 |
| --- | --- |
| `x86_64-pc-windows-msvc` | 内测首选平台 |
| `x86_64-apple-darwin` | 后续 |
| `aarch64-apple-darwin` | 后续；当前仅用于本地交付链验证 |
| `x86_64-unknown-linux-gnu` | 本地 nightly 验证用 |

- 每个平台一个发布包；包名必须含目标三元组，不允许一个包混装多架构。
- 优先验证 Windows x64 的完整交付链，再逐个平台扩展；这是排期建议，不改变跨平台目标。

## 2. 入口与产物

| 产物 | 说明 |
| --- | --- |
| `bin/ol-host`（Windows `bin/ol-host.exe`） | 独立服务入口，唯一对外服务进程 |
| `bin/oext-sandbox`（Windows `bin/oext-sandbox.exe`） | 进程外沙箱执行体；**本渠道包不附带**，见下 |

交付范围（2026-11-08）：当前只交付服务入口 `bin/ol-host`。`oext-sandbox` 的源码仍在本仓库，但**不作为本次交付内容**，发布包不含该执行体，清单固定 `sandbox: null`。下文关于沙箱的布局与校验规则仅在未来实际包含该执行体时适用。

强制规则：

- 服务入口固定为 `ol-host`，消费者不得硬编码其他二进制名称，也不得再使用旧名称 `oext-host`（版本 `1` 的入口名，已取消，不提供兼容别名）。
- `oext-sandbox` 是执行体，不是服务入口；消费者不得把它当作 Host 启动，也不得从 `PATH` 猜测它的位置。
- 包内是否包含 `oext-sandbox` 必须与清单一致：清单 `sandbox` 为 `null` 时包内不得有该文件，消费者不得据此启用进程外沙箱后端；`sandbox` 为文件名时 `bin/` 下必须真实存在该文件，否则包校验失败。
- 不含沙箱时 Host 仍必须能启动、握手、报告可用后端并正常退出；`host.hello` 的 `sandbox` 字段为 `null`，`backends` 不含 `sandbox`。宿主不得把「未附带沙箱」表述为具备进程隔离能力。

消费者适配：

- CLI/HomeWorld 的下载与启动路径改为 `bin/ol-host`，并在启动失败时区分「可执行文件缺失」与「沙箱不可用」。
- 启用进程外沙箱前必须以 `sandbox` 字段与运行期 `host.hello` 双重确认；`sandbox: null` 或握手返回 `null` 时退回声明式后端（`quickjs`/`dylib`）或提示用户单独安装沙箱。

## 3. 包内文件布局

```text
oext-<version>-<target>.tar.gz / .zip
└── oext-<version>-<target>/
    ├── bin/ol-host              # 服务入口（Unix 0755；Windows 同级为 ol-host.exe）
    ├── bin/oext-sandbox         # 可选；本渠道包不包含
    ├── manifest.json            # 本清单（包元数据 + 兼容矩阵 + 载荷校验）
    ├── SHA256SUMS               # 包内除自身外全部文件的 sha256（含 manifest.json）
    ├── build-info.json          # 仅 nightly：提交、运行编号、目标平台
    ├── docs/                    # README、CHANGELOG、service-contract、release-contract
    └── examples/                # 示例扩展包（不参与运行时校验）
```

强制规则：

- 入口必须真实存在于 `bin/` 下并被清单覆盖；清单声明了 `sandbox` 就要求 `bin/oext-sandbox` 真实存在，否则包校验失败。
- 清单与校验和必须位于包根，便于消费者在解包前读取。
- 不得在包内放日志、运行时状态、发现文件或单例锁（`logs/`、`run/`、`state/`、`data/`、`*.log` 一律拒绝）——这些是运行期产物，属于安装根目录，不属于发布包。
- 归档名与顶层目录名必须一致，且都以目标三元组结尾；消费者据此判定平台。

## 4. 清单（manifest.json）

清单是包根下的 `manifest.json`，字段如下：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schemaVersion` | 整数 | 清单格式版本，当前 `2` |
| `name` | 字符串 | 发布名，固定 `oext` |
| `version` | 字符串 | 包版本（含 `-nightly.<run>.<sha8>` 等预发布段） |
| `hostVersion` | 字符串 | 入口可执行文件报告的版本，等于 `ol-host --version` 输出 |
| `channel` | 字符串 | `stable` \| `nightly` |
| `target` | 字符串 | 目标三元组 |
| `entry` | 字符串 | 服务入口文件名，固定 `ol-host`（Windows `ol-host.exe`） |
| `sandbox` | 字符串 \| `null` | 沙箱执行体文件名；未附带时为 `null` |
| `protocolVersion` | 字符串 | 服务协议主版本，见服务契约第 4.2 节 |
| `apiVersion` | 字符串 | 扩展清单 API 版本 |
| `abiVersion` | 整数 | 插件 ABI 版本，见服务契约第 4.4 节 |
| `files` | 对象 | `路径 → sha256`，覆盖包内除 `manifest.json` 与 `SHA256SUMS` 之外的全部文件 |

强制规则：

- `version` 与 `hostVersion` 分开：nightly 的包版本带预发布段，而二进制的 `--version` 仍输出 Cargo 版本。二者不得互相替代。
- `files` **不得**包含 `manifest.json` 或 `SHA256SUMS`：包含前者会让清单描述自身，包含后者会与其覆盖清单的要求形成环。
- 消费者**先校验 `manifest.json` 再信任其内容**：清单本身的哈希由 `SHA256SUMS` 覆盖，`SHA256SUMS` 的完整性由第 5 节的归档哈希保证。
- `protocolVersion`/`abiVersion` 与包内二进制实际行为不一致时，以运行期握手结果为准；清单值仅作下载前的兼容性预判，不得替代握手。
- 未知字段（`schemaVersion` 升级新增字段）消费者应忽略而非报错，保证前向兼容；但 `schemaVersion` 大于消费者所知的版本时，消费者不得继续执行校验。

## 5. 完整性校验

校验算法固定 `sha256`；`SHA256SUMS` 为 `sha256sum` 兼容格式（`<hex>  <path>`）。

### 5.1 无环校验链

```
<archive>.sha256  ──►  归档  ──►  SHA256SUMS（覆盖 manifest.json 与全部载荷）
                                └►  manifest.files（覆盖除 manifest.json、SHA256SUMS 外的载荷）
```

| 层级 | 覆盖范围 | 由谁提供 |
| --- | --- | --- |
| 归档哈希 | 归档文件本身 | `<archive>.sha256` 旁挂文件；其值同时出现在发布页，供消费者按外部渠道核对 |
| `SHA256SUMS` | 包内除自身外的全部文件，**含 `manifest.json`** | 包根 |
| `manifest.files` | 包内除 `manifest.json`、`SHA256SUMS` 外的全部文件 | 包根 `manifest.json` |

生成顺序固定为「载荷 → `manifest.json` → `SHA256SUMS` → 归档 → 归档哈希」，校验顺序相反。任何一层都不需要描述自身，因此链条既可生成也可验证；消费者只需从发布方获得一个归档哈希（发布页、下载元数据或签名），其余全部自洽。

### 5.2 消费者校验流程

1. 计算归档 sha256，与外部获得的归档哈希（及 `<archive>.sha256`）比对。
2. 解包，对 `SHA256SUMS` 中每一条目重新计算哈希并比对；确认它覆盖包内除自身外的全部文件，且包含 `manifest.json`。
3. 读取 `manifest.json`，用其 `files` 与 `SHA256SUMS` 交叉核对；确认 `files` 未包含 `manifest.json`/`SHA256SUMS`，且与载荷集合完全一致。
4. 核对 `entry`、`target`、`sandbox`：入口文件真实存在且被覆盖；`sandbox` 为 `null` 时包内无沙箱二进制，为文件名时该文件存在。
5. 任一步不匹配即拒绝安装/执行，保留上一可用产物，并提供重试入口；旧版本不得被不兼容校验强行降级。

参考实现：`.github/scripts/verify_package.py`（第 1 步支持 `--expected-sha256` 传入外部值）。

## 6. 兼容矩阵

| 维度 | 字段 | 判定规则 |
| --- | --- | --- |
| 服务协议 | `protocolVersion` | 客户端与 Host 主版本相同即兼容，握手见服务契约第 4.2/4.3 节 |
| 宿主版本 | `hostVersion` | 与 `ol-host --version` 一致；用于诊断，不参与兼容判定 |
| 扩展清单 | `apiVersion` | 必须为 `"1"`，见协议文档清单校验 |
| 插件 ABI | `abiVersion` | 整数；`dylib` 插件加载前比对，不匹配拒绝 `-32004` |
| 沙箱 | `sandbox` | `null` 时 `--backend auto` 跳过沙箱后端，`host.hello` 的 `sandbox` 为 `null`；非 `null` 时文件必须存在 |
| 目标平台 | `target` | 必须与消费者平台三元组一致，否则拒绝 |

## 7. 完整清单样例

```json
{
  "schemaVersion": 2,
  "name": "oext",
  "version": "0.1.0",
  "hostVersion": "0.1.0",
  "channel": "stable",
  "target": "x86_64-pc-windows-msvc",
  "entry": "ol-host.exe",
  "sandbox": null,
  "protocolVersion": "1",
  "apiVersion": "1",
  "abiVersion": 1,
  "files": {
    "bin/ol-host.exe": "3c9e...（64 位十六进制）",
    "docs/README.md": "c3d4...",
    "docs/CHANGELOG.md": "e5f6...",
    "docs/service-contract.md": "1a2b...",
    "docs/release-contract.md": "4d5e...",
    "examples/hello/extension.json": "07a8...",
    "examples/hello/main.js": "9c1d..."
  }
}
```

同包 `SHA256SUMS`（节选，`manifest.json` 必须在列）：

```text
3c9e...  bin/ol-host.exe
c3d4...  docs/README.md
...
6f7a...  manifest.json
```

同包 `<archive>.sha256`：

```text
8b9c...  oext-0.1.0-x86_64-pc-windows-msvc.zip
```

## 8. 不兼容样例（消费者必须拒绝）

1. **入口不存在**：`entry` 为 `ol-host` 但包内只有 `oext-host`、`bootidea` 或 `oext-sandbox`——拒绝，入口不真实存在。
2. **旧入口名**：`entry` 为 `oext-host`——拒绝，版本 `2` 的入口固定 `ol-host`。
3. **沙箱缺失但声明存在**：`sandbox` 为 `oext-sandbox.exe` 但 `bin/` 下无此文件——拒绝，校验失败。
4. **沙箱存在但声明 `null`**：包内实际带 `bin/oext-sandbox` 而清单为 `null`——拒绝，清单与包内容不一致。
5. **校验和不匹配**：`files` 中任一 sha256 与 `SHA256SUMS` 不一致，或包内文件未被 `SHA256SUMS` 覆盖——拒绝，保留旧版本。
6. **清单自我引用**：`files` 含 `manifest.json` 或 `SHA256SUMS`——拒绝，该清单无法自洽校验（这正是版本 `1` 的缺陷）。
7. **协议主版本不兼容**：清单 `protocolVersion` 为 `"2"` 而客户端为 `"1"`——下载前拒绝，或握手时收到 `-32023`。
8. **目标平台不符**：`target` 为 `x86_64-unknown-linux-gnu` 但消费者是 Windows——拒绝。
9. **包含运行期产物**：包内出现 `run/`、`state/`、`logs/`、`*.log`——拒绝，包结构不合法。

## 9. 与发布链的关系

- 正式版与 nightly **共用同一构建与打包任务**（`.github/workflows/build.yml`）和同一打包脚本（`.github/scripts/package_release.py`），因此包结构、清单字段与校验链完全一致，差异只在 `channel`、`version`（含预发布段）与 `build-info.json`。
- 打包前先构建 release 二进制并冒烟 `ol-host --version`；打包后从归档解压执行第 10 节验收，任一失败则该平台不产出可用产物。
- 发布阶段只做传输完整性检查与资产计数（`sha256sum --check`），不重新打包：正式版由 `release-attach.yml` 附到 GitHub Release，nightly 由 `publish-nightly.sh` 发布为预发布。
- 本地 nightly 由 `.act/nightly.yml` 生成，产物写 `dist/nightly/<stamp>/`，不碰远端；其打包与验收使用与正式版相同的脚本。
- 本地打包与 CI 走同一脚本、同一包结构：`python3 .github/scripts/package_release.py --build --verify`。省略 `--target` 时目标三元组取自 `rustc -vV` 的 `host`，`target/release/` 与 `target/<triple>/release/` 两种 cargo 产物目录都可接受；显式 `--target` 时只接受对应目录，避免把本机二进制标成其他平台。
- 无论 nightly 还是正式版，**包内都必须带 `manifest.json` 与 `SHA256SUMS`**，否则视为不完整产物，不得交付。
- 版本号即标签（去掉 `v` 前缀），打 tag 前先同步 `Cargo.toml`/`version.txt`/`Cargo.lock`；预发布（含 `-` 的 tag）标记 `prerelease`。

## 10. 冒烟验收（从解压后的发布包执行）

发布包只有在通过下列检查后才算交付完成；参考实现 `.github/scripts/smoke_host.py`，两个渠道都在构建任务内执行。

| 检查 | 期望 |
| --- | --- |
| `bin/ol-host --version` | stdout 恰好一行、等于清单 `hostVersion`，退出码 `0`，stderr 为空 |
| `bin/ol-host --help` | 退出码 `0`，用法中出现 `ol-host` |
| 未知参数 | 退出码 `2`，用法写 stderr，stdout 为空 |
| `host.hello` | 成功；`protocolVersion`/`apiVersion`/`abiVersion`/`hostVersion` 与清单一致；`root` 等于 `--home` |
| `host.info` | 成功，且与 `host.hello` 同源（`hostVersion` 一致、含扩展计数） |
| 后端报告 | `sandbox` 为 `null` 且 `backends` 不含 `sandbox`；`runtime` 为已知值 |
| `--backend sandbox`（包内无沙箱） | 非零退出并说明原因，不得静默退化为进程内执行 |
| stdio EOF | 进程及时退出且退出码 `0`，`<home>/run/` 无残留 |
| stdout 纯净性 | 每一行都是合法 JSON-RPC 帧（`jsonrpc: "2.0"`、合法 `id`），启动横幅与诊断只在 stderr |
| `host.shutdown` | 返回 `{"stopping": true}`，退出码 `0` |

## 11. 消费者适配要点（版本 2）

| 消费者 | 需要改动 |
| --- | --- |
| CLI | 下载与启动 `bin/ol-host`；删除一切 `oext-host` 引用；按第 5.2 节校验后再启动；`serve` 命令迁移或删除（C01/C03） |
| HomeWorld | 下载源与启动参数改为 `bin/ol-host`；按根目录计算发现文件；不假定包内附带沙箱（F01） |
| Desktop | 按状态机区分未安装、未连接、连接中、已连接、失败（DT2/DT3） |
| 构建脚本 | 使用 `package_release.py`/`verify_package.py` 生成与校验，不再手写清单与校验和 |

迁移步骤（`oext-host` → `ol-host`）：替换可执行文件名与下载清单中的 `entry`/`files` 键；不修改 `host.*` 协议方法、`OEXT_*` 环境变量与安装根目录布局——本次更名不涉及协议、配置与数据目录的兼容性变更。
