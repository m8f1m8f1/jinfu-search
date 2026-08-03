use std::{path::PathBuf, process::Command, sync::Arc, time::Duration};

use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router,
};
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::{DEFAULT_PIPE_NAME, call_pipe};

const HOST_START_RETRIES: usize = 20;
const HOST_START_DELAY: Duration = Duration::from_millis(150);

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchFilesRequest {
    #[schemars(description = "文件名、扩展名或用空格分隔的多个名称关键词")]
    pub query: String,
    #[schemars(description = "最多返回多少项；默认 50，范围 1–200")]
    pub limit: Option<usize>,
    #[schemars(description = "可选索引根，例如 C:\\ 或 D:\\资料；不确定时省略")]
    pub root: Option<PathBuf>,
}

/// 只读 MCP 门面。索引维护由 GUI 宿主承担，适配器不暴露扫描、删除或关机工具。
#[derive(Clone)]
pub struct JinfuSearchMcp {
    startup_lock: Arc<Mutex<()>>,
    tool_router: ToolRouter<Self>,
}

impl Default for JinfuSearchMcp {
    fn default() -> Self {
        Self {
            startup_lock: Arc::new(Mutex::new(())),
            tool_router: Self::tool_router(),
        }
    }
}

#[tool_router]
impl JinfuSearchMcp {
    #[tool(
        description = "搜索本机文件和目录的名称元数据。先调用 index_status 判断覆盖是否完整；返回路径、名称、目录标记、大小和修改时间，不读取文件正文，也不授权打开、修改或删除结果文件。"
    )]
    async fn search_files(
        &self,
        Parameters(SearchFilesRequest { query, limit, root }): Parameters<SearchFilesRequest>,
    ) -> String {
        let result = self
            .rpc(
                "search.query",
                json!({"query": query, "limit": limit.unwrap_or(50).clamp(1, 200), "root": root}),
            )
            .await;
        render_result(result)
    }

    #[tool(
        description = "读取自动索引覆盖概况。开始搜索前应先调用：roots 为 0 表示首次索引尚未开始或宿主异常；incomplete_roots 大于 0 表示部分卷仍在后台校准，没搜到不能直接断言文件不存在。"
    )]
    async fn index_status(&self) -> String {
        render_result(self.rpc("status.get", Value::Null).await)
    }

    #[tool(
        description = "列出每个索引根、最后更新时间、完整性和是否使用 NTFS USN 增量。仅用于判断搜索边界；MCP 默认不提供扫描、删除索引根或关闭宿主等写操作。"
    )]
    async fn list_index_roots(&self) -> String {
        render_result(self.rpc("index.list_roots", Value::Null).await)
    }
}

#[tool_handler]
impl ServerHandler for JinfuSearchMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "用于定位本机文件和目录名称的只读工具。先调用 index_status；若 incomplete_roots 大于 0，未命中不等于文件不存在。search_files 只返回元数据路径，不授权读取、执行、修改或删除文件。MCP 不提供索引管理和关机操作。"
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "jinfu-search".to_owned(),
                title: Some("金福搜索".to_owned()),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                description: Some("只读的 Windows 本机文件名搜索 MCP 适配器".to_owned()),
                icons: None,
                website_url: Some("https://github.com/m8f1m8f1/jinfu-search".to_owned()),
            },
            ..Default::default()
        }
    }
}

impl JinfuSearchMcp {
    async fn rpc(&self, method: &str, params: Value) -> Result<Value, String> {
        let request = json!({
            "jsonrpc": "2.0",
            "id": format!("mcp-{method}"),
            "method": method,
            "params": params,
        });
        if let Ok(response) = call_pipe(DEFAULT_PIPE_NAME, request.clone()).await {
            return extract_result(response);
        }

        let _guard = self.startup_lock.lock().await;
        if let Ok(response) = call_pipe(DEFAULT_PIPE_NAME, request.clone()).await {
            return extract_result(response);
        }
        start_sibling_host()?;
        for _ in 0..HOST_START_RETRIES {
            tokio::time::sleep(HOST_START_DELAY).await;
            if let Ok(response) = call_pipe(DEFAULT_PIPE_NAME, request.clone()).await {
                return extract_result(response);
            }
        }
        Err("金福搜索宿主未能在 3 秒内就绪。请确认 jinfu-search.exe 与 jinfu-search-mcp.exe 位于同一目录，并允许主程序运行。".to_owned())
    }
}

fn start_sibling_host() -> Result<(), String> {
    let adapter =
        std::env::current_exe().map_err(|error| format!("无法定位 MCP 适配器：{error}"))?;
    let host = adapter.with_file_name("jinfu-search.exe");
    if !host.is_file() {
        return Err(format!(
            "未找到同目录宿主：{}。请同时复制 jinfu-search.exe。",
            host.display()
        ));
    }
    Command::new(&host)
        .arg("--background")
        .spawn()
        .map(|_| ())
        .map_err(|error| format!("无法启动 {}：{error}", host.display()))
}

fn extract_result(response: Value) -> Result<Value, String> {
    if let Some(error) = response.get("error") {
        return Err(format!("金福搜索返回错误：{error}"));
    }
    response
        .get("result")
        .cloned()
        .ok_or_else(|| "金福搜索返回了不完整的 JSON-RPC 响应".to_owned())
}

fn render_result(result: Result<Value, String>) -> String {
    match result {
        Ok(value) => serde_json::to_string_pretty(&json!({"ok": true, "data": value}))
            .unwrap_or_else(|error| format!("{{\"ok\":false,\"error\":\"{error}\"}}")),
        Err(error) => serde_json::to_string_pretty(&json!({"ok": false, "error": error}))
            .unwrap_or_else(|_| "{\"ok\":false,\"error\":\"serialization failed\"}".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_result_keeps_errors_machine_readable() {
        let rendered = render_result(Err("宿主未运行".to_owned()));
        let value: Value = serde_json::from_str(&rendered).unwrap();
        assert_eq!(value["ok"], false);
        assert_eq!(value["error"], "宿主未运行");
    }

    #[test]
    fn json_rpc_errors_are_not_misreported_as_success() {
        let response = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "error": {"code": -32000, "message": "索引不可用"}
        });
        assert!(extract_result(response).unwrap_err().contains("索引不可用"));
    }
}
