# 金福搜索（Jinfu Search）

Windows 优先的 Rust 本地文件搜索服务：首次整卷索引读取 NTFS MFT 元数据，后续通过 USN Journal 做增量更新；默认不读取、不保存文件正文。

`v0.2.0` 的 GUI 成品是 x64 Windows 10/11 单文件绿色版：Rust、SQLite 和 MSVC CRT 均静态链接，不需要安装 Rust、SQLite DLL 或 Microsoft Visual C++ Redistributable。程序仍会按正常应用惯例在 `%LOCALAPPDATA%\JinfuSearch\index.db` 创建本机索引数据；该数据库是运行数据，不是随 EXE 搬运的依赖。

## 当前能力

- 搜索文件和目录名称，支持 Unicode 大小写无关匹配、扩展名优先和空格分隔的多个名称相关词；路径文本也可作为辅助匹配条件，但不承诺完整的 Unicode 大小写折叠。
- `index-volume C` 使用 `FSCTL_ENUM_USN_DATA` 建立 NTFS 整卷元数据快照；不会遍历打开每个文件，也不会创建或修改 USN 日志。
- 托盘驻留使用 Tao + `tray-icon`，没有 WebView；空闲时等待事件。
- 直接双击主程序会打开原生搜索窗口；窗口内可建立/更新本机固定磁盘索引，输入名称、扩展名或多个名称关键词后搜索，双击结果可在资源管理器中定位文件。
- 完整建立的 NTFS 索引每 15 秒只读取 USN 增量。日志换代、截断或路径树不完整时标记“需重建”，不会在后台偷偷启动整卷扫描。
- Agent API 使用本机 Windows Named Pipe，而非 HTTP/TCP：默认管道为 `\\.\pipe\jinfu-search-v1`，ACL 仅授予当前用户、SYSTEM 和 Administrators，并拒绝远程客户端。
- SQLite 使用 WAL；索引只保存路径、名称、目录标记、大小、修改时间和 NTFS 文件引用号。

## 构建

```powershell
cd C:\Users\Administrator\.codex\JINFU-WORK-ONE\jinfu-search
cargo build --release
```

构建会生成两个入口：

- `target\release\jinfu-search.exe`：无窗口的托盘宿主，供直接双击启动。
- `target\release\jinfu-search-cli.exe`：控制台与 Agent 适配器，输出稳定的 JSON。

仓库的 `.cargo/config.toml` 为 `x86_64-pc-windows-msvc` 启用静态 CRT，因此只复制 `jinfu-search.exe` 就能使用图形搜索。CLI 仅在脚本或 Agent 需要现成的 JSON/Named Pipe 适配器时使用，不是 GUI 的运行依赖。Windows 自带的 `kernel32.dll`、`user32.dll` 等系统组件仍由操作系统提供。

直接双击 `jinfu-search.exe` 会启动系统托盘驻留并打开搜索窗口（不会自动扫描任何磁盘，也不会弹出空白控制台）。关闭搜索窗口只会回到托盘待命；图标可能位于时钟旁的 `^` 隐藏图标区。右键图标可再次打开搜索窗口、建立 C: 索引、刷新、同步或退出。启动失败会显示明确的 Windows 错误提示。
需要脚本、终端或 Agent 适配器操作时，使用 `jinfu-search-cli.exe`。

## 首次使用搜索窗口

1. 双击 `target\release\jinfu-search.exe`，会出现“金福搜索”窗口。
2. 首次使用点击窗口里的“建立/更新本机索引”。程序会索引本机固定磁盘；优先读取 NTFS 元数据，不具备卷访问条件时自动回退到普通目录扫描。首次索引数百万文件可能需要几分钟，期间窗口不会卡死。
3. 完成后，在搜索框输入文件名、扩展名（例如 `pdf`），或以空格分隔的多个名称相关词（例如 `季度 报告`），点击“搜索”。
4. 双击某条结果，即可在资源管理器中定位该文件或目录；关闭窗口后程序仍留在托盘。

