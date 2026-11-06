# 原始 Wasm 的调试符号与源码标记

`debug-map`、`inspect` 和完整函数 diff 使用嵌入的调试辅助信息。这些是编译器提供的
线索，不证明源码身份、运行时状态或修改意图。原始 Wasm 仍是提交、对象哈希、补丁、
GC 和恢复的唯一内容来源；不会把解析结果存成 JSON。命令的 JSON 是对外输出，便于
编辑器、调试器或 WasmHub 将标记叠加在原文件上。

## 查询与交互

```sh
wasm-git debug-map app.wasm --json
wasm-git debug-map app.wasm --offset 173 --json
wasm-git debug-map app.wasm --code-offset 42 --json
wasm-git debug-map app.wasm --symbol calculate --json
wasm-git debug-map app.wasm --function 7 --json
wasm-git debug-map app.wasm --source /source/module.cpp --line 4 --json
wasm-git debug-map HEAD --repo ./history --file app.wasm --symbol calculate --json
wasm-git diff old.wasm new.wasm --compact --format json
```

数字使用十进制；一次使用一种查询条件。符号匹配原始 `name` 或 `linkage_name`，
返回全部候选。源码路径使用报告中的完整路径，不按 basename 猜测。源码行反查包含
该行的 line-table 范围和以内联调用点标记在该行的范围；不会改写实际 line-table 行。
不带 `--json` 输出可读文本。`--file` 用于选择仓库中的模块。

集成方应先核对 `module_sha256` 与所展示的 Wasm，再使用半开区间
`file_span: [start,end)` 标记原始字节。`code_span` 以 code section **payload**
的起点为零，不含 section ID/长度，也不是线性内存地址。`function_index` 包含
前面的导入函数；`instruction_span` 在查询指令立即数时仍指向包含该字节的实际指令。
`metadata_offset` 是 DIE 或符号条目在原文件中的位置。源码文件若有 DWARF5 MD5，
会原样列出 `file_md5`；本工具不打开源码验证该声明。

## 支持的解释

- `name` 函数名称；`linking` v2 函数、global、table、tag、data 符号名称。
  只有可确认的函数体提供代码范围，数据符号不会伪装成代码地址。
- 嵌入式 DWARF 2–5：编译单元、函数、词法块、参数/变量名称、声明位置、
  内联实例、调用位置，以及同一单元内 `abstract_origin` / `specification` 引用。
- `.debug_line` 的文件、行、列、discriminator、序列边界及可选文件校验和；
  `.debug_str` / `.debug_line_str` / `.debug_str_offsets` 字符串；
  `.debug_addr`、`.debug_ranges` / `.debug_rnglists` 的地址与不连续范围。
- `.debug_loc` / `.debug_loclists` 和内嵌位置表达式；`DW_OP_WASM_location` 的
  local、global、operand-stack 引用在 `wasm_slots` 中单独列出。
  `expression_hex` 保留原始表达式，`operations` 是静态解码，`evaluation` 始终为
  `not_performed`。空表达式标为 `empty_unavailable`，不假造变量值。
  位置列表的 `declared_code_range` 保留原始 PC 区间；实际标记范围是它与词法作用域
  的交集，避免把变量标到作用域外，也不因 LLVM 给出更宽的有效范围而丢弃整个列表。

`inspect.debug_info` 提供完整的已支持视图；diff 的 `debug_context` 给出两个输入的
摘要和哈希，`FunctionDelta.source_annotations` 对应 `operators` 中的改动序号，
分别列出旧、新字节位置的源码与内联候选。为避免放大报告，不在每个指令改动上
重复整套变量位置表达式。只改调试信息不会产生函数指令改动。

现有 `name` 名称优先；没有该名称时，唯一、覆盖整个函数体的 DWARF/linking 符号
可以作为匹配提示。仍遵守原有函数体/类型门槛、有限同名加分和歧义标记。
`function_symbol_sources` / `symbol_sources` 指出使用 `name` 还是调试符号后备来源。
同名不会使一个可能配对变成函数身份、调用目标或等价性的证明。

## 精度边界与保留策略

真实 LLVM 输出可能把 DWARF 起点放在 locals 之前的函数体起点；报告明确写成
`function_body_start`。其余起止地址必须是已解码的指令边界或完整函数终点，且在
同一个函数体内。越界、落在立即数中间、未知指令之后的未确认位置不会被取整或钳制。
序列终点不继承上一条源码行；line=0 不伪造源码归属；重叠候选全部保留。

编译优化可能把调用者的常量折叠进内联函数，并把指令归到内联函数的源码行。
报告同时保留内联调用位置，不将该行替换成推测的用户修改行。编译器剥离的信息
无法从符号中恢复；伪造或过期但结构自洽的调试信息仍只是未认证声明。

重复 DWARF section 名、截断、未知版本、循环引用和超出解释预算会明确诊断，
原始 custom payload 始终保留。有问题的 DWARF 单元不会导致仓库拒收模块。
尚未应用的 DWARF relocation 按 payload 的目标 section 判定，不只看 section 名；
此时不把待链接地址当作最终地址。

当前不应用目标文件的 DWARF relocations、不跨编译单元/外部文件解引用、不解释
完整 DWARF 类型系统；位置列表的 default-location 区间保留但不推测覆盖范围。
不获取 `sourceMappingURL` / `external_debug_info` 指向的文件，
也不执行位置表达式或调试 Wasm。仅有独立 line table 而没有编译单元关联时保持未解析。
存在外部引用时，报告明确说明只使用当前文件中嵌入的 DWARF。
原始 section 审计保留完整二进制差异；跨 section 的调试关系在独立调试视图中展示。
`decoded` 表示这里支持的映射已解码，不表示整个 DWARF 标准或所有 vendor 扩展都已解释。

解释预算为 128 MiB 调试载荷/累计单元、缩写表及行表输入、4,096 个单元、200,000 个记录、
2,000,000 次遍历/指令定位步骤，单个 DWARF/linking 解释字符串或位置表达式 16 KiB。
超限不会改变原始数据或存储格式。编译单元与函数布局共用现有线程池，默认使用
进程可用的全部 CPU；`--threads 1` 可复核确定性。源码路径复用内存，地址查询使用
区间索引，避免为每一处指令改动遍历整个调试表。

## 依据与复现

WebAssembly 社区的 [DWARF 约定](https://github.com/WebAssembly/tool-conventions/blob/main/Dwarf.md)、
[调试容器约定](https://github.com/WebAssembly/tool-conventions/blob/main/Debugging.md) 和
[linking 约定](https://github.com/WebAssembly/tool-conventions/blob/main/Linking.md)
描述工具间协作方式；这些不是 Core 的 custom payload 准入规则。
解析采用 native Rust / `gimli`，复用原有 Wasm 解码器。

`cargo test --all-features --test debug_symbols` 不需要外部编译器。
`tools/test-debug-symbols.py` 是显式执行的真实编译器对照：C DWARF2/3/4、C++ DWARF5、
Rust DWARF4/5、TinyGo，分别保留/移除 name section；检查运行结果、源码行或内联调用点、
线程数一致性和逐字节补丁恢复。源码位于 `tests/fixtures/debug_symbols/`。
脚本要求已有编译器与 Node、64 GiB 以下 cgroup，并把产物写入新的 `/tmp` 目录。
