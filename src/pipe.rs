use std::{
    io,
    mem::size_of,
    ptr,
    sync::Arc,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::windows::named_pipe::{ClientOptions, NamedPipeServer, PipeMode, ServerOptions},
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{sleep, timeout},
};
use windows_sys::{
    Win32::{
        Foundation::{
            CloseHandle, ERROR_ACCESS_DENIED, ERROR_PIPE_BUSY, ERROR_PIPE_NOT_CONNECTED, HANDLE,
            LocalFree,
        },
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SDDL_REVISION_1,
            },
            GetTokenInformation, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER, TokenUser,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    },
    core::PWSTR,
};

use crate::{SearchIndex, handle_rpc};

pub const DEFAULT_PIPE_NAME: &str = r"\\.\pipe\jinfu-search-v1";
const MAX_RPC_FRAME_BYTES: usize = 1024 * 1024;
const MAX_PIPE_INSTANCES: usize = 16;
const MAX_ACTIVE_PIPE_CONNECTIONS: usize = MAX_PIPE_INSTANCES - 1;
const PIPE_CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
const PIPE_FRAME_TIMEOUT: Duration = Duration::from_secs(15);

/// 运行只限本机当前用户的 Agent 控制服务。
///
/// 管道 ACL 明确只授予当前用户、SYSTEM 和 Administrators 完全访问权；默认
/// `PIPE_REJECT_REMOTE_CLIENTS` 也拒绝远程命名管道连接。
pub async fn serve_pipe(
    index: SearchIndex,
    pipe_name: String,
    shutdown: watch::Receiver<bool>,
) -> io::Result<()> {
    serve_pipe_inner(index, pipe_name, shutdown, None).await
}

/// 与 `serve_pipe` 相同，但允许通过已认证的本机管道调用 `app.shutdown`。
/// CLI `serve` 和托盘宿主使用此入口；嵌入式调用方可选择不暴露停机能力。
pub async fn serve_pipe_controlled(
    index: SearchIndex,
    pipe_name: String,
    shutdown: watch::Receiver<bool>,
    shutdown_sender: watch::Sender<bool>,
) -> io::Result<()> {
    serve_pipe_inner(index, pipe_name, shutdown, Some(shutdown_sender)).await
}

