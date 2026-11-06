# HTTP 与 SSH 服务

`wasm-git server` 为同一服务根目录下的多个 uwgit 仓库提供读取及受控引用更新。
每个子目录是一个独立仓库；服务不执行 Wasm。客户端和服务端使用 uwgit 的对象及 RPC 格式。

## HTTP

```sh
mkdir -p /srv/uwgit
wasm-git init /srv/uwgit/project
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419
wasm-git clone http://127.0.0.1:9419/project ./peer
```

默认监听 loopback，默认只读。写入服务使用管理员提供的 token 文件：

```sh
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419 \
  --token-file /run/secrets/uwgit-token --allow-write
export UWGIT_REMOTE_TOKEN='your-token'
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
wasm-git clone ssh://alice@example.org/srv/uwgit/project ./peer
wasm-git remote ./peer add origin ssh://alice@example.org/srv/uwgit/project
wasm-git fetch ./peer origin
wasm-git pull ./peer origin main -m 'integrate remote'
wasm-git push ./peer origin main:main
```

URL 的绝对路径父目录成为服务根，最后一段为仓库名。客户端调用
`ssh -T ... -- USER@HOST 'wasm-git-server server --stdio --root ...'`，通过标准输入输出交换单个请求。
服务端使用 SSH 的系统用户权限，stdio 默认允许写入；HTTP token 不会转发给 SSH。
远端程序及路径逐项 shell 引用，客户端不将输入拼接成未引用的 shell 命令。

`UWGIT_REMOTE_PROGRAM` 可指定远端单个可执行文件路径，`UWGIT_SSH_PROGRAM` 可指定本机单个 SSH
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
引用批次通过持久 journal 在再次获取 writer 锁时向前恢复引用和 reflog；最终发布或同步失败仍明确报告不确定结果。

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

