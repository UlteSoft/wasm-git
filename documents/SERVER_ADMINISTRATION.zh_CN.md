# uwgit 服务端部署与运维手册

**版本：** 2026-11-06 · [English](SERVER_ADMINISTRATION.eu_US.md) · [文档目录](README.zh_CN.md)

## 1. 部署模型

`wasm-git-server` 为指定根目录下已有的 uwgit 仓库提供服务，每个仓库是直接子目录。
名称为 1–128 个 ASCII 字符，以字母或数字开头，其余仅允许字母、数字、`-`、`_`、`.`。
仓库名不是任意文件系统路径。服务端不执行上传的 Wasm。

两端共用对象、审计、图验证、事务和传输实现，构建服务端无需复制源码：

```sh
cargo build --locked --release --no-default-features --features server
cargo build --locked --release --features server
```

第二条同时提供管理用客户端。发行前格式应使用匹配版本的两端程序。
Git 客户端不能通过 Git 协议访问本服务。
WasmHub 类平台可将其作为仓库后端，但需补充账户、审阅 UI、调度、TLS、监控，以及所需的
更强跨节点引用协调。

## 2. 初始目录布局

典型部署分开保存程序、仓库和配置：

- `/usr/local/bin/wasm-git`、`/usr/local/bin/wasm-git-server`：安装的程序。
- `/srv/uwgit/project/`：单个仓库，包含 `.uwgit/`。
- `/etc/uwgit/`：授权策略、token、作者信任配置。
- 独立于在线存储卷的备份位置。

使用专用非特权账户，仅授予必要写权限，并以该账户初始化仓库。
以下假设账户和目录已准备好：

```sh
sudo -u uwgit /usr/local/bin/wasm-git init /srv/uwgit/project
sudo -u uwgit /usr/local/bin/wasm-git fsck /srv/uwgit/project
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419
```

最后一条是前台运行示例，也应以服务账户执行。HTTP 默认 loopback、只读；服务根需提前存在。
源码项目的 `.tmp/` 规则用于开发产物，不约束生产仓库必须放在哪里。

## 3. HTTP 认证与公网 HTTPS

单一传输身份可配置私有 token 文件：

```sh
wasm-git-server --root /srv/uwgit --listen 127.0.0.1:9419 \
  --token-file /etc/uwgit/token --allow-write
```

Token 为 1–4096 个非空白可打印 ASCII 字节，文件允许一个末尾换行。
应生成高熵 token 并限制文件权限。配置 token 后，读取和写入都需 Bearer 认证。
真实 token 不得放入源码、服务命令行或公开示例；客户端从 `UWGIT_REMOTE_TOKEN` 读取，
不保存到 remote 配置。

公网通过 TLS 反向代理转发仓库 RPC 到 loopback；原生监听器不处理 TLS。
代理应原样传递请求/响应体，并按 packet/chunk 边界设置长度与超时。
客户端验证 HTTPS 证书，不跟随重定向，不读取 curlrc 和代理环境配置；凭据写入私有临时
header 文件，不放进 curl 参数。非 loopback 明文认证需显式 `--allow-insecure-http`。

```sh
wasm-git clone https://wasm.example/project ./peer
wasm-git remote ./peer add origin https://wasm.example/project
wasm-git push ./peer origin main:main
```

运行客户端前通过部署凭据机制提供环境 token。传输访问身份与提交作者身份是不同策略。

## 4. 仓库与引用授权

策略可定义多个传输 principal 和授权范围。管理员编写外部 JSON，再编译为二进制运行记录：

```sh
wasm-git-server compile-policy policy.json policy.bin
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write
```

外部输入示例：

```json
{
  "version": 1,
  "principals": [
    {
      "name": "release-bot",
      "token_sha256": "0000000000000000000000000000000000000000000000000000000000000000",
      "grants": [
        {
          "repository": "project",
          "read": true,
          "write": ["refs/heads/release", "refs/tags/release-*"],
          "force": false,
          "create": false
        }
      ]
    }
  ]
}
```

将 64 个零替换为实际 bearer token 字节的 SHA-256，不含 token 文件末尾换行。
该示例说明 schema，不是可直接共享的凭据。编译拒绝未知字段、非法身份、重复 principal/
凭据及错误规则。输出只能创建新的私有文件，不覆盖旧文件；运行时采用二进制。

仓库/引用规则支持精确匹配或仅末尾一个 `*` 的前缀模式，不是通用 glob。
写权限限于 `refs/heads/`、`refs/tags/`，读权限按仓库划分。
`create` 仅授予预期的服务端 fork 目标。强推还需服务端 `--allow-force` 与客户端精确 lease。
HTTP 写开关仍需显式启用；编译策略与 `--token-file` 互斥。

