# Jinfu Search MCP 接入说明

`jinfu-search-mcp.exe` 是一个很薄的本机 STDIO MCP 适配器。它把 MCP 工具请求转换为同一用户会话中的 Windows Named Pipe 请求，不开放网络端口，也不复制索引数据库。

## 安装

1. 把 `jinfu-search.exe` 和 `jinfu-search-mcp.exe` 放在同一目录。
2. 在 MCP 客户端中把 `jinfu-search-mcp.exe` 配为 STDIO 服务器。
3. 重启 MCP 客户端并确认出现三个工具。

Codex 命令：

```powershell
codex mcp add jinfu-search -- "D:\Tools\JinfuSearch\jinfu-search-mcp.exe"
```

通用 JSON 配置示意：

```json
{
  "mcpServers": {
    "jinfu-search": {
      "command": "D:\\Tools\\JinfuSearch\\jinfu-search-mcp.exe"
    }
  }
}
```

不同客户端的配置字段可能不同，以对应客户端文档为准。

## 推荐调用顺序

1. 每个任务首次搜索前调用 `index_status`。
2. 若 `incomplete_roots` 大于 0，可调用 `list_index_roots` 判断受影响的磁盘。
3. 调用 `search_files`，先使用能区分目标的名称相关词，再用 `root` 缩小范围。
4. Agent 在读取、执行、修改、移动、上传或删除命中的文件前，必须遵守宿主环境的权限与用户授权流程。

## 工具

### `search_files`

参数：

- `query`：必填；文件名、扩展名或空格分隔的多个相关词，最多 512 个字符、16 个词。
- `limit`：可选，1–200，默认 50。
- `root`：可选，例如 `C:\Users\name\Documents`，只返回该路径边界内的结果。

返回路径、是否目录、大小和修改时间等索引元数据，不返回正文。

### `index_status`

返回记录数、根数量和不完整根数量。首次自动索引期间记录会逐步增加。

### `list_index_roots`

返回每个根的完成状态、跳过条目、最后更新时间，以及是否使用 NTFS USN 增量维护。

## 自动启动与生命周期

如果本机宿主没有运行，适配器会静默启动同目录下的 `jinfu-search.exe --background`，最多等待 3 秒连接本机管道。MCP 客户端退出只会结束适配器；搜索宿主会留在托盘继续维护索引。用户可从托盘菜单退出宿主。

## 安全边界

- 工具全部只读，不提供索引删除、强制扫描、应用退出或任意路径读取能力。
- Named Pipe 仅允许本机连接，ACL 限制为当前用户、SYSTEM 和 Administrators。
- 工具返回的路径可能指向敏感文件；MCP 客户端应避免把结果发送到不受信任的远端服务。
- 不完整索引中的空结果不是文件不存在的可靠证据。
- 搜索结果只说明元数据中存在匹配项，不代表 Agent 获得了后续操作授权。