async fn serve_pipe_inner(
    index: SearchIndex,
    pipe_name: String,
    mut shutdown: watch::Receiver<bool>,
    shutdown_sender: Option<watch::Sender<bool>>,
) -> io::Result<()> {
    validate_pipe_name(&pipe_name)?;
    let security = PipeSecurity::for_current_user()?;
    let mut server = match security.create_server(&pipe_name, true) {
        Ok(server) => server,
        Err(error)
            if matches!(
                error.raw_os_error(),
                Some(code) if code == ERROR_PIPE_BUSY as i32 || code == ERROR_ACCESS_DENIED as i32
            ) =>
        {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                format!("本地 Agent 管道已被占用：{pipe_name}；请确认现有服务，或传入随机 --pipe"),
            ));
        }
        Err(error) => return Err(error),
    };
    let mut connections = JoinSet::new();
    // 永远保留一个未连接实例；即使同一 SID 的客户端故意占满连接，也不会
    // 因尝试创建第 17 个实例而让服务退出。
    let connection_slots = Arc::new(Semaphore::new(MAX_ACTIVE_PIPE_CONNECTIONS));

    loop {
        if *shutdown.borrow() {
            break;
        }

        while connections.try_join_next().is_some() {}

        let permit = tokio::select! {
            permit = connection_slots.clone().acquire_owned() => {
                permit.map_err(|_| io::Error::other("本地 Agent 连接槽已关闭"))?
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
                continue;
            }
        };

        tokio::select! {
            connected = server.connect() => {
                connected?;
                // 先建立下一个实例，避免客户端在连接间隙得到 NotFound。
                let next_server = create_next_server(&security, &pipe_name).await?;
                let connected_server = server;
                server = next_server;
                let index = index.clone();
                let shutdown_sender = shutdown_sender.clone();
                connections.spawn(async move {
                    let _permit = permit;
                    let _ = serve_connection(connected_server, index, shutdown_sender).await;
                });
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }

    // `app.shutdown` 的应答必须先完整写回，再让托盘/CLI 退出；同时为恶意或
    // 半开连接设置有界等待，避免一个不发帧的本地客户端阻塞关停。
    if timeout(Duration::from_secs(2), async {
        while connections.join_next().await.is_some() {}
    })
    .await
    .is_err()
    {
        connections.abort_all();
        while connections.join_next().await.is_some() {}
    }

    Ok(())
}

async fn create_next_server(
    security: &PipeSecurity,
    pipe_name: &str,
) -> io::Result<NamedPipeServer> {
    let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
    loop {
        match security.create_server(pipe_name, false) {
            Ok(server) => return Ok(server),
            Err(error)
                if error.raw_os_error() == Some(ERROR_PIPE_BUSY as i32)
                    && Instant::now() < deadline =>
            {
                // 已释放的任务许可可能比内核管道实例的销毁早一个调度周期；
                // 这是暂态忙碌，等待而不是让服务整体退出。
                sleep(Duration::from_millis(20)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

/// 供 CLI/MCP 适配器调用的本地客户端。它只连接 Windows Named Pipe，不会
/// 打开或使用 TCP 端口。
pub async fn call_pipe(pipe_name: &str, request: Value) -> io::Result<Value> {
    validate_pipe_name(pipe_name)?;
    let deadline = Instant::now() + PIPE_CONNECT_TIMEOUT;
    let mut client = loop {
        let error = match ClientOptions::new().open(pipe_name) {
            Ok(mut client) => {
                match timeout(PIPE_FRAME_TIMEOUT, write_frame(&mut client, &request)).await {
                    Ok(Ok(())) => break client,
                    Ok(Err(error)) if is_transient_connection_error(&error) => error,
                    Ok(Err(error)) => return Err(error),
                    Err(_) => {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "写入本地 Agent 请求超时",
                        ));
                    }
                }
            }
            Err(error) => error,
        };
        if !is_transient_connection_error(&error) || Instant::now() >= deadline {
            return Err(error);
        }
        sleep(Duration::from_millis(20)).await;
    };
    match timeout(PIPE_FRAME_TIMEOUT, read_frame(&mut client)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "等待本地 Agent 响应超时",
        )),
    }
}

fn is_transient_connection_error(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound
        || matches!(
            error.raw_os_error(),
            Some(code)
                if code == ERROR_PIPE_BUSY as i32 || code == ERROR_PIPE_NOT_CONNECTED as i32
        )
}

async fn serve_connection(
    mut pipe: NamedPipeServer,
    index: SearchIndex,
    shutdown_sender: Option<watch::Sender<bool>>,
) -> io::Result<()> {
    let request = match timeout(PIPE_FRAME_TIMEOUT, read_frame(&mut pipe)).await {
        Ok(Ok(request)) => request,
        Ok(Err(error)) => return Err(error),
        Err(_) => {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "等待本地 Agent 请求超时",
            ));
        }
    };
    let shutdown_requested = shutdown_sender.is_some() && is_shutdown_request(&request);
    let response = if shutdown_requested {
        json!({
            "jsonrpc": "2.0",
            "id": request.get("id").cloned().unwrap_or(Value::Null),
            "result": {"stopping": true}
        })
    } else {
        match tokio::task::spawn_blocking(move || handle_rpc(&index, request)).await {
            Ok(response) => response,
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": {"code": -32603, "message": format!("服务任务失败：{error}")}
            }),
        }
    };
    let result = match timeout(PIPE_FRAME_TIMEOUT, write_frame(&mut pipe, &response)).await {
        Ok(result) => result,
        Err(_) => Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "写回本地 Agent 响应超时",
        )),
    };
    if result.is_ok()
        && shutdown_requested
        && let Some(shutdown_sender) = shutdown_sender
    {
        let _ = shutdown_sender.send(true);
    }
    // 不主动调用 DisconnectNamedPipe：它会丢弃客户端尚未读取的输出缓冲。
    // 连接在任务返回时释放句柄，客户端仍能读取已刷新的响应。
    result
}

fn is_shutdown_request(request: &Value) -> bool {
    request.get("jsonrpc") == Some(&Value::String("2.0".to_owned()))
        && request.get("method") == Some(&Value::String("app.shutdown".to_owned()))
}

async fn read_frame<T>(stream: &mut T) -> io::Result<Value>
where
    T: AsyncRead + Unpin,
{
    let mut length = [0_u8; 4];
    stream.read_exact(&mut length).await?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_RPC_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC 帧大小无效",
        ));
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    serde_json::from_slice(&payload).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("JSON-RPC 不是合法 JSON：{error}"),
        )
    })
}

async fn write_frame<T>(stream: &mut T, value: &Value) -> io::Result<()>
where
    T: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value).map_err(|error| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("无法编码 JSON-RPC：{error}"),
        )
    })?;
    if payload.is_empty() || payload.len() > MAX_RPC_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "JSON-RPC 响应大小无效",
        ));
    }
    stream
        .write_all(&(payload.len() as u32).to_le_bytes())
        .await?;
    stream.write_all(&payload).await?;
    stream.flush().await
}

fn validate_pipe_name(pipe_name: &str) -> io::Result<()> {
    let Some(name) = pipe_name.strip_prefix(r"\\.\pipe\") else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "管道名必须以 \\\\.\\pipe\\ 开头",
        ));
    };
    if !name.is_empty() && name != "." && name != ".." && !name.contains(['\\', '/', '\0']) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "管道名必须是 \\\\.\\pipe\\ 下的单个本地名称",
        ))
    }
}

struct PipeSecurity {
    sddl: Vec<u16>,
}

