# 压缩存储、共享 fork 与恢复

## 构建与部署

默认 `cargo build --release` 只产生 client `wasm-git`。
`--no-default-features --features server` 只产生 `wasm-git-server`；
`--features server` 产生两个可执行文件。两者调用同一个 Rust library，使用同一套
原生对象、引用事务及 RPC 类型。默认 client 不编译监听模块。

HTTP 仍默认只读并监听 loopback。写服务显式配置 token 和 `--allow-write`，外部访问
通过 HTTPS 反向代理；SSH 使用系统账户授权。systemd 模板在
`contrib/wasm-git-server.service`。这不建立托管平台的多用户仓库 ACL 或审查系统。

## 共享而独立的 fork

`wasm-git fork SOURCE_DIR NEW_DIR` 只接受新目标目录。源 writer 锁保持至对象及引用安装
结束；基础 loose object 和 pack 通过 hardlink 共享。对象只做原子新建，后续压缩
也发布新 pack 后再移除旧目录项，绝不就地修改共享对象。

fork 拥有独立 refs、HEAD 和恢复日志。删除源仓库、源 GC 或 source repack 不会删除
fork 的 hardlink。不同于 alternates，fork 不向源目录借用未保留的对象。
同一文件系统是该 fork 的要求；跨文件系统会报错，可能留下完整或部分初始化的新目录，
不会覆盖原仓库。普通 local clone 优先 hardlink，不能共享时复制并核验对象。

HTTP/SSH 的 `wasm-git fork URL NAME` 在同一服务根创建独立仓库，沿用已有认证和写权限。
它不下载基础历史到客户端，不改变源引用。目标名称遵守现有 server 名称约束。
该操作创建的是存储与引用边界，授权仍由管理员控制。

## pack、repack 与 GC

`wasm-git gc DIR` 默认压缩历史并保留所有对象；`gc -n` 只生成计划、不写 pack。
`--prune=now` 才显式删除保留根不可达的对象；`--pack` 是默认压缩行为的显式别名。
`repack DIR` 压缩完整已验证对象集，包括未引用对象。maintenance 的 gc 任务也执行压缩。
规划、编码、发布和清理在同一 writer lease 内完成，不再先 prune、再重新全库压缩两次。

pack 使用 Zstandard level 6，独立对象任务在共享 Rayon 线程池并行，默认使用本进程全部可用
逻辑 CPU。`--threads 1` 可串行处理，显式线程数仍可限制资源。Zstandard 自身不开嵌套线程池，
每个 Rust worker 复用编解码上下文。少量大对象也能并行，不再受 32 个对象的启动门槛限制。

候选顺序来自真实提交 DAG 和文件路径，新版本先行。每组每 64 个版本保留完整锚点，
其余对象比较锚点与相邻版本的块级 delta，并与完整压缩的实际长度比较后选择。
copy 指令可引用 base 的任意区间，insert 保存原字节，因而支持分散修改、移动、非对齐插入和删除。
这是存储相关性选择，不构成重命名、函数身份或语义等价证明。

块索引最多采样 32,768 处、占用 256 KiB，每次最多探测 4 个候选；不相关内容采用有界跳步扫描。
优先沿上一段 copy 的位移匹配，遇到连续缺口再检查 ±32 字节邻域，按需建立块索引。
锚点索引共享，邻接版本索引仅在当前 worker 使用；delta 指令流预算为目标大小一半且最多 16 MiB，
超预算退回完整压缩。链深度最多 8，超深时使用已计算的完整锚点候选，无须重新压缩。
候选搜索有明确边界，因此不宣称得到所有可能编码中的最小 pack。

索引使用独立排序的原始 32 字节 OID 表、256 项 fanout、固定宽度记录、CRC32 和索引 SHA-256。
物理 payload 按历史分组，可与 OID 顺序不同；base 必须在物理位置上先于依赖者。
索引上限 64 MiB，单个原始对象上限 4 GiB。解码核对实际 frame 长度、禁止尾随 frame、
解码长度、copy/insert 边界、对象摘要和完整 pack 摘要。旧前缀/后缀 pack 读取路径不保留。

完整读取按依赖深度分层并行解码，原始对象只做一次摘要/结构校验；不再复制并缓存完整 base 链。
按需读取最多沿 8 层 base 还原，保留当前 base 和结果。pack 前后对象 ID、Wasm 字节、patch 和
bundle 不变。完整 snapshot 仍载入并验证全库，并非恒定内存流式实现。