策略替换应作为配置发布管理，重启实例加载新策略，不假设热更新。
不得允许不可信客户端自行选择 SSH principal 或服务根目录。

## 5. SSH 传输

```sh
wasm-git clone ssh://alice@host.example/srv/uwgit/project ./peer
wasm-git fetch ./peer origin
```

URL 父路径成为服务根，末段为仓库名。客户端通过无伪终端 SSH 调用
`wasm-git-server server --stdio --root ...`。Stdio 默认允许系统账户权限范围内的写入。
使用 `--read-only` 禁止写入，或使用可信 forced-command 包装器固定 root、二进制策略及
`--principal NAME`。

`UWGIT_REMOTE_PROGRAM` 指定一个远端程序路径，`UWGIT_SSH_PROGRAM` 指定一个本地 SSH
程序或包装器，均不是任意 shell 命令字符串。SSH 非交互认证并遵守正常主机密钥检查；
HTTP Bearer 凭据不转发给 SSH 子进程。Forced-command 包装器必须验证或替换客户端请求
参数；若账户仍有任意 shell 权限，仅附加 principal 不能将其限制在 uwgit 策略内。

## 6. 接收流程、hooks 与审计

接收分别核对传输身份、仓库/引用授权、对象完整性、闭包、旧引用预期值、force 策略、
作者信任及可选 pre-receive。引用批次使用共享持久事务机制，客户端用 `push --atomic`
请求整批全部成功或全部拒绝。

```sh
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write \
  --pre-receive /usr/local/libexec/uwgit-pre-receive \
  --post-receive /usr/local/libexec/uwgit-post-receive
wasm-git-server audit-log /srv/uwgit/receive-audit.bin
```

Hook 是管理员安装的程序，不执行仓库上传脚本。工作目录为仓库，提供 `UWGIT_ACTOR`、
`UWGIT_REPOSITORY`、`UWGIT_QUARANTINE_BUNDLE`、`UWGIT_UPDATES_FILE`。
标准输入与 updates 文件均为二进制更新记录，不能按 Git 文本 hook 协议解析。
Pre-receive 获得隔离 bundle；post-receive 在发布后收到通知，其 bundle 为空。
直接 hook 进程有 15 秒期限；若可创建后代进程，部署需补充进程隔离。

Pre-receive 失败阻止发布；post-receive 失败不会撤销已经接受的更新，另行报告。
二进制审计记录 principal、仓库、事务、更新及 prepared/accepted/rejected/unknown/
post-receive-failed 等结果。发布后的完成审计本身也可能写失败，因此需保留 stderr，并查询
实际引用核对结果。仅显式渲染审计时输出 JSON。

## 7. 作者签名策略

```sh
wasm-git trust-author /etc/uwgit/authors.bin 'Alice <alice@example.org>' alice.pub
wasm-git-server --root /srv/uwgit --policy policy.bin --allow-write \
  --signed-authors /etc/uwgit/authors.bin
```

服务端对新公开提交执行严格 Ed25519 验证及作者/公钥精确绑定检查。
已有公开祖先可以保留未签名历史，但仅在 dangling 存储中的未签名提交不能凭“对象已经存在”
绕过策略。只更新引用而不上传对象时也检查新公开历史。
正常删除与引用已公开历史不要求重签旧祖先。

传输身份、引用权限、作者信任与 hook 审批独立生效。
轮换密钥时可按策略先加入新绑定，再移除旧绑定。`untrust-author` 撤销指定公钥或某名称全部
绑定。重签改变提交 ID，可能需要审定的非快进更新，不在旧 ID 下修改内容。

## 8. 共享 fork 与压缩存储

```sh
wasm-git fork /srv/uwgit/project /srv/uwgit/project-fork
wasm-git fork https://wasm.example/project project-fork
wasm-git repack /srv/uwgit/project
wasm-git gc /srv/uwgit/project
```

同文件系统 fork 链接不可变 pack，独立维护 refs、HEAD、恢复记录与 pool 身份。
共享前先压缩源 loose 对象。后续 GC 保留适用共享 pack，只写私有新增。
跨 pack delta 可使用保留的共享基底；删除源路径不删除 fork 硬链接。

不得就地改写共享 pack，应通过仓库引擎管理。
显式 prune 若不能保留整包，可重写该仓库表示。
`pack_bytes` 指本次新写 pack，不是所有保留/共享存储总量。
容量规划还需考虑硬链接统计和 pool promise。
跨文件系统硬链接 fork 会失败，普通 local clone 可退回复制并核验。

## 9. 浅历史与部分客户端

