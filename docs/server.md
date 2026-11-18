# HTTP 与 SSH 服务

`wasm-git server` 为同一服务根目录下的多个 wasm-git 仓库提供读取及受控引用更新。
每个子目录是一个独立仓库；服务不执行 Wasm。客户端和服务端使用 wasm-git 的对象及 RPC 格式。

## HTTP

```sh
mkdir -p /srv/wasm-git
wasm-git init /srv/wasm-git/project
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419
wasm-git clone http://127.0.0.1:9419/project ./peer
```

默认监听 loopback，默认只读。写入服务使用管理员提供的 token 文件：

```sh
wasm-git-server --root /srv/wasm-git --listen 127.0.0.1:9419 \
  --token-file /run/secrets/wasm-git-token --allow-write
export WASM_GIT_REMOTE_TOKEN='your-token'
wasm-git remote ./peer add origin http://127.0.0.1:9419/project
wasm-git push ./peer origin main:main
```

示例 token 须自行替换。token 为 1–4096 个无空白的可打印 ASCII 字节，文件可有一个末尾换行。
服务配置 token 时，读取和写入请求均需 Bearer 认证。客户端不会保存 token 到 remote 配置，
不会将其放入 curl 的命令行参数；临时 header 文件仅当前用户可读，并在操作结束后删除。
每个客户端操作直接连接指定地址；不自动重定向，不读取 curlrc，也不使用代理环境配置。

外部网络使用 HTTPS 反向代理连接 loopback HTTP 服务，客户端验证正常 TLS 证书。
`server` 本身没有 TLS 终端。HTTP 远端使用明文认证需显式
`remote DIR add NAME URL --allow-insecure-http`；loopback 测试不需要此选项。
`--allow-force` 由管理员另行开启，客户端还必须提供准确的 `--force-with-lease OLD_ID`。

## SSH

在服务机器安装 `wasm-git-server`，使用既有 OpenSSH 用户及密钥授权：

```sh
wasm-git clone ssh://alice@example.org/srv/wasm-git/project ./peer
wasm-git remote ./peer add origin ssh://alice@example.org/srv/wasm-git/project
wasm-git fetch ./peer origin
wasm-git pull ./peer origin main -m 'integrate remote'
wasm-git push ./peer origin main:main
```

URL 的绝对路径父目录成为服务根，最后一段为仓库名。客户端调用
`ssh -T ... -- USER@HOST 'wasm-git-server server --stdio --root ...'`，通过标准输入输出交换单个请求。
服务端使用 SSH 的系统用户权限，stdio 默认允许写入；HTTP token 不会转发给 SSH。
远端程序及路径逐项 shell 引用，客户端不将输入拼接成未引用的 shell 命令。

`WASM_GIT_REMOTE_PROGRAM` 可指定远端单个可执行文件路径，`WASM_GIT_SSH_PROGRAM` 可指定本机单个 SSH
可执行文件或包装脚本。两者都不是 shell 命令字符串。主机密钥遵守 OpenSSH 的正常策略，
连接使用 BatchMode，不会挂起等待交互密码。
允许 SSH 强制推送时，管理员可提供包装程序，在接收 `server --stdio` 参数时加入
`--allow-force`。SSH 账户或 forced-command 的授权范围由部署管理员配置。

## 多个 Wasm 文件

远端传输提交中的完整文件树，每个路径对应一份原始 Core Wasm 字节。
路径、空树、文件新增和删除都是提交内容；服务不会把多个模块合并成一个 Wasm，
也不会通过文件内容相同来猜测重命名。

```sh
wasm-git add ./work modules/client.wasm modules/server.wasm
wasm-git commit ./work -m 'track both modules'
wasm-git tag ./work base
wasm-git remote ./work add origin https://example.org/project
wasm-git push ./work origin
wasm-git push ./work origin --tags
wasm-git clone https://example.org/project ./peer
wasm-git restore ./peer --source HEAD --staged --worktree
wasm-git show ./peer HEAD --file modules/client.wasm --output /tmp/client.wasm
wasm-git diff base HEAD --repo ./peer --file modules/client.wasm --format json
```

`clone` 保存完整历史及所有分支、标签，随后可以显式恢复工作文件。
`fetch` 更新独立的 remote tracking，不改变本地分支、暂存内容或工作文件。
默认 tree diff 逐路径报告新增、删除、修改和未变；`--file PATH` 可选择单个模块。
删除最后一个暂存路径后仍可提交空树，空树也能正常推送和克隆。