发布顺序为完整 pack 写入并同步、原子发布 pack、再移除 loose commits
（子先父后）、tree/blob，最后删除旧 pack。中断可保留冗余对象，所保留图仍有完整依赖。
未完成对象写入不会改变引用；压缩失败需检查返回的具体阶段，不能推断所有旧文件已删除。
全部仓库统一使用新的二进制格式标记；不接受旧 JSON 仓库，也不提供迁移或旧格式读取分支。
loose 与 pack 可以同时存在，linked worktree 使用相同的二进制元数据结构。完整布局见[二进制格式](binary-format.md)。

`gc --prune=now` 对已有 pack 按保留闭包重新压缩，再移除旧副本。
引用、reflog、stash base、notes 目标及保守辅助状态保留规则延续原合同。
保留的旧提交可通过 show/checkout/restore 还原原始模块；真正 prune 且无备份的对象无法恢复。

## diff 的并行与解释

`wasm-git --threads N ...` 为扫描、diff、压缩及解码共用同一线程池，N=0 使用全部可用逻辑 CPU、N=1 串行。
两侧模块扫描、文件树任务、函数特征及候选评分复用线程池，合并结果保持输入顺序。
候选特征只计算一次；同分歧义按原始端点索引，不因贪心消费而制造唯一性。相似度搜索上限为 16,777,216 个同类型候选对；达到上限时 `function_matching.similarity_search_complete=false`，未配对函数通过索引列出而不计为已知新增/删除，完整字节审计继续返回。

实现对应优先纠正与完整唯一静态依赖见证冲突的导出角色配对。export 改绑仍以逐目标
证据显示，不应增加未改动实现的指令修改计数。同名唯一 export 改签名使用明确的
`same_export_role`、歧义标志及 type_changed 表达，不声称证明历史身份。
locals 只改变相邻同类分组时保留字节证据，但不当成实际局部类型变化。

`--compact` 的 JSON 保留完整 ByteEdit、函数修改、验证结果、diagnostics 与非 supported
的标准证据；明确标注被省略的 region 树、逐位置说明和 supported 标准证据数量。
文本模式显示变化函数及原始编辑摘要，长十六进制只预览前 64 字节并指出完整 JSON/patch
入口。紧凑 JSON 在序列化前裁剪省略字段，避免创建随后删除的临时 JSON 树；
精确编辑、诊断、未支持证据与省略计数保持一致。默认完整报告合同保持可用，`--short` 仍提供最简计数。

## 验证与未覆盖范围

新增测试验证 pack 前后各历史原字节、继续提交、bundle/clone、压缩/解压损坏拒绝、
GC 保留闭包、fork inode 共享、源仓库删除后的读取、HTTP 认证 fork、65 MiB 模块 clone 和超过 128 MiB 的历史 push。
编译组合和所有特性回归需分别验证；硬件性能报告保存在本次任务的 `/tmp` 目录。

这些变化没有实现 Git 原生仓库/线协议兼容。单帧 64 MiB，大 bundle 通过 1 MiB 分片交换，完整原始数据和对象图验证后才发布引用，见[传输边界](server.md#协商传输与大仓库)。交互式/保留 merge 拓扑 rebase、Git sparse 工作区、任意引用
命名空间仍是独立的后续范围；作者签名与浅/部分克隆见服务器存储扩展；不能把本次变化宣称为全部 Git 选项已支持。


## 持续共享的 fork 基础历史

fork 前如有 loose 对象，先在源仓库现有锁内压成不可变 pack，再链接给 fork。
每次 repack 检查 pack 的硬链接数：仍由多个仓库持有、且所有对象均在 keep 集内的 pack
原样保留，仅重新编码本仓库的新增对象。报告另含 `shared_base_packs` 和 `shared_base_objects`；
`pack_bytes`/`raw_object_bytes` 描述本轮新 pack，不含保留的共享基础 pack。
因此源仓库与 fork 分别提交、GC 后仍共享基础历史，且不用依赖外部 alternates 路径。
显式 prune 的 keep 集若排除共享 pack 中对象，则允许重写该仓库的副本以确实删除不可达对象。
网络传输现在也支持 have 闭包省略、同路径祖先块增量、并行 Zstandard 和原子多引用发布，见 [server](server.md)。

`WGTPACK4` 支持保留共享 pack 的外部原始对象 ID 基底，总 delta 深度仍限制为 8，
并检查跨文件循环/缺失/损坏。fork 的首个私有版本可直接对共享完整基底做增量。
多节点不可变对象池、复制确认后的本地清除与独立 fork pin 见 [服务器存储扩展](server-storage.md)。
