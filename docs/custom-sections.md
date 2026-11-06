# Custom section 更新与符号提示

嵌入式 DWARF 的源码行、内联调用和变量位置描述，以及原始字节与符号之间的双向查询，
见 [调试符号与源码标记](debug-symbols.md)。`debug-map` 的结果可用于原始 Wasm 的审计标记。

原始 Wasm 字节仍然是存储、哈希、补丁、提交、GC 和恢复的事实来源。这里新增的是 Rust native 审计视图，没有新的中间存储格式，也没有 JSON 仓库存储。Sigstore 容器原本可能使用 JSON；只在识别该外部约定时读取其有界 envelope，原始字节保持不变。

## 不把约定当成准入规则

Core 规定 custom section 的外层 framing 和 UTF-8 名称；名称后面的数据可任意。名称匹配仅表示尝试一个解释器。无法按该解释器理解、格式版本不同、扩展字段未知或解释预算超出时，报告 `opaque` / `custom_opaque`，保留所有数据及位置，不拒绝 add、commit、diff、patch 或 GC。外层长度/名称编码本身损坏仍单独报告 Core 编码问题。

`decoded` 表示完成下表明确列出的视图，不代表整个社区格式经过语义验证、签名可信、运行时可以加载，或与所有同名实现一致。不会下载 source map、执行 AOT 内容、加载动态库或读取容器指向的外部资源。

| 名称 | 解释范围 |
|---|---|
| `name` | 现有固定 name 约定；函数符号另作有界、保守的匹配提示 |
| `producers`, `target_features` | 现有生产工具、版本与 feature 条目 |
| `sourceMappingURL`, `external_debug_info` | 单个长度前缀 UTF-8 地址；不访问地址 |
| `metadata.code.branch_hint` | 函数索引、函数体偏移和一字节 0/1 分支提示；提示是 advisory，不推断执行行为 |
| `dylink.0` | TLV framing；memory/table size/alignment、needed、export/import flags、runtime paths；未知子节独立保留原始范围及摘要 |
| `linking` | version 2 与 TLV framing；子节内容保留摘要和原始范围，尚不解释 symbol table 等细节 |
| `uwvm.wsea.v2` | 最多 16 MiB 的 canonical CBOR envelope；单份 v1 和 2–8 份贡献的 v2；逐份 COSE、certificates、CRLs 的范围、长度和摘要 |
| `wasm-cosign` | 最多 1 MiB 的 Sigstore JSON envelope 外形；不校验签名、证书或透明日志 |
| 其他（包括 `.debug_*`、`wasmedge`、任意未来名字） | 原始数据、SHA-256、精确字节编辑与物理位置；不声称理解 DWARF/AOT 内部字段 |

WSEA 的 envelope 解释仅检查外形、canonical CBOR 编码和非空字段，不等价于 wasm-cosign 的完整证据有效性检查。未知或不同社区布局使用同名 section 仍可被保存、更新和恢复。

## 变更对应与审计输出

`audit.custom_changes` 按名称分组，先按 payload 摘要查找，再逐字节验证相等内容。相等内容优先对应，避免在模块前插入 custom section 后把所有后续 section 的内容误认为修改。只剩一对同名 section 时报告 `remaining_same_name`；剩余多对不猜测对应关系，保留为带歧义标记的未配对条目。重复名字即使找到相同 payload，也不会声称建立历史身份。

每个变更包含：

- `old` / `new` 的 section 序号、完整物理范围、去掉外层名称后的 `data_span`、解释状态，以及完整 section 和 data 的 SHA-256。
- `content_changed`（包括 header 编码变化）、`data_changed`、`position_changed`。位置变化不等于用户主动移动；可能只是前面的内容变长。
- `pairing`、`ambiguous`，以及按 section 相对路径比较的 `decoded_changes`，字段仍附两侧原始字节位置。
- 若有关联签名约定，提供 `signature_input`。原始 byte edits 始终独立存在，负责全部变化的无损覆盖。

全量文本和 `--compact` 保留字段变化；`--short` 只显示每节摘要和字段变化数。JSON 是可选报告输出，不是存储格式。原有 `semantic_changes` 继续按物理 section occurrence 和条目索引解释；它与新的 custom 对应视图职责不同，不可把位置相同误认为身份相同。

## 签名输入变化，不冒充验签

`signature_input.verification` 固定为 `not_performed`。

