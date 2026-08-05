# 金福搜索（Jinfu Search）

<p align="center">
  <img src="assets/icon-512.png" alt="金福搜索" width="120">
</p>

一个面向 Windows 的轻量本机文件名搜索器。双击即用，自动维护索引，也可通过只读 MCP 工具供本地 Agent 搜索文件位置。

> Windows-first local filename search with automatic indexing and a read-only MCP adapter for local agents.

<p align="center">
  <img src="https://img.shields.io/github/actions/workflow/status/m8f1m8f1/jinfu-search/ci.yml?branch=master&label=CI" alt="CI">
  <img src="https://img.shields.io/github/v/release/m8f1m8f1/jinfu-search?label=Release" alt="Release">
  <img src="https://img.shields.io/github/license/m8f1m8f1/jinfu-search" alt="License">
  <img src="https://img.shields.io/badge/platform-Windows-blue" alt="Platform">
  <img src="https://img.shields.io/github/downloads/m8f1m8f1/jinfu-search/total" alt="Downloads">
</p>



## 特点

- **自动索引**：无需按钮。首次启动自动识别固定磁盘并建立索引，之后持续维护。
- **低资源占用**：NTFS 卷优先读取 MFT 元数据；有 USN Journal 时每 15 秒只读取创建、删除和重命名事件，并把异常积压切成 16 KiB 小批次。其他卷用文件系统变更通知，750 ms 合并事件，并每 6 小时低优先级差量校准一次。
- **界面不卡顿**：索引、通知处理和校准串行运行在独立的 Windows 后台优先级线程，不占用 GUI 或 MCP 请求线程。
- **单文件绿色版**：主程序和 MCP 适配器分别是静态 CRT 单 EXE；目标电脑无需 Rust、SQLite DLL 或 VC++ Redistributable。
- **隐私克制**：索引只保存路径、名称、目录标记、大小、修改时间和 NTFS 文件引用号，不读取或保存文件正文。
- **Agent 友好**：MCP 服务器通过标准输入输出运行，提供自描述的只读工具，并在需要时静默启动同目录的搜索宿主。

## 下载与使用

从 GitHub Releases 下载：

- `jinfu-search.exe`：图形搜索器。复制到任意目录后双击即可。
- `jinfu-search-mcp.exe`：本地 MCP 适配器。需要与 `jinfu-search.exe` 放在同一目录。
- `jinfu-search-cli.exe`：可选的命令行与 JSON-RPC 调试入口。

第一次启动会自动扫描固定磁盘，搜索窗口始终可以操作。扫描期间结果会逐步可用；托盘提示会显示后台正在自动维护。索引数据库位于 `%LOCALAPPDATA%\JinfuSearch\index.db`，它是可重建的本机运行数据，不是 EXE 的依赖。

在搜索框输入文件名、扩展名或以空格分隔的多个相关词，例如 `季度 报告`。双击结果会在资源管理器中选中对应文件或目录。关闭窗口后程序留在托盘继续维护索引。

## 自动维护机制

| 场景 | 行为 |
| --- | --- |
| 新电脑、首次运行或发现新固定磁盘 | 自动建立完整快照 |
| NTFS 且有活动 USN Journal | 每 15 秒读取一次名称/路径增量；高写入积压按 16 KiB 分轮追平 |
| 没有可用 USN Journal | 递归监听文件系统事件，750 ms 合并后只刷新受影响子树 |
| 通知溢出、丢事件或路径树异常 | 自动触发完整校准 |
| 无 USN 的正常卷 | 每 6 小时差量校准，未变化条目不重写，修正极端情况下漏掉的事件 |

为了保持流畅，后台只使用一个低优先级工作线程，不会同时扫多个磁盘。休眠、断电或文件系统异常后，最坏情况下也会由下一次校准恢复正确性。网络盘、临时移动盘不会默认自动建立全盘索引，可通过 CLI 显式添加目录。

## MCP：让本地 Agent 搜索文件

把 `jinfu-search-mcp.exe` 与 `jinfu-search.exe` 放在同一目录。以 Codex 为例：

```powershell
codex mcp add jinfu-search -- "D:\Tools\JinfuSearch\jinfu-search-mcp.exe"
codex mcp list
```

或在 `~/.codex/config.toml` 中配置：

```toml
[mcp_servers.jinfu-search]
command = "D:\\Tools\\JinfuSearch\\jinfu-search-mcp.exe"
startup_timeout_sec = 10.0
tool_timeout_sec = 60.0
```

适配器暴露三个只读工具：

- `index_status`：先检查索引是否就绪以及是否存在不完整卷。
- `list_index_roots`：查看已识别的磁盘、最后更新时间和维护方式。
- `search_files`：按名称、扩展名、相关词和可选根路径查找，默认最多返回 50 条。

单次查询最多 512 个字符、16 个空格分隔词和 200 条 MCP 结果，避免异常客户端占用过多本机资源。

MCP 初始化响应内含正确使用顺序和安全边界。它只返回索引元数据；搜索结果不是读取、执行、修改、上传或删除文件的授权。若某个根仍在首次索引或标为不完整，“没搜到”不能直接推断“文件不存在”。详细说明见 [docs/MCP.md](docs/MCP.md)。

## 构建与验证

需要 Rust stable 与 Visual Studio Build Tools（MSVC）：

```powershell
git clone https://github.com/m8f1m8f1/jinfu-search.git
cd jinfu-search
cargo test --all-targets
cargo build --release --bins
```

`.cargo/config.toml` 为 `x86_64-pc-windows-msvc` 启用静态 CRT。Windows 自带的 `kernel32.dll`、`user32.dll` 等系统组件仍由操作系统提供。

CLI 示例：

```powershell
.\target\release\jinfu-search-cli.exe status
.\target\release\jinfu-search-cli.exe search "季度报告"
.\target\release\jinfu-search-cli.exe roots
.\target\release\jinfu-search-cli.exe index "D:\资料"
```

底层本机服务使用仅限本机的 Windows Named Pipe `\\.\pipe\jinfu-search-v1`，不监听 TCP 端口。CLI 还保留显式索引管理和 JSON-RPC 调试接口；普通用户和 MCP Agent 不需要调用它们。

## 当前边界

- 这是文件名和路径元数据搜索，不是文件正文全文检索。
- NTFS MFT/USN 是最快路径；FAT、exFAT 和无卷访问权限的场景会退回普通目录扫描与变更通知。
- 单个 NTFS 文件引用号目前只维护一条路径，不展开硬链接的每一条目录项。
- 首次扫描数百万文件以及日志换代后的完整校准仍需要磁盘读取时间，但在后台低优先级执行。
- 首次 MFT 快照需要在内存中解析父子路径树，瞬时内存峰值会随文件数量增长；完成后会释放，日常只做小批量增量。

## 参与和支持

欢迎提交 Issue、修复、文档和适配测试，规则见 [CONTRIBUTING.md](CONTRIBUTING.md)。如果这个项目帮助了你，Star、分享和贡献代码就是最直接的支持。

作者完成可用的赞助账户后，会在 `.github/FUNDING.yml` 和本节加入官方链接；在此之前本项目不接受任何冒名付款链接。

## 许可证

[MIT License](LICENSE)。版本记录见 [CHANGELOG.md](CHANGELOG.md)，安全问题请按 [SECURITY.md](SECURITY.md) 私下报告。
