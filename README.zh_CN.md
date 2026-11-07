# wasm-git

[English](README.eu_US.md) · [文档目录](documents/README.zh_CN.md)

wasm-git 是原生 WebAssembly 审计与版本历史系统。它直接保留原始二进制，解释更新时不经过
WAT 往返转换，也不使用规范化表示替换原文件。命令行客户端名为 `wasm-git`，可独立选择
构建的服务端名为 `wasm-git-server`。

## 提供的能力

- 带原始偏移与上下文的精确字节证据，以及校验源文件和目标文件 SHA-256 的可逆二进制补丁。
- section、函数、控制块、指令、操作数、调用绑定和导出依赖的结构化比较，明确报告歧义与分析不完整。
- custom section 更新比较、已识别社区约定的有界解析，以及供审阅参考的 DWARF、name 和 linking 符号映射。
- 原生内容寻址提交与命名模块树、分支、标签、暂存、合并与历史重放、多工作树和离线 bundle。
- 并行 Zstandard 压缩、有界二进制 delta、历史 GC、硬链接 fork、跨 pack 基底、浅克隆、部分克隆和按需补拉。
- HTTP 与 SSH 传输、仓库和引用授权、原子引用批次、接收 hooks、作者签名和不可变对象池复制。
- SSH/OpenPGP 提交签名、跨 fork 持久化 PR、审批与 CI 门槛、分支保护，以及可恢复的
  快进/merge/squash 发布。见 [WasmHub 手册](documents/WASMHUB.zh_CN.md)。

[白皮书](documents/WHITEPAPER.zh_CN.md)说明证据模型和实现边界；
[使用手册](documents/USER_MANUAL.zh_CN.md)介绍日常操作；
[服务端手册](documents/SERVER_ADMINISTRATION.zh_CN.md)介绍部署、运维与恢复。

## 本地网页版

独立 Python 界面支持 English / 简体中文切换、仓库审阅与操作、完整原生命令入口，
并提供单独的服务器监管入口。项目工作台可打开、创建、克隆和切换仓库，
在同一视图中审阅暂存／未暂存改动、Wasm 证据并提交。未找到原生程序时会自动构建。

应用可在命令前选择 `--format json` 或 `--format binary-v1`。GUI 使用版本化
二进制协议并筛选展示字段；原始报告与命令流需要主动查看或导出。参见
[接口、字节序与隐私边界](documents/COMMAND_PROTOCOL.zh_CN.md)。

```sh
python3 webgui/client.py start --open
python3 webgui/server.py start --root /path/to/hosting-root --open
python3 webgui/client.py stop
python3 webgui/server.py stop
```

详见 [Web 界面手册](documents/WEBGUI.zh_CN.md)：程序选择、审计预览、签名/PR
表单、服务管理、限制和启动关闭。

## 构建客户端、服务端或两者

需要较新的 Rust/Cargo，以及用于编译 Zstandard 依赖的本机 C 工具链。
开发验证指南采用 Rust 1.93.1 为基线，不承诺更低的最低版本。
依赖通过锁文件固定，仓库包含经过审阅的 `wasmparser` 源码。

```sh
# 默认只构建客户端。
cargo build --locked --release

# 只构建服务端。
cargo build --locked --release --no-default-features --features server

# 从同一个共享库构建两个可执行文件。
cargo build --locked --release --features server
```

默认客户端不包含监听服务模块。同时启用两种 feature 时，也可使用 `wasm-git server`。
SSH 默认调用远端安装的独立 `wasm-git-server`。HTTP(S) 客户端依赖 `curl`，SSH 客户端
依赖兼容 OpenSSH 的 `ssh` 可执行文件。

## 审阅更新

```sh
wasm-git inspect module.wasm --json
wasm-git diff old.wasm new.wasm
wasm-git diff old.wasm new.wasm --compact --format json
wasm-git diff old.wasm new.wasm --section code --section data
wasm-git diff old.wasm new.wasm --section custom:producers
wasm-git debug-map module.wasm --symbol calculate --json
```

`--threads 0` 使用进程可用的逻辑 CPU；1–256 的显式值控制共享原生线程池。
全局选项放在命令前，例如 `wasm-git --threads 4 diff old.wasm new.wasm`。
`--compact` 保留所有字节编辑，省略较大的解释树和普通已支持特性的逐项证据。
`--short` 是摘要，不是可用于还原的产物。

section 选择器可重复，选择顶层物理 section 的并集。筛选报告不是整模块补丁，也不重新
计算跨 section 的调用、导出或签名输入依赖；这些问题应使用完整 diff。

```sh
wasm-git patch old.wasm new.wasm --output change.wgp
wasm-git apply old.wasm change.wgp --check
wasm-git apply old.wasm change.wgp --output restored.wasm
```

补丁和导出的目标必须是新文件。应用补丁时先核对源字节、编辑范围、旧字节证据和最终摘要，
完整验证成功后才发布输出。

## 保存和交换历史

```sh
wasm-git init ./history
wasm-git commit ./history old.wasm -m 'Initial module'
wasm-git commit ./history new.wasm -m 'Update calculation'
wasm-git log ./history HEAD
wasm-git diff HEAD~1 HEAD --repo ./history --format json
wasm-git checkout ./history HEAD restored.wasm
wasm-git fork ./history ./independent-fork
wasm-git gc ./history
wasm-git fsck ./history
```

保存模块树时，将 Wasm 文件放在仓库目录内，通过 `add` 暂存相对路径，提交时不传单模块
位置参数。本地 fork 在同一文件系统上共享不可变 pack，但拥有独立引用；删除源路径不会
删除 fork 的硬链接。`gc` 默认压缩并保留对象；删除需要显式 `--prune=now`，且受保留根约束。

## 部署仓库服务

```sh
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419
```

先在服务根目录下建立仓库。HTTP 默认只读；写服务需要显式 token 或编译后的授权策略，并
启用 `--allow-write`。对公网提供 HTTPS 时使用反向代理。SSH 使用系统账户认证，可通过
forced command 限定访问。启用写入、签名作者策略、共享对象池或引用重写前，请阅读部署手册。

## 兼容性与保证范围

存储、压缩、传输和补丁保留被接受的 Wasm 原字节，包括未知扩展与 custom payload。
Core 结构分析基于固定语法，提供 `core1`、`core2`、`core3` 和 `all-known` 配置。
`wasm-git features --json` 区分保留的语法、部分投影、社区约定与上游生命周期信息。
弃用编码继续识别；提案阶段本身不能证明实现已经完成。

Component Model 支持原字节与已知外层 section framing，不提供完整组件语义。
不符合某个已识别社区约定的 custom payload 保持 opaque，不因该原因拒绝。
符号与相似度是审阅证据，不是源代码意图、作者身份或行为等价的证明。

仓库内部使用原生二进制格式；JSON 用于用户报告或明确的外部配置导入。
wasm-git 不读取 Git 对象库、不使用 Git 网络协议，也不迁移已废弃的发行前格式。
未支持的 Git 选项与资源边界详见手册。

## 开发

参阅[开发与验证指南](documents/DEVELOPMENT.zh_CN.md)。所有生成文件和测试输出放在
`.tmp/`，不得提交该目录。有限测试只能描述所记录输入的实际结果，不能证明所有可能或未来
Wasm 模块都正确。

## 发布工程

请参阅[平台与发布手册](documents/RELEASE.zh_CN.md)，了解原生与 WASIp1 构建、CI/CD 门禁、QEMU 测试及能力边界。