两个客户端产生文件树冲突后，可以用完整暂存树显式保存解决结果及两个父提交：

```sh
wasm-git merge ./peer IMMUTABLE_TARGET_ID -m 'review complete resolved tree' \
  --resolved-index --expect-head IMMUTABLE_HEAD_ID
wasm-git push ./peer origin
```

这条手工命令精确使用已存在的暂存树，不改暂存区或工作文件；未暂存的修改仍保留。
真实的空暂存树表示删除所有路径，缺少暂存区则拒绝。文件变目录、目录变文件都按实际路径删除和新增传输，
不会推测重命名。响应逐文件保存 base、两个父版本和解决结果的原始字节与审计，显式选择的未知特性仍会标记。
另一客户端 `fetch` 仍只更新 tracking，随后 `pull` 才将干净的受跟踪工作文件同步到所接收的完整树。

RPC 帧和广告状态继续使用 version/format 1。文件树位于 bundle 内部：
Tree 对象为 kind 3，tree commit 为 format 2，包含这些对象的 bundle manifest 为 format 3。
接收端先校验完整对象图、规范路径和每个 blob 的对象摘要，再安装对象或更新引用。
非法路径、缺失 blob、对象类型错误和损坏对象均会拒绝；不支持新对象格式的版本也会明确拒绝。
所有文件的编码、名称和原始字节都受 bundle 校验约束，64 MiB 的帧上限同样适用于多文件传输。

完整快照按依赖顺序发布：先 Wasm blob，再 tree，最后按父先子后的顺序安装 commit，不能按对象 ID 排序代替依赖顺序。
本地 clone、bundle/网络 clone、unbundle、fetch 安装和服务端 receive 共用此规则，
并保留完整快照中合法的不可达对象与历史。对象安装中断可留下未引用但闭包完整的对象，
引用在所需对象完整安装前不发布；重试仍可验证已有存储，不需要先删除合法 dangling 对象。
引用批次通过持久 journal 向前恢复引用和 reflog；严格入口只恢复与原操作精确匹配的范围，
普通严格读取遇到待恢复 journal 时拒绝。最终发布或同步失败仍明确报告不确定结果。

## 一致性与失败结果

`fetch` 安装已校验对象并保存 remote tracking，保留本地分支。
`clone` 保存所有 advertised 分支、标签和 unborn HEAD。
fetch 网络交换前记录完整的可选 tracking 基线，安装对象后在 remote 配置锁内同时核对配置和整条基线，
再原子保存新 tracking。比较包括记录缺席、HEAD、全部分支和标签，而非仅比较某一分支 tip 或祖先关系。
并发 fetch 若已先发布新状态，延迟旧响应会明确报错，保留最新 tracking；已安装对象保留，本地分支、index 和工作文件不变。
后续独立 fetch 仍能接受远端合法的强制回退。固定并发与中断回归。
push 绑定 advertised 旧提交或“引用不存在”；服务端在仓库锁内重新检查旧值。
普通分支推送只允许快进，已有标签不能被普通推送替换。
标签替换需要显式租约及服务端 force 许可，删除也要核对旧引用。
服务端拒绝删除当前 HEAD 分支。

`push DIR origin --tags` 现在通过一次请求发送引用批次和对象包，逐项报告成功、未变化或拒绝；
任意拒绝均非零退出，被接受的集合通过持久事务发布。`--atomic` 则要求整批全部接受。
响应丢失时客户端不能据此断言服务端未发布，应重新 fetch/advertise 检查。
RPC 的 `publication_unknown` 单独标记进入引用发布阶段后的不确定失败；
客户端不会将这种错误当作已拒绝；一次请求中全部已发送的引用均保留未知结果，不自动重试。
磁盘写入失败可能留下合法的未引用对象，引用不会在输入校验或租约检查完成前更新。
ref 已发布后同步或 reflog 写入失败会明确报告已发布状态。

服务根、仓库路径及 token 路径拒绝符号链接与 `..`。
服务端持有暂存锁后检查当前分支：有 tracked index 的当前分支拒绝远端更新，
避免覆盖用户正在使用的工作文件。服务仓库通常只保存对象和引用，不维护 tracked 工作文件。
仓库锁保护遵守同一协议的协作进程；操作系统账户对目录的任意外部替换不受其保护。

## 线格式与资源界限

