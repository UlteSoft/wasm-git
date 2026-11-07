# 浅历史、承诺对象、作者签名与多节点对象池

本页描述 wasm-git 原生格式的工作流；服务端与客户端共用实现，默认只构建 client。
仓库对象、边界、承诺记录、密钥、信任表、对象池配置、pack 和 RPC 均为二进制。
JSON 仅用于命令输出或外部配置输入。原始 Wasm、提交内容寻址及精确字节补丁保持独立。

## 浅克隆与部分克隆

```sh
wasm-git clone SOURCE NEW_DIR --depth 10 --branch main --filter blob:none
wasm-git clone https://host/project NEW_DIR --filter blob:limit=1048576
wasm-git clone SOURCE NEW_DIR --filter path:app.wasm,lib/
wasm-git clone-status DIR
wasm-git fetch DIR --deepen 20
wasm-git fetch DIR --unshallow
wasm-git fetch DIR --filter all
wasm-git hydrate DIR
```

SOURCE 支持本地仓库、HTTP(S) 和 SSH。HTTP 明文仍需显式 `--allow-insecure-http`；
凭据沿用既有传输设置，存储中不保存访问令牌。

浅历史以提交为单位，沿全部父边截断，保留边界提交的真实父 ID 与原对象哈希。
`Commit.shallow` / `history_parents()` 区分原始父列表和当前可遍历历史；不会改写父 ID
伪造根提交。`log`、修订遍历、fsck、GC、bundle、fork 使用同一边界。
`--deepen N` 从上次边界再延伸 N 层，远端新增提交不会吃掉这 N 层。
`--depth N` 重新按远端所选 tip 定义深度；`--unshallow` 请求当前源能提供的全部历史。
如果源本身是浅仓库，返回的真实边界仍会报告，不能凭空生成缺失祖先。

部分克隆保留提交和 wasm-git 的平面文件树，按以下条件省略完整 Wasm 内容对象：

- `blob:none`：省略所有 Wasm 内容；
- `blob:limit=N`：省略超过 N 字节的 Wasm 内容；
- `path:PATH[,PATH]`：只保留这些路径或目录前缀的内容；
- `all`：请求所有内容。

承诺对象不会被表示成空 Wasm，也不计为已经校验过的内容。
`fsck` 分别报告 `promised_wasm_blobs` 和 `shallow_commits`；未被承诺的缺失对象仍是错误。
只读历史不触发补拉。查看、diff、debug-map 或导出指定文件时只补拉所需模块，校验
原始 envelope 的 SHA-256、类型和长度后才安装；失败不产生替代内容或部分对象。
`hydrate` 可预取现有历史所需的所有承诺对象，使用现有全 CPU 线程池。

```sh
wasm-git promisor DIR SOURCE_A SOURCE_B
wasm-git show DIR HEAD --file app.wasm --output restored.wasm
```

多源读取按顺序回退。远端 URL 由本地操作显式配置，传入 bundle 不能指定补拉地址。
对象 RPC 只提供当前公开引用可达的对象；知道 dangling 对象的 ID 不能绕过仓库读取授权。
新对象到达前先持久化承诺/边界，补齐旧边界则在相关对象安装后才移除标记。

浅仓库可以向已有祖先的服务器推送新提交。接收端用自己校验过的对象补齐图，
只有最终完整的对象图才能发布到服务端引用；向没有这些祖先的新服务器发布时须先补齐。
原生 byte-range、DWARF 和 custom-section 审计始终针对取得的原始 Wasm。
这里的 filter 是 wasm-git 文件模型的选项，不声称接受 Git 的全部过滤器拼写或原生线协议。

## 提交作者签名

```sh
wasm-git keygen author.key author.pub
wasm-git trust-author trusted-authors.bin 'Alice <alice@example.org>' author.pub
wasm-git commit-signed DIR app.wasm -m 'update' \
  --author 'Alice <alice@example.org>' --key author.key
wasm-git verify-commit DIR HEAD --trust trusted-authors.bin
wasm-git untrust-author trusted-authors.bin 'Alice <alice@example.org>' --public author.pub
wasm-git sign-commit DIR EXPECTED_HEAD_ID \
  --author 'Alice <alice@example.org>' --key author.key
wasm-git-server --root ROOT --signed-authors trusted-authors.bin \
  --token-file TOKEN_FILE --allow-write
```

使用 native Rust Ed25519 严格验证。签名覆盖带用途域分隔的规范二进制提交：
内容/树 ID、有序父 ID、消息、legacy 路径、作者、公钥和时间戳。
私钥为独立二进制文件，Unix 创建权限 0600；进程中的临时秘密缓冲区清零。
`--timestamp` 可指定可重现的秒值；签名绑定该声明，不把它当作可信时间证明。

普通存储校验会拒绝损坏或不匹配的签名，但不会把一个有效自签名自动标成可信作者。
`--trust` 另外检查作者和公钥的精确绑定，信任表可同时保留多个轮换公钥。`untrust-author` 可撤销指定公钥，省略 `--public` 则撤销该作者的全部公钥。
服务端 `--signed-authors` 对新公开的历史执行相同检查，发生在对象/引用发布前，
与 transport principal、引用权限、保护分支和 pre-receive hook 独立生效。
已有公开祖先可以保持原来的未签名状态；此前仅存在于 dangling 存储的未签名提交
不能借此绕过作者策略，包括不上传对象、只创建引用的请求。正常删除引用和引用已公开
历史不要求重新签名。信任表更新采用独立写锁和原子替换，支持相对路径。

