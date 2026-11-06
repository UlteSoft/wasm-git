# uwgit 使用手册

**版本：** 2026-11-06 · [English](USER_MANUAL.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 约定与准备

示例使用启用 client feature 的 `wasm-git`。`DIR` 是 uwgit 仓库目录，不是 `.uwgit`
元数据目录。独立审计参数指普通文件；暂存树路径相对于 `DIR`。
`REV` 表示受支持的修订表达式，例如分支、标签、完整提交 ID、`HEAD` 或 `HEAD~1`。
`TARGET_ID`、`REVIEWED_HEAD_ID` 等名称是占位符，必须替换为操作返回的实际完整 ID。

不能假设每个 Git 选项都有对应实现。使用具体命令的 `--help` 查询参数。
脚本应检查退出状态、结构化结果与诊断；JSON 中的解释不完整字段不等于命令失败。
构建方法见[项目介绍](../README.zh_CN.md)。将可执行文件放入 `PATH`，或把示例中的命令
替换为完整路径。在本源码仓库工作时，临时仓库、报告、导出模块和测试输出均放到 `.tmp/`。

## 2. 检查单个模块

```sh
wasm-git inspect app.wasm --json
wasm-git inspect app.wasm --standard core3 --json
wasm-git features --json
wasm-git features threads --standard core3 --json
```

物理审计描述原始头、section、已知嵌套区域、偏移、诊断和验证结果。
标准配置有 `core1`、`core2`、`core3`、`all-known`，默认使用最后一项。
配置改变准入报告，不改变原字节和已识别立即数长度。模块在一个配置下合法，不代表在另一个
配置下也合法。

存储接受范围宽于完整语义解释。未知版本/layer 可保留 opaque 后续区域；已知损坏编码明确
诊断。不符合某个社区约定的 custom payload 保留原始证据，不静默丢弃。

## 3. 比较与审阅证据

```sh
wasm-git diff old.wasm new.wasm
wasm-git diff old.wasm new.wasm --format json
wasm-git diff old.wasm new.wasm --compact --format json
wasm-git diff old.wasm new.wasm --short
wasm-git --threads 1 diff old.wasm new.wasm --compact --format json
```

工具需要完整 region 树与位置解释时使用完整 JSON。
紧凑 JSON 保留精确编辑、验证、诊断、函数变化及非 supported/deprecated 特性证据，并记录
省略内容。short 仅用于摘要。

建议按以下顺序审阅：

1. 核对模块摘要与输入来源，确认文件或修订正确。
2. 查看物理变化状态与验证诊断。
3. 检查相关变化的范围和旧/新字节证据。
4. 查看函数、块、指令对齐及依赖绑定。
5. 判断唯一身份或最小修改前，检查歧义、搜索完整性与回退标记。
6. 将调试标注视为编译器辅助信息，不独立证明源码位置或符号可信。

`exact_minimal=false` 表示采用有界回退，不表示还原近似；脚本仍保留完整变化字节。
数值不变的调用可能改绑 import，应结合调用绑定与导出依赖，不仅看 opcode 文本。

## 4. 选择 section

```sh
wasm-git diff old.wasm new.wasm --section code
wasm-git diff old.wasm new.wasm --section code --section data --format json
wasm-git diff old.wasm new.wasm --section custom
wasm-git diff old.wasm new.wasm --section custom:producers
wasm-git diff old.wasm new.wasm --section id:200 --section ordinal:3
```

多个选择器取并集。Core 名称包括 `type`、`import`、`function`、`table`、`memory`、
`global`、`export`、`start`、`element`、`code`、`data`、`data_count`、`tag`。
`custom:NAME` 按 UTF-8 原文精确匹配，不解释 glob；`id:N` 范围为 0–255；
`ordinal:N` 分别选择两侧零基物理序号。还可使用 `opaque`、`all`。

Component 外层支持 `core_module`、`core_instance`、`core_type`、`component`、
`component_instance`、`alias`、`component_type`、`canonical`、`component_start`、
`component_import`、`component_export`。嵌套内容保留原 payload，不递归推测 Core 对应。

检查 `selection_complete` 和 `unmatched_selectors`。名称或边界扫描不完整时，未观察到
匹配不能证明 section 不存在。区分 `content_changed`、`payload_changed`、
`position_changed`。模块头不属于 section。筛选报告的 `whole_module_patch=false`，
也不计算跨节依赖；整次更新应使用完整 diff 与完整补丁。

## 5. 将调试信息映射回原二进制

```sh
wasm-git debug-map app.wasm --offset 173 --json
wasm-git debug-map app.wasm --code-offset 12 --json
wasm-git debug-map app.wasm --symbol calculate --json
wasm-git debug-map app.wasm --function 3 --json
wasm-git debug-map app.wasm --source /source/module.cpp --line 70 --json
```

每次使用一种选择方式。`--offset` 是绝对文件字节偏移；`--code-offset` 采用报告说明的
code-section 坐标基准。能够建立全局索引时，函数索引包含 import。
源码路径与符号按记录原文匹配；编译期路径重映射会改变查询字符串。

输出可含源码行范围、内联调用点、声明、支持的变量位置和原始指令范围。
核对模块 SHA-256、坐标基准、状态、诊断及范围解释。
不会通过执行 Wasm 或获取外部 URL 来补全缺失行表、不支持表达式或冲突 section。
完整 diff 自动附带可用映射；映射缺失不影响字节证据。

## 6. 创建与应用二进制补丁

```sh
wasm-git patch old.wasm new.wasm --output update.uwgp
wasm-git apply old.wasm update.uwgp --check
wasm-git apply old.wasm update.uwgp --output restored.wasm
```

源文件必须与补丁预期原字节一致。验证成功后发布新目标文件，不覆盖已有目标。
`--check` 只验证和审计，不写目标。交换 `patch` 的两侧即可创建反向补丁。
也可从同一仓库快照读取提交内容：

```sh
wasm-git patch main topic --repo ./history --file modules/app.wasm --output update.uwgp
wasm-git apply main update.uwgp --repo ./history --file modules/app.wasm --check
```

`--file` 选择命名树路径。补丁操作不会提交或更新引用。
JSON 是审阅报告，`.uwgp` 才包含二进制还原数据。

## 7. 建立单模块历史

```sh
wasm-git init ./history
wasm-git commit ./history old.wasm -m 'Initial module'
wasm-git commit ./history new.wasm -m 'Change calculation'
wasm-git log ./history HEAD
wasm-git inspect HEAD --repo ./history --json
wasm-git diff HEAD~1 HEAD --repo ./history --format json
wasm-git checkout ./history HEAD restored.wasm
wasm-git fsck ./history
```

带模块位置参数的提交保存单份原始二进制。导出 checkout 要求新文件路径，应与分支 checkout
和路径恢复区分。仓库也可保存命名树；把原先未命名模块纳入 tree 时，需要显式路径信息。

## 8. 跟踪模块树

```sh
wasm-git init ./work
mkdir -p ./work/modules
cp app.wasm ./work/modules/app.wasm
cp library.wasm ./work/modules/library.wasm
wasm-git add ./work modules/app.wasm modules/library.wasm
wasm-git status ./work
wasm-git diff ./work --cached --format json
wasm-git commit ./work -m 'Track application and library'
wasm-git ls-tree ./work HEAD --json
wasm-git show ./work HEAD --file modules/app.wasm --output exported.wasm
```

index 是待提交的完整树；不带模块参数的 commit 保存该 index。
空树、增加和删除均可进入历史。目录隐式表示；重命名以删除和增加呈现，不虚构身份。

`add`、`rm`、`mv`、`restore` 和路径 checkout 使用仓库相对选择器。
精确文件、目录前缀与支持的 pathspec 有明确规则。
未匹配或文件/目录阻挡可使整批操作在写入前拒绝；工作区恢复可能覆盖未跟踪文件时进行保护。

```sh
wasm-git diff ./work --format json
wasm-git diff ./work --cached --section code
wasm-git restore ./work --staged -- modules/app.wasm
wasm-git restore ./work --source HEAD --staged --worktree
```

普通 diff 比较工作区与 index；`--cached` 比较 index 与 HEAD。
取消暂存保留工作字节；恢复工作区会丢弃所选 tracked 修改，执行前应检查状态并保存需要的内容。

## 9. 分支、标签与工作区切换

```sh
wasm-git branch ./work topic HEAD
wasm-git switch ./work topic
wasm-git branch ./work -vv
wasm-git tag ./work release-candidate
wasm-git switch ./work main
wasm-git checkout ./work -b experiment HEAD
wasm-git checkout ./work HEAD -- modules/app.wasm
```

分支 checkout 与 switch 在工作区和多工作树保护下切换分支；路径 checkout 恢复所选路径；
三个位置参数的导出形式用于写新副本，含义不同。
Detached HEAD 与 orphan 分支由显式 switch 选项提供，不代表全部 Git checkout 选项都可用。

Linked worktree 共享对象与引用，独立维护 HEAD/index。
`worktree add/list/remove/move/lock/unlock/prune/repair` 管理工作树，分支占用阻止跨树冲突
写入。fork 则是共享不可变数据的独立仓库。

## 10. 合并与冲突解决

```sh
wasm-git merge-base ./work main topic
wasm-git merge ./work topic -m 'Integrate topic'
wasm-git merge ./work topic --ff-only
```

自动合并采用保守规则。独立函数和受支持指令修改可能合成；重叠或无法证明的变换返回冲突
证据，不构成行为正确性证明。`--ff-only` 拒绝分叉，`--no-ff` 在适用时要求合并提交，
`--squash` 只准备 index，不发布合并父关系。

录制冲突流程：

```sh
wasm-git merge ./work topic -m 'Integrate topic' --record-conflict
# Review conflict reports, edit module files, and stage the complete resolution.
wasm-git add ./work modules/app.wasm
wasm-git diff ./work --cached --format json
wasm-git merge ./work --continue --resolved-index
```

应审阅完整 index，包括非冲突路径。录制的工作区操作在适用时准备 stage 0 与冲突 stages。
未解决 index 不得发布为 tree。`--abort` 在安全检查下恢复记录检查点；`--quit` 删除续作状态
并保留当前文件，二者不可互换。

无 pending 状态时也可显式绑定审定的不可变 ID：

```sh
wasm-git merge ./work TARGET_ID -m 'Reviewed resolution' \
  --resolved-index --expect-head REVIEWED_HEAD_ID --expect-branch main
```

该操作使用现有完整 index，并检查预期 HEAD。
不能把未暂存文件或不完整 index 当成已经审定的合并结果。
外部 mergetool 导出的是审阅产物，不是仓库内部状态。

## 11. 重放、撤销与历史查询

```sh
wasm-git cherry-pick ./work CHANGE_ID --record-conflict
wasm-git revert ./work CHANGE_ID --record-conflict
wasm-git rebase ./work main --record-conflict
wasm-git reflog ./work
wasm-git trace ./work HEAD --file modules/app.wasm --range 40:48
wasm-git blame ./work HEAD --file modules/app.wasm --range 40:48
```

Pick/revert 支持冻结的多目标队列及 continue、skip、abort、quit。
重放 merge 提交需选定 `--mainline`。Rebase 支持线性重放，不提供交互编辑或通用 merge
拓扑保留。Pending 状态固定源 ID 与恢复检查点；并发变化可能要求重新审阅，不能盲目继续。

`reset --soft` 移动分支但保留 index 和工作区；`--mixed` 还恢复 index；`--hard` 进一步恢复
tracked 工作内容，会丢弃所选 tracked 修改。Stash 保存受支持 tracked 状态，不保存所有
untracked 文件。Revert 创建反向提交并保留旧历史。

Trace/blame 在预算内报告字节来源候选与 frontier；重复字节不证明唯一来源。
其他查询包括 `rev-list`、`show-ref`、`for-each-ref`、`shortlog`、`range-diff`、`describe`、
`archive` 和不切换工作区的 `bisect`。它们基于 uwgit 实际对象模型，选项子集请查看帮助。

## 12. 克隆、拉取、推送与离线交换

```sh
wasm-git clone ./history ./peer
wasm-git bundle ./peer peer.uwgb
wasm-git unbundle ./history peer.uwgb incoming
wasm-git remote ./peer add origin https://host.example/project
wasm-git fetch ./peer origin
wasm-git push ./peer origin main:main
wasm-git push ./peer origin --tags
```

Unbundle 在验证及快进规则下更新显式接收分支。Fetch 安装已验证对象与 tracking 状态，
不移动本地分支或更改工作区；pull 进一步整合，可能产生冲突。
完整 clone 保留广告分支与标签，随后可用 `restore DIR --source HEAD --staged --worktree`
物化树文件。

HTTP 凭据通过 `UWGIT_REMOTE_TOKEN` 提供，不保存到 remote 配置。
非 loopback 使用 HTTPS；明文远端需要明确允许。SSH 使用远端服务程序与系统账户策略，
详见部署手册。

非快进 push 同时需要服务端许可和客户端精确 `--force-with-lease OLD_ID`。
`--atomic` 请求多引用原子推送，任一项拒绝则不发布整批。
网络失败结果不明时，应先查询远端引用，再判断是否失败或更换 lease 重试。

## 13. 浅历史与部分内容

```sh
wasm-git clone SOURCE ./partial --depth 10 --branch main --filter blob:none
wasm-git clone-status ./partial
wasm-git fetch ./partial --deepen 20
wasm-git fetch ./partial --unshallow
wasm-git fetch ./partial --filter all
wasm-git hydrate ./partial
```

SOURCE 可为本地仓库或受支持 HTTP(S)/SSH 地址。浅边界保留真实父 ID；deepen 从原边界
继续，unshallow 只能取得源实际拥有的历史。
`blob:none` 省略 Wasm 内容；`blob:limit=N` 保留不超过 N 字节的内容；
`path:PATH[,PATH]` 保留指定路径或目录前缀；`all` 请求完整内容。

Fsck 单独报告承诺内容。需要对象的操作先补拉并验证所需模块；只读图遍历不会无差别下载所有内容。`promisor DIR SOURCE_A SOURCE_B` 配置有序回退源。
未被承诺的缺失对象仍是损坏，不能隐式视为 promise。

## 14. 签署与验证提交

```sh
wasm-git keygen author.key author.pub
wasm-git trust-author trusted-authors.bin 'Alice <alice@example.org>' author.pub
wasm-git commit-signed ./history app.wasm -m 'Reviewed change' \
  --author 'Alice <alice@example.org>' --key author.key
wasm-git verify-commit ./history HEAD --trust trusted-authors.bin
wasm-git sign-commit ./work EXPECTED_HEAD_ID \
  --author 'Alice <alice@example.org>' --key author.key
```

密钥为 uwgit 二进制 Ed25519 格式，不是 SSH/OpenPGP 编码。私钥不得进入源码控制。
验证区分密码学有效性和信任表内的作者/公钥绑定。签名时间戳是签署者声明，不是可信时间证明。
`sign-commit` 要求预期 HEAD，适用于 tree，创建新不可变签名提交，不在旧 ID 下改写内容。
`untrust-author` 可撤销一个公钥或某作者全部公钥。

## 15. 压缩、fork 与对象池

```sh
wasm-git gc ./history -n
wasm-git gc ./history
wasm-git repack ./history
wasm-git fork ./history ./fork
wasm-git fsck ./fork
wasm-git gc ./history --prune=now
```

Dry-run 只生成计划。普通 GC/repack 压缩并保留对象；显式 prune 只移除保留根闭包外对象。
真正 prune 后，若无其他副本或备份便无法还原。共享基础 pack 和跨 pack delta 不改变对象 ID。
本地 fork 要求新目标和同文件系统硬链接能力；失败后应先检查目标再决定复用。

```sh
wasm-git pool attach ./history https://node-a.example/pool https://node-b.example/pool
wasm-git pool sync ./history
wasm-git pool status ./history
wasm-git pool sync ./history --evict
wasm-git pool release ./history
```

使用对象池前阅读部署手册。Evict 等待全部配置节点确认拥有者 pin；release 先补齐必要本地
内容，再移除该拥有者的远端 pin。这些是存储管理操作，不代替独立备份或引用共识。

## 16. 故障处理与解释边界

- **目标已存在：** 为 patch、export、clone、fork 选择新路径，不为满足命令而删除有用文件。
- **Busy 或遗留锁：** 检查所有写入者和 linked worktree；确认无人持有后才移除持久 writer/index
  锁。不得删除仍在使用的对象池锁 inode。
- **未知语法或禁用特性：** 查看原始审计、标准配置和覆盖信息；改变展示配置不会实现缺失语义。
- **承诺对象不可用：** 恢复授权源或显式配置回退，不能以空模块替代。
- **Lease 或工作区不一致：** 重新查询并审阅；旧预期 ID 表示当前状态已不再绑定原审阅。
- **Trace 或对齐不完整：** 查看 frontier 与预算；精确字节还原和完整解释对应是两种独立性质。

部署限制与备份见[服务端运维](SERVER_ADMINISTRATION.zh_CN.md)，存储版本与字段见
[二进制格式](BINARY_FORMAT.zh_CN.md)。