HTTP 仅接受 `POST /v1/wasm-git`，关闭每次连接，要求唯一的 `Content-Length`，拒绝
`Transfer-Encoding`、重复 header、超限 header 及不完整输入。HTTP 请求格式依据
[RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html) 的 framing 规则。
请求中的 repository 指定服务根下受限的仓库路径，可用 `team/project.git`。
每段最多 128 个 ASCII 字节，总长最多 512 字节、16 段；拒绝空段、穿越、符号链接和仓库内部嵌套；
remote 配置名仍使用独立的 64 字节限制。

HTTP body 与 SSH stdio 使用同一帧：

| 字段 | 编码 |
|---|---|
| magic | 8 字节 `WGTRPC02` |
| metadata length | u32 大端，1–65536 |
| payload length | u64 大端，转换为本机长度前检查 |
| metadata | 带版本头的按位置编码二进制记录，拒绝额外/截断字段 |
| payload | 原生 bundle、协商压缩流、引用页、批量更新或空 |

完整帧最多 64 MiB，帧之后的尾随内容拒绝。
metadata version 为 1，操作含 advertise、fetch、push、update_ref、fork、refs_page、publish_batch，
以及 fetch_slice/upload_slice/apply_upload/cancel_upload、publish_options/check_batch/publish_partial。
push 的 expected、update_ref 的 expected 和 new 均有显式的二进制 Option 标志；None 分别表示预期引用不存在或明确删除。
字段截断、非法 opcode/Option 标志和尾随数据会拒绝整个请求；apply_upload 内层使用非递归的发布/预演枚举，确切编号见[二进制格式](../documents/BINARY_FORMAT.zh_CN.md)。
push/update_ref 只发布明确指定的一条引用，bundle 中其他分支及标签不会被隐式更新。
HTTP header 最多 16 KiB，连接有总时限，最多同时运行 16 个连接 worker。
`--max-connections N` 供有限集成测试使用，生产服务默认持续监听。

磁盘 pack 与网络增量复用二进制块差量算法；网络按已知提交的可达对象闭包协商。
它使用原生 wasm-git 线协议；提交作者签名、浅历史与部分克隆见 [服务器存储扩展](server-storage.md)。
对象完整性来自原始编码的 SHA-256；认证只证明运输层授权，不能证明提交作者身份。

## 独立共享 fork 与部署

使用 `cargo build --release --no-default-features --features server` 单独构建服务，
或 `--features server` 同时构建 client/server。源码和对象引擎共用，默认构建只有 client。

`wasm-git fork SOURCE_DIR NEW_DIR` 创建独立引用及恢复元数据，用 hardlink 共享不可变
loose objects 和 pack 文件；不会复制整份基础对象，也不依赖源仓库继续保留目录项。
网络入口 `wasm-git fork URL NEW_REPOSITORY_NAME` 在同一服务根创建 fork，要求服务写权限
和现有认证。已有目标、非法名称、只读服务和未经授权的请求会拒绝，不会覆盖目标仓库。
fork 需要同一文件系统；跨文件系统明确失败。普通 local clone 优先 hardlink，无法共享时
才复制原始对象。fork 的 refs、HEAD、reflog 和后续写入独立；worktree 仍用于同一仓库。

可使用仓库中的 `contrib/wasm-git-server.service` 作为 systemd 部署模板。管理员需配置
服务账户、仓库目录与 token 文件；模板不会自行创建账户或安装软件。HTTPS 仍由反向代理提供。
详见[压缩对象、共享 fork 与恢复](packed-storage.md)。

## 协商传输与大仓库

新客户端的 fetch payload 是原生 `FetchOptions`，其中 `have` 是本地分支、标签和已获取的
远端提交。服务端只发送当前公开分支、标签及 detached HEAD 可达、且对方尚不拥有的对象。
删除分支后仅由 reflog 保留的对象不会进入新 clone；显式 `bundle` 备份仍保存整个已验证对象库。
未知 have 忽略；已知 have 必须是本次可达图中的提交，不能用对象 ID 请求未公开历史。

同路径模块或单模块提交可选最近已知祖先的原始对象作为 delta base，最多检查 1,024 个祖先。
复用 pack 的有界块复制/插入算法，实际字节逐段匹配；增量不够小则传完整原始对象。
base 来自对方已知提交闭包，或本次传输携带的完整根提交模块。传输内 delta 深度限制为一，
禁止循环和链式基础依赖；首次 clone 同样可共享完整基础对象。服务端生成增量、压缩分块使用同一个默认全 CPU
线程池；接收端先核对 base、还原边界、原始对象 SHA-256、引用和完整对象图，再安装或发布。
存储增量不构成函数身份或语义等价判断，Wasm 审计算法及原始字节不变。

