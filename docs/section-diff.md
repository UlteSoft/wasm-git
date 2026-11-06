# 按 section 查看更新

```sh
wasm-git diff old.wasm new.wasm --section code
wasm-git diff old.wasm new.wasm --section code --section data --compact --format json
wasm-git diff old.wasm new.wasm --section custom:name
wasm-git diff old.wasm new.wasm --section custom:producers
wasm-git diff old.wasm new.wasm --section id:200
wasm-git diff old.wasm new.wasm --section ordinal:3
wasm-git diff old.component.wasm new.component.wasm --section core_module
wasm-git diff HEAD main --repo ./history --file module.wasm --section custom
wasm-git diff ./history --cached --section code
wasm-git diff-files ./history --section custom:name
```

重复 `--section` 取并集，按原始文件的**顶层** section 选择：

- Core 类型：`type`、`import`、`function`、`table`、`memory`、`global`、`export`、`start`、`element`、`code`、`data`、`data_count`、`tag`。
- `custom` 选择全部 custom；`custom:NAME` 精确选择名字，不解释 glob。名字包含 NUL 或超过命令行限制时使用 `ordinal:N`。
- `id:N` 为十进制 0–255，未知 ID 同样可选；不同未知 ID 不互相配对。
- `ordinal:N` 是零基原始 section 序号，分别在两侧选择；它不声称历史身份。不同名字或种类可显示为删除/增加。
- `opaque` 选择尚未理解或损坏的区域；`all` 选择全部 section。文件头不属于 section，要看头变化请使用无筛选的完整 diff。
- Component 的顶层类型包括 `core_module`、`core_instance`、`core_type`、`component`、`component_instance`、`alias`、`component_type`、`canonical`、`component_start`、`component_import`、`component_export`。嵌套模块内容目前作为原始 payload 保留，不递归猜测函数对应。

尚未找到匹配的有效选择器返回 `unmatched_selectors`；拼错类型或越界数字报错。`selection_complete` 分别说明两侧边界/名称能否完整用于查找：遇到截断 framing、未知编码或非法名称时，不把“没有观察到匹配”声称为“该 section 不存在”，文本也不会把这种无变更记录说成已确认相同。
`diff-files/index/tree` 的 section 视图不能与 `--name-only`、`--name-status`、`--quiet`、`--exit-code` 同用，避免把全文件状态误说成所选节的状态。

## 精确范围与歧义

报告的 `kind` 为 `section_diff`。`old/new.sections` 只保留所选节；`changes` 保存两侧原始序号、ID、名字、解码状态、完整节/头/payload 范围、SHA-256、字段说明和精确 ByteEdit。
Core 字段仍使用完整模块上下文解码；操作数、块、局部变量、声明和各段的字段路径沿用完整审计，不把截取的节伪装成一个完整模块。

同格式、同 section ID、同 custom 名称先组成候选组。相同原始内容优先对应，摘要只作索引，随后必须逐字节核对。剩余恰好一对才作保守配对，多对剩余内容分别列为增加/删除；重复组的歧义不因先消费相同内容而消失。没有合法 custom 名称的配对也保留歧义。

`content_changed` 包括外层长度编码变化；`payload_changed` 只比较 payload；`position_changed` 单列原始偏移/序号变化。前面的节变长不会伪造后续节内容修改。非最短 LEB 编码、同名不同社区格式、未知指令与截断尾部都保留原始字节。无法读取外层长度时停止，不扫描猜测后续边界。

每组编辑只在对应的 section 内重放，偏移仍是原文件绝对位置。移动的 section 可以形成非单调对应，所以报告固定 `whole_module_patch=false`；不能把这些分组拼起来替换整个模块。完整模块更新使用独立的 `patch` / `apply` 命令。

`selected_content_changed`、`selected_position_changed` 表示在已观察到的所选范围内发现的变化；若 `selection_complete` 为 false，不能据此推断缺失范围不变。`module_binary_changed` 独立比较整个原文件。树视图的文件状态和计数仍描述整个文件；筛选结果放在每个文件的 `diff` 中。新增/删除文件用明确标记的虚拟空容器表示缺失侧，不输出未选中的完整文件内容。

## 性能与解释范围

只选 custom 时仅扫描外层 framing/名字，再解释所选 payload；不解析不相关函数、不构造全模块指令树或函数匹配矩阵。选择 Core 标准节时保留完整 Core 上下文和标准验证，之后裁剪显示。`validation_scope` 明示采用哪条路径；快速 custom 视图不声称完整模块合法。

此视图回答所选**物理内容及字段**如何更新，不重新计算跨 section 的调用、导出和签名输入依赖。即使签名容器自身没有变化，被签内容也可能变化；请用完整 diff 查看 `signature_input` 等依赖证据。报告通过 `cross_section_dependencies` 明示这一点。

`--compact` 保留全部分组、状态、字段和精确编辑，省略两侧 region 树及 supported 的逐条标准证据，并记录省略数。`--short` 只给每组摘要。所有格式默认使用全部可用逻辑 CPU，原始范围和排序不随线程数改变。

## Wasm 接受范围

完整存储、GC、fork、传输和二进制 patch 接受至少 8 字节且带 Wasm magic 的输入。Core version 1 保留原有结构化解析；[Component 当前二进制格式](https://github.com/WebAssembly/component-model/blob/main/design/mvp/Binary.md)采用已知外层 section framing；未知 version/layer 只形成一个 opaque 后续区域。
[Core section framing](https://webassembly.github.io/spec/core/binary/modules.html#sections)和 Component framing 的处理不会被推广成对未来二进制格式的猜测。

这不是“所有当前和未来提案都已实现语义”的承诺。未知操作码不猜测立即数或控制流；Component 不执行完整组件验证；并发修改非 Core 文件时返回显式合并冲突，只有已能按完整字节选择一侧的三方合并可自动完成。原字节的保存、比较、补丁和恢复不依赖这些语义能力。
