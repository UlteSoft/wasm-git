# wasm-git 开发与验证指南

**版本：** 2026-11-07 · [English](DEVELOPMENT.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 源码与产物规则

版本控制保留源码、锁文件、已审阅依赖补丁、可复现测试程序、必要小型 fixture 和长期文档。
可执行文件、编译输出、下载语料、性能采样、日志、运行 receipt、中间归档和随机样例统一
放到项目 `.tmp/`，该目录由 Git 忽略，不得 force-add。

`tests/fixtures/` 中两个 41 字节 Wasm 是有意保留的回归输入，C/C++/Rust/Go 源文件也是
输入，不是批量编译产物。历史逐轮运行报告不进入发布历史；方法文档应描述复现方式，而非
嵌入特定机器成功记录。

在源码根目录构建或测试前配置：

```sh
mkdir -p .tmp/target .tmp/temp .tmp/logs .tmp/cargo-home .tmp/pycache
export CARGO_TARGET_DIR="$PWD/.tmp/target"
export CARGO_HOME="$PWD/.tmp/cargo-home"
export TMPDIR="$PWD/.tmp/temp"
export PYTHONPYCACHEPREFIX="$PWD/.tmp/pycache"
```

也可使用已有可信 Cargo 缓存，但不能让生成文件进入跟踪源码。
需要较新 Rust、rustfmt、Clippy 与本机 C 编译器。开发基线为 Rust 1.93.1，未声明更低 MSRV。
保留根目录及 fuzz 两份锁文件，使用 `--locked` 固定依赖选择。

## 2. 资源约束

编译、测试和性能分析在预先准备的 cgroup 内运行。Linux 基线为 memory.max=68719476736
即 64 GiB、memory.swap.max=0。核对实际进程归属，不能仅证明某个受限容器存在。
检查继承限制并保存前后 memory.events。OOM 或外部中断不能视为成功完成。

E-core 测试应根据主机拓扑识别核心；适用 Intel 混合架构可读取
`/sys/devices/cpu_atom/cpus`，不能把一台机器的编号套到另一台。
每个独立 worker 固定一个请求的物理 E-core，记录实际 affinity。
进程只绑定一个 CPU 时，自动线程模式不等于真实 16-worker 测试，需另做 1 对 16 线程比较。

真实编译器脚本检查有效 cgroup 内存上限不超过 64 GiB，输出必须是本项目 `.tmp/` 下的新
目录，拒绝已有或逃逸路径。脚本不安装工具链、不创建 cgroup；父进程应按上节设置临时目录。

## 3. 必要构建与回归

```sh
cargo fmt --all -- --check
python3 tools/test-wasm-features.py
cargo clippy --locked --all-features --all-targets -- -D warnings
cargo test --locked --all-features
cargo test --locked --no-default-features --features client --test build_modes
cargo check --locked --no-default-features --features server --lib --bins
cargo fmt --manifest-path fuzz/Cargo.toml -- --check
cargo check --locked --manifest-path fuzz/Cargo.toml --bin raw_audit
```

这些检查分别验证共享实现、client 行为、server-only 编译和独立 fuzz workspace。
Fuzz 目标编译检查不是实际 fuzz 或 ASan 运行。Ignored 项只有在外部工具/语料齐备并明确
选取时才执行；分别报告 passed、failed、ignored、filtered。

Git 对照集成测试需要独立 Git 可执行文件支持 `update-ref --batch-updates`
（Git 2.50 或更新版本）。测试统一摘要宽度和缺失 OID 的输出写法，并探测
Git 2.52 修复的 dangling symref 保护。对于涉及该修复的两种情况，旧 Git
结果单独断言；wasm-git 始终执行引用存在性保护。两个新增引用名称互相冲突时，原生
批次与输入顺序无关地拒绝两者；对照程序也接受新版 Git 保留首个创建的结果，并
精确断言其引用状态。引用、index 与工作区的最终
状态仍须核对。wasm-git 可执行程序本身不调用 Git。

局部改动采用相关检查。常用 suite 包括 structured_diff_cli、call_dependencies_cli、
custom_audit_cli、section_diff_cli、debug_symbols、proposal_diff_cli、history_gc_cli、
pack_fork_cli、server_hosting_cli、server_expansion。跨层发布前适合执行完整回归。
除日志外还要保存进程退出码，单个 target 的 test result 行不代表整个 Cargo 成功。

## 4. 独立还原性质

审计验证器应从源文件和物理编辑独立重建目标，逐项检查有序范围、旧/新证据和所有未改间隙。
拼接未变旧字节与替换字节必须等于完整目标。检查正反两侧及补丁序列化往返。

第二个 producer 应绕过主要 diff 构造 whole-file replacement。
小输入可用独立二次动态规划插入/删除距离验证 exact_minimal=true。
只比较摘要不能验证报告位置，也不能发现解释展示错误。

仓库测试保存提交前原文件，在 GC、repack、fork、删除源、bundle、浅历史加深、部分补拉、
签名和对象池故障后恢复相关历史，不只检查 HEAD。
损坏测试应包括外层 checksum 正确但内部 delta 非法的 pack。

## 5. 结构意图与真实编译器

受控源码变化提供独立预期：常量、分支条件、函数增删重排、import 重排、export 改绑、布局
或元数据修改。已知时核对精确函数索引和符号。
优化可能将指令归到内联调用者或被调用者，应保留 DWARF 原证据，不强行改成预期源码行。

```sh
python3 tools/test-cpp-intent.py \
  --output "$PWD/.tmp/cpp-intent-run1" \
  --wasm-git "$CARGO_TARGET_DIR/release/wasm-git"
python3 tools/test-debug-symbols.py \
  --output "$PWD/.tmp/debug-symbols-run1" \
  --wasm-git "$CARGO_TARGET_DIR/release/wasm-git"
```

先构建 release client，并按各脚本 --help 配置编译器。
C++ 测试需要 Clang/LLD、本机 C++、Node 和 WABT；调试测试还需 Rust Wasm target、TinyGo，
覆盖 C/C++ DWARF 变体及带/不带 name 的 Rust/TinyGo 元数据。
这些是可选工具，不是普通库测试依赖。
运行结果与静态审计分开记录；运行时不支持某编码时，静态映射成功不能当成执行验证。

`tools/test-debug-languages.py` 增加真实 Objective-C/Objective-C++、MoonBit 普通 Wasm 与
WasmGC、AssemblyScript、普通 Go，以及可选的 Zig。提供已安装的 `--clang`、`--moon`、
`--moon-home`、`--asc`、`--node`、`--go`，可选 `--zig`，同时指定 `--wasm-git` 和全新的
`.tmp/` `--output` 目录；脚本不安装依赖。Objective-C 样例核对方法符号和受控方法修改，
不链接或执行 Objective-C 运行时。source-map 与 Zig 样例还会执行导出的测试函数。
所有样例核对原字节还原、单/多线程 diff 一致性。Go 样例将 Wasm 函数索引入口提升到虚拟 PC
坐标后，用 Go 自带的独立 `debug/gosym` 对每个源码范围的首末 PC 交叉验证。

`debug_language_formats` 覆盖 DWARF 语言编号（含未知编号）、所有 name 索引空间、补齐长度、
source-map 未映射边界/索引映射/data URL、外部文件绑定及失败行为，以及四代 Go pclntab。
额外定时测试先以 release 模式编译该测试，找到测试可执行文件，然后运行
`tools/cgroup-run.py -- python3 tools/fuzz-debug-languages.py --test-binary PATH
--output .tmp/debug-fuzz-1 --seconds 600`。需要可用的 16–31 号 E-core CPU，以及 64 GiB、
零 swap cgroup。16 个独立确定性 worker 覆盖十类变异；每例验证生成的 Core 模块合法性，
解析元数据、检查查询范围及双向字节补丁还原。记录种子、源码/二进制哈希、耗时、分组计数
与 cgroup 资源证据。使用 `-C instrument-coverage` 编译可采集各 worker 的 LLVM profile。
通过测试是针对这些输入的证据，不是对所有可能编译器输出的证明。

## 6. 随机与覆盖引导测试

现有 fuzz/fuzz_targets/raw_audit.rs 在 Core header 下测试任意成对 payload，包括非法内容，
验证精确证据、重建、标准配置物理一致性和独立补丁 producer。
fuzz/generate_seeds.rs 生成种子。真正 libFuzzer/ASan 运行需要兼容 nightly 与 cargo-fuzz，
其 target、语料、发现样例和日志显式放到 `.tmp/`。
旧远端包装脚本依赖特定环境，使用前必须审阅配置；本地项目产物规则优先。

**合法模块随机测试**是另一类实验。采用固定版本 wasm-smith/wasm-mutate 或确定性合法语法
生成器，先验证两侧再计入样本，丢弃候选单独记录。
覆盖独立模块、多阶段变异、受控指令修改、大 payload 插删、函数移动、已知 section 和
符合/不符合社区约定的 custom，并加入真实编译器模块。

要求 30 分钟/16 核时，各 worker 单调时钟有效运行至少 1800 秒，在 10/20/30 分钟保存监督，
检查 16 个 affinity、进度、失败、资源事件和必要特性/流程计数。
中断须明确记录，从持久化已完成 case 边界续跑，排除停机和重复计数。
不能把分段累计执行写成连续未中断执行。

记录生成器/工具链版本、源码与二进制摘要、种子公式、输入摘要、逐 worker 时长、覆盖计数
及失败原文件。覆盖 section 重放、custom/debug 范围、函数/块/指令解释、二进制历史、
压缩、网络、浅/部分还原、作者签名、pool/fork 保留。
分类计数证明类别被执行，不代表全路径/分支覆盖。
零观察失败只说明已完成样本通过，不证明任意输入正确或普遍推断意图。

## 7. 使用已有 VTune 分析性能

使用已安装的 Intel VTune，不重复安装。Release 保留调试行和 frame pointer，结果放 `.tmp/`。
以下假设官方路径存在：

```sh
RUSTFLAGS='-C debuginfo=1 -C force-frame-pointers=yes' \
  cargo build --locked --release --features server
/opt/intel/oneapi/vtune/latest/bin64/vtune -collect hotspots \
  -knob sampling-mode=hw -result-dir .tmp/vtune-hotspots \
  -- "$CARGO_TARGET_DIR/release/wasm-git" --threads 16 \
  diff .tmp/old.wasm .tmp/new.wasm --compact --format json
/opt/intel/oneapi/vtune/latest/bin64/vtune -collect uarch-exploration \
  -result-dir .tmp/vtune-uarch \
  -- "$CARGO_TARGET_DIR/release/wasm-git" --threads 16 \
  diff .tmp/old.wasm .tmp/new.wasm --compact --format json
```

使用新结果目录，并将命令/报告输出按需重定向到 .tmp/logs。
依据已安装版本帮助选择 knobs。硬件采样需要既有 PMU 权限；不可用时如实报告，不将软件
采样冒充硬件数据，也不随意更改主机安全设置。Hotspot 应映射到实际采样源码。

预热、交错 baseline/candidate、重复同等负载，报告中位数、输入大小、历史结构、线程数、
affinity 和内存限制。Retired instructions 按完成工作量归一化。
混合架构微架构百分比只在同 PMU/核心类型内解释。
先核对 audit/patch/history 逐字节一致，再评价速度变化。
分别测量完整 diff、compact、section、debug、GC，局部函数优化不等于端到端保证。

## 8. 特性生命周期与文档维护

src/features.rs 保留已实现 reader，src/feature_upstream.rs 保存审阅后的社区信息。
GitHub 定期工作流检查变化并生成候选，不删除 reader、不禁用历史编码、不自动合并，也不
自动实现新提案。上游 deprecated 时仍保留回归输入。

```sh
python3 tools/test-wasm-features.py
python3 tools/update-wasm-features.py --check
```

第二条会联网，可通过显式 input/revision 做离线固定版本审阅。
行为变化时同步更新 `.eu_US.md` 与 `.zh_CN.md`，保证命令、数值、字段和实现边界一致。
后缀是项目指定约定，不擅自把 `.eu_US` 改成 `.en_US`。

## 9. 发布卫生

提交前审阅暂存路径与大小、链接、示例及相关检查。
`.tmp/`、生成程序、日志、下载包、凭据和 profile 不进 index。
清理发布时通过 Git 对象清单与完整性检查覆盖全部可达历史；仅检查工作树无法发现历史删除
产物。

历史清理应精确列出路径，在未跟踪位置保存备份，在隔离 clone 中改写，对照最终源码树并
验证仓库。仅推送预期引用，使用 expected-old-tip lease，不使用无条件 mirror push，不覆盖
清理期间新增的远端工作。发布后新 clone 验证 tip、tree、对象和排除项。
托管平台缓存与独立旧 clone 不受分支重写的彻底擦除保证。
