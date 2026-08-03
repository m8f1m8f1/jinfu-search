//! 没有 WebView 的最小系统托盘宿主。

use std::io;

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

use crate::{SearchIndex, maintenance, search_ui, serve_pipe_controlled};

enum UserEvent {
    Menu(MenuEvent),
    Shutdown,
}

/// 在主 UI 线程上运行托盘；Named Pipe 服务运行在 Tokio 工作线程。
pub fn run(index: SearchIndex, pipe_name: String, show_initial_window: bool) -> Result<(), String> {
    let mut event_loop = EventLoopBuilder::<UserEvent>::with_user_event().build();
    let menu = Menu::new();
    let open_search = MenuItem::new("打开搜索窗口", true, None);
    let status = MenuItem::new(status_label(&index), false, None);
    let refresh = MenuItem::new("刷新状态", true, None);
    let quit = MenuItem::new("退出金福搜索", true, None);
    menu.append(&open_search)
        .map_err(|error| error.to_string())?;
    menu.append(&status).map_err(|error| error.to_string())?;
    menu.append(&refresh).map_err(|error| error.to_string())?;
    menu.append(&quit).map_err(|error| error.to_string())?;

    let _tray = TrayIconBuilder::new()
        .with_tooltip("金福搜索 — 后台自动维护本机索引")
        .with_menu(Box::new(menu))
        .with_icon(status_icon().map_err(|error| error.to_string())?)
        .build()
        .map_err(|error| error.to_string())?;

    // 直接双击主程序时立刻显示搜索入口；MCP 自动唤起宿主时仅进入托盘。
    if show_initial_window {
        search_ui::open(index.clone())?;
    }

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
    maintenance::start(index.clone(), shutdown.subscribe());
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
    let ui_stop = runtime.spawn(notify_ui_on_shutdown(shutdown.subscribe(), proxy.clone()));
    let refresh_id = refresh.id().clone();
    let quit_id = quit.id().clone();
    let open_search_id = open_search.id().clone();
    let tray_index = index.clone();
    let tray_shutdown = shutdown.clone();

    event_loop.run_return(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(UserEvent::Menu(event)) => {
                if event.id == open_search_id {
                    if let Err(error) = search_ui::open(tray_index.clone()) {
                        status.set_text(format!("无法打开搜索窗口：{error}"));
                    }
                } else if event.id == refresh_id {
                    status.set_text(status_label(&tray_index));
                } else if event.id == quit_id {
                    let _ = tray_shutdown.send(true);
                    *control_flow = ControlFlow::Exit;
                }
            }
            Event::UserEvent(UserEvent::Shutdown) => {
                *control_flow = ControlFlow::Exit;
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
            ui_stop
                .await
                .map_err(|error| io::Error::other(format!("托盘 UI 通知任务失败：{error}")))
        })
        .map_err(|error| error.to_string())
}

async fn notify_ui_on_shutdown(
    mut shutdown: watch::Receiver<bool>,
    proxy: tao::event_loop::EventLoopProxy<UserEvent>,
) {
    if shutdown.changed().await.is_ok() && *shutdown.borrow() {
        let _ = proxy.send_event(UserEvent::Shutdown);
    }
}

fn status_label(index: &SearchIndex) -> String {
    match index.status() {
        Ok(status) if status.incomplete_roots > 0 => format!(
            "自动维护：{} 文件，{} 目录，{} 根目录；{} 根正在校准",
            status.indexed_files, status.indexed_directories, status.roots, status.incomplete_roots
        ),
        Ok(status) => format!(
            "自动维护：{} 文件，{} 目录，{} 根目录",
            status.indexed_files, status.indexed_directories, status.roots
        ),
        Err(_) => "自动维护：暂时无法读取索引状态".to_owned(),
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