压缩传输头为 `WGTNET01`，每个独立 Zstandard frame 最多还原 1 MiB；内部记录为 `WGTTRN01`。
无变化 fetch 不携带对象；引用状态放在二进制 payload 内，避免大仓库的 64 KiB metadata 限制。
`ls-remote` 对过大的广告改用每页至多 256 个引用的 `refs_page`；游标和 SHA-256 快照代次
绑定，引用在分页中改变会明确要求重试，客户端也核对最终完整快照摘要。

单帧仍最多 64 MiB。超限 fetch 自动转为 1 MiB `fetch_slice`，冻结的是本次协商产生的
压缩流；每片有总长及 SHA-256 身份，最终摘要匹配后才解码。下载缓存每仓库最多八份且合计
不超过 8 GiB，淘汰只导致明确重试，不会接上另一个版本。超过 24 小时的缓存由实际 GC 清理。

大于 32 MiB 的 push 或原子批次通过 `upload_slice` 暂存不可变片段，`apply_upload`
核对全部片段与摘要后再重新检查该身份的引用权限。每仓库最多 32 个未完成上传，按声明总长
预留合计 8 GiB，避免用稀疏分片绕过配额；最近一个新片段超过 24 小时即过期。
缺片可续传；完整但摘要错误的上传立即删除，完成发布或被拒绝的上传也清理。
`cancel_upload` 可主动取消。上传开始和实际 GC 会清理过期上传；GC dry-run 不清理任何文件。

单次传输及声明的还原体积最多 4 GiB。最终验证仍需要载入本地完整对象库，不能据此宣称恒定
内存或任意大小仓库。浅历史与部分克隆使用显式边界/承诺对象，见 [服务器存储扩展](server-storage.md)。旧的空 fetch 请求仍可取得可达历史的原生
完整 bundle；这只保留同一二进制 RPC 中的小传输入口，不读取旧 JSON 仓库。

## 多用户权限与原子发布

单 `--token-file` 是可信管理员模式，该 token 可访问整个服务根；不是多租户隔离。
部署 wasmhub 时使用 `--policy FILE`，它与 `--token-file` 互斥。策略的 JSON 是外部管理员输入，
编译后服务器只读取二进制策略，不会把 JSON 存进仓库。示例中的摘要须替换为实际 token 的 SHA-256：

```json
{"version":3,"principals":[{"name":"alice","token_sha256":"<64 lowercase SHA-256 hex digits>","grants":[{"repository":"alice.*","read":true,"write":["refs/heads/*","refs/tags/*"],"force":false,"create":true,"expires_at":null}]}]}
```

```sh
wasm-git-server compile-policy /tmp/policy-input.json /etc/wasm-git/policy.bin
wasm-git-server --root /srv/wasm-git --policy /etc/wasm-git/policy.bin --allow-write
```

仓库和引用规则只支持精确匹配或末尾 `*` 前缀匹配；没有隐式默认授权。
HTTP token 的 SHA-256 与策略中的凭据比较，日志只记 principal 名称。
引用写入要求匹配 write 规则；强制更新还需匹配 force 规则及服务器 `--allow-force`。
fork 要求源仓库 read 和目标名称 create；不会因可读取源仓库而获得其他租户的目标权限。
首次启动将编译策略装入服务根的原生授权库，同时保存初始化回执；初始化操作 ID 为编译策略文件的
SHA-256，回执与该策略一同原子提交。之后所有实例均读取该库，不重新加载启动文件。
策略更新使用 `wasm-git-server --format json policy-set ROOT REQUEST_JSON`，请求包含版本、
唯一 `operation_id`、`expected_generation` 和完整新策略。授权库同时原子保存当前策略和原操作回执；
相同操作及内容可重放，不同内容不得复用操作 ID。代号冲突也产生绑定原输入的持久回执；命令成功只表示
返回了回执，调用方须区分 `applied` 与 `generation_conflict`。`policy-query ROOT REQUEST_JSON` 按操作 ID 查询回执；
查询无记录只表示查询时未提交，不证明其他在途请求不会稍后提交。`policy-status ROOT` 查询当前代号及摘要，
不能代替原操作回执。授权库使用 SQLite 事务保存当前策略和不可覆盖、不可删除的原生回执；
操作 ID 通过索引点查，不按全部历史重写；回执本身没有独立的终生操作次数上限，审计索引仍有容量上限。
磁盘不足或无法提交时不报告策略已经应用，须按原操作查询或重试。请求及机器输出由
`integration-validate` 的相应原生协议校验。