客户端可选择深度、分支、内容过滤器，随后 deepen 或 hydrate。
服务端仍使用正常完整仓库模型；如果接收方已有缺失祖先并能验证最终完整图，可接受浅客户端
的新提交。空接收端不能发布缺失必要祖先的历史。

部分克隆不把 promise 当成已验证数据。对象分片端点提供被授权且公开可达的对象，不提供
仅知道摘要的任意 dangling 对象。补拉源由本地显式配置，收到的 bundle 不得安装任意网络地址。

## 10. 多节点不可变对象池

每个 pool 节点使用普通仓库服务，仅授予适当的后端身份访问。
源仓库显式配置：

```sh
wasm-git pool attach /srv/uwgit/project https://node-a.example/pool https://node-b.example/pool
wasm-git pool sync /srv/uwgit/project
wasm-git pool status /srv/uwgit/project
wasm-git pool sync /srv/uwgit/project --evict
```

每个拥有者具有随机身份，通过 `pool/<owner>/<commit-id>` 引用 pin 提交。
同步复制不可变数据，复用验证和 lease 发布路径。所有节点确认 pin 后才清除本地 Wasm。
任一节点失败保留本地数据；成功副本可供重试复用。读取跨节点回退并校验原对象摘要。

Fork 获得独立 pin，不为池内 Wasm 再下载冗余副本；新本地提交应先 sync。
`pool release DIR` 先恢复必要本地内容，再移除自己的远端 pin，可重试部分失败。
节点 reflog 仍可能保留对象，因此删除 pin 不代表磁盘空间立即回收。

Pool 仓库本身就是授权边界，不应让必须隔离的租户共享公开可读 pool。
复制提供不可变对象可用性，不提供多主可变引用共识。
拥有者 ID、pin 和来源配置是恢复的一部分。
`pool.gate` 的稳定 inode 用于协调锁；仍可能有人持锁时不得删除。

## 11. 服务监督与限制

仓库提供 [systemd 模板](../contrib/wasm-git-server.service)，使用专用账户、严格 umask、
文件系统保护和私有 token 的 loopback 写服务。安装前核对路径与所有权；根据负载设置
CPU/内存限制，并允许服务根和必要 hook 数据访问。

独立服务程序不提供客户端式前置 `--threads`，可通过服务 CPU affinity/cgroup 限制可用 CPU。
原生并行操作复用线程池；HTTP 同时最多 16 个连接 worker，饱和时返回 503。
**`--max-connections N` 是进程退出前总共接受的连接数量，不是并发设置**，主要用于有限测试。

当前限制包括单 RPC packet 64 MiB、metadata 64 KiB、传输 chunk 1 MiB、bundle 4 GiB，
每仓库最多 32 个 pending upload、8 GiB staging 预留预算，上传 24 小时过期。
Header、直接请求/hook 时限和审计渲染累计大小也有边界。
这些不是内存使用保证；完整 snapshot/闭包验证仍可载入较多历史，应测试代表性负载与并发。

## 12. 备份、还原与事故处理

一致性文件系统备份应停止写入者，或通过协调快照同时捕获对象、pack、ref、恢复控制记录和
linked-worktree 元数据。需要节省空间时保留硬链接。
服务策略、作者信任与私钥凭据应按权限单独备份。
可移植 bundle 覆盖其选定历史，不代表全部运行配置和未保存工作区。

Partial 或 pool-evicted 仓库并非自包含备份：导出前先 hydrate，或整体备份所需 pool 副本、
pin 与拥有者状态。将备份还原到新目录，运行 fsck，再导出历史模块逐字节比较。

中断后保留 journal、不完整审计尾和服务诊断；通过正常引擎重新打开，让持久事务恢复运行。
不确定 push 先查 ref。不要手动删除 pack、promise 或 pin 来消除报错。
移除持久 writer/index 锁前必须确认无写入者；进程持有的 pool 锁有不同生命周期。

使用 `gc DIR -n` 审阅计划。正常 GC 压缩且不 prune；`--prune=now` 明确回收保留根之外对象。
真正删除的无引用历史需其他保留副本或备份恢复。
审计轮换应按完整记录并与写入者协调；截断尾需要调查时保留原文件。

## 13. 运维验收

开启写流量前，在新 staging 部署验证认证读写与拒绝、旧 lease、含拒绝项的原子批次、hook
拒绝及 post-receive 失败、可信/不可信签名、GC/删除源后的 fork、浅/部分补拉、evict 前副本
失败和备份恢复。测试应受资源限制，原始输入和日志放在源码项目 `.tmp/`。

本服务不提供托管 UI、自动证书、组织生命周期、自动跨节点引用故障切换或全部 Git 选项。
集成平台时应明确这些外围职责。
