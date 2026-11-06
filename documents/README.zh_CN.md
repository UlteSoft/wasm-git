# uwgit 文档目录

[English](README.eu_US.md) · [项目介绍](../README.zh_CN.md)

本目录说明当前发行前实现，分别描述物理还原保证、解释证据、仓库流程和部署职责。
示例采用实际 `wasm-git` 与 `wasm-git-server` 接口。英文与简体中文按项目指定约定分别使用
`.eu_US.md` 和 `.zh_CN.md`。

## 阅读顺序

- [WasmHub 集成](WASMHUB.zh_CN.md)：SSH/OpenPGP 签名、持久化 PR、审批/CI、分支保护
  与合并恢复。[English](WASMHUB.eu_US.md)
- [Git 工作流对照](GIT_PARITY.zh_CN.md)：已实现合同、本地/服务端缺口及验证边界。
  [English](GIT_PARITY.eu_US.md)

- [技术白皮书](WHITEPAPER.zh_CN.md)：问题、证据模型、匹配与对齐、custom/debug、存储、
  分发、性能、威胁模型与验证边界。[English](WHITEPAPER.eu_US.md)
- [使用手册](USER_MANUAL.zh_CN.md)：审计、section、源码映射、补丁、树、分支、冲突、重放、
  交换、浅/部分历史、签名、GC 与故障处理。[English](USER_MANUAL.eu_US.md)
- [服务端部署与运维](SERVER_ADMINISTRATION.zh_CN.md)：HTTP/SSH、二进制授权策略、hooks、
  审计、作者信任、共享 fork、对象池、边界、备份与恢复。[English](SERVER_ADMINISTRATION.eu_US.md)
- [二进制格式参考](BINARY_FORMAT.zh_CN.md)：对象、manifest、UWGPACK4、补丁、控制记录、
  签名、RPC 布局及验证要求。[English](BINARY_FORMAT.eu_US.md)
- [开发与验证](DEVELOPMENT.zh_CN.md)：项目 .tmp、cgroup、构建组合、独立还原、编译器判据、
  fuzz、VTune 与发布卫生。[English](DEVELOPMENT.eu_US.md)

本地使用先读使用手册，部署先读运维手册，集成开发结合白皮书和格式参考。
现有 docs/ 提供细项实现说明和历史开发背景；本目录是维护的双语总入口。
实现独立工具时仍需结合源码合同和回归测试。

## 解释规则

“精确”指有明确范围的原字节与证据，函数或源码对应仍可能存在歧义。
“支持”必须说明是无损保存、framing、类型化分析、验证还是运行执行；uwgit 审计不执行 Wasm。
类 Git 工作流使用原生格式和明确的选项子集。发行前格式可能变化且不迁移。

行为更新时同步两种语言，保持命令和数值一致，不能把有限测试写成普遍正确性保证。
逐轮运行报告与采样文件放在 .tmp/。