原有 v1 授权库不会被静默初始化或丢弃。维护窗口中停止不兼容实例、保存一致备份后，使用
`wasm-git-server --format json policy-migrate ROOT REQUEST_JSON`，请求绑定原文件的完整 SHA-256。
迁移核对所有旧回执，在不可寻址的临时数据库中建立完整事务，持久化后才替换原路径；源内容不匹配时不替换。
迁移回执保存在新库中，回复丢失可按相同源摘要重放。后续策略代已推进时仍返回原迁移回执，不将策略回退。
机器请求及回执同样使用 `integration-validate` 的原生迁移校验接口。旧 v1 程序遇到新数据库格式会拒绝访问。
授权库备份须使用一致数据库备份或停写后的完整快照；不能在事务活动时只复制主文件而遗漏恢复所需 journal。
恢复先经原生接口完成数据库恢复和持久同步，未确认成功时不开放新授权操作。

每个 HTTP/stdio 请求在服务根的稳定锁上读取当前策略并持锁到响应写出；首次提交策略更新取得独占锁，
`applied` 表示该代提交时旧持锁操作已经结束，之后的受保护动作按当时最新策略判断。重放返回的历史回执
只证明原代曾经生效，不证明后来未发生重新授权；确认当前状态须核对当前代号、摘要和协调顺序。
不能取得锁或无法确认持久化时不报告屏障完成。
旧引用事务如已有持久提交决定，后续恢复只滚前原决定；上传和保留查询仍按当前权限及原 actor 约束。
引用事务在发布前汇总所需权限的截止上界并统一检查，grant 的 `expires_at` 为独占 Unix 秒截止时间。
使用严格操作凭据的入口将该上界与原操作期限求交；保留和上传状态在原子发布前检查，
读取在首次写出响应前检查。已经开始的单次响应可以完成；首次交付前失去权限时不返回受保护内容，
已经提交或结果未知的写入仍须按原操作核对，不改判为从未提交。
授权检查属于持有方决定点；已经完成该有效授权决定后的持久同步或既有决定的恢复，不因晚到而变成新操作。

### 严格操作凭据

受信协调器通过 `wasm-git-server --format json operation-credential-issue ROOT REQUEST_JSON`
发放操作凭据，使用 `operation-credential-query` 核对原发放结果。请求和回执由
[原生契约](../src/protocol_operation_credentials.rs)及 `integration-validate` 维护；Hub 不自行计算原生请求摘要。
协调器必须从原身份、受理、授予及请求期限取最早上界，毫秒期限向下取整为秒，不能自行续期。
原生库绑定实际主体、精确请求及 payload 摘要、凭据摘要和期限；当前主体策略仍须允许该动作。
首次发放还核对并绑定当前策略代号。任何策略更新都会使旧代凭据失效，之后重新授予权限也不能使其复活；
当前授权下的新查询使用新代凭据核对原结果，已经提交的决定不因此撤销。
相同发放请求重放原回执；换输入、凭据或期限不得复用原操作。原生写入动作还按仓库、主体及
会话分片或保留阶段独立去重，因此换发放 ID 也不能为同一动作延长期限。

新的凭据发放与审计登记在同一事务中将授权库升级到数据库版本 6。旧程序不理解该版本并拒绝访问；
新服务必须以 `--require-operation-credentials --policy BOOTSTRAP_FILE` 启动 HTTP 入口，
不能回落到普通主体 token 或 stdio principal。升级在根独占屏障内等待在途请求结束，
但本地管理权限和不遵循已有根屏障的更旧程序仍须由部署隔离；不要用旧文件覆盖已升级数据库。
版本 3、4 的历史发放回执仍可查询；升级审计库不会重建历史原请求或发放归属，
历史凭据的恢复限制见下文。

严格入口开放原生读取、有持久回执的 scoped 上传及发布保留；直接引用发布、旧无主体上传、PR 和 fork
不属于其支持范围。严格分片使用[原操作分片契约](../src/protocol_chunks.rs)，旧 Slice 仅保留普通策略入口用法，
不得为它新发严格凭据，历史凭据也不能在严格入口执行该动作。
每次分片、查询和续传独立发放精确凭据。期限到达或策略改变后，可用当前授权发放新的查询凭据，
核对原操作，不能换写入身份重做。查询自己的原操作要求当前仓库读取权及真实主体匹配，
不要求重新取得原写入权；查询回执不会授予新的写入、续传或跨主体恢复权限。