`sign-commit` 支持单模块和文件树提交，要求当前 HEAD 与给定 ID 一致；生成新的不可变
提交对象，原提交保留用于审计，不修改原 ID 下的内容。重签已有公开提交会改变 ID，
后续引用更新仍受原有 lease/非快进策略约束。keygen 的密钥不是 SSH/OpenPGP 文件格式。

## 跨 pack delta

磁盘格式为 `WGTPACK4`。索引保留排序原始对象 ID、fanout、压缩长度、解压长度、CRC、
本 pack 基底编号，并增加固定 32 字节的外部基底 ID。
私有增量可引用仍保留的共享 pack 中的完整 Wasm 基底；不需要再存一份完整私有起始版本。
其余对象继续使用同 pack 的块 copy/insert + Zstandard。

点查与全库校验都检查跨文件依赖，拒绝缺失基底、循环和总深度超过 8 的链。
限制是整条链的总深度，不能通过每个 pack 单独低于 8 来绕过。
写入完整新 pack 并同步后才移除旧 pack；若 GC 不再保留旧基底，则先重建可独立恢复的
新表示。fork 持有自己的不可变 pack 链接，源路径删除不会删除 fork 的基底。
`repack` 另报告 `cross_pack_delta_objects`；`pack_bytes` 只包含本轮写入的 pack，
不包含仍保留的共享基础 pack，不能将它当成整个仓库的总占用。

## 多节点不可变对象池

每个节点可部署现有 wasm-git-server，创建一个受控 pool 仓库，再配置后端凭据。
引用权限仍由各仓库主服务管理；对象池复制不可变数据，不引入多主引用共识协议。

```sh
wasm-git pool attach DIR https://node-a/pool https://node-b/pool
wasm-git pool sync DIR
wasm-git pool sync DIR --evict
wasm-git pool status DIR
wasm-git fork DIR NEW_FORK
wasm-git pool release DIR
```

也支持本地 pool 仓库路径，适合单机或挂载存储。每个拥有者具有随机身份，使用
`pool/<128-bit-owner>/<commit-id>` 的持久引用保留复制后的所有提交，包括仍在本地
存储中的恢复历史。对象按原 SHA-256 去重，多个仓库不必重复保存相同 Wasm 内容。
pool 是后端共享存储，读取权限应授予后端服务身份；不同可见性租户不要共用公开可读的
pool 仓库。pool 的授权边界就是其节点仓库，和普通受授权 Git 对象仓库一样。

同步通过已有校验、分片上传、CAS 和原子多引用接收路径，按节点并行。
所有配置节点确认 pin 后才允许 `--evict` 把本地 Wasm 替换为承诺记录；任一节点失败
保留本地数据并报告失败，已成功节点的不可变对象与 pin 可安全复用后重试。
读取在节点间回退，每次重建均校验原始对象哈希。GC 由真实 pin 保留对象，不依赖租约超时。

fork 只复制独立引用/控制数据并共享 pack；使用对象池时，为 fork 取得独立的节点 pin，
不会为此下载原有 Wasm 副本。源仓库释放自身 pin 后 fork 仍可读取。
`pool release` 先恢复本地当前存储所需的所有承诺内容，再逐节点删除本拥有者的 pin；
失败可重试。节点 reflog 的既有保留策略仍生效，需要按现有维护流程过期后才真正回收。
新增但尚未同步的提交不能凭不存在的节点 pin 完成池 fork；先执行 `pool sync`。
同一仓库及其全部 worktree 的池同步、释放、重新配置和 fork 使用同一进程间锁；
并发冲突在修改前返回 busy，重试即可。锁随进程退出自动释放；持久 `pool.gate` 文件
仅作为锁的稳定 inode，不保存对象，不应在仍有操作运行时删除。

## 协议与验证边界

保留原有单帧 64 MiB、分片 1 MiB、单次 bundle 4 GiB 的显式限制。
新增对象分片请求绑定目标对象 ID 和长度，客户端最终验证完整内容后才安装。
无效输入不降级成 JSON 存储；未发行旧对象/manifest/pack 不提供兼容路径。
HTTP 与 SSH stdio 共用授权、选取、对象校验和接收实现，服务端从不执行上传的 Wasm。

语义参考 Git 的 [shallow](https://git-scm.com/docs/shallow)、
[partial clone](https://git-scm.com/docs/partial-clone)、
[pack 格式](https://git-scm.com/docs/pack-format) 和
[签名格式](https://git-scm.com/docs/gitformat-signature)，密码实现使用
[ed25519-dalek 严格验证](https://docs.rs/ed25519-dalek/2.2.0/ed25519_dalek/struct.VerifyingKey.html#method.verify_strict)。
这些是等价工作流的设计依据，不表示 wasm-git 原生二进制可以交给 Git 直接读取。