HTTP 仅接受 `POST /v1/uwgit`，关闭每次连接，要求唯一的 `Content-Length`，拒绝
`Transfer-Encoding`、重复 header、超限 header 及不完整输入。HTTP 请求格式依据
[RFC 9112](https://www.rfc-editor.org/rfc/rfc9112.html) 的 framing 规则。
请求中的 repository 指定服务根下受限的仓库路径，可用 `team/project.git`。
每段最多 128 个 ASCII 字节，总长最多 512 字节、16 段；拒绝空段、穿越、符号链接和仓库内部嵌套；
remote 配置名仍使用独立的 64 字节限制。

HTTP body 与 SSH stdio 使用同一帧：

| 字段 | 编码 |
|---|---|
| magic | 8 字节 `UWGRPC02` |
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
它使用原生 uwgit 线协议；提交作者签名、浅历史与部分克隆见 [服务器存储扩展](server-storage.md)。
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

压缩传输头为 `UWGNET01`，每个独立 Zstandard frame 最多还原 1 MiB；内部记录为 `UWGTRN01`。
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
{"version":1,"principals":[{"name":"alice","token_sha256":"<64 lowercase SHA-256 hex digits>","grants":[{"repository":"alice.*","read":true,"write":["refs/heads/*","refs/tags/*"],"force":false,"create":true}]}]}
```

```sh
wasm-git-server compile-policy /tmp/policy-input.json /etc/uwgit/policy.bin
wasm-git-server --root /srv/uwgit --policy /etc/uwgit/policy.bin --allow-write
```

仓库和引用规则只支持精确匹配或末尾 `*` 前缀匹配；没有隐式默认授权。
HTTP token 的 SHA-256 与策略中的凭据比较，日志只记 principal 名称。
引用写入要求匹配 write 规则；强制更新还需匹配 force 规则及服务器 `--allow-force`。
fork 要求源仓库 read 和目标名称 create；不会因可读取源仓库而获得其他租户的目标权限。
策略改变后重启服务生效。SSH 通过管理员设置的 forced-command 固定
`--stdio --root /srv/uwgit --policy ... --principal alice`；身份不能来自客户端请求体。
`--read-only` 可限制 SSH key，也可限制 HTTP 服务。不同分支不构成读取隔离，私有数据应放在不同仓库。

```sh
wasm-git push ./peer origin --tags --atomic
wasm-git push ./peer origin main:main HEAD:refs/tags/release --atomic
```

`publish_batch` 将 1–4,096 个显式 branch/tag 更新放在同一事务中。所有旧 ID 租约、目标对象、
快进/标签替换规则、工作区保护和命名冲突先检查；任一失败均不发布引用。原子批次使用本地
持久化引用事务日志，崩溃后任何加锁查询先完成整批恢复；裸服务器 HEAD 可更新，已跟踪工作区
的当前分支仍受保护。响应携带请求摘要收据，传输故障时客户端明确报告发布结果未知。
默认非原子 `--tags` 仍逐项报告结果；带 `--atomic` 时拒绝整个批次，不谎报部分成功为原子成功。

## 接收检查与审计接口

`--pre-receive /absolute/program` 和 `--post-receive /absolute/program` 只运行管理员指定的
可执行文件，不执行仓库里的 Wasm 或脚本，也不拼接 shell 命令。每个钩子最多 15 秒。
stdin 为原生二进制 `Vec<RefUpdate>`，同时提供以下环境变量：

- `UWGIT_ACTOR`：已认证身份；兼容管理员 token/SSH 模式分别为 `token`/`ssh`。
- `UWGIT_REPOSITORY`：服务器仓库的绝对路径。
- `UWGIT_UPDATES_FILE`：本批次二进制更新记录的临时路径。
- `UWGIT_QUARANTINE_BUNDLE`：接收前是完整、已校验、尚未安装的原生 bundle；接收后为空文件。

接收前钩子拒绝时，新对象和引用均不会进入公开库，隔离文件退出时删除。
钩子应检查隔离 bundle，不要递归更新正在接收的仓库。接收后钩子失败不会把已完成推送报告为失败，
其失败写 stderr 和二进制审计事件；上层可据此实现合并准入、索引通知或 CI 调度。

服务根的 `receive-audit.bin` 为长度分帧的原生记录，包含版本、事务 ID、时间、身份、仓库、
旧/新引用及 prepared/accepted/rejected/unknown 状态，fork 另记目标名称。
prepared 事件在发布前持久化；崩溃或最终审计写入失败可能只留下 prepared，必须结合实际引用核对，
不能把它视为已接受。接收后事件失败不改变已完成事务的事实。管理员负责保留与轮转日志；
日志是操作审计，不是提交作者签名或不可篡改证明。

```sh
wasm-git-server audit-log /srv/uwgit/receive-audit.bin
```

上面的 JSON 只是对外查询输出，内部策略、日志、对象、事务、压缩历史和网络 payload 均为二进制。

## 并发与 fork 发布

服务端在已有固定格式文件的 inode 上取得进程间锁，协调 HTTP worker 与独立 SSH 进程，
不会因网络读者竞争本地 fail-fast writer lock 而互相拒绝。读取引用只检查引用目标提交，
不解压所有 Wasm；完整 transfer/fsck 另做对象验证。服务不会给只读查询新建锁文件。
本地 CLI 维护仍保留原有写锁及崩溃检查规则，遇到其占用会明确报忙。

接入循环会回收任意已完成 worker，不等待最早的慢连接；16 个活动连接全满时快速拒绝。
连接仍有 15 秒总 I/O 时限。单仓库服务操作串行协调，压缩和对象工作使用共享线程池；
这不等于无限并发或多节点分布式锁。

远端 fork 先在不可寻址的临时目录完成，再发布目录和有界收据，不再在发布后读取庞大引用表。
loose 基础历史先压成共享 pack，fork 后各自增加私有 pack；GC 保留仍完整可达的共享基础 pack，
不会每次将整份基础历史复制。显式 prune 必须丢弃基础 pack 中部分对象时可重写本仓库的 pack，
其他 fork 的 hardlink 不受影响。普通同文件系统 fork 不依赖源仓库路径，删除源仓库也可读取。

这些接口可作为 wasmhub 的存储与接收核心。账号注册、组织成员管理、Web UI、PR/审批、CI 执行、
限流计费、日志归档与多主引用协调属于上层；多节点不可变对象池已由原生后端提供。仓库创建/删除仍通过受控本地 Repository/CLI；
浅历史/部分克隆、作者签名、跨 pack delta 和多节点对象池已实现，见 [工作流与协议边界](server-storage.md)。仍使用原生 uwgit 协议，不声称支持 Git 的每个命令行选项。


## 浅历史、作者签名与对象池

参见 [完整操作说明](server-storage.md)：本地/HTTP/SSH 选择性 clone/fetch、按需对象分片、
签名信任策略、跨 pack 基底校验、多节点复制、独立 fork pin 与故障回退。

当前层级仓库、四阶段 hooks、push options、部分/原子批次及预演合同见[中英文运维手册](../documents/SERVER_ADMINISTRATION.zh_CN.md)。