### 本地运维与受限恢复

本地管理入口接收 JSON 请求文件并以 JSON 返回结果：

```sh
wasm-git-server --format json operation-context /srv/wasm-git context.json
wasm-git-server --format json operation-recover /srv/wasm-git recovery.json
wasm-git-server --format json audit-query /srv/wasm-git audit-page.json
wasm-git-server --format json capacity-query /srv/wasm-git capacity.json
```

这四个入口仅支持 Linux。进程使用实际有效 UID，要求它为 root 或服务根的所有者，服务根不可由
group/other 写入；路径不得包含符号链接或 `..`，祖先目录的所有者和写权限也须可信。
`operation-context` 按原操作 ID 查找已登记的原发放请求；`audit-query` 分页读取服务根审计索引，
翻页时保留首次结果的 high-water，直到没有下一页；它固定查询范围，观察仍可在读取后回填。
`capacity-query` 查询指定仓库的记录容量。严格网络读取与容量查询不会回放待恢复的引用 journal；
遇到该状态时先按 journal 对应的原操作执行受限恢复，不能借普通读取恢复其他操作。
调用方用原生 `integration-validate` 校验请求、机器输出和实际进程退出码。
请求与响应的唯一契约见[恢复](../src/protocol_recovery.rs)、[审计](../src/protocol_audit.rs)和
[容量](../src/protocol_capacity.rs)源码，不要自行推断或重建原始请求。

原主体撤权或凭据过期后，管理员可用与原发放回执严格匹配的原请求，运行 `operation-recover` 的
`Inspect` 或 `Close`。恢复在服务根授权屏障取得独占锁，等待先前持锁请求结束，然后核对原主体、
仓库及上传会话或发布保留范围。`Inspect` 只观察或恢复该范围已有的持久决定，不创建终止决定。
`Close` 可关闭未完成的上传或尚未转正的保留；确认 v4 apply 没有 journal 且已排除在途操作时才记录中断。
无法判定的旧版进行中上传保持未知，已提交或已转正的结果保留。恢复不提供新上传、正文读取或新的发布、转正决定，
也不续期或更换原操作 ID。恢复 ID 绑定原操作、动作和本地操作人，重试时保持相同身份和输入。
独占屏障只协调参与该服务根锁协议的程序，本地直接写入或不兼容旧程序仍须由部署隔离。

旧凭据回执只保留请求摘要，缺少完整原请求和原始审计归属，`operation-context` 可能无记录。
管理员须提供保存的原请求，核对其与原回执严格一致后，才可登记为 `LegacyImported` 以供恢复；
记录的是核验者和观察时间，不能补造历史发放者或时间，也不能用其发起新动作。

通过初步认证授权的严格写操作及本地恢复每次尝试都在执行前持久登记；原生回执仍是仓库决定的事实来源。
`Unknown`、`Unobserved` 或缺失观察均不表示原操作未执行；未观察到决定时，尚未被关闭屏障排除的
原请求仍可能迟到。显式 `reconcile` 可将同一原操作的恢复观察回填到指定尝试，记录的是该范围的
状态观察，不宣称该次尝试导致了状态。批量对账原子提交，重试保留已经完成的历史观察。
审计索引在累计新任务受理水位处拒绝新工作，撤权和本地恢复不受该水位阻断；分页结果同时报告水位和累计使用量。
存储故障时仍须恢复存储并按原操作查询或重试。

### 分片决定与清理

分片以原操作身份、真实主体、仓库、上传会话、固定发布输入、位置及字节摘要绑定。
原生库先同步可恢复字节，再在最终授权检查之后持久记录接受决定，随后安装上传分片。
每个位置独立保存决定，不因另一分片写入而重写所有分片回执。已提交决定可在重开或查询时滚前，
既有决定的恢复不重新冒用发起者或延长旧凭据期限；查询访问仍须经过当前授权。

