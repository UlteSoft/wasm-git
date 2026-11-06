# uwgit 二进制格式参考

**版本：** 当前发行前实现，2026-11-06
[English](BINARY_FORMAT.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 范围与版本策略

本参考描述当前 checkout 写入的格式，不承诺未发行布局永久稳定。
仓库标记为八字节 `UWGRPO\0\x03`。旧 JSON 仓库、文本控制文件与已废弃发行前 pack 不迁移，
而是明确拒绝。读取器必须核对版本、完整记录、语义约束和引用图，仅识别 magic 不够。

可读对象 ID 和十六进制编辑证据属于展示。
规范对象与热索引引用使用原始 32 字节 SHA-256。路径、消息等必要 UTF-8 字符串是二进制
记录内的长度前缀字段，不代表仓库采用文本或 JSON 文档存储。

存在两类编码：对象/pack/patch 显式 framing 使用定长大端整数；冷状态与 RPC metadata
使用第 6 节的位置记录。不得混用两类整数规则。

## 2. 对象基本编码

以下 `u32be`、`u64be` 为无符号大端整数；`oid` 是原始 32 字节；`bytes` 为 u32be 长度加
对应原字节；`string` 同样编码并要求合法 UTF-8。
数量必须适配字段，且剩余输入足以容纳最小记录。溢出、截断、尾随字段、错误顺序和标志拒绝。

### 2.1 Envelope 与身份

```text
8 bytes   magic = UWGO\0\0\0\x02
1 byte    kind
u64be     payload length
N bytes   payload
```

对象 ID 对整个 envelope 求摘要，包括 kind 和长度。Kind 1 为 Wasm，2 为 commit，3 为命名
树。验证、规范化、压缩或签名展示均不重写 Wasm payload。
被接受的头后可以是 opaque 未支持编码；解释状态与存储身份分离。

### 2.2 Commit payload

字段依次为：

1. Layout 字节：1 为单模块，2 为命名树。
2. 目标 oid：由布局决定为 Wasm 或 tree。
3. 父数量 u32be，随后为按声明顺序保存的父 oid。
4. Message string。
5. 显式路径标志：0 无，1 后跟 path string。
6. 签名标志：0 无，1 后跟 bytes 字段，其中是规范 AuthorSignature 位置记录。

布局/路径组合还需语义检查。父顺序决定 merge/mainline 含义。
拒绝重复或非法父、错误目标类型、非法路径和必要图缺失。
本地 shallow 标记不写入提交，不改变真实父 ID 或对象 ID。

签名记录依次含 version、author、timestamp、32 字节公钥和 64 字节 Ed25519 签名。
被签内容为用途域 `uwgit.author.commit.v1\0`，随后是显式 string 作者、u64be 时间戳、
公钥、长度前缀的规范未签名 commit payload；该未签名形式不带签名内容。
严格密码验证与可信作者绑定是独立操作。

### 2.3 命名树 payload

Tree 为 u32be 数量，随后按 UTF-8 路径字节严格升序的 path string / target oid。
空树为零数量。拒绝重复/逆序、非法路径、文件/目录前缀冲突、未承诺的缺失目标和错误对象类型。
不包含目录对象、POSIX mode、任意 Git blob 或符号链接条目。

## 3. Bundle 与选择性 manifest

```text
8 bytes   magic = UWGB\0\0\0\x02
u64be     manifest byte length
N bytes   manifest
u64be     number of objects
repeated  oid, u64be envelope length, complete object envelope
```

对象按 ID 严格排序。Manifest 包含 layout 字节、HEAD identity string、排序分支与标签表，
然后是排序 shallow 边界与 promised 对象 ID 集合。
Map 为 u32be 数量加 name string / oid；集合为 u32be 数量加原始 oid。
Layout 1–4 对应支持的单模块、tree、tag、detached HEAD 组合，并结合对象图验证。

Promise 表示缺少 Wasm 内容，不虚构 envelope；浅边界保留真实父关系。
缺失对象的来源是本地配置，不接受 manifest 任意引入 URL。
完整发布在允许时使用接收方已有对象补齐闭包；安装顺序为 Wasm 在 tree 前、父在子前，
最后发布选定引用。

## 4. 不可变 pack

### 4.1 外层布局

当前 pack 为 `UWGPACK4`，文件名为整个文件的 SHA-256。
内嵌索引与数据通过一个不可变目录项发布：

```text
8 bytes   magic = UWGPACK4
u32be     object count C
1024      256 cumulative u32be fanout values
32 * C    strictly sorted raw object IDs
72 * C    corresponding fixed-width index records
32        SHA-256 of magic, count, and complete index
remaining compressed payloads
```

索引最多 64 MiB，单个还原对象最多 4 GiB。Fanout 单调、终值为 C，且精确描述 ID 表。
ID 顺序与物理压缩数据顺序独立。

每个 72 字节记录依次为 offset u64be、compressed_len u64be、raw_len u64be、base u32be、
payload_len u64be、crc32 u32be、32 字节 external base ID。
偏移相对压缩数据起点，各项必须完整覆盖数据，不允许重叠、空洞或尾随。
CRC32 覆盖对应压缩项。

### 4.2 完整对象与 delta 基底

`base=0xffffffff` 为完整对象，解压结果是完整 envelope，payload_len=raw_len。
普通索引值指同 pack 基底，其物理位置必须先于依赖项。
`base=0xfffffffe` 指外部对象 ID 基底；只有外部基底记录的 external 字段非零。

写入器使用保留共享 pack 中完整 Wasm 作跨 pack 基底。
读取器仍检查跨文件完整依赖图，包括缺失、环和总深度最多 8。
引用外部基底的 pack 不独立可移植，需同时提供基底对象。

### 4.3 压缩与指令

每项恰好一个 Zstandard frame。Delta 解压后得到有界指令流，其长度 payload_len 最多为
raw_len+5。指令使用显式大端编码：

- Tag 0：insert，随后 u32be 字节数与原始字节。
- Tag 1：copy，随后 u32be 基底偏移与 u32be 字节数。

拒绝零长度、未知 tag、截断、越界 copy、拼接 frame、尾随内容和还原长度不符。
最终 envelope 必须匹配索引对象 ID，索引 checksum 与 CRC 不能代替对象验证。

当前写入器采用 Zstandard level 6、共享原生 worker、有界历史/路径候选、完整锚点和
有限 delta 预算。压缩参数是表示选择，不进入对象身份。Prune 必须保留所需基底，或先写出
可独立恢复的新表示。

## 5. 离线字节补丁

```text
8 bytes   magic = UWGPAT01
32 bytes  SHA-256 of the complete original file
32 bytes  SHA-256 of the complete target file
u64be     original length
u64be     target length
u32be     edit count
repeated  old start/end u64be, new start/end u64be,
          original span bytes, target span bytes
```

Span 长度决定原始字节长度，不保存 hex/JSON。
摘要指原 Wasm 文件，不是仓库 envelope。
内存报告 format 为 `uwgit-byte-patch-binary-v1`，wire format 中不额外保存该字符串。

验证单调范围、未改间隙长度、旧字节相等、总长度与目标摘要。
补丁有效不表示结构解码完整、语义等价或编辑全局最小。
完整验证后才原子发布新目标文件。

## 6. 规范位置记录与控制文件

冷记录以 `UWGM\0\0\0\x01` 开头，按 schema 字段顺序使用 bincode 1.3.3 规范变长整数。
0–250 单字节；251/252/253/254 在 schema 允许时分别引出 little-endian u16/u32/u64/u128。
有符号值遵守 codec 的有符号映射；bool/Option 标志、枚举编号、集合长度与字段顺序由确切
Rust schema 决定，不是自描述 map 格式。

解码预算受输入长度限制，拒绝尾随字节；重新编码必须与原输入完全一致，从而拒绝非最短
整数、非规范 map 顺序和重复键编码。Schema 变化必须审阅并版本化；字段名虽然不落盘，
类型变化仍可能改变持久字节。

Index 外头为 `UWGITIDX\x06`，后跟当前位置记录。
Config、index 图像、stash、pending/rebase、reflog、worktree、事务 journal、部分历史、
pool、trust、upload 状态均为二进制。部分冷恢复字段中的 UTF-8 仍是 schema 值，不是 JSON。

仓库标记 `UWGRPO\0\x03` 对应带时间戳的 reflog 和事务前像。拒绝 version 2 仓库，
不迁移尚未发行的旧格式；不可变对象 ID 与 pack 编码不变。
Reflog 条目字段依次为 `unix_seconds: u64`、`sequence: u64`、`reference: String`、
`old: Option<String>`、`new: Option<String>`、`operation: String`。
序号必须为正且严格递增，过期后允许空隙，追加从最后保留的序号继续。
引用事务及 HEAD 图像在持久决定前冻结时间戳，恢复时不替换为恢复时刻。
本地 HEAD 日志最多 4096 项；共享引用日志通过显式过期清理。

HEAD/ref 采用 `UWGREF\0\x01` 加 tag：1 为原始 32 字节 oid；0 为长度前缀分支名，仅允许
symbolic HEAD。Writer 锁为 `UWGLCK\0\x01` 加大端 PID，PID 不授权自动删除锁。
Worktree commondir 用位置记录。私钥为 `UWGKEY01` 加 32 字节种子，公钥为 `UWGPUB01`
加 32 字节公钥。

## 7. RPC 与协商对象流

RPC 为 `UWGRPC02`、metadata 长度 u32be、payload 长度 u64be、位置 metadata 和原 payload。
Metadata 最大 64 KiB，完整 packet 最大 64 MiB；传输 slice 为 1 MiB，bundle 上限 4 GiB。
Envelope 版本与 metadata 内逻辑请求 version 是不同概念。

外层 operation 按声明顺序编号：Advertise=0、Fetch=1、FetchSlice=2、UploadSlice=3、
ApplyUpload=4、Fork=5、Push=6、UpdateRef=7、RefsPage=8、CancelUpload=9、
PublishBatch=10、ObjectSlice=11、PublishOptions=12、CheckBatch=13、PublishPartial=14。
PublishPartial 有一个布尔字段 dry_run。上传内层使用独立非递归枚举 Push=0、UpdateRef=1、
PublishBatch=2、PublishOptions=3、CheckBatch=4、PublishPartial=5，不可嵌套上传。
确切位置字段见[协议类型](../src/protocol.rs)，不得猜测必要 Option 字段可以省略。

PublishOptions、CheckBatch 和 PublishPartial 的 payload 为位置编码的
`OptionsPublication { publication: BatchPublication, options: Vec<String> }`。
BatchPublication 依次保存有序 updates 与 bundle 字节。批次成功响应的 tip 是精确 payload 的
SHA-256。部分发布另返回 `PartialReceipt { rejections: Vec<Option<String>> }`，与请求逐项
对应：None 表示接受，Some 表示拒绝原因。数量不符或回执损坏不能确认发布结果。

协商压缩流为 `UWGNET01`、总解压长度 u64be，随后重复 u32be frame 长度及 Zstandard frame。
除最后一块外，各块解压恰好 1 MiB。
解压内容以 `UWGTRN01` 开头，包含 manifest 长度/内容及对象数，条目按 ID 排序。
Mode 0 为完整长度前缀 envelope；mode 1 包含原始 base ID、目标 envelope 长度、delta 长度与
copy/insert 指令。

基底须已在接收方验证，或是本传输中的完整 mode-0 对象；拒绝传输内 delta 链和环。
还原后核对原 ID 与闭包，才允许发布。传输压缩不改变存储身份。

## 8. 服务记录与集成规则

授权策略和 hook updates 使用位置 codec。Receive audit 每条前有 u32be 长度，单条最大
4 MiB；渲染有累计限制并拒绝不完整尾。
当前 audit schema 为 version 3，字段依次为 version、transaction、Unix 毫秒、actor、
repository、updates、outcome、push options、rejections。
部分批次的最终记录逐项保存可选拒绝原因，其他阶段用空向量；拒绝旧 audit schema。
JSON 策略输入和审计输出都是外部接口；mergetool 导出的 manifest.json 同样只是审阅产物。

独立读取器需验证 framing、整数溢出、剩余长度、schema/version、顺序、路径、压缩限制、
delta 依赖、对象摘要、存在时的签名以及图语义。只通过外层 checksum 不够。
写入器优先使用共享库，不能仅抄字段布局而跳过语义验证。

实现依据包括[二进制基础](../src/binary.rs)、[对象 codec](../src/repository_binary.rs)、
[仓库 envelope](../src/repository.rs)、[pack](../src/repository_pack.rs)、
[补丁](../src/patch.rs)、[网络流](../src/repository_network.rs)、[RPC](../src/protocol.rs)和
[作者签名](../src/repository_signing.rs)。回归源码覆盖截断、损坏、边界与原字节还原。
