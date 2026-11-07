# 社区特性进度与永久保留

```sh
wasm-git features
wasm-git features threads --standard core3 --json
wasm-git features legacy_exceptions
```

此命令离线读取编译进共用库的社区快照，client/server 使用同一份资料。
不访问网络，不创建仓库状态；内部对象、补丁、pack 和传输仍为原有二进制格式。
JSON 仅作为可选的对外报告。更新工具在内存中读取社区提供的 JSON，生成 Rust 源码，
不把它放入 `.wasm-git`，也不引入运行时 JSON 缓存。

## 社区复核（2026-11-06）

主要依据是 [WebAssembly/proposals](https://github.com/WebAssembly/proposals/tree/df6a2aca1cd9d779eb4f2f3e19dae8277f336015)，
固定提交 `df6a2aca1cd9d779eb4f2f3e19dae8277f336015`；输入 SHA-256 为
`3bb2629b9bb29db8af0366690e15ba32ab60b12f20d3e39c9a718efe929a6b89`。
清单有 20 个 finished、31 个 active、10 个 inactive 条目。
另保留旧版异常处理独立记录：其[原说明](https://github.com/WebAssembly/exception-handling/blob/main/proposals/exception-handling/legacy/Exceptions.md)
明确已被新版替代，[社区实现清单](https://github.com/WebAssembly/website/blob/main/features.json)
将旧异常列为 inactive。提案仓库本身被 archive 不代表特性弃用：完成的提案也可能归档。

交叉复核也发现来源不同步：website 的
[固定快照](https://github.com/WebAssembly/website/blob/128c38126af305f53bc22b31c30aefa3f5efe824/features.json)
仍将 FP16/halfPrecision 写为 Phase 1，而 proposals 已是 Phase 2。
阶段优先采用 proposals 的固定提交，website 用于交叉核对；不根据某个引擎已实现就推定提案已毕业。

社区的[阶段定义](https://github.com/WebAssembly/meetings/blob/main/process/phases.md)区分
Phase 4（标准化阶段）、Phase 5（已标准化）和并入主规范后的 finished。
本项目将 Phase 4、Phase 5、finished 标为 `standardization_ready=true`，作为接入候选；
只有经过实现与验证的语法才能标为实际支持。finished 条目的 `affected_specs` 也单独保留，
不能把 JS API 或文本语法提案当成新增 Core 二进制操作码。

- Threads、Compact Import Section、Wide Arithmetic：Phase 4，已知编码在默认
  `all-known` 可用；它们仍不属于当前 `core3` 配置。
- Stack Switching、Custom Page Sizes、Custom Descriptors：Phase 3。
- Acquire-Release Atomics、FP16、Extended Name Section：Phase 2。
- Shared-Everything Threads：Phase 1，区别于普通 threads。
- JS Promise Integration 和 Web Content Security Policy：Phase 5、尚未并入主规范，
  属于宿主集成，不声称 wasm-git 实现 JavaScript 执行环境。

完整清单与精确来源由 `features --json` 提供。`coverage` 分别表示已知 Core 指令及操作数、
部分 Core 投影、custom 约定、普通 import 结构、Component 外层、宿主/文本或尚未审阅。
`default_recognition` 不代表运行时执行、完整语义等价证明或支持提案未来的任意修订。
例如 custom descriptors/stack switching 的部分函数投影仍有明确边界；
Component Model 只审计外层；FP16 不能冒充已实现。详见[标准支持边界](standards.md)。

## 一旦实现，永久保留

1. `src/features.rs` 的 `RETAINED` 是只增不减的已实现解码器承诺。
   更新 `src/feature_upstream.rs` 不得改变这个表、parser flags、指令读法或原始字节。
2. 官方 inactive/撤回/替代的已实现特性标为 `deprecated`，继续默认识别、diff、保存、
   patch、GC 和还原。弃用不是非法编码，也不是停止支持。
3. 从完整上游清单消失的条目保留旧 ID、名称和来源，状态记为 `missing`。
   已实现条目在 wasm-git 中暂标 `deprecated` 并注明需要核对改名/移除，不能把“消失”
   冒充官方已投票否决。确认改名后保留旧 ID 并补充新身份；不得静默删除旧记录。
4. 新编码替代旧编码时分别保留读取规则和固定二进制测试；若不能消除编码歧义，
   明确报告不确定性，不用新版解释强行改写历史。
5. `core1/core2/core3` 是固定标准范围。某特性毕业或弃用都不会追改旧标准的成员。
   默认 `all-known` 的现有识别能力不因社区状态变化而关闭。
6. 未知 opcode 和非共识 custom 内容继续原样保留。社区资料缺失、网络失败或
   未实现的新提案都不影响已有仓库的使用。

实际出现的已弃用 Core 特性在 inspect/diff 中附 `standard.deprecated_features`；
其原有 `supported`/`recognized_but_disabled` 符合性证据不变。
精简 JSON 保留对应位置证据；section 筛选只保留所选范围内的弃用提示。
当前固定旧异常二进制测试核对原始指令边界、标准配置、单字节修改、精确补丁还原和显示范围。

## 每周检查及更新

`.github/workflows/wasm-features.yml` 设置每周一北京时间 09:23 检查，也可手动触发。
它从同一个不可变上游提交读取清单，生成保留所有旧 ID 的候选 Rust 快照；
发现新增、阶段变化、撤回、消失、规范版本或来源改变时失败提醒，并上传候选源码供审阅。
不自动提交、不合并代码、不删实现、不发布评论；加入默认分支且 GitHub Actions 启用后才会运行。
仅 champion 等不影响所跟踪资料的编辑不会触发变化提醒。

本地更新：

```sh
python3 tools/update-wasm-features.py --check
python3 tools/update-wasm-features.py --update
# 离线重现（输入可位于 /tmp）
python3 tools/update-wasm-features.py --check --input /tmp/proposals.json --revision COMMIT_SHA
python3 tools/test-wasm-features.py
cargo test --locked --lib features::tests
cargo test --locked --test feature_lifecycle --test standard_profiles --test proposal_diff_cli
```

更新器只原子替换社区元数据源码，不修改解码器。所有旧记录始终参与合并，缺失不会删除；
未知输入 schema、非法阶段、重复键、空/明显不完整的清单或网络错误会停止更新。
`--check` 不写任何文件。快照中的提交和摘要用于复核输入；已观察过的状态历史保留在 Git 历史中。
定期检查是资料更新入口，不代表自动完成新算法：新 Phase 4/5/finished 特性必须补齐覆盖分类与测试，
否则回归检查会拒绝将它描述为已审阅。依赖升级也必须保留旧 wire fixtures 和已实现 flags；
上游库若删除旧 reader，应在本项目维护兼容实现，而不是删除回归用例。
