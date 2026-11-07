# wasm-git 0.1.1 预发布

无损 WebAssembly 审计与二进制版本历史，原生客户端和服务端共用源码。本预发布
使用 wasm-git 原生仓库及网络格式，不是原生 Git 协议的直接替代。

- Windows 发布程序静态链接 C/C++ 运行库，并在打包前检查真实 PE 导入表。修复未安装
  VC redistributable 的干净 Windows 无法启动的问题；请用本版本替代 0.1.0 Windows 包。
- 八种原生包：Linux x86_64/AArch64/ARMv7/RV64GC、Windows x86_64/ARM64、
  macOS x86_64/ARM64；均包含客户端和独立服务端。
- 两种 WASIp1 客户端：MVP、Core 2.0，标准库、libc、Zstd 按要求重编译，最终
  模块分别通过独立验证。可执行文件自身指令档位不限制审计输入的特性范围。
- 二进制历史、压缩/GC、逐字节补丁还原，section/函数/block/指令/custom-section
  审计，以及可用的调试符号映射。
- 原生 SSH/OpenPGP 提交签名、HTTP/SSH 服务、共享 fork、持久化 PR、review/check、
  分支保护和合并恢复。
- 发布门禁包含回归与构建矩阵、Linux QEMU 执行、Windows/macOS 原生执行、
  WASI runtime smoke 和产物白名单。

下载后请校验 `SHA256SUMS`。附带双语发布手册说明命令、验证架构、WASI 能力边界、
目录同步语义，以及原生 CI 与本机 QEMU guest 测试的区别。WASIp1 只有一个 worker，
不提供外部子进程、网络服务或依赖 advisory lock 的工作流。预发布格式仍可能变化，
不迁移已经废弃的早期仓库格式。
