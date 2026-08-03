//! 共享的 CLI/托盘启动逻辑；两个可执行入口只在宿主形态上不同。

use std::{io::Write, path::PathBuf, process::ExitCode};

use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use tracing_subscriber::EnvFilter;

use crate::{SearchIndex, SearchRequest, handle_rpc};

#[cfg(windows)]
use crate::{DEFAULT_PIPE_NAME, call_pipe, serve_pipe_controlled};

#[cfg(all(windows, feature = "tray"))]
use crate::tray;

#[cfg(windows)]
use tokio::sync::watch;

#[derive(Debug, Parser)]
#[command(name = "jinfu-search-cli", about = "低资源本地文件搜索（第一版）")]
struct Cli {
    /// SQLite 索引库路径。默认位于当前用户的 LocalAppData。
    #[arg(long, global = true)]
    database: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// 递归建立一个目录根的元数据索引；不会读取文件正文。
    Index { root: PathBuf },
    /// 通过 NTFS MFT 建立整卷元数据快照；例如 `index-volume C`。
    #[cfg(windows)]
    IndexVolume { volume: char },
    /// 读取已建立 NTFS 索引的 USN 增量；日志换代时自动回退为一次 MFT 快照。
    #[cfg(windows)]
    SyncVolume { volume: char },
    /// 只增量同步所有已建立 NTFS 索引的卷；需要重建的卷只会被标记，不会自动全盘扫描。
    #[cfg(windows)]
    SyncAllVolumes,
    /// 只读验证一个 NTFS 卷的 MFT/USN 通路，不建立索引。
    #[cfg(windows)]
    ProbeVolume { volume: char },
    /// 搜索已建立的索引。
    Search {
        query: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// 显示索引概况。
    Status,
    /// 列出所有索引根及其完成状态。
    Roots,
    /// 用 stdin 的单条 JSON-RPC 请求测试 Agent 控制协议。
    Rpc { request: String },
    /// 启动仅限本机 Named Pipe 的 Agent 控制服务；按 Ctrl+C 正常退出。
    #[cfg(windows)]
    Serve {
        #[arg(long, default_value = DEFAULT_PIPE_NAME)]
        pipe: String,
    },
    /// 通过 Named Pipe 发送一条 JSON-RPC 请求，供 MCP/Agent 适配器复用。
    #[cfg(windows)]
    Agent {
        #[arg(long, default_value = DEFAULT_PIPE_NAME)]
        pipe: String,
        request: String,
    },
    /// 驻留系统托盘，并在后台启动同一条本地 Agent 管道。
    #[cfg(all(windows, feature = "tray"))]
    Tray {
        #[arg(long, default_value = DEFAULT_PIPE_NAME)]
        pipe: String,
    },
}

/// 无窗口的用户入口：只接受双击/无参数启动，避免 Windows 把它误用为 CLI。
pub fn run_gui() -> ExitCode {
    if std::env::args_os().nth(1).is_some() {
        return show_gui_cli_usage();
    }
    run(true)
}

/// 供人和 Agent 适配器调用的控制台入口，保留稳定的 stdout JSON 协议。
pub fn run_cli() -> ExitCode {
    run(false)
}

fn run(show_dialogs: bool) -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_target(false)
        .without_time()
        .init();

    let cli = Cli::parse();
    let default_tray_start = cli.command.is_none();
    let database = cli.database.unwrap_or_else(default_database_path);
    let index = match SearchIndex::open(database) {
        Ok(index) => index,
        Err(error) => {
            report_error(
                &format!("初始化索引失败：{error}"),
                default_tray_start,
                show_dialogs,
            );
            return ExitCode::FAILURE;
        }
    };

