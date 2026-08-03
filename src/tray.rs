//! 没有 WebView 的最小系统托盘宿主。

use std::{io, time::Duration};

use tao::{
    event::Event,
    event_loop::{ControlFlow, EventLoopBuilder},
    platform::run_return::EventLoopExtRunReturn,
};
use tokio::sync::watch;
use tray_icon::{
    Icon, TrayIconBuilder,
    menu::{Menu, MenuEvent, MenuItem},
};

use crate::{ScanReport, SearchIndex, search_ui, serve_pipe_controlled};

enum UserEvent {
    Menu(MenuEvent),
    Shutdown,
    IndexFinished(Result<ScanReport, String>),
}

const USN_SYNC_INTERVAL: Duration = Duration::from_secs(15);

/// 在主 UI 线程上运行托盘；Named Pipe 服务运行在 Tokio 工作线程。
pub fn run(index: SearchIndex, pipe_name: String) -> Result<(), String> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let menu = Menu::new();
    let open_search = MenuItem::new("打开搜索窗口", true, None);
    let index_c = MenuItem::new("建立 C: 索引…", true, None);
    let status = MenuItem::new(status_label(&index), false, None);
    let refresh = MenuItem::new("刷新状态", true, None);
    let sync = MenuItem::new("同步已建立的 NTFS 索引", true, None);
    let quit = MenuItem::new("退出金福搜索", true, None);
    menu.append(&open_search)
        .map_err(|error| error.to_string())?;
    menu.append(&index_c).map_err(|error| error.to_string())?;
    menu.append(&status).map_err(|error| error.to_string())?;
    menu.append(&refresh).map_err(|error| error.to_string())?;
    menu.append(&sync).map_err(|error| error.to_string())?;
    menu.append(&quit).map_err(|error| error.to_string())?;

    let _tray = TrayIconBuilder::new()
        .with_tooltip("金福搜索 — 待命（USN 增量同步）")
        .with_menu(Box::new(menu))
        .with_icon(status_icon().map_err(|error| error.to_string())?)
        .build()
        .map_err(|error| error.to_string())?;

    // 直接双击主程序时，用户应立刻看到搜索入口；关闭窗口只会回到托盘待命。
    search_ui::open(index.clone())?;

    let proxy = event_loop.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |event| {
        let _ = menu_proxy.send_event(UserEvent::Menu(event));
    }));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| error.to_string())?;
    let (shutdown, shutdown_rx) = watch::channel(false);
    let service_proxy = proxy.clone();
    let service_shutdown = shutdown.clone();
    let service_index = index.clone();
    let service = runtime.spawn(async move {
        let result = serve_pipe_controlled(
            service_index,
            pipe_name,
            shutdown_rx,
            service_shutdown.clone(),
        )
        .await;
        if result.is_err() {
            let _ = service_shutdown.send(true);
            let _ = service_proxy.send_event(UserEvent::Shutdown);
        }
        result
    });
    let idle_sync = runtime.spawn(run_idle_sync(index.clone(), shutdown.subscribe()));
    let ui_stop = runtime.spawn(notify_ui_on_shutdown(shutdown.subscribe(), proxy.clone()));
    let refresh_id = refresh.id().clone();
    let sync_id = sync.id().clone();
    let quit_id = quit.id().clone();
    let open_search_id = open_search.id().clone();
    let index_c_id = index_c.id().clone();
    let tray_index = index.clone();
    let tray_shutdown = shutdown.clone();
    let runtime_handle = runtime.handle().clone();
    let index_finished_proxy = proxy.clone();

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Menu(event)) => {
                if event.id == open_search_id {
                    if let Err(error) = search_ui::open(tray_index.clone()) {
                        status.set_text(format!("无法打开搜索窗口：{error}"));
                    }
                } else if event.id == index_c_id {
                    if confirm_c_volume_index() {
                        index_c.set_enabled(false);
                        status.set_text("正在建立 C: 索引…");
                        let index = tray_index.clone();
                        let completed = index_finished_proxy.clone();
                        runtime_handle.spawn(async move {
                            let result =
                                tokio::task::spawn_blocking(move || index.scan_ntfs_volume('C'))
                                    .await
                                    .map_err(|error| format!("索引任务异常结束：{error}"))
                                    .and_then(|result| result.map_err(|error| error.to_string()));
                            let _ = completed.send_event(UserEvent::IndexFinished(result));
                        });
                    }
                } else if event.id == refresh_id {
                    status.set_text(status_label(&tray_index));
                } else if event.id == sync_id {
                    status.set_text("正在同步已建立的 NTFS 索引…");
                    let index = tray_index.clone();
                    runtime_handle.spawn(async move {
                        let _ =
                            tokio::task::spawn_blocking(move || index.sync_indexed_ntfs_volumes())
                                .await;
                    });
                } else if event.id == quit_id {
                    let _ = tray_shutdown.send(true);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Shutdown) => {
                *control_flow = ControlFlow::Exit;
            }
            Event::UserEvent(UserEvent::IndexFinished(result)) => {
                index_c.set_enabled(true);
                match result {
                    Ok(report) => status.set_text(format!(
                        "C: 索引完成：{} 文件，{} 目录",
                        report.indexed_files, report.indexed_directories
                    )),
                    Err(error) => status.set_text(format!("C: 索引失败：{error}")),
                }
            }
            _ => {}
        }
    });

    let _ = shutdown.send(true);
    runtime
        .block_on(async {
            service
                .await
                .map_err(|error| io::Error::other(format!("托盘服务任务失败：{error}")))?
        })
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(async {
            idle_sync
                .await
                .map_err(|error| io::Error::other(format!("托盘同步任务失败：{error}")))
        })
        .map_err(|error| error.to_string())?;
    runtime
        .block_on(async {
            ui_stop
                .await
                .map_err(|error| io::Error::other(format!("托盘 UI 通知任务失败：{error}")))
        })
        .map_err(|error| error.to_string())
}