- `wasm-cosign`：按参考实现的约定，移除所有名为 `wasm-cosign` 的完整 section，保留其余每个原始字节，然后计算约定输入的 SHA-256。
- `uwvm.wsea.v2`：只有单个、位于文件末尾且外层长度采用 canonical LEB 的容器才可排除；约定输入是其前面的原始模块。多份同名 section、非末尾位置、非 canonical 外层或 framing 不完整时，输入范围为未确定（`null`）。不存在该 section 时输入是完整模块。

复制了原签名但改动代码或其他已覆盖的 custom 数据，即使签名字节和位置没有变化，也会显示签名输入变化。修改签名自身而约定输入不变则分别记录。不能据此断言签名原本有效、现在无效或来自可信主体；这需要完整密码学和信任策略验证。

## `name` 函数符号如何影响匹配

完整函数体或 canonical key 相同、但存在多个候选时，先配对两侧全域唯一的同名函数，再按索引处理剩余候选；包括本次修改使函数体碰巧等于另一个函数的情况。它只改变同一 key 内的排序，不跨不同函数体覆盖强证据，也不覆盖 import/export 角色。所有重复候选及未配对剩余项的歧义继续保留。

在上述证据之后，剩余相似度候选排序增加最多 `0.03` 的同名加分。必须先满足相同函数类型和原有 `0.6` body similarity 门槛。报告中的 `similarity` 仍是未加分的函数体分数，`symbol_hint` 明示采用的未认证名称；文本区分同 key 候选排序和相似度加分。

只采用两侧各自唯一的非空函数符号。重复 name section、重复函数名 subsection、重复或倒序函数索引、截断/非法编码撤回整组提示；重复符号值不给加分，超出函数索引空间的条目不参与。可跳过未知但长度明确的 name 子节，保留已完整解码的函数名提示；全量 name 约定视图此时仍可能报告 opaque。已有函数体歧义不会因为名称相同而变成证明。

对整组相似度相同的函数，先在线性大小的索引中配对唯一名称，再处理剩余对应，不构造平方大小矩阵。一般候选继续受原有比较预算限制。名称不参与独立的目标实现身份证明。

## 性能实现与参考源码

审计比较现在借用已有 region path，对两棵排序索引进行一次合并，只为真正变化的路径分配字符串。去掉了旧实现两轮路径复制和第三棵去重树。符号提示、custom 解码和匹配均是当前 Rust 程序内的 native 代码；默认线程设置仍为所有可用逻辑 CPU，可用 `--threads N` 控制。

参考了本机这些实现，仅读取，未修改它们：

- `../wasm-cosign`，`1910b75fe597b5af5acbca230366da1ffe145162`：`pkg/wasm/usign.go`、`doc/wasm-signatures.md`、`pkg/wasmsecure/embedded.go`。
- `../uwvm2-ros`，基于 `ad0841780a55badbe356a696189f6f9fc6f46bc8` 的现有工作目录：`src/uwvm2/parser/wasm_custom/`、`src/uwvm2/uwvm/wasm/custom/handler.h` 的按名分派和 name 解析。
- `../wasmtime`，`b4b23fe583173131a047141947a785b4c1c646ae`：`crates/environ/src/compile/module_environ.rs` 的 name、branch hint、调试数据处理。
- `../WasmEdge`，`1cd04e4b35ebc86c937527f0146c2ec56fd701b3`：`lib/loader/ast/section.cpp` 与 `module.cpp` 的原始 custom 数据和 `wasmedge` AOT 处理。
- `/tmp/uwgit-custom-20261105/wamr`，`3a6fb9a8e7871f20471c48592390b9d2342287b1`：`core/iwasm/interpreter/wasm_loader.c` 的 custom/name/branch hint 分派与保留逻辑。
- 项目固定的 `wasmparser 0.249.0`：`readers/core/custom.rs`、`branch_hinting.rs`、`dylink0.rs`、`linking.rs`。解释的是这些实现对应的约定，未把运行时加载时的拒绝策略搬进审计器。

Core 3.0 的标准选择与全部已知 proposal 解码保持独立。未来未知 opcode 不猜测指令边界；只要外层 framing 可读，后续 custom section 仍可独立审计。更外层 framing 损坏也仍保留完整原始字节编辑和补丁。

## 2026-11-05 本机验证与性能记录

