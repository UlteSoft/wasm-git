# wasm-git 使用手册

**版本：** 2026-11-07 · [English](USER_MANUAL.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 约定与准备

示例使用启用 client feature 的 `wasm-git`。`DIR` 是 wasm-git 仓库目录，不是 `.wasm-git`
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

### 按格式支持语言

调试信息按实际格式解析，不设置源语言白名单。DWARF 2–5 编译单元展示 `language_code`、
DWARF 语言名称、`producer`、源文件名称、版本与地址大小。C、Objective-C、C++、
Objective-C++、Rust、TinyGo 及其他输出 DWARF 的 Wasm 工具链共用范围、内联作用域与
变量位置解析器。未知语言编号保留为数值证据，不因此停止解析其可读 DWARF 内容。

`name` 视图保留 module、function、local、label、type、table、memory、global、element、
data、field、tag 名称。local/label 保留函数作用域，field 保留类型作用域。
支持包括 Go 链接器输出在内的补齐 Wasm LEB 长度。重复标签保留为候选；有歧义的函数名称表
不会被选作确定的身份提示。

source-map v3 支持平面映射、内嵌索引映射、source/name 表、未映射边界，以及 base64 或
百分号编码的 `data:application/json` 引用。generated column 是 **Wasm 文件绝对偏移**，
generated line 为零；展示的源码行列从一开始。只有真实指令或函数体边界能够形成源码范围，
不会把一个函数的最后一条映射延伸到没有映射点的后续函数。索引映射中的 URL 子项不会被获取。
源码路径只是记录标签，不打开或执行源码文件、`sourcesContent`。空来源名称保留为 null，
路径标签保留 sourceRoot 前缀，不规范化 URL，也不根据本地外部文件路径暗中补全来源。

当前 MoonBit `wasm-gc` 后端及 AssemblyScript 在启用对应选项后输出 source map；
MoonBit 普通 `wasm` 后端当前只输出名称，不输出源码映射。已有名称可查询，缺失行信息不补猜。
实际存在的元数据取决于编译器版本、后端和编译参数。

### 显式调试文件与来源

```sh
wasm-git debug-map app.wasm --debug-file app.wasm.map --source /source/main.mbt --line 12 --json
wasm-git inspect app.wasm --debug-file app.debug.wasm --json
wasm-git diff old.wasm new.wasm --old-debug-file old.wasm.map --new-debug-file new.wasm.map --format json
```

`debug-map`、`inspect` 可重复使用 `--debug-file`；diff 可分别重复使用
`--old-debug-file`、`--new-debug-file`。接受 source-map JSON、包含 DWARF 的 Wasm 文件、
以及 `函数索引:名称` 的数字索引符号表。仓库文件树需要 `--file`，树 diff 的两侧均需存在该模块。
仅 section 的 diff 不能组合这些外部调试选项。

每个成功接入的文件记录名称、字节数、SHA-256、格式和关联依据。
`matching_code_payload` 表示独立 DWARF 文件的 code payload 与原模块完全相同，不表示作者签名。
不含 code 的 DWARF、source map、符号表标记为 `explicit_input_unverified`：关联由调用者提供，
wasm-git 无法证明编译器为当前模块生成了它。`embedded_reference` 表示映射内嵌于原 Wasm。
DWARF PC 仍使用原模块 code payload 基址，`metadata_source` 标识元数据偏移所属的外部文件。
source-map 名称没有伪造的 JSON 字节偏移。格式不匹配时记录诊断，原始字节审计继续；
显式指定的文件不存在则报告 I/O 错误。内嵌引用不会触发文件或网络访问。

这些 JSON 是**编译器输入和面向用户的展示格式**，不是仓库内部存储。
对象、历史、索引和 pack 继续使用现有二进制格式。

### 普通 Go 的运行时符号

```sh
wasm-git debug-map go.wasm --symbol main.calculate --json
wasm-git debug-map go.wasm --go-pc 268435456 --json
```

普通 Go 的 Wasm 目标关闭 DWARF，将运行时 `pclntab` 放在线性内存数据中。
支持 Go 1.2、1.16、1.18、1.20 表布局，以及当前 Go 使用的函数索引入口编码。
解析时有界还原 active data 段之间被省略的零字节，核对函数表与原函数布局，并保留原符号名称。

`go_lines` 使用 `go_wasm_virtual_pc`：`PC_F << 16 | 恢复点`，其中
`PC_F = 0x1000 + 已定义函数序号`。仅在输出 Wasm 函数索引时加上 import 函数数量。
这些数值**不是 Wasm 指令字节偏移**。函数符号可以定位原文件中的函数范围，
`go_entry_source` 单独标明虚拟入口位置。Go 行表不会被放进物理指令源码范围索引。
通过 `--go-pc`、`--function`、`--symbol`、`--source` 查询，避免伪造逐指令的源码精度。
导入内存、重叠初始化、不支持的偏移表达式、有歧义的候选表或未知后续格式均产生约定诊断。

解释设有资源上限：128 MiB 调试输入/初始化内存跨度、每表 200,000 条记录、两百万遍历步、
16 KiB 字符串、最多 32 个显式输入。未知、被裁剪、不支持或厂商私有的元数据仍受精确字节与
custom-section 审计覆盖。支持某种格式不等于所有语言都会输出调试信息，也不等于能够证明
优化前的源码意图。

互操作依据包括社区 [Wasm 调试约定](https://github.com/WebAssembly/tool-conventions/blob/main/Debugging.md)、
[ECMA-426 source map](https://tc39.es/ecma426/)、
[Go 链接器表](https://github.com/golang/go/blob/master/src/cmd/link/internal/ld/pcln.go)、
[MoonBit 后端能力](https://github.com/moonbitlang/moon/blob/main/crates/moonutil/src/target.rs)。
这些 custom-section 约定是可选元数据格式，不增加 Core Wasm 的合法性要求。

## 6. 创建与应用二进制补丁

```sh
wasm-git patch old.wasm new.wasm --output update.wgp
wasm-git apply old.wasm update.wgp --check
wasm-git apply old.wasm update.wgp --output restored.wasm
```

源文件必须与补丁预期原字节一致。验证成功后发布新目标文件，不覆盖已有目标。
`--check` 只验证和审计，不写目标。交换 `patch` 的两侧即可创建反向补丁。
也可从同一仓库快照读取提交内容：

```sh
wasm-git patch main topic --repo ./history --file modules/app.wasm --output update.wgp
wasm-git apply main update.wgp --repo ./history --file modules/app.wasm --check
```

`--file` 选择命名树路径。补丁操作不会提交或更新引用。
JSON 是审阅报告，`.wgp` 才包含二进制还原数据。

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

脚本可用 `update-ref --stdin` 执行原子批次，`--batch-updates` 分别报告语义拒绝。
两个新增名称互相冲突时，与输入顺序无关地拒绝两者。
修改符号 HEAD 需要 `--no-deref`。旧对象 ID 全零表示所选引用必须不存在；
在 `--no-deref` 下，尚无提交的符号 HEAD 仍已存在，因此不满足此条件。
应使用 `symref-update HEAD NEW_TARGET ref OLD_TARGET` 按精确旧符号目标切换。
不带 `--no-deref` 的 `verify HEAD ZERO_ID` 才是检查解引用后分支不存在。
只修改引用指针的事务保留 index 与工作文件。

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
`archive` 和不切换工作区的 `bisect`。它们基于 wasm-git 实际对象模型，选项子集请查看帮助。

引用事件包含 UTC Unix 时间戳。`reflog DIR` 保留共享事件视图；
`reflog DIR show [REF]` 按新到旧显示指定日志，默认 HEAD。`list` 列出有记录的名字，
`exists REF` 通过退出码 0/1 判断。
`rev-parse DIR 'HEAD@{2026-11-01T00:00:00Z}'` 返回不晚于指定时刻的最近事件；
纯数字仍表示序号。日期使用 UTC，也接受 `@UNIX_SECONDS`、`now`、`yesterday` 和
`7.days.ago` 等数字相对时间。已过期或缺失的证据会明确报错，不猜测。

## 12. 克隆、拉取、推送与离线交换

```sh
wasm-git clone ./history ./peer
wasm-git bundle ./peer peer.wgb
wasm-git unbundle ./history peer.wgb incoming
wasm-git remote ./peer add origin https://host.example/project
wasm-git fetch ./peer origin
wasm-git push ./peer origin main:main
wasm-git push ./peer origin --tags
```

Unbundle 在验证及快进规则下更新显式接收分支。Fetch 安装已验证对象与 tracking 状态，
不移动本地分支或更改工作区；pull 进一步整合，可能产生冲突。
完整 clone 保留广告分支与标签，随后可用 `restore DIR --source HEAD --staged --worktree`
物化树文件。

HTTP 凭据通过 `WASM_GIT_REMOTE_TOKEN` 提供，不保存到 remote 配置。
非 loopback 使用 HTTPS；明文远端需要明确允许。SSH 使用远端服务程序与系统账户策略，
详见部署手册。

非快进 push 需要服务端许可，以及显式 `--force-with-lease OLD_ID`、`--force`/`-f`、
`+SOURCE:DESTINATION` 或 `--mirror`。所有形式仍以远端广告中的精确旧值执行 CAS；
`--force-with-lease` 还核对调用者给出的旧 ID。
`push DIR origin main` 推送同名分支，`:topic` 删除引用。
`--all` 推送全部本地分支，`--tags` 推送本地标签，`--mirror` 同步原生分支/标签集合，
包括删除和强制替换；仍禁止删除远端 HEAD。`--all -u` 仅在确认后保存 upstream。

推送映射支持完整源/目标引用各含一个 `*`，以及仅写源模式（目标同名）和以 `^` 开头的
目标排除模式。模式需加引号，避免 shell 提前展开。`:` 只推送两侧都存在的同名分支，
`+:` 额外允许强制更新。`--prune` 只删除所选目标映射内不存在对应源的引用；
否定模式与 Git push 一样，在映射后匹配完整目标名，被排除的目标不会被 prune。默认单分支 push 不删除其他分支，
`--all --prune` 选择所有分支，`--branches` 等价于 `--all`。
映射推送的 `-u` 在回执确认后设置原始源分支的 upstream；同一请求中不能将一个源分支
绑定到多个目标。

多个 refspec 不带 `--atomic` 时一次提交批次并逐项返回结果；`--atomic` 要求任意一项
失败均阻止整批发布。`--dry-run` 预演服务端验证，不安装对象、不改引用、不运行 hooks，
因此实际推送仍可能被 hook 拒绝。重复 `-o VALUE` 可传递有界、原样的 push options：

```sh
wasm-git push ./peer origin main:main topic:review --atomic -o review=ready
wasm-git push ./peer origin --mirror --dry-run
wasm-git push ./peer origin 'refs/heads/release/*:refs/heads/public/*' '^refs/heads/public/private/*' --prune --dry-run
```

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

默认密钥为 wasm-git 二进制 Ed25519 格式。`--format ssh` 接受 OpenSSH 密钥；
`--format openpgp` 使用完整 GPG 指纹签名、使用公钥证书导入信任。
agent、撤销与验证行为见[签名与 PR 手册](WASMHUB.zh_CN.md)。私钥不得进入源码控制。
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

Reflog 会保留已离开分支的历史，需先显式清理，再 prune 对象：

```sh
wasm-git reflog ./history expire --all --expire=90.days.ago --expire-unreachable=30.days.ago -n
wasm-git reflog ./history expire --all --expire=90.days.ago --expire-unreachable=30.days.ago
wasm-git gc ./history --prune=now
```

过期要求严格早于截止点；`all` 忽略年龄，`never` 禁用该条件。
不可达过期按所选分支/标签 tip 判断，HEAD 则采用当前本地/tracking tips 与 detached
工作树 HEAD 的集合；old/new 两端均须可达。`--all` 包含已注册工作树的 HEAD 日志，
`--single-worktree` 限制只处理本工作树的私有 HEAD 日志。
首次改写前验证所有选中日志；任一已注册工作树存在待恢复的历史操作时禁止修改。
逐日志原子替换，I/O 失败可能留下明确报告的部分过期，引用和工作文件保持不变。
已注册工作树必须仍可访问以完成验证。

`--rewrite` 仅重接所选日志的保留链；
`reflog DIR delete 'main@{1}' --rewrite [-n]` 删除单项但不移动分支。
`write REF OLD_ID NEW_ID MESSAGE` 追加经过验证的手工记录，要求完整引用名和原生 commit ID，
不存在用 64 个零表示。
`maintenance DIR run --task=reflog-expire` 使用 90/30 天策略；默认 maintenance 依次执行
verify、过期、GC。单独 `gc` 不自动清理 reflog。

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
- **输出发布失败：** 原子发布前失败不会创建目标文件；发布成功后若清理暂存文件或同步目录
  失败，错误会明确说明完整输出已发布。保留并核验该文件后再重试；目录同步失败表示断电后的
  持久性尚不确定，不表示文件不存在。
- **Busy 或遗留锁：** 检查所有写入者和 linked worktree；确认无人持有后才移除持久 writer/index
  锁。不得删除仍在使用的对象池锁 inode。
- **未知语法或禁用特性：** 查看原始审计、标准配置和覆盖信息；改变展示配置不会实现缺失语义。
  已识别 custom 外层格式但含未解释子段时，标记为 `partially_decoded` 并报告 `custom_partial`，
  `--section opaque` 也会包含它；已解码字段仍可查看。不符合已知惯例的 custom 载荷保留为
  opaque，不因此判定 Core Wasm 无效。
- **承诺对象不可用：** 恢复授权源或显式配置回退，不能以空模块替代。
- **Lease 或工作区不一致：** 重新查询并审阅；旧预期 ID 表示当前状态已不再绑定原审阅。
- **Trace 或对齐不完整：** Component 与未来版本 Wasm 可追溯物理字节历史；不适用 Core 验证时
  明确标记 `not_validated`。预算耗尽会保留未完成的 frontier，不伪报遍历完成。
  精确字节还原和完整解释对应是两种独立性质。
- **终端或管道失败：** 错误消息转义内嵌终端控制符和双向排版控制符；非 UTF-8 命令参数返回
  错误而不 panic。下游关闭输出管道不会触发 panic，也不会抹去命令原本表示差异或冲突的退出码；
  其他输出 I/O 错误仍导致命令失败。诊断流关闭也不会引发二次 panic。
  服务端日志写入失败不会把已经接受的发布变成失败的协议响应；HTTP 启动地址公告写出失败时，
  在进入连接接收循环前正常返回启动错误。

部署限制与备份见[服务端运维](SERVER_ADMINISTRATION.zh_CN.md)，存储版本与字段见
[二进制格式](BINARY_FORMAT.zh_CN.md)。