窗口默认枚举本机固定磁盘；如需索引移动盘、网络盘或单独目录，请继续使用下方的 CLI `index` 命令。
托盘确认只约束人工点击；受信任 Agent 只有在收到明确的
`index.scan_volume` 请求时才会建立整卷索引。

## 人工使用

首次对 NTFS 卷建立索引是一次明确的、可能耗时的操作：

```powershell
.\target\release\jinfu-search-cli.exe index-volume C
```

非 NTFS 卷或只想建立某个目录的索引：

```powershell
.\target\release\jinfu-search-cli.exe index "D:\资料"
```

搜索和查看状态：

```powershell
.\target\release\jinfu-search-cli.exe search "季度报告"
.\target\release\jinfu-search-cli.exe status
.\target\release\jinfu-search-cli.exe roots
```

启动托盘和本地 Agent 服务：

```powershell
.\target\release\jinfu-search-cli.exe tray
```

单独作为无界面的本地服务运行：

```powershell
.\target\release\jinfu-search-cli.exe serve
```

默认数据库位于 `%LOCALAPPDATA%\JinfuSearch\index.db`。可通过全局 `--database` 指向其他 SQLite 文件。

## Agent 控制接口

服务采用单请求/连接的 JSON-RPC 2.0，帧格式是 `u32 little-endian 长度 + UTF-8 JSON`。Agent 可直接实现该协议，也可以调用 CLI 的 `agent` 子命令：

```powershell
.\target\release\jinfu-search-cli.exe agent '{"jsonrpc":"2.0","id":1,"method":"search.query","params":{"query":"合同","limit":20}}'
```

默认管道名适合单机人工使用。若 Agent 自动化会依据返回结果做后续动作，请为
每次服务启动传入随机、由启动方安全传递给 Agent 的管道名，避免同一 Windows
用户的其他进程预占固定名称：

```powershell
$pipe = "\\.\pipe\jinfu-search-$([guid]::NewGuid().ToString('N'))"
.\target\release\jinfu-search-cli.exe tray --pipe $pipe
# Agent 端使用同一个 $pipe 调用 `agent --pipe $pipe <json-rpc>`。
```

可用方法：

| 方法 | 参数 | 作用 |
| --- | --- | --- |
| `status.get` | 无 | 索引数量与不完整根数 |
| `index.list_roots` | 无 | 索引根、完成状态、是否使用 USN |
| `search.query` | `query`, 可选 `limit`, `root` | 搜索元数据，不返回文件内容 |
| `index.scan_root` | `root` | 建立/刷新一个目录根 |
| `index.remove_root` | `root` | 删除一个索引根，即使移动盘已断开也可移除 |
| `index.scan_volume` | `volume` | 显式建立 NTFS 全卷 MFT 快照 |
| `index.sync_volume` | `volume` | 显式重放一个卷的 USN 增量；必要时允许重建 |
| `index.sync_all_volumes` | 无 | 只同步已建立的 NTFS 索引；不会自动重建整卷 |
| `app.shutdown` | 无 | 优雅停止通过 `serve` 或 `tray` 启动的本地服务 |

例如，Agent 明确要求建立 C 盘索引时：

```json
{
  "jsonrpc": "2.0",
  "id": "index-c",
  "method": "index.scan_volume",
  "params": { "volume": "C" }
}
```

## 边界与下一阶段

- 本版没有正文全文检索；这是一项隐私与资源约束。若需要正文搜索，应另加可选择、可排除目录、可暂停的 Tantivy 内容索引层。
- NTFS MFT/USN 路径适用于 Windows NTFS。FAT、exFAT、网络盘和不具备卷访问权限的场景使用 `index <目录>` 回退扫描。
- 初次全卷快照和日志换代后的重建可能需要较高的卷访问权限；请由用户或受信任 Agent 显式触发。
- 当前路径树按单个 NTFS 文件引用号维护，不把硬链接的每一条目录项展开为多条结果；这是下一阶段应补齐的 NTFS 语义。
- 同类开源项目与 Agent API 的阶段性评估见 [`docs/stage-2-evaluation.md`](docs/stage-2-evaluation.md)。
