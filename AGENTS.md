# 项目范围与协作边界

本文件约束本仓库内的开发工作。修改对外契约时同步说明消费者需要的适配。

## 职责与交付

- Host 是扩展运行基础设施，负责安装、校验、授权、生命周期、后端路由、扩展存储、事件与定时任务。
- 本仓库交付 Rust 库及独立沙箱执行体 `oext-sandbox`，不提供第二套用户 CLI。
- 执行后端包括进程外 sandbox、进程内 QuickJS 和原生 dylib；明确区分资源限制、进程隔离与操作系统权限隔离。
- dylib 插件共享宿主地址空间，仅用于可信代码，能力授权不能强制约束其直接文件或网络访问。

## 向其他项目提供能力

```text
bootidea.cli --编译时依赖--> Host Rust 库
Host --运行时加载--> harness.dll / 其他扩展
HomeWorld --可选管理外部进程--> bootidea serve --内部使用--> Host 库
Host --按需启动--> oext-sandbox
```

- CLI 将 Host 库编译进 `bootidea`，负责命令、配置、进程入口和 JSON-RPC 传输适配。
- Harness 是独立发布的动态库插件。Host 不编译依赖 harness，也不内置其模型循环。
- HomeWorld 不直接链接 Host 库，不加载 Rust 动态库，不通过 FFI 调用宿主。
- HomeWorld 可下载 CLI 发布包并启动 `bootidea serve`，以外部进程方式使用 Host；是否安装或运行不影响 HomeWorld 基础功能。
- `oext-sandbox` 是执行体，不是完整宿主命令入口；CLI 发布包是否包含它必须按真实产物说明，不能混淆。

## 不属于本项目的工作

- 模型供应商适配、agent 循环和上下文策略属于 harness。
- 用户命令、终端体验及服务传输入口属于 CLI。
- 家庭账号、成员权限、资料库、网页和家庭业务数据库属于 HomeWorld。
- 对 ABI、工具协议、能力名称和包格式的变更应保留兼容性或明确迁移方案；各仓库保持可独立构建。
