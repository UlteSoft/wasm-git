# 可验证的离线字节补丁

补丁绑定前后两个完整 Wasm 文件的原始 SHA-256，携带每次编辑的旧/新物理区间和精确字节。
它不要求认识所有指令或 custom 格式，也不通过 WAT 重编码文件。

```sh
wasm-git patch old.wasm new.wasm --output change.wgp
wasm-git apply old.wasm change.wgp --check
wasm-git apply old.wasm change.wgp --output restored.wasm
```

`--check` 返回重新计算的完整审计 JSON，不创建输出文件。
`--output` 必须指定尚不存在的文件；它与 `--check` 互斥，二者必须选一个。
成功退出码为 0，格式、完整性、输入或输出错误为 1。
未知指令、无效标准 payload、无法识别的 custom 约定及合法的非规范 LEB 都可原样保存和重放，
前后文件须至少有 8 字节并具有 Wasm magic；Component Model 和未知 version/layer 也可原样重放。
这只是容器接受条件，不声称未知编码有效，也不按 Core 语法解释它们。
生成器直接复用完整审计的原始字节差异算法，不构建随后丢弃的指令树和标准解释；
原二进制补丁格式、合并相接编辑和摘要校验规则不变。应用后仍从已验证字节重新生成完整审计。
`diff --section` 的局部编辑不能直接作为整个文件的补丁；请用完整输入运行 `patch`。

## 校验顺序与证据

应用时依次核对：

1. 格式、摘要编码、整数范围、编辑排序及两侧区间的非重叠性。
2. 每个区间长度与原始载荷字节数、未编辑间隙长度，以及尾部间隙长度。
3. 完整 base 的长度和摘要，以及每次编辑声明的旧字节。
4. 重放时实际产生的新位置、最终长度、Wasm magic/头长度和目标摘要。

所有校验成功后才允许发布输出。不会根据补丁宣称的目标长度预分配内存；输出只随实际输入和编辑字节增长。
重新生成的 `audit` 包括未知内容、解码诊断和当前验证状态，补丁本身不携带可被误信的 feature、
上下文、最小性或验证声明。`audit.exact_minimal` 属于本次重新比较的结果，不是对传入编辑脚本的证明。
生成器会合并相接的编辑，因此补丁 `edit_count` 与重新比较的编辑分组数不一定相同。

输出使用同目录独占临时文件，完整写入并同步后，通过不覆盖目标的 hard link 原子发布，再同步父目录。
已有文件、目录及软链都不替换；竞争发布失败会清理本操作的临时文件。
发布后的清理或目录同步错误可能留下完整目标文件，进程中断可能留下独特临时文件；最终路径不暴露部分内容。
不支持 hard link 的文件系统会明确报错，不回退为直接写最终文件。
checkout、bundle 等原有新文件输出也使用相同发布方式。

## 与仓库协作

```sh
wasm-git patch main incoming --repo ./history --output change.wgp
wasm-git apply HEAD change.wgp --repo ./peer --check
wasm-git apply HEAD change.wgp --repo ./peer --output restored.wasm
wasm-git commit ./peer restored.wasm -m 'apply reviewed byte patch'
```

`patch --repo` 在同一锁和已验证快照中解析两侧引用，返回 JSON 的 `inputs.old/new.commit`
记录实际不可变提交 ID。`diff --repo` 使用同一批读取方式，`HEAD HEAD` 不会因两次读取之间的提交而变成不同输入。
`inspect --repo` 和 `apply --repo` 也返回实际输入提交 ID。
释放锁后的计算始终使用已复制的字节；后续引用变化不改变本次输入。
`--check --repo` 仍会短暂创建和删除 `.wasm-git/write.lock`，需要仓库可写且没有其他持锁操作；
不创建输出文件不等于零文件系统写入。

补丁中的摘要属于完整原始文件，与包含类型和长度 envelope 的仓库对象 ID 不同。
相同文件可位于不同提交和仓库；应用只要求 base 字节完全相同。
补丁不携带提交 DAG，也不修改对象和引用；需要双父合并历史时使用[合并命令](merge.md)。

## 二进制 v1 编码

文件以 8 字节 `WGTPAT01` 开始，依次保存前后两个原始 32 字节 SHA-256、
两个 u64 大端文件长度、u32 大端编辑数量。固定头共 92 字节。
每项编辑保存四个 u64 大端值 old_start/old_end/new_start/new_end，随后直接保存
两段原始编辑字节，长度由对应半开区间推导；不保存 JSON、十六进制字符串或语义报告。
单次小修改的格式开销为头 92 字节和每项 32 字节，再加实际旧/新字节。

解码先按剩余输入约束编辑数量及区间载荷，拒绝截断、尾随内容、逆向/重叠区间、
空编辑和无变化替换。不支持旧 JSON 补丁。生成器对相同文件生成空编辑数组，仍校验完整摘要。
相同前后文件可以有不同的有效编辑脚本，导入脚本始终以完整重放校验为准。
CLI 报告中的十六进制字段由原始字节生成，仅用于审阅。

摘要保证内容绑定，不提供作者身份认证；补丁验证成功也不等于模块验证或行为等价成功。
