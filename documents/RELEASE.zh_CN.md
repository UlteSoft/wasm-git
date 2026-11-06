# 发布工程与平台支持

**版本：** 2026-11-06 · [English](RELEASE.eu_US.md) · [文档库](README.zh_CN.md)

## 1. 发布内容与兼容性

GitHub Release 包含十个平台压缩包和 `SHA256SUMS`。原生包包含
`wasm-git` 客户端、`wasm-git-server`、Apache 许可证、项目说明和本双语手册；
WASI 包只包含客户端及上述文档。打包使用明确的文件白名单，不扫描整个构建目录，
因此不会带入测试仓库、生成的 Wasm、性能分析、下载工具、令牌或临时签名密钥。

原生发布目标为 Linux x86_64、AArch64、ARMv7 hard-float、RISC-V RV64GC
（静态 musl）；Windows x86_64/ARM64（MSVC，静态链接 CRT）；macOS x86_64/ARM64。
另有 `wasm32-wasip1-mvp` 和 `wasm32-wasip1-core2`。该清单表示发布验证范围，
不代表覆盖所有 CPU 或操作系统版本。工具链固定为 Rust 1.93.1；Linux 交叉编译
使用 cargo-zigbuild 0.24.1 和 Zig 0.15.2。Windows/macOS 发布包由对应操作系统的
原生 GitHub runner 构建并执行。

Windows 包无需额外安装 VC redistributable。打包时检查两个可执行文件的实际 PE
导入表，发现动态 CRT 依赖就拒绝发布。

Cargo 默认仍只构建客户端；`--no-default-features --features server` 只构建
服务端，`--features server` 构建两者。对象、压缩包和网络协议保持 uwgit 原生
二进制格式，不表示原生 Git 格式互通。

## 2. WASI Preview 1 的两个构建档位

两份 Wasm 都是仅导入 `wasi_snapshot_preview1` 的命令模块。MVP 与 Core 2.0
表示 **uwgit 可执行文件本身** 使用的指令范围，不限制被审计 Wasm 的功能范围。
两份构建共用完整的无损解析器和 custom-section 支持。

`tools/build-wasi.py` 使用仅对本次命令生效的 `RUSTC_BOOTSTRAP=1` 和
`-Zbuild-std` 重编译 Rust `std`、`panic_abort`；同时从 WASI SDK 27 对应的
确切 wasi-libc revision 重建 libc。`-mcpu=mvp` 配合禁用 Makefile 对内存复制
函数强制添加的 bulk-memory 编译参数，使用上游已有的标量 musl 实现。
Zstd C 代码也使用对应档位重新编译，链接时明确排除 Rust 预编译 libc。
最终完整文件必须通过独立 wasm-tools 的 `mvp` 或 `wasm2` 验证，并检查全部导入
模块。SDK、libc、验证器、runtime 下载包均校验固定 SHA-256。

在 x86_64 Linux 上准备 Python 3.11+、make、curl、tar，以及带 `rust-src`、
`wasm32-wasip1` 的 Rust 1.93.1：

```sh
python3 tools/build-wasi.py --profile mvp
python3 tools/build-wasi.py --profile core2
```

压缩包位于 `.tmp/dist/`。使用 Wasmtime 时，将工作目录预打开为 guest 根目录：

```sh
wasmtime run --dir .::/ wasm-git.wasm diff old.wasm new.wasm --compact
wasmtime run --dir .::/ wasm-git.wasm init repository
```

WASIp1 使用单个 worker；`--threads 0`、`--threads 1` 可用，大于 1 的显式请求
返回错误，不会尝试创建不受支持的线程。原生构建仍默认使用所有可用逻辑 CPU。
本 Rust WASIp1 目标不提供子进程、可移植网络监听/连接及 advisory file lock；
外部 SSH/GPG/curl/hooks、HTTP 服务、需要服务锁的选择性 fetch 和复制对象池操作
需要原生版本，不通过忽略锁来假装成功。发布测试验证离线 diff、section 筛选、
patch/apply、提交历史、分支、GC、完整性检查、bundle 导入和本地克隆的逐字节一致性。

