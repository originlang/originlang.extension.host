# 执行后端与插件形态

宿主不假设扩展代码怎么运行。一个后端实现 `Runtime`，由 `RuntimeProvider` 产出实例；宿主只负责生命周期、授权与存储。当前有三个后端，按**信任级别**划分，而不是按功能划分。

## 三个后端

| 后端 | `runtime` 取值 | 代码形态 | 隔离 | 适用 |
| --- | --- | --- | --- | --- |
| 沙箱进程 | `sandbox` | `main.js` | 每实例子进程 + 作业对象 / PDEATHSIG | 不可信第三方 |
| QuickJS 工作线程 | `quickjs` | `main.js` | 内存/栈/墙钟上限 + 中断 + 引擎退役 | 一般插件、脚本化逻辑 |
| 动态库 | `dylib` | `main.dll` / `.so` / `.dylib` | 无 | 一等方插件 |

清单里用 `runtime` 声明，缺省为 `quickjs`：

```json
{
  "id": "harness",
  "main": "harness.dll",
  "runtime": "dylib"
}
```

`bootidea` 默认以 `--backend auto` 运行：按清单路由到对应后端，一份进程里可以同时装着 dylib 插件和 QuickJS 插件。`--backend quickjs|sandbox|dylib` 把所有扩展钉在单后端上，用于隔离与排查。

## 为什么 dylib 只给一等方

动态库插件和宿主共享地址空间。它能直接读文件、开 socket、读其他扩展的存储，不需要经过 `host.extension.call`，因此**能力授予对它只是约定，不是边界**。这对"自己人写的插件"是可接受的取舍，对开放生态则等于取消沙箱。

所以规则是：

- **一等方插件** → `dylib`。可信代码，不为它付沙箱成本。
- **第三方插件** → 沙箱（现有 `sandbox` 后端，将来 `wasm`）。它们进入的是受限环境，不是进程内。

`runtime` 字段的校验故意不锁定枚举：后端集合属于宿主二进制，装了什么由 `--backend` 决定。写了未注册的后端，加载时报错并列出已注册的值，而不是静默回退到别的后端——回退会让一个 `.dll` 交给 QuickJS，错误信息会误导成"main 必须是 .js"。

## dylib 后端的三个已知代价

这三点在 `crates/runtime-dylib/src/lib.rs` 的模块文档里也写着，属于设计取舍而非缺陷：

**调用不可中断。** QuickJS 装中断处理器，超时靠停解释器实现。原生函数无法从另一个线程停掉。截止时间过后后端会**退役实例**——不再接受调用并报超时，但那次调用会一直跑到自己返回。超时的插件会一直占着宿主线程。检查的意义是拦住**后续**调用，不是抢占。

**库永不卸载。** 对启动过线程、注册过 TLS 回调、或持有带析构的静态变量的 Rust `cdylib` 调用 `FreeLibrary`，会留下悬空指针。卸载只在后端无法验证的条件下才安全，所以干脆不做。代价是**不能热替换**：换掉磁盘上的文件要重启宿主才生效。收益是加载之后不会在退出时崩溃。

**能力退化为约定。** 见上一节。经过 `HostVTable` 仍然有价值：审计路径统一、按调用计时、日志归属一致。但它不是边界。

## ABI 契约

宿主与插件之间只有 C 类型：NUL 结尾的 UTF-8 字符串装 JSON，加上一个不透明句柄。定义在 `crates/plugin-abi`，宿主与插件共用同一个 crate。

插件导出八个符号：

| 符号 | 作用 |
| --- | --- |
| `oext_plugin_abi_version` | 返回 ABI 版本，宿主在绑定其他符号前先校验 |
| `oext_plugin_create` | 接收 `HostVTable`，返回实例句柄 |
| `oext_plugin_register` | 返回身份、方法名与声明的能力 |
| `oext_plugin_call` | 调用一个方法 |
| `oext_plugin_emit` | 投递事件 |
| `oext_plugin_is_running` | 存活探测 |
| `oext_plugin_shutdown` | 跑清理钩子并释放实例 |
| `oext_plugin_string_free` | 释放插件分配的字符串 |

几条必须遵守的规则：

1. **谁分配谁释放。** 一个地址空间里有两个分配器。宿主用 `HostVTable::string_free` 释放宿主返回的串；插件用 `oext_plugin_string_free` 释放插件返回的串。跨界 `free` 会破坏堆。
2. **每个导出都要 `catch_unwind`。** Rust 1.81 起 `extern "C"` 里 panic 直接 abort，而这里 abort 的是宿主。用 `plugin_abi::guard` 包住即可，它把 panic 转成错误信封。
3. **信封只有两种形状。** `{"result": …}` 与 `{"error": {"code": …, "message": …}}`，不认识的额外字段忽略。

## 写一个 dylib 插件

```toml
[lib]
crate-type = ["cdylib"]

[dependencies]
originlang-ext-plugin-abi = { path = ".../crates/plugin-abi" }
```

`crates/plugin-abi` 提供类型、信封编解码与 `guard`。参考实现见 `originlang.extension.harness` 的 `crates/plugin`：八个导出、约 300 行，全部逻辑在库 crate 里，插件层只做 JSON 到函数调用的翻译。

## 第三方后端的预留

`RuntimeProvider` 从设计上就是多后端的，加 WASM 只需要新增一个 `crates/runtime-wasm` 并在 `oext` 装配时注册。宿主侧一次引入 wasmtime（约 5–8 MB），之后装载多少插件体积不变；沙箱、内存上限、trap 隔离都保留，ABI 用 wit 在编译期校验。

需要提前知道的一点：**沙箱里没有 socket**。所以任何需要访问外部 API 的插件，无论 `quickjs`、`wasm` 还是未来的形态，"发起 HTTP 请求"这项能力都必须由宿主提供并受能力门控。dylib 插件不受此限，因为它本来就在进程里。
