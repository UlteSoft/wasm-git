# Git 工作流对照与待完成范围

**复查日期：** 2026-11-06 · [English](GIT_PARITY.eu_US.md) · [文档目录](README.zh_CN.md)

## 范围

uwgit 保留原生 Wasm 对象、二进制控制记录及网络协议，对照的是可用的版本管理与托管
工作流，不要求兼容 Git 对象/线协议。仅有命令名不能证明等价，还需实现发布、拒绝、恢复
和原字节保持。**当前尚未覆盖全部 Git 命令、选项和服务端设施。**

## 已实现的托管合同

- HTTP/SSH 认证、二进制 principal/仓库/写引用策略、只读服务、精确旧值 lease、
  快进约束和 force 授权。
- 原子多引用发布和逐项返回结果的普通批次。每批只解码一次协商对象包，接受项共用
  一个持久决定；对象图/系统故障或 pre-receive 拒绝停止整个请求。
- Pre-receive、update、post-receive、post-update、有界原样 push options、包含逐项
  拒绝原因的二进制审计和绑定请求摘要的回执。回执损坏或丢失时，全部已发送项保持未知。
- 同名、显式及多 refspec、空源删除、`--tags`、`--all`、原生分支/标签 `--mirror`、
  `--force`、`+` refspec、显式 `--force-with-lease`。Force/mirror 仍带广告中的精确 lease；
  `--all -u` 在确认后保存 upstream，dry-run 不发布、不运行 hooks。
- 完整名称的通配 push 映射、目标排除选择、同名已存在分支的 `:`/`+:` 和映射范围内
  `--prune`；排除规则也保护映射后的目标。多引用 `-u` 将确认后的源分支绑定到实际目标。
- `team/project` 仓库、命名空间策略和共享 pack fork。HTTP 与明确根边界的 SSH 可访问
  相同名称，fork 引用独立。
- 完整/浅/过滤 clone/fetch、补深和补全、验证后的按需对象获取、引用分页、有界上传、
  协商 delta 和拥有者 pin 保护的复制对象池。

详见[服务端手册](SERVER_ADMINISTRATION.zh_CN.md)。对照来源：Git 官方
[receive-pack](https://git-scm.com/docs/git-receive-pack)、
[push](https://git-scm.com/docs/git-push)、[namespaces](https://git-scm.com/docs/gitnamespaces)。

## 已实现的历史保留

带时间戳的二进制 reflog 支持选定日志展示、日期/序号修订、list/exists/write/delete、
年龄/不可达过期、dry-run 和链重写。首次写入前验证选中的工作树日志，存在待恢复历史操作
时拒绝修改。引用和工作文件保持不变；逐日志原子替换在 I/O 故障时可能留下明确报告的部分进度。
Maintenance 提供普通 90 天、不可达 30 天保留策略。显式过期后 GC 可释放废弃历史，
单独 GC 不自动清理日志。

Zstandard pack、索引、有界 delta、跨 pack 依赖、共享 fork 基底、repack 和显式不可达
prune 保持原 ID 与原字节。详见[用户手册](USER_MANUAL.zh_CN.md)、Git 官方
[reflog](https://git-scm.com/docs/git-reflog) 与 [GC](https://git-scm.com/docs/git-gc)。

## 待完成的真实缺口

下列项目**尚未实现**，不能作为已完成功能宣称：

- `refs/pull/*` 等任意引用、一般符号别名和单个物理仓库内部的引用命名空间虚拟化。
  仓库路径命名空间与引用命名空间不同。
- 隐藏引用广告及匹配的对象读取限制。当前读权限以仓库为单位，ref 模式控制写入。
- Push certificate、proc-receive 委派、reference-transaction hooks、完整 receive/upload
  可配置策略和多节点可变引用共识。已有作者签名、对象池是不同能力。
- 全部 fetch 映射、remote helper 和远端 archive。Push 模式要求完整 ref 名称，
  mirror 仅覆盖原生分支/标签。
- Annotated tag、一般未签名提交的 author/committer 元数据及关联日期查询、交互式及
  保持 merge 拓扑的 rebase、全部 merge 策略、携带无关脏修改的 checkout、完整 sparse-checkout
  和 stash 工作流。
- Reflog `--updateref`/`drop`/`--stale-fix`、按对象年龄自动 prune、外部 FSMonitor/split-index，
  全部 revision/pathspec/格式化选项。
- Git 邮件/文本补丁、任意文件模式、submodule 和完整导入导出工作流的原生等价能力。

## 证据与格式边界

[托管测试](../tests/server_hosting_cli.rs)、[传输测试](../tests/remote_cli.rs)、
[维护测试](../tests/maintenance_cli.rs)及[引用恢复测试](../src/repository_ref_transactions.rs)
覆盖实际 HTTP/stdio、混合接受、精确 lease、hook 顺序、隔离区拒绝、预演保持、共享 fork、
过期/GC 和中断发布。有限测试不能证明全部 Git 等价，也不能证明任意源码意图均可还原。

本地测试通过 TMPDIR 使用项目 `.tmp/`，并置于 64 GiB cgroup。运行日志、profile、参考源码
不进入发布。仓库标记 `UWGRPO\0\x03` 和审计 schema 3 拒绝旧发行前记录；不可变对象与
pack 编码不变。详见[格式参考](BINARY_FORMAT.zh_CN.md)。
