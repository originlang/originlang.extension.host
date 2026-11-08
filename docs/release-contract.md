# Host 发布契约

契约版本：`1`。编写日期：2026-11-06。状态：已冻结。对应工作包 H02。

本文定义独立 Host 服务**发布产物的可观察约定**：平台与架构、入口名称、包内文件布局、完整性校验、协议/插件 ABI/沙箱兼容矩阵，并附完整清单样例与不兼容样例。消费者（CLI、HomeWorld、Desktop）按本文下载、校验、安装、启动 Host，不硬编码未约定的二进制名称，也不从当前实现反推包结构。

约定：

- 「必须 / 不得」为强制项；「建议」为不参与验收的实现建议。
- 版本号只描述本文规定的发布行为集合。新增可选字段、新增平台属于次版本；删除产物、改入口名称、改校验算法属于主版本。

## 1. 平台与架构

| 目标三元组 | 说明 |
| --- | --- |
| `x86_64-pc-windows-msvc` | 内测首选平台 |
| `x86_64-apple-darwin` | 后续 |
| `aarch64-apple-darwin` | 后续 |
| `x86_64-unknown-linux-gnu` | 本地 nightly 验证用 |

- 每个平台一个发布包；包名必须含目标三元组，不允许一个包混装多架构。
- 优先验证 Windows x64 的完整交付链，再逐个平台扩展；这是排期建议，不改变跨平台目标。

## 2. 入口与产物

交付范围更新（2026-11-08）：当前优先交付 `bin/oext-host`（Windows 为 `bin/oext-host.exe`）。`oext-sandbox` 是否由本仓库交付待定；下文关于沙箱的布局与校验规则仅在实际包含该执行体时适用，不构成当前交付承诺。

消费者适配：CLI 与 HomeWorld 不得假定 Host 包附带沙箱；须读取清单的 `sandbox` 字段，并以运行期握手结果确认可用性。不含沙箱的包使用 `sandbox: null`，不能据此启用进程外沙箱后端。

| 产物 | 说明 |
| --- | --- |
| `oext-host`（Windows `oext-host.exe`） | 独立服务入口，唯一对外服务进程 |
| `oext-sandbox`（Windows `oext-sandbox.exe`） | 进程外沙箱执行体，是否由本仓库交付待定；可用时由 Host 按服务契约第 3.4 节发现 |

强制规则：

- 服务入口固定为 `oext-host`，消费者不得硬编码其他二进制名称。
- `oext-sandbox` 是执行体，不是服务入口；消费者不得把它当作 Host 启动，也不得从 `PATH` 猜测它的位置。
- 发布包是否包含 `oext-sandbox` 必须按真实产物说明；不含沙箱的包须在清单中明确 `sandbox: null`，Host 此时以 `--backend auto` 跳过沙箱后端。

## 3. 包内文件布局

```text
oext-<version>-<target>.tar.gz / .zip
└── oext-<version>-<target>/
    ├── bin/oext-host            # 服务入口（Unix 0755；Windows 同级放 oext-host.exe）
    ├── bin/oext-sandbox         # 沙箱执行体（可选，见第 2 节）
    ├── manifest.json            # 本清单（包元数据 + 兼容矩阵 + 校验）
    ├── SHA256SUMS               # 包内所有文件的 sha256（不含自身）
    ├── docs/                    # README、CHANGELOG、service-contract、release-contract
    └── examples/                # 示例扩展包（不参与运行时校验）
```

强制规则：

- 入口必须真实存在于 `bin/` 下；清单声明了 `sandbox` 就要求 `bin/oext-sandbox` 真实存在，否则包校验失败。
- 清单与校验和必须位于包根，便于消费者在解包前读取。
- 不得在包内放日志、运行时状态、发现文件或单例锁——这些是运行期产物，属于安装根目录，不属于发布包。

## 4. 清单（manifest.json）

清单是包根下的 `manifest.json`，字段如下：

| 字段 | 类型 | 说明 |
| --- | --- | --- |
| `schemaVersion` | 整数 | 清单格式版本，当前 `1` |
| `name` | 字符串 | 发布名，固定 `oext` |
| `version` | 字符串 | 语义化版本（含 `-nightly`/`-pre` 等预发布段） |
| `channel` | 字符串 | `stable` \| `nightly` |
| `target` | 字符串 | 目标三元组 |
| `entry` | 字符串 | 服务入口文件名，固定 `oext-host`（Windows `oext-host.exe`） |
| `sandbox` | 字符串 \| `null` | 沙箱执行体文件名；不含时为 `null` |
| `protocolVersion` | 字符串 | 服务协议主版本，见服务契约第 4.2 节 |
| `apiVersion` | 字符串 | 扩展清单 API 版本 |
| `abiVersion` | 整数 | 插件 ABI 版本，见服务契约第 4.4 节 |
| `files` | 对象 | `路径 → sha256` 映射，覆盖 `bin/`、`docs/`、`examples/` 全部文件 |
| `checksums` | 字符串 | `SHA256SUMS` 文件自身的 sha256 |