WASI 路径通过 wasi-libc `realpath` 在预打开目录范围内解析符号链接，不能用纯字符串
归一化替代安全检查。缺失的进程 ID 仅在诊断和临时名称中记为 0；独占文件创建仍然
负责发布和写锁互斥。Windows/WASI 缺少本实现可移植使用的目录 `fsync`；文件数据在
发布前仍会同步，崩溃恢复机制保留，但目录项的断电持久性取决于宿主文件系统。
Unix 原生目录同步错误仍然向上传递。`version --build-options` 会报告宿主能力差异。

## 3. 测试与本机资源限制

每份发布构建运行 `tools/release.py smoke`。输入包含 64 个具名函数、一个嵌套
block 中的常量修改，以及超过 64 KiB 的 opaque custom-section 修改。断言包括：
准确识别一个函数改动；串行与默认模式审计证据一致；section 筛选有效；补丁还原
逐字节相等；拒绝覆盖已存在的导出文件；GC 不改变提交 ID；旧、新历史内容完整还原；
bundle 导入和本地克隆正确。原生包还启动独立 HTTP 服务端，通过客户端克隆已压缩
历史并检查还原结果。回归套件另外执行；smoke 不宣称穷尽所有指令或操作系统情况。

四种 Linux 发布架构均在 QEMU user-mode 中执行。这验证 CPU ABI 和 Linux 系统
调用行为，**不等于启动 Windows/macOS guest**。这两个系统的本机 QEMU 全系统
验证还需要可启动镜像以及 guest 登录/自动化配置；其原生 CI 结果必须单独描述。

在支持 systemd user session 的 Linux 主机上，以下命令强制设置 64 GiB 内存上限
与 0 swap：

```sh
python3 tools/cgroup-run.py -- cargo test --locked --all-features
python3 tools/cgroup-run.py -- python3 tools/release.py smoke \
  --label aarch64-qemu --runner-arg=qemu-aarch64 \
  --client .tmp/target/aarch64-unknown-linux-musl/release/wasm-git \
  --server .tmp/target/aarch64-unknown-linux-musl/release/wasm-git-server
```

wrapper 校验内核中的实际限制，记录峰值内存与 OOM 事件，只停止自己创建的临时
服务。构建、测试输入/输出、日志、缓存和下载工具链全部放入 `.tmp/`；必要的回归
源码和有意保留的小型 fixture 仍纳入版本管理。64 GiB 是本机测试限制，不是对
GitHub runner 内存大小的声明。

## 4. CI 与发布门禁

CI 检查格式、feature 目录、无警告 Clippy、完整 Linux 回归套件、client-only/
server-only 构建及整个平台矩阵。Windows/macOS 执行库和 CLI/server 回归以及发布
smoke；WASI 两份最终文件分别验证并在 Wasmtime 中执行。

推送 `vVERSION` 标签会触发相同门禁。全部 job 成功且标签与 `Cargo.toml` 版本一致
后，发布 job 只下载本次 run 的十份发布产物，生成校验和，并按仓库中的说明发布
GitHub **prerelease**。PR 构建只有只读权限，仅最终标签发布 job 有 release 写权限。
标签前运行 `tools/release.py inventory`，检查被跟踪的中间产物路径以及全部可达 Git
历史中至少 1 MiB 的 blob。大型有意源码需审查并调整规则；生成垃圾必须先从历史清除。

## 5. 参考

- [Rust WASIp1 目标说明](https://doc.rust-lang.org/rustc/platform-support/wasm32-wasip1.html)
- [Rust Wasm 特性与 std 重编译](https://doc.rust-lang.org/rustc/platform-support/wasm32-unknown-unknown.html)
- [WASI SDK](https://github.com/WebAssembly/wasi-sdk)
- [cargo-zigbuild](https://github.com/rust-cross/cargo-zigbuild)
- [GitHub 托管 runner 平台](https://docs.github.com/en/actions/reference/runners/github-hosted-runners)
