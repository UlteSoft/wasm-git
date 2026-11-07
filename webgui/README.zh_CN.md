# wasm-git 本地网页版

[English](README.eu_US.md) · [中文完整手册](../documents/WEBGUI.zh_CN.md)

使用 Python 3.10+ 标准库调用原生客户端与服务端；不需要 pip、npm 或独立 GUI
二进制。未找到原生程序时，会自动使用本机已有 Cargo 从仓库源码构建。

在项目根目录运行：

```sh
python3 webgui/client.py start --repo /path/to/repository --open
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
