# 第二阶段调研：同类项目与 Agent API

调研日期：2026-08-03。这里只记录项目官方 GitHub 仓库能够直接证实的能力，不把“免费使用”误写成“开源”。

## 同类项目结论

| 项目 | 强项 | 与金福搜索的关键差异 |
| --- | --- | --- |
| [voidtools/ES](https://github.com/voidtools/ES) | Everything 的 MIT 开源命令行客户端；查询语法、过滤、排序和导出成熟 | 只是客户端，必须另装并运行 Everything 主程序；不是独立开源索引后端 |
| [Flow Launcher](https://github.com/Flow-Launcher/Flow.Launcher) | MIT；Windows 启动器、插件、拼音、预览和交互体验明显更成熟 | 文件搜索依赖 Everything 或 Windows Index；产品范围更大，也不是单个自带索引的 EXE |
| [FSearch](https://github.com/cboxdoerfer/fsearch) | GPL-2.0；即时结果、通配符、正则、过滤、排除规则和多字段排序成熟 | 面向 Unix-like/GTK，不支持 Windows 原生 MFT/USN 路径 |
| [sist2](https://github.com/sist2app/sist2) | GPL-3.0；正文、元数据、缩略图、OCR、压缩包、标签与语义检索远强于本项目 | 面向 Linux/WSL/Docker，组件和资源成本高，目标不是轻量 Windows 文件名定位 |
| [mcp-everything](https://github.com/SoMaCoSF/mcp-everything) | 已验证“让 Agent 使用极速本机文件搜索”有实际需求；提供 MCP 和高级 Everything 查询 | 依赖 Everything、Python 与 Node.js，部署面和信任边界远大于本项目 |

金福搜索目前最有价值的差异化组合是：原生 Windows、自己读取 MFT/USN、约 5 MiB 的零外部运行库 GUI 单文件、元数据最小化、以及仅限本机当前用户的 Named Pipe API。它还没有超过成熟项目的高级查询语法、拼音/正则/过滤、界面打磨，也没有 sist2 的正文/OCR/缩略图能力。

## Agent API 是否有意义

有意义。v0.3.0 已在 JSON-RPC 2.0 Named Pipe API 之上提供标准 STDIO MCP 适配器。对 Agent 的主要价值不是“替用户再做一个搜索框”，而是：

1. 在数百万条目中以结构化 JSON 快速定位候选文件，避免 Agent 递归遍历所有磁盘。
2. 先返回路径、类型、大小和修改时间等元数据，再由 Agent 在得到任务授权后只读取少数目标文件，减少隐私暴露与工具调用。
3. 通过 `status.get` 和 `index.list_roots` 判断结果覆盖是否完整，避免把“没搜到”误判为“文件不存在”。
4. Named Pipe 不开放 TCP 端口，ACL 限制到当前用户、SYSTEM 和 Administrators，适合本机 Agent。

推荐采用分级能力：

- 默认暴露只读工具：`search.query`、`status.get`、`index.list_roots`。
- `index.scan_root`、`index.scan_volume`、`index.sync_volume`、`index.remove_root` 和 `app.shutdown` 属于会改变状态或可能耗时的管理能力，不应让 Agent 在模糊请求下自动调用。
- `jinfu-search-mcp.exe` 已把三个默认只读能力暴露给 Codex 等 MCP 客户端，并复用现有 Named Pipe；主 EXE 不加入 HTTP 服务，模型也不直接读取整个索引数据库。

## 推荐优先级

1. 保持当前轻量文件名搜索内核，不改造成重型全文引擎。
2. 下一功能优先补高级查询过滤（文件/目录、扩展名、大小、日期、根目录）与结果分页；这同时改善人工和 Agent 使用。
3. 保持 MCP 适配器只读；索引管理继续留在人工 CLI/底层受控接口，不进入默认 Agent 工具。
4. 正文/OCR/语义搜索应作为可选的独立索引层，不进入默认单文件核心。