强制规则：

- 消费者**先校验 `manifest.json` 再信任其内容**：以外部渠道（发布页、签名）获得的 `checksums` 比对 `SHA256SUMS` 文件，再逐文件比对 `files` 映射。
- `protocolVersion`/`abiVersion` 与包内二进制实际行为不一致时，以运行期握手结果为准；清单值仅作下载前的兼容性预判，不得替代握手。
- 未知字段（`schemaVersion` 升级新增字段）消费者应忽略而非报错，保证前向兼容；但 `schemaVersion` 大于消费者所知的版本时，消费者不得继续执行校验。

## 5. 完整性校验

- 校验算法固定 `sha256`；`SHA256SUMS` 为 `sha256sum` 兼容格式（`<hex>  <path>`）。
- 消费者拿到包后按顺序：
  1. 用外部获得的 `checksums` 值校验 `SHA256SUMS` 文件；
  2. 用 `SHA256SUMS` 逐文件校验包内容；
  3. 用 `manifest.json` 的 `files` 映射二次核对 `bin/` 下入口与沙箱。
- 任一步不匹配即拒绝安装/执行，保留上一可用产物，并提供重试入口；旧版本不得被不兼容校验强行降级。

## 6. 兼容矩阵

| 维度 | 字段 | 判定规则 |
| --- | --- | --- |
| 服务协议 | `protocolVersion` | 客户端与 Host 主版本相同即兼容，握手见服务契约第 4.2/4.3 节 |
| 扩展清单 | `apiVersion` | 必须为 `"1"`，见协议文档清单校验 |
| 插件 ABI | `abiVersion` | 整数；`dylib` 插件加载前比对，不匹配拒绝 `-32004` |
| 沙箱 | `sandbox` | 缺失沙箱时 `--backend auto` 跳过沙箱后端，`host.hello` 的 `sandbox` 为 `null` |
| 目标平台 | `target` | 必须与消费者平台三元组一致，否则拒绝 |

## 7. 完整清单样例

```json
{
  "schemaVersion": 1,
  "name": "oext",
  "version": "0.1.0",
  "channel": "stable",
  "target": "x86_64-pc-windows-msvc",
  "entry": "oext-host.exe",
  "sandbox": "oext-sandbox.exe",
  "protocolVersion": "1",
  "apiVersion": "1",
  "abiVersion": 1,
  "files": {
    "bin/oext-host.exe": "3c9e...（64 位十六进制）",
    "bin/oext-sandbox.exe": "a1b2...",
    "docs/README.md": "c3d4...",
    "docs/CHANGELOG.md": "e5f6...",
    "examples/hello/extension.json": "07a8..."
  },
  "checksums": "9b8a...（SHA256SUMS 文件自身的 sha256）"
}
```

## 8. 不兼容样例（消费者必须拒绝）

1. **入口改名**：`entry` 为 `oext-host` 但包内只有 `bootidea` 或 `oext-sandbox`——拒绝，入口不真实存在。
2. **沙箱缺失但声明存在**：`sandbox` 为 `oext-sandbox.exe` 但 `bin/` 下无此文件——拒绝，校验失败。
3. **校验和不匹配**：`files` 中任一 sha256 与 `SHA256SUMS` 不一致——拒绝，保留旧版本。
4. **协议主版本不兼容**：清单 `protocolVersion` 为 `"2"` 而客户端为 `"1"`——下载前拒绝，或握手时收到 `-32023`。
5. **目标平台不符**：`target` 为 `x86_64-unknown-linux-gnu` 但消费者是 Windows——拒绝。

## 9. 与发布链的关系

- 本地 nightly 由 `.act/nightly.yml` 生成，产物写 `dist/nightly/<stamp>/`，不碰远端；正式版由 push `vX.Y.Z` 标签触发构建，产物附到 GitHub Release。
- 无论 nightly 还是正式版，**包内都必须带 `manifest.json` 与 `SHA256SUMS`**，否则视为不完整产物，不得交付。
- 版本号即标签（去掉 `v` 前缀），打 tag 前先同步 `Cargo.toml`/`version.txt`/`Cargo.lock`；预发布（含 `-` 的 tag）标记 `prerelease`。