接受决定和数据是否仍可用分别核验。取消、完成和过期清理先持久关闭该会话的数据安装，再删除数据并同步目录；
清理完成后仍保留接受回执，迟到重放不能重新创建已清理数据。未完成清理和目录同步失败须沿原操作恢复，
不能由文件缺席推断从未接受，也不能由暂存文件存在推断某次凭据已执行。
普通会话查询的 Open 状态不证明某个分片的结果；分片查询无决定只表示查询时未观察到接受，
不阻止仍有效的旧请求迟到。要确认不会再接受，先取得会话关闭屏障，再核对相关原分片。
会话与保留记录的活跃容量和迁移规则见下文；分片决定与会话数据清理分别核验。

原生 CLI 对不完整、损坏或无法绑定的写入回执保留结果未知并要求查询原操作；跨语言消费者不得解析人类诊断文本
来判断是否提交。静态能力报告不证明实际服务根、入口隔离或恢复链路已通过接入验收。

此屏障要求所有服务器实例运行支持该屏障的版本，并访问同一受控服务根、同一稳定锁 inode 和提供一致进程间共享/独占锁、
原子替换及持久同步语义的文件系统；不得在服务运行时删除或替换授权库、锁文件或根目录。
不参与该屏障的旧服务实例须先停止或隔离，不能仅依赖新实例报告撤权。
本地管理员、hook 及直接写仓库的人属于可信管理边界，绕过服务器的本地写入不受该屏障保护。
已写入操作系统缓冲的响应不保证客户端在屏障返回前已收到；阻塞的 stdio 或 I/O 可使屏障超时而失败。
Windows 不提供目录 fsync，不能据成功响应推断断电后目录项一定持久。主体 `name` 是稳定身份，
不得跨不同主体复用；同一主体轮换 token 时保留名称。

SSH 通过管理员设置的 forced-command 固定
`--stdio --root /srv/wasm-git --policy ... --principal alice`；身份不能来自客户端请求体。
`--read-only` 可限制 SSH key，也可限制 HTTP 服务。不同分支不构成读取隔离，私有数据应放在不同仓库。

```sh
wasm-git push ./peer origin --tags --atomic
wasm-git push ./peer origin main:main HEAD:refs/tags/release --atomic
```

`publish_batch` 将 1–4,096 个显式 branch/tag 更新放在同一事务中。所有旧 ID 租约、目标对象、
快进/标签替换规则、工作区保护和命名冲突先检查；任一失败均不发布引用。原子批次使用本地
持久化引用事务日志，崩溃后按原事务完成整批恢复；严格入口遵循上文的原操作恢复边界。裸服务器 HEAD 可更新，已跟踪工作区
的当前分支仍受保护。响应携带请求摘要收据，传输故障时客户端明确报告发布结果未知。
默认非原子 `--tags` 仍逐项报告结果；带 `--atomic` 时拒绝整个批次，不谎报部分成功为原子成功。

### 会话与保留容量

每仓库的 scoped 上传会话和发布保留分别按单条记录存储；新会话及新的保留准备各自受 4,096 条
活跃记录和 64 MiB 活跃记录字节水位约束。关闭、完成或转正等终态记录保留，但不占活跃水位；
`capacity-query` 分别返回活跃与累计记录数和编码字节数。累计值仅覆盖这两类记录，不含仓库对象
或分片正文；累计扫描也有上限，查询失败不能当成零。磁盘或同步失败后先恢复存储，再按原 ID 查询
或重试，不能依靠换 ID 绕过未知结果。

旧会话与保留账本在首次使用新存储时迁移，包括通过 `capacity-query` 首次打开：新记录持久化后，
原账本路径以原子 marker 切换权威来源。
升级前先做一致备份，在维护窗口停止所有旧版写入者和本地维护程序，不得混用旧版；保留 marker 与
新记录目录一同备份、还原。不要手工删除迁移残留或以旧账本覆盖已切换的数据。

## 接收检查与审计接口

`--pre-receive /absolute/program` 和 `--post-receive /absolute/program` 只运行管理员指定的
可执行文件，不执行仓库里的 Wasm 或脚本，也不拼接 shell 命令。每个钩子最多 15 秒。
stdin 为原生二进制 `Vec<RefUpdate>`，同时提供以下环境变量：

- `WASM_GIT_ACTOR`：已认证身份；兼容管理员 token/SSH 模式分别为 `token`/`ssh`。
- `WASM_GIT_REPOSITORY`：服务器仓库的绝对路径。
- `WASM_GIT_UPDATES_FILE`：本批次二进制更新记录的临时路径。
- `WASM_GIT_QUARANTINE_BUNDLE`：接收前是完整、已校验、尚未安装的原生 bundle；接收后为空文件。