    let output = match cli.command {
        #[cfg(all(windows, feature = "tray"))]
        None => tray::run(index, DEFAULT_PIPE_NAME.to_owned())
            .map(|_| json!({"stopped": true}))
            .map_err(tray_error),
        #[cfg(not(all(windows, feature = "tray")))]
        None => {
            report_error(
                "此构建不包含系统托盘；请指定一个子命令。",
                true,
                show_dialogs,
            );
            return ExitCode::FAILURE;
        }
        Some(Command::Index { root }) => index
            .scan_root(root)
            .and_then(|report| serde_json::to_value(report).map_err(json_error)),
        #[cfg(windows)]
        Some(Command::IndexVolume { volume }) => index
            .scan_ntfs_volume(volume)
            .and_then(|report| serde_json::to_value(report).map_err(json_error)),
        #[cfg(windows)]
        Some(Command::SyncVolume { volume }) => index
            .sync_ntfs_volume(volume)
            .and_then(|report| serde_json::to_value(report).map_err(json_error)),
        #[cfg(windows)]
        Some(Command::SyncAllVolumes) => index.sync_indexed_ntfs_volumes().and_then(|reports| {
            serde_json::to_value(json!({"reports": reports})).map_err(json_error)
        }),
        #[cfg(windows)]
        Some(Command::ProbeVolume { volume }) => crate::ntfs::probe_volume(volume)
            .map_err(pipe_error)
            .and_then(|probe| serde_json::to_value(probe).map_err(json_error)),
        Some(Command::Search { query, limit, root }) => {
            let mut request = SearchRequest::new(query);
            request.limit = limit;
            request.root = root;
            index
                .search(&request)
                .and_then(|items| serde_json::to_value(json!({"items": items})).map_err(json_error))
        }
        Some(Command::Status) => index
            .status()
            .and_then(|status| serde_json::to_value(status).map_err(json_error)),
        Some(Command::Roots) => index
            .indexed_roots()
            .and_then(|roots| serde_json::to_value(json!({"roots": roots})).map_err(json_error)),
        Some(Command::Rpc { request }) => {
            let request: Value = match serde_json::from_str(&request) {
                Ok(request) => request,
                Err(error) => {
                    report_error(
                        &format!("JSON-RPC 请求不是合法 JSON：{error}"),
                        false,
                        show_dialogs,
                    );
                    return ExitCode::FAILURE;
                }
            };
            Ok(handle_rpc(&index, request))
        }
        #[cfg(windows)]
        Some(Command::Serve { pipe }) => serve(index, pipe).map(|_| json!({"stopped": true})),
        #[cfg(windows)]
        Some(Command::Agent { pipe, request }) => {
            let request: Value = match serde_json::from_str(&request) {
                Ok(request) => request,
                Err(error) => {
                    report_error(
                        &format!("JSON-RPC 请求不是合法 JSON：{error}"),
                        false,
                        show_dialogs,
                    );
                    return ExitCode::FAILURE;
                }
            };
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    report_error(
                        &format!("启动本地 Agent 客户端失败：{error}"),
                        false,
                        show_dialogs,
                    );
                    return ExitCode::FAILURE;
                }
            };
            runtime
                .block_on(call_pipe(&pipe, request))
                .map_err(pipe_error)
        }
        #[cfg(all(windows, feature = "tray"))]
        Some(Command::Tray { pipe }) => tray::run(index, pipe)
            .map(|_| json!({"stopped": true}))
            .map_err(tray_error),
    };

    match output {
        Ok(_) if default_tray_start && show_dialogs => ExitCode::SUCCESS,
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(text) => {
                let mut stdout = std::io::stdout().lock();
                match writeln!(stdout, "{text}") {
                    Ok(()) => ExitCode::SUCCESS,
                    Err(error) => {
                        report_error(&format!("输出 JSON 失败：{error}"), false, show_dialogs);
                        ExitCode::FAILURE
                    }
                }
            }
            Err(error) => {
                report_error(&format!("输出 JSON 失败：{error}"), false, show_dialogs);
                ExitCode::FAILURE
            }
        },
        Err(error) => {
            report_error(
                &format!("命令执行失败：{error}"),
                default_tray_start,
                show_dialogs,
            );
            ExitCode::FAILURE
        }
    }
}

fn default_database_path() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("JinfuSearch")
        .join("index.db")
}

fn report_error(message: &str, default_tray_start: bool, show_dialogs: bool) {
    #[cfg(all(windows, feature = "tray"))]
    if default_tray_start && show_dialogs {
        show_default_tray_error(message);
        return;
    }

    eprintln!("{message}");
}

#[cfg(all(windows, feature = "tray"))]
fn show_gui_cli_usage() -> ExitCode {
    show_message_box(
        "金福搜索命令行入口",
        "jinfu-search.exe 是无窗口的托盘宿主。\n\n请双击它启动金福搜索；命令行和 Agent 适配器请使用同目录的 jinfu-search-cli.exe。",
        false,
    );
    ExitCode::FAILURE
}

#[cfg(not(all(windows, feature = "tray")))]
fn show_gui_cli_usage() -> ExitCode {
    eprintln!("请使用 jinfu-search-cli.exe 执行命令行操作。");
    ExitCode::FAILURE
}

#[cfg(all(windows, feature = "tray"))]
fn show_default_tray_error(message: &str) {
    show_message_box(
        "金福搜索启动失败",
        &format!(
            "金福搜索没有成功进入系统托盘。\n\n{message}\n\n如果已经运行一个实例，请在时钟旁的 ^ 中检查托盘图标；可先从托盘菜单选择“退出金福搜索”，再重试。"
        ),
        true,
    );
}

#[cfg(all(windows, feature = "tray"))]
fn show_message_box(title: &str, body: &str, is_error: bool) {
    use std::iter::once;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let body: Vec<u16> = body.encode_utf16().chain(once(0)).collect();
    let title: Vec<u16> = title.encode_utf16().chain(once(0)).collect();
    let style = MB_OK | if is_error { MB_ICONERROR } else { 0 };
    unsafe {
        MessageBoxW(std::ptr::null_mut(), body.as_ptr(), title.as_ptr(), style);
    }
}

fn json_error(error: serde_json::Error) -> crate::SearchError {
    crate::SearchError::Io {
        path: PathBuf::from("<json output>"),
        source: std::io::Error::new(std::io::ErrorKind::InvalidData, error),
    }
}

#[cfg(windows)]
fn serve(index: SearchIndex, pipe: String) -> Result<(), crate::SearchError> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(pipe_error)?;
    runtime.block_on(async move {
        let (shutdown, shutdown_rx) = watch::channel(false);
        let ctrl_c_shutdown = shutdown.clone();
        let ctrl_c = tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                let _ = ctrl_c_shutdown.send(true);
            }
        });
        let result = serve_pipe_controlled(index, pipe, shutdown_rx, shutdown).await;
        ctrl_c.abort();
        result.map_err(pipe_error)
    })
}

#[cfg(windows)]
fn pipe_error(error: std::io::Error) -> crate::SearchError {
    crate::SearchError::Io {
        path: PathBuf::from("<named pipe>"),
        source: error,
    }
}

#[cfg(all(windows, feature = "tray"))]
fn tray_error(error: String) -> crate::SearchError {
    crate::SearchError::Io {
        path: PathBuf::from("<system tray>"),
        source: std::io::Error::other(error),
    }
}

#[cfg(all(test, windows, feature = "tray"))]
mod tests {
    use super::*;

    #[test]
    fn no_arguments_are_accepted_for_default_tray_startup() {
        let cli = Cli::try_parse_from(["jinfu-search-cli"]).expect("无参数应启动托盘而非报错退出");
        assert!(cli.command.is_none());
    }
}
