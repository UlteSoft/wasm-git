# WasmHub 集成：签名、Pull Request 与受保护发布

**版本：** 2026-11-06 · [English](WASMHUB.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 后端契约

共用 client/server 引擎提供原生 Wasm 仓库传输、认证身份、共享存储 fork、持久化
Pull Request（PR）、讨论、审阅、CI 状态、受保护分支和审定发布。PR 与仓库操作使用
相同的有界二进制 RPC，支持 HTTP(S) 和 SSH。`wasm-git pr URL --help` 列出客户端接口；
平台可使用 Rust `pull` 类型实现自己的前端。

SSH 传输认证与 SSH 提交签名独立。传输身份控制仓库访问；提交签名根据明确管理的作者
信任表验证规范提交内容；PR 审批记录经过托管认证的身份和确切被审阅提交。

Git server 的传输协议本身不提供 GitHub PR。本托管层实现原生审阅工作流，不实现 Git
网络协议或 GitHub REST API。完整托管产品仍需账户/密钥注册、审阅 UI、OAuth/SSO、
CI 调度与执行隔离、通知、TLS、配额、备份和运维。尚未实现 issues、releases、行内审阅
线程、CODEOWNERS、合并队列、rebase 合并、具有投递重试能力的 webhook 服务。
另见[尚存的 Git 工作流缺口](GIT_PARITY.zh_CN.md)。

## 2. 标准 SSH 与 OpenPGP 签名

`commit-signed`、`sign-commit`、`trust-author`、`untrust-author` 接受
`--format native|ssh|openpgp`，默认仍为原生二进制 Ed25519。
SSH 使用已安装的 `ssh-keygen` SSHSIG 实现，用途域为 `commit@wasm-git`。
OpenPGP 使用已安装的 `gpg`，支持固定主证书之下的签名子钥。运行时不下载签名工具。

```sh
# 使用现有 SSH 签名密钥；已有 agent 加载私钥时也可传 .pub 文件。
wasm-git trust-author authors.bin 'Alice' ~/.ssh/id_ed25519.pub --format ssh
wasm-git commit-signed ./repo app.wasm -m 'Audited update' \
  --author Alice --format ssh --key ~/.ssh/id_ed25519
wasm-git verify-commit ./repo HEAD --trust authors.bin

# 导出一个 OpenPGP 公钥证书，使用完整主密钥指纹。
gpg --export FULL_PRIMARY_FINGERPRINT > alice.pgp
wasm-git trust-author authors.bin Alice alice.pgp --format openpgp
wasm-git commit-signed ./repo app.wasm -m 'Audited update' \
  --author Alice --format openpgp --key FULL_PRIMARY_FINGERPRINT
```

OpenPGP 选择器必须为完整 40 或 64 位十六进制指纹，可加 `!` 要求使用指定密钥。
验证要求恰好一个有效 detached signature，且属于预期主密钥；拒绝无效、过期或已撤销
的签名/密钥。验证使用临时隔离公钥环，关闭网络取钥和 agent 自动启动，不继承用户 GPG
配置或 trustdb。每个工具输出流最多 1 MiB，每次调用最多 30 秒。签名使用用户的密钥环
和已可用 agent，关闭交互式口令提示。

信任为精确的作者/密钥绑定，与 GPG ownertrust 无关。重新执行 `trust-author` 导入更新
的公钥证书，使新撤销和子钥生效；当前信任证书覆盖提交内嵌的旧证书。
`untrust-author authors.bin Alice` 移除该作者全部绑定；加 `--public KEY --format FORMAT`
仅移除一个。信任更新由管理员分发。尚未实现外部 SSH allowed-signers 策略导入、SSH
CA/KRL 策略、X.509 签名、签名 tag 或 push certificate。

签名绑定作者、声明时间戳、密钥身份、规范未签名提交、原始内容 ID、有序父提交和消息。
使用 uwgit 专用用途域，不能替代 Git 签名。两个外部格式都存储二进制公钥材料和二进制
签名，提交对象内没有 ASCII armor。仓库不保存私钥材料。

普通历史读取和 `fsck` 检查外部签名结构及对象完整性，不执行取钥工具。密码学验证使用
`verify-commit`，加 `--trust` 检查作者绑定。服务端 `--signed-authors authors.bin` 对新公开
历史同时强制执行这两项，包括已存储 dangling 对象和 PR 导入。既有公开祖先不要求重签。
原生 Ed25519 仍在对象验证时于进程内完成密码学检查。

信任文件改为位置式 schema version 2，增加公钥证书。请通过 `trust-author` 重建旧发行前
信任文件，不迁移未发行的 version 1 trust store。既有原生签名不可变提交的编码和 ID 不变。

## 3. 创建并审计提案

```sh
wasm-git pr https://wasm.example/team/project create \
  --source-repository alice/fork --source-branch feature --base-branch main \
  --title 'Correct the bounds check' --body 'Review the affected function' --draft
wasm-git pr https://wasm.example/team/project list --limit 30
wasm-git pr https://wasm.example/team/project show 1
wasm-git pr https://wasm.example/team/project files 1 --version 1
wasm-git pr https://wasm.example/team/project diff 1 --format json --section code
```

创建要求目标仓库读权限、源分支写权限以及可写服务端。此操作主动把捕获的源历史分享给
目标读者。源/目标分支均须存在；托管要求完整历史和已物化内容。PR 固定 base/head ID，
导入验证过的二进制对象图。之后的源分支 push 不会暗中改变已审提案。
`update NUMBER --version VERSION --refresh` 同时捕获新 head 和当前目标，刷新时再次检查
源写权限。

默认 diff 比较唯一共同祖先与提案 head，类似三点审阅。`--against target` 直接比较固定的
目标与 head；`files` 接受相同参数并返回实际 `diff_base`。没有唯一共同祖先时，默认 diff
明确失败，仍可使用 target 比较。合并不会猜测多个共同祖先中的某一个。

多文件提案使用 `diff NUMBER --file path/module.wasm`；旧单模块仓库使用空路径。
内容通过 1 MiB 二进制分片读取，受 PR version 和原对象 hash 约束。客户端验证完整字节后，
调用正常的 section/function/block/instruction 审计。新增/删除文件显示实际存在的一侧，
不会伪造空 Wasm；同样遵守 `--section`，保留带原始区间的所选字节证据，不筛选时 JSON
保留完整实际文件。`--compact` 省略解释区域树但保留字节证据。JSON 仅为输出形式。

## 4. 讨论、审阅和 CI

创建之后的每次修改必须提供当前显示的 `--version`。并发编辑拒绝旧版本，不覆盖另一人
的修改。事件保留认证身份、时间、base/head；创建和元数据更新事件还保存当时标题与正文。

```sh
# 每次修改后刷新 show，使用新返回的 version。
wasm-git pr URL ready 1 --version VERSION
wasm-git pr URL comment 1 --version VERSION --body 'Reviewed the byte evidence'
wasm-git pr URL review 1 --version VERSION --decision approve --body 'Looks correct'
wasm-git pr URL check 1 --version VERSION --context build --state success
wasm-git pr URL merge 1 --version VERSION --head HEAD_ID --base BASE_ID \
  --method merge -m 'Merge the reviewed change'
```

读者可评论；作者和目标分支写者可编辑元数据、关闭/重开、切换草稿/就绪状态。仅目标分支
写者能提交审阅和检查结果，不能批准自己的 PR。已合并 PR 可继续讨论，但不能重开或编辑；
合并结果待恢复时，必须先恢复再编辑。

审批使用每位有资格审阅者针对当前确切 base/head 的最新非 comment 决定。任何有效的
request-changes 都阻止合并。任一提交改变后，旧审批和检查不再满足发布条件，但事件历史
仍保留。仅修改标题/正文不改变被审字节，因此相同提交对的审批继续有效。合并时重新检查
审阅者权限。检查按 context 和提交对选取最新合格结果；pending/failure 不等于 success。
上报检查不会执行 CI 作业，作业由 CI 服务执行，再以其身份认证上报。

## 5. 分支保护与签名合并

将管理员提供的外部 JSON 编译为二进制策略：

```json
{"version":1,"rules":[{"repository":"team/*","branches":["refs/heads/main"],
"required_reviews":1,"required_checks":["build"],
"check_issuers":{"build":["ci"]},"methods":["FastForward","Merge","Squash"]}]}
```

```sh
wasm-git-server compile-protection protection-input.json protection.bin
wasm-git-server --root /srv/uwgit --policy principals.bin --allow-write \
  --protection protection.bin --signed-authors authors.bin \
  --merge-author 'Merge service' --merge-signing-format ssh \
  --merge-signing-key /etc/uwgit/merge-key
```

仓库/分支模式支持精确值或尾部 `*`，分支使用完整 `refs/heads/` 名称。所有匹配规则共同
生效；即使审批/检查数量为零，匹配规则仍要求 PR 发布。直接 push、force push、只改引用、
原子/部分/上传批次都不能绕过保护。部分批次内允许的未保护引用仍各自返回结果。有文件系统
写权限的本地管理员属于受信任边界。

允许的检查上报身份还须有目标分支写权限。请为必要 context 设置 `check_issuers`，防止
普通写者冒充 CI；不设置时，任一授权目标写者均可上报。作者、审阅者和 CI 应使用不同策略
身份，共享 token 只有一个身份，不能提供独立审批。

访问与分支保护策略在服务端启动时加载，替换后须重启服务。作者信任文件在每次验证/准入
时读取，原子替换后的信任更新对后续操作生效，无需重启。

合并支持 fast-forward、双父语义 merge、单父 squash。目标仍须等于已审 base。请求中的
两个 ID、PR version、权限、合并方式、审批、检查、对象图、签名、receive hooks 均须通过。
冲突不发布部分输出。merge/squash 可使用服务端配置的签名身份；启用签名准入时该身份必须
可信。快进保留源提交及其签名。squash 不公开源父链，准入检查最终可达历史。

## 6. 持久性、保留与恢复

PR 位于共享仓库元数据 `pulls/NUMBER.bin`，格式为 `UWGPR001`、八字节大端解码长度、
Zstandard 压缩的位置式二进制记录。`counter.bin` 保存二进制递增编号，均不存 JSON。
更新使用仓库写锁与原子持久替换。失败创建可留下编号空隙，不重用已发编号。

记录编码/解码上限均为 16 MiB，最多 4096 个事件及 4096 个捕获版本。标题最多 256 字节，
正文/审阅评论 64 KiB，检查 context 128 字节、描述 4096 字节。列表采用排他的 `after`
游标，请求数量 1–100，响应数据预算 32 MiB；可能少于请求数并返回后续游标。长度按 UTF-8
字节计算。

所有捕获版本，包括关闭/已合并 PR，均为 GC 根。删除源分支/fork 不会删除审阅证据。
`fsck` 检查 PR 编码和所保留提交的存在性。PR 元数据仅属于托管端：clone/fork 和对象 bundle
不导出讨论、策略或账户。备份应包含完整仓库元数据和外部策略/信任配置。尚未提供 PR 删除
或归档命令，审阅历史被有意持久保留。

引用事务前先把 PR 准备为 `Merging`。之后精确匹配的 reflog 事件证明候选已发布。丢失回执时，
`pr recover NUMBER --version VERSION` 可完成合并状态，即使目标随后再次前进。若目标未变且
没有发布记录，恢复为 open；不一致的结果保留明确不确定状态。存在待恢复合并时禁止修改
所依赖的 reflog。post-receive 失败不会撤销发布，不能将超时视为回滚。

## 7. 验证与上游参考

回归覆盖实际 HTTP 策略执行、审批/检查时效、跨 fork 保留、原字节 diff、语义 merge/squash、
签名准入、receive hooks、旧版本和丢失回执恢复。外部签名测试使用真正的临时 OpenSSH/GnuPG
密钥，覆盖字段篡改、签名子钥和撤销。本机独立 `sshd` 还验证主机公钥固定、forced-command
身份绑定、clone/push/fork、PR 创建/diff/合并。测试及产物在项目 `.tmp/`、64 GiB cgroup 中运行。

参考契约：[Git 签名配置](https://git-scm.com/docs/git-config)、
[OpenSSH 签名](https://man.openbsd.org/ssh-keygen)、
[GnuPG 输入输出](https://www.gnupg.org/documentation/manuals/gnupg/GPG-Input-and-Output.html)、
[GitHub PR](https://docs.github.com/en/rest/pulls/pulls)。它们用于指导等价工作流，不表示 API
兼容或 Git/GitHub 全功能已经齐全。