接收前钩子拒绝时，新对象和引用均不会进入公开库，隔离文件退出时删除。
钩子应检查隔离 bundle，不要递归更新正在接收的仓库。接收后钩子失败不会把已完成推送报告为失败，
其失败写 stderr 和二进制审计事件；上层可据此实现合并准入、索引通知或 CI 调度。

普通接收的 `receive-audit.bin` 为长度分帧的原生记录，包含版本、事务 ID、时间、身份、仓库、
旧/新引用及 prepared/accepted/rejected/unknown 状态，fork 另记目标名称。
prepared 事件在发布前持久化；崩溃或最终审计写入失败可能只留下 prepared，必须结合实际引用核对，
不能把它视为已接受。接收后事件失败不改变已完成事务的事实。管理员负责保留与轮转日志；
日志是操作审计，不是提交作者签名或不可篡改证明。该旧日志只辅助核对普通或历史接收；
严格操作的尝试和恢复以服务根审计索引及相应原生回执核对。

```sh
wasm-git-server audit-log /srv/wasm-git/receive-audit.bin
```

上面的 JSON 只是对外查询输出，内部策略、日志、对象、事务、压缩历史和网络 payload 均为二进制。

## 并发与 fork 发布

服务端在已有固定格式文件的 inode 上取得进程间锁，协调 HTTP worker 与独立 SSH 进程，
不会因网络读者竞争本地 fail-fast writer lock 而互相拒绝。读取引用只检查引用目标提交，
不解压所有 Wasm；完整 transfer/fsck 另做对象验证。服务协调锁复用已有格式文件。
原生仓库的 writer、index 和传输状态锁使用持久的版本化文件，首次采用该后端时可能创建锁文件；
Unix 资源锁正常结束时显式解锁；进程异常退出后，仍持锁的继承描述符须关闭才能释放。文件存在不表示仍被占用。不得删除或替换原生锁 inode。
旧存在型锁保持原样拒绝，须停用全部使用者、确认没有活动操作后受控移除旧格式锁。
WASIp1 保留独占创建锁和异常退出后的维护语义，遇到已有原生锁文件时拒绝访问；不同后端不得未经维护转换而混用。

接入循环会回收任意已完成 worker，不等待最早的慢连接；16 个活动连接全满时快速拒绝。
连接有 15 秒累计 I/O 时限；完整请求缓存后的校验与落盘处理时间不计入其中，
避免慢主机完成发布后无法发送回执。零碎读取不会重置 I/O 预算。
客户端 HTTP/SSH 每次 RPC 默认 300 秒，可用 `WASM_GIT_RPC_TIMEOUT_SECONDS` 设置为 1–3600 秒；
连接建立限时 10 秒，子进程与管道收尾另有 5 秒。超时后应先查询远端 ref，再决定重试。
单仓库服务操作串行协调，压缩和对象工作使用共享线程池；
这不等于无限并发或多节点分布式锁。

远端 fork 先在不可寻址的临时目录完成，再发布目录和有界收据，不再在发布后读取庞大引用表。
loose 基础历史先压成共享 pack，fork 后各自增加私有 pack；GC 保留仍完整可达的共享基础 pack，
不会每次将整份基础历史复制。显式 prune 必须丢弃基础 pack 中部分对象时可重写本仓库的 pack，
其他 fork 的 hardlink 不受影响。普通同文件系统 fork 不依赖源仓库路径，删除源仓库也可读取。

这些接口可作为 wasmhub 的存储与接收核心。账号注册、组织成员管理、Web UI、PR/审批、CI 执行、
限流计费、日志归档与多主引用协调属于上层；多节点不可变对象池已由原生后端提供。仓库创建/删除仍通过受控本地 Repository/CLI；
浅历史/部分克隆、作者签名、跨 pack delta 和多节点对象池已实现，见 [工作流与协议边界](server-storage.md)。仍使用原生 wasm-git 协议，不声称支持 Git 的每个命令行选项。


## 浅历史、作者签名与对象池

参见 [完整操作说明](server-storage.md)：本地/HTTP/SSH 选择性 clone/fetch、按需对象分片、
签名信任策略、跨 pack 基底校验、多节点复制、独立 fork pin 与故障回退。

当前层级仓库、四阶段 hooks、push options、部分/原子批次及预演合同见[中英文运维手册](../documents/SERVER_ADMINISTRATION.zh_CN.md)。
