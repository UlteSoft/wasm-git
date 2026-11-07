# wasm-git 本地网页版

[English](README.eu_US.md) · [中文完整手册](../documents/WEBGUI.zh_CN.md)

使用 Python 3.10+ 标准库调用原生客户端与服务端；不需要 pip、npm 或独立 GUI
二进制。未找到原生程序时，会自动使用本机已有 Cargo 从仓库源码构建。

在项目根目录运行：

```sh
python3 webgui/client.py start --open
python3 webgui/client.py stop

python3 webgui/server.py start --root /path/to/hosted-repositories --open
python3 webgui/server.py stop
```

客户端界面默认端口为 **8737**，服务器监管界面为 **8738**。不传 `--repo` 时可
在浏览器中创建、克隆或选择仓库。另有 `serve` 前台运行和 `status` 状态查询。
使用 `--client` / `--server` 指定已有程序，`--no-build` 禁止自动构建，`--build`
从当前源码重新构建。使用自定义端口或状态目录时，关闭命令须传入相同参数。

右上角可以切换 English / 简体中文，切换时保留表单输入和当前结果。提交说明、
路径、符号、指令、原生诊断和下载审计输出保持原文。

常用仓库操作使用专门表单；功能目录与命令工作台提供全部原生参数、二进制标准
输入输出、签名、PR、存储和底层命令。server 入口增加托管仓库列表、HTTP/stdio
服务管理、策略/保护编译与接收审计。各项边界详见完整手册。

请打开启动时打印的完整认证 URL。管理界面仅监听 `127.0.0.1`，用于本地管理。
关闭界面会停止其启动的命令与服务；默认状态和二进制临时输出位于已被 Git 忽略的
`.tmp/webgui/` 中。

## 项目工作台与输出边界

直接执行 `python3 webgui/client.py start --open`，在项目入口打开、创建或克隆仓库；
左侧切换已打开项目，最近列表可重新打开项目。关闭项目不会删除文件。
改动视图把暂存／未暂存文件、逐条 Wasm 审计和提交表单放在同一工作区。
项目切换会清理旧结果，提交草稿仅保存在当前页面内存并按项目隔离。

GUI 使用[带版本的二进制命令协议](../documents/COMMAND_PROTOCOL.zh_CN.md)。
普通页面呈现筛选后的字段，不直接展示 JSON；默认响应不返回凭据、完整参数和环境变量。
原始报告与命令流需要主动查看或导出。内部项目列表使用 SQLite 二进制数据库。

隐私过滤与翻译相互独立：常规元数据／诊断会掩码已知凭据，原始导出保持证据完整。
客户端界面克隆后会检出命名文件；如果检出失败，仍保留已克隆项目，并显示恢复提示。