impl PipeSecurity {
    fn for_current_user() -> io::Result<Self> {
        let sid = current_user_sid()?;
        // P = protected DACL；不继承宽泛默认 ACL，避免 BUILTIN\Users 获得控制权。
        let sddl = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)(A;;GA;;;BA)");
        Ok(Self {
            sddl: wide_null(&sddl),
        })
    }

    fn create_server(&self, pipe_name: &str, first_instance: bool) -> io::Result<NamedPipeServer> {
        let mut descriptor = ptr::null_mut();
        let mut descriptor_size = 0_u32;
        let converted = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                self.sddl.as_ptr(),
                SDDL_REVISION_1,
                &mut descriptor,
                &mut descriptor_size,
            )
        };
        if converted == 0 || descriptor.is_null() {
            return Err(io::Error::last_os_error());
        }
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor,
            bInheritHandle: 0,
        };
        let mut options = ServerOptions::new();
        options
            .pipe_mode(PipeMode::Message)
            .max_instances(MAX_PIPE_INSTANCES)
            .reject_remote_clients(true)
            .first_pipe_instance(first_instance);

        // Tokio 只在创建时借用 SECURITY_ATTRIBUTES，随后内核已复制安全描述符。
        let result = unsafe {
            options.create_with_security_attributes_raw(
                pipe_name,
                (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
            )
        };
        unsafe {
            LocalFree(descriptor);
        }
        result
    }
}

fn current_user_sid() -> io::Result<String> {
    let mut token: HANDLE = ptr::null_mut();
    let opened = unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) };
    if opened == 0 {
        return Err(io::Error::last_os_error());
    }
    let token = OwnedHandle(token);

    unsafe {
        let mut required = 0_u32;
        let _ = GetTokenInformation(token.0, TokenUser, ptr::null_mut(), 0, &mut required);
        if required == 0 {
            return Err(io::Error::last_os_error());
        }

        let word_count = (required as usize).div_ceil(size_of::<usize>());
        let mut buffer = vec![0_usize; word_count];
        if GetTokenInformation(
            token.0,
            TokenUser,
            buffer.as_mut_ptr().cast(),
            required,
            &mut required,
        ) == 0
        {
            return Err(io::Error::last_os_error());
        }

        let user = buffer.as_ptr().cast::<TOKEN_USER>();
        let mut sid: PWSTR = ptr::null_mut();
        if ConvertSidToStringSidW((*user).User.Sid, &mut sid) == 0 || sid.is_null() {
            return Err(io::Error::last_os_error());
        }
        let value = string_from_pwstr(sid);
        LocalFree(sid.cast());
        Ok(value)
    }
}

struct OwnedHandle(HANDLE);

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

fn wide_null(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

unsafe fn string_from_pwstr(value: PWSTR) -> String {
    let mut length = 0_usize;
    while unsafe { *value.add(length) } != 0 {
        length += 1;
    }
    let slice = unsafe { std::slice::from_raw_parts(value, length) };
    String::from_utf16_lossy(slice)
}

#[cfg(test)]
mod tests {
    use std::io;

    use serde_json::json;
    use tokio::{
        net::windows::named_pipe::ServerOptions,
        time::{Duration, sleep},
    };

    use super::{
        call_pipe, is_transient_connection_error, read_frame, validate_pipe_name, write_frame,
    };

    #[test]
    fn accepts_only_local_pipe_names() {
        assert!(validate_pipe_name(r"\\.\pipe\jinfu-search-test").is_ok());
        assert!(validate_pipe_name(r"\\.\pipe\nested\jinfu-search-test").is_err());
        assert!(validate_pipe_name(r"\\server\pipe\jinfu-search-test").is_err());
    }

    #[test]
    fn treats_pipe_not_connected_as_a_transient_startup_race() {
        assert!(is_transient_connection_error(
            &io::Error::from_raw_os_error(233)
        ));
    }

    #[tokio::test]
    async fn client_retries_while_a_server_is_between_creation_and_connect() {
        let pipe_name = format!(
            r"\\.\pipe\jinfu-search-connect-race-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let mut server = ServerOptions::new()
            .first_pipe_instance(true)
            .create(&pipe_name)
            .unwrap();
        let client_pipe = pipe_name.clone();
        let client = tokio::spawn(async move {
            call_pipe(
                &client_pipe,
                json!({"jsonrpc": "2.0", "id": "race", "method": "status.get"}),
            )
            .await
        });

        // 确保客户端先观察到“实例存在但尚未 ConnectNamedPipe”的真实窗口。
        sleep(Duration::from_millis(100)).await;
        server.connect().await.unwrap();
        let request = read_frame(&mut server).await.unwrap();
        assert_eq!(request["method"], "status.get");
        write_frame(
            &mut server,
            &json!({"jsonrpc": "2.0", "id": "race", "result": {"ok": true}}),
        )
        .await
        .unwrap();
        let response = client.await.unwrap().unwrap();
        assert_eq!(response["result"]["ok"], true);
    }
}