fn confirm_c_volume_index() -> bool {
    use std::iter::once;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONWARNING, MB_YESNO, MessageBoxW,
    };

    let body: Vec<u16> = "这会读取 C: 的 NTFS 元数据并建立本地索引；首次执行可能需要较长时间。\n\n不会读取或保存文件正文。现在开始吗？"
        .encode_utf16()
        .chain(once(0))
        .collect();
    let title: Vec<u16> = "建立 C: 索引".encode_utf16().chain(once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_YESNO | MB_ICONWARNING,
        ) == IDYES
    }
}

async fn notify_ui_on_shutdown(
    mut shutdown: watch::Receiver<bool>,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
) {
    if shutdown.changed().await.is_ok() && *shutdown.borrow() {
        let _ = proxy.send_event(UserEvent::Shutdown);
    }
}

async fn run_idle_sync(index: SearchIndex, mut shutdown: watch::Receiver<bool>) {
    loop {
        tokio::select! {
            _ = tokio::time::sleep(USN_SYNC_INTERVAL) => {
                let index = index.clone();
                // 只有已经建立过 MFT 快照的卷才会进入此处；日志换代时会
                // 标记“需重建”，绝不在待命状态偷偷发起全盘扫描。
                let _ = tokio::task::spawn_blocking(move || index.sync_indexed_ntfs_volumes()).await;
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break;
                }
            }
        }
    }
}

fn status_label(index: &SearchIndex) -> String {
    match index.status() {
        Ok(status) if status.incomplete_roots > 0 => format!(
            "待命：{} 文件，{} 目录，{} 根目录；{} 根需重建",
            status.indexed_files, status.indexed_directories, status.roots, status.incomplete_roots
        ),
        Ok(status) => format!(
            "待命：{} 文件，{} 目录，{} 根目录",
            status.indexed_files, status.indexed_directories, status.roots
        ),
        Err(_) => "待命：暂时无法读取索引状态".to_owned(),
    }
}

fn status_icon() -> Result<Icon, tray_icon::BadIcon> {
    const SIZE: u32 = 16;
    let mut rgba = vec![0_u8; (SIZE * SIZE * 4) as usize];
    for y in 0..SIZE {
        for x in 0..SIZE {
            let index = ((y * SIZE + x) * 4) as usize;
            let edge = x == 0 || y == 0 || x == SIZE - 1 || y == SIZE - 1;
            rgba[index] = if edge { 17 } else { 12 };
            rgba[index + 1] = if edge { 110 } else { 155 };
            rgba[index + 2] = if edge { 158 } else { 214 };
            rgba[index + 3] = 255;
        }
    }
    Icon::from_rgba(rgba, SIZE, SIZE)
}