构建、回归、真实 C 编译/执行实验和性能采样均通过已有的
`/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/app.slice/uwvm-implementation.service/work`
运行：`memory.max=68719476736`（64 GiB）、swap 为 0，结束时 `max/oom/oom_kill` 事件仍均为 0。
Intel i9-14900HX 提供 32 个可用逻辑 CPU；默认 `--threads 0`，同时核对 `--threads 1`。
原有 `/opt/intel/oneapi/vtune/latest/bin64/vtune` 用于采样，没有重装 VTune。

验证结果：118 个测试目标合计 **1,383 passed、0 failed、11 ignored**；其中本轮 custom 专项 21 项。
`clippy --all-features --all-targets -- -D warnings`、格式检查、默认 client 和 server-only 检查通过。
11 项忽略是原有显式忽略测试，不计入通过数。

真实 C 实验使用现有 Clang 21，`--target=wasm32 -O2 -g -nostdlib -fno-inline`：
重新排列 `alpha/beta` 的定义，再将 `alpha` 的常量 11 改为 12。
审计显示 `alpha` 的一处常量替换，以及 `entry` 的两处原始 call index 替换。
`beta` 的目标有完整函数体证据，标为 `same_target_index_relocated`；
修改后的 `alpha` 由相似函数体加同名提示配对，调用目标仍保留 `unresolved_target`，不把符号提示升级成实现身份的证明。
`.debug_info/.debug_line/.debug_str/name` 的更新分别可见。
Wasmtime 实际执行 `entry(3)` 得到 35 → 36；单/多线程报告一致，二进制补丁恢复与新文件完全相同。
另外，12,000 个同类型、反向排列且函数体均改变的命名函数逐个核对了对应关系和补丁恢复，所有原函数体歧义标记仍保留。

基准比较本轮开始前的二进制与最终构建。隔离审计阶段为 7 轮、每轮 2 次的中位数；
CLI 为 5 轮、随机交替顺序的端到端中位数（`--short`），包括读文件、分析与输出。
构建与测试不和采样并行，但主机仍有其他任务，所有原始样本均保存；不把这些有限输入的结果推广为任意 Wasm 的加速保证。

| 场景 | 修改前 | 修改后 | 说明 |
|---|---:|---:|---|
| 嵌套控制流：仅物理/结构审计，默认线程 | 0.5972 s | 0.5343 s | 约减少 10.5%，原始编辑及位置解释数量相同 |
| 嵌套控制流：完整 CLI | 1.5567 s | 1.4281 s | 约减少 8.3% |
| 800 个小函数：完整 CLI | 0.0529 s | 0.0572 s | 增加约 4.4 ms，不能声称所有小输入都加速 |
| 实际 Lua Wasm：完整 CLI | 1.1018 s | 1.0932 s | 近似持平 |
| 4 MiB custom 数据：完整 CLI | 0.9109 s | 0.8189 s | 峰值 RSS 中位数 575.4 → 513.4 MiB |
| 12,000 个命名函数：完整 CLI | 0.7605 s | 0.6961 s | 新增正确的同名排序提示；RSS 216.6 → 225.3 MiB |

VTune hardware hotspots 对相同嵌套样本各运行 20 次，均报告 247,720 个原始编辑组、983,100 个位置解释变化：
CPU Time **15.585 → 14.360 s（约减少 7.9%）**；硬件采样估算 Instructions Retired
**223.114 → 204.761 billion（约减少 8.2%）**。两侧都保存了 function 与 source-file/source-line 报告。
分配/释放与内存比较仍是主要热点；这次减少路径复制和第三棵索引树，未削减审计数据的覆盖范围。

最终构建另采集 microarchitecture exploration：P-core Retiring 52.6%、Front-End Bound 14.9%、
Bad Speculation 5.1%、Back-End Bound 27.4%。这些是该 PMU 的采样估算；E-core 指令样本较少，
不将其百分比与 P-core 相加，也不把不同采样模式下的 CPU 时间直接拼接。
系统的 kernel symbol 权限限制保留，项目源码行可归属；没有为采样改动系统安全配置。

本机复现与证据位于 `/tmp/uwgit-custom-20261105/`：
`source-before/`、`source-final/`、`source-final-manifest.json`、`custom-audit-changes.patch`、
`benchmark.py` / `benchmark.json`、`vtune-run.py` / `vtune-runs.json` / `vtune-metrics.json`、
`before-source.csv` / `after-source.csv`、`after-uarch.csv`、`native-check.py` / `native/results.json`、
`scale-check.py` / `scale-results.json`、`full-delivery-tests.log`、`test-results.json`。
`after-bin/wasm-git` 和 `after-bin/wasm-git-server` 是最终 release 产物。
