# uwvm2-ros VM 指令扩展

[English](UWVM2_ROS_EXTENSIONS.eu_US.md) · [文档库](README.zh_CN.md)

## 二进制契约

私有主操作码 **0xFF** 的名称为 **uwvm2-ros VM 指令扩展**。统一外层格式为：
`FF ULEB32(扩展指令编号) 由该编号定义的操作数字节`。

编号是一个完整无符号 32 位数，**没有统一位域、隐含版本、子域、memory index、
操作数数量或负载长度字段**。未来非 WasmMTE 指令可以使用完全不同的操作数。
必须先匹配已登记的完整编号，再读取其操作数。

ULEB32 遵循 WebAssembly 无符号 LEB：最多五字节，每字节从低到高携带七个值位，
第五字节不大于 0x0F，接受长度合法的非最短填充。这不是本机字节序的 u32 内存
转储，也不是 wasm-git 命令协议的大端整数。审计、补丁和存储不交换原始 Wasm
字节序，也不自动规范化编码。

未知编号的操作数长度未知，从该指令开始停止解码，当前表达式余下部分保留为不透明
字节；不能根据其中看似 end、FF 或标准操作码的字节重新猜测边界。独立外层边界内
的后续函数与段仍可审计。已证明字段起点的 LEB 截断／溢出是已知编码错误；未知编号
不能授权读取后面的 memoryidx。标准 FB/FC/FD/FE 语法不变。本扩展不是已批准的
Core 特性，core1/core2/core3 均不启用其指令。

## WasmMTE 指令登记

下列完整编号登记 19 条 WasmMTE 指令。其历史分配形似版本／域／操作字段，**但这
不是所有编号的通用解释规则**。每条指令独立规定一个 ULEB32(memoryidx) 立即数，
memory64 的此立即数仍为 u32。所有栈操作数和结果均为选定 memory 的地址类型：
memory32 为 i32、memory64 为 i64。输入按从深到浅排列。n 是无符号**字节长度**，
不是 granule 数；q 提供源指针 tag；t 为 tag 或 tag offset。

- `0x01010020` `memtag.status`: `[] -> [mode]`.
- `0x01010021` `memtag.extract`: `[p] -> [tag]`.
- `0x01010022` `memtag.insert`: `[p, t] -> [p']`.
- `0x01010023` `memtag.tagbits`: `[] -> [tag_bits]`.
- `0x01010024` `memtag.startbit`: `[] -> [start_bit]`.
- `0x01010025` `memtag.copy`: `[p, q] -> [p']`.
- `0x01010026` `memtag.sub`: `[p, q] -> [difference]`.
- `0x01010027` `memtag.load`: `[p] -> [p']`.
- `0x01010028` `memtag.untag`: `[p] -> [untagged_p]`.
- `0x01010029` `memtag.untagstore`: `[p, n] -> [untagged_p]`.
- `0x0101002A` `memtag.untagstorez`: `[p, n] -> [untagged_p]`.
- `0x0101002B` `memtag.store`: `[p, n] -> []`.
- `0x0101002C` `memtag.storez`: `[p, n] -> []`.
- `0x0101002D` `memtag.random`: `[p] -> [p']`.
- `0x0101002E` `memtag.randomstore`: `[p, n] -> [p']`.
- `0x0101002F` `memtag.randomstorez`: `[p, n] -> [p']`.
- `0x01010036` `memtag.hint`: `[p, q, t] -> [p']`.
- `0x01010037` `memtag.hintstore`: `[p, n, q, t] -> [p']`.
- `0x01010038` `memtag.hintstorez`: `[p, n, q, t] -> [p']`.

0x01010033（memtag.add）**保留并拒绝**，不杜撰算术和栈语义。未登记编号（包括
保留的 GC 域编号）保持未知。识别线性内存指令不代表实现 guest GC tagging。

独立字节样例：

```text
memtag.status memory 0: FF A0 80 84 08 00
memtag.store memory 0: FF AB 80 84 08 00
memtag.hintstorez memory 1: FF B8 80 84 08 01
```

参考执行 ABI 中，memory32/64 的指针 tag 占高 4/8 位，allocation tag 按 16 字节
granule 管理。load 读取 allocation tag，不是数据 load。tag-store 覆盖与范围相交
的每个 granule，z 形式只清零 n 个数据字节。零长度 random-store 仍可能消费熵、
改变返回指针 tag；disabled lowering 保留四种 z 指令按普通边界检查清零数据的
效果。这些语义用于解释审计影响，wasm-git 不执行它们，也不认证运行时边界、
owner、熵源、硬件 MTE 模式或并发安全。Unsafe-path 分析把 allocation-tag load
和所有 tag-store 形式列为内存访问风险点。

## Globals metadata 约定

仅为 uwvm.memtag.linear.globals 名称提供 allocation metadata 字段视图：

```text
ULEB64(2) ULEB64(count)
重复 count 次：
    ULEB32(memoryidx) ULEB64(address) ULEB64(byte_length)
```

ULEB64 最多十字节，第十字节只允许最低值位，接受合法填充。在 32 位宿主上地址和
长度仍保留 u64。允许空列表与空 descriptor。用受检加法防止 address + byte_length
溢出后被展示为合法范围。版本、数量、每项 memory index／地址／字节长度均有独立
字段及原始范围。不验证运行时 canonical owner 或 allocation overlap，也不因为
名称相同就认证生产者。分配视图前限制到 100,000 项及实际负载可容纳数量；超过解释
预算的段仍完整存储并支持字节 diff。

Custom section 是社区约定。版本不同、截断、尾随字节、范围溢出或预算超出时，
给出“不符合此格式”的诊断并保留不透明视图，**不会仅因不符合 custom 约定拒绝整个
Wasm**。旧 memtag.globals 和旧指令格式不保留兼容解码／类型验证实现，其原始字节
与其他未知数据一样交给通用不透明审计。

## 审计与后续维护

memtag_extension 在 all-known 中提供类型化识别，在标准 Core 配置中显示已识别
但未启用的证据。函数 IR 保留所有 memory 立即数。操作码或 memory 重绑定变化进入
函数／基本块／指令 diff，并带原始偏移与字节证据；仅填充变化保留为字节修改，不
伪造函数体变化。Globals 字段支持 custom diff 与
`--section custom:uwvm.memtag.linear.globals`。二进制补丁、历史、GC、bundle 与
clone 保留精确字节及未知后缀。`--format json` 和全局 `--format binary-v1` 是应用
输出视图，不改变内部对象存储。

新增扩展须分配未占用的完整 u32 编号，逐条记录操作数编码、栈类型、特性开关和失败
行为，加入各自的解码分支与类型化 IR。不得改变已有编号的语法，或把 WasmMTE 的
memory index 强加给其他扩展族。不兼容修订分配新编号，启用保留编号须明确设计。
中英文规范、字节样例、异常输入、来源记录及发行测试同步维护。原始 custom／字节
证据可能包含私有内容；解释过程不跟随嵌入 URL，也不访问运行时 owner。

WasmMTE 映射与 globals 布局对照用户提供的 2026-11-08 设计 README，SHA-256：
48492664d0e9fa2ffabbc600b3efeb33863f83dfd46ce7a74c6e9edab765db24；并按后续指示
统一为通用 u32 编号空间。未经迁移补丁修改的外部 LLVM mt-2 producer 使用已废弃
编码，不能直接生成本格式。
