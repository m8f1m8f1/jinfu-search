//! Windows 原生搜索窗口：不引入 WebView，只读取既有 SQLite 元数据索引。

use std::{
    ffi::{OsString, c_void},
    path::{Path, PathBuf},
    process::Command,
    sync::{Mutex, MutexGuard},
    thread,
};

use crate::{SearchIndex, SearchRequest};

const SEARCH_LIMIT: usize = 100;

#[derive(Debug, Clone, PartialEq, Eq)]
struct SearchPanelView {
    hint: String,
    rows: Vec<String>,
    paths: Vec<PathBuf>,
}

/// 可测试的窗口查询状态；原生控件只负责输入与展示。
struct SearchPanelModel {
    index: SearchIndex,
}

impl SearchPanelModel {
    fn new(index: SearchIndex) -> Self {
        Self { index }
    }

    fn search(&self, input: &str) -> SearchPanelView {
        let query = input.trim();
        if query.is_empty() {
            return SearchPanelView {
                hint: self.idle_hint(),
                rows: Vec::new(),
                paths: Vec::new(),
            };
        }

        let mut request = SearchRequest::new(query);
        request.limit = SEARCH_LIMIT;
        match self.index.search(&request) {
            Ok(items) if items.is_empty() => SearchPanelView {
                hint: self.empty_result_hint(query),
                rows: Vec::new(),
                paths: Vec::new(),
            },
            Ok(items) => {
                let paths = items.iter().map(|item| PathBuf::from(&item.path)).collect();
                let rows: Vec<String> = items
                    .iter()
                    .map(|item| format!("{}  —  {}", item.name, item.path))
                    .collect();
                SearchPanelView {
                    hint: format!("找到 {} 项；双击结果可在资源管理器中定位。", rows.len()),
                    rows,
                    paths,
                }
            }
            Err(error) => SearchPanelView {
                hint: format!("搜索失败：{error}"),
                rows: Vec::new(),
                paths: Vec::new(),
            },
        }
    }

    fn idle_hint(&self) -> String {
        match self.index.status() {
            Ok(status) if status.roots == 0 => {
                "还没有索引。点击“建立/更新本机索引”，完成后即可搜索。".to_owned()
            }
            Ok(status) => format!(
                "已索引 {} 个文件、{} 个目录；输入关键词后点击“搜索”。",
                status.indexed_files, status.indexed_directories
            ),
            Err(error) => format!("暂时无法读取索引状态：{error}"),
        }
    }

    fn empty_result_hint(&self, query: &str) -> String {
        match self.index.status() {
            Ok(status) if status.roots == 0 => {
                "还没有索引。点击“建立/更新本机索引”，完成后即可搜索。".to_owned()
            }
            Ok(_) => format!("没有找到“{query}”；可换个名称关键词或扩展名。"),
            Err(error) => format!("没有找到“{query}”；索引状态读取失败：{error}"),
        }
    }
}

const FOCUS_EXISTING_SEARCH_WINDOW: u32 = 0x8000 + 41;

#[derive(Default)]
struct SearchWindowLifecycle {
    hwnd: isize,
    opening: bool,
    generation: u64,
}

static SEARCH_WINDOW: Mutex<SearchWindowLifecycle> = Mutex::new(SearchWindowLifecycle {
    hwnd: 0,
    opening: false,
    generation: 0,
});

fn search_window_lifecycle() -> MutexGuard<'static, SearchWindowLifecycle> {
    SEARCH_WINDOW
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn mark_search_window_created(generation: u64, hwnd: isize) {
    let mut lifecycle = search_window_lifecycle();
    if lifecycle.generation == generation {
        lifecycle.hwnd = hwnd;
        lifecycle.opening = false;
    }
}

fn mark_search_window_destroying(hwnd: isize) {
    let mut lifecycle = search_window_lifecycle();
    if lifecycle.hwnd == hwnd {
        lifecycle.hwnd = 0;
        lifecycle.opening = false;
    }
}

fn clear_window_launch_if_current(generation: u64) {
    let mut lifecycle = search_window_lifecycle();
    if lifecycle.generation == generation && lifecycle.hwnd == 0 {
        lifecycle.opening = false;
    }
}

fn forget_search_window_if_matches(hwnd: isize) {
    let mut lifecycle = search_window_lifecycle();
    if lifecycle.hwnd == hwnd {
        lifecycle.hwnd = 0;
        lifecycle.opening = false;
    }
}

/// 打开唯一的搜索窗口；重复点击托盘菜单会将已有窗口带到前台。
pub fn open(index: SearchIndex) -> Result<(), String> {
    let (existing, generation) = {
        let mut lifecycle = search_window_lifecycle();
        if lifecycle.hwnd != 0 {
            (Some(lifecycle.hwnd), None)
        } else if lifecycle.opening {
            (None, None)
        } else {
            lifecycle.opening = true;
            lifecycle.generation = lifecycle.generation.wrapping_add(1);
            (None, Some(lifecycle.generation))
        }
    };
    if let Some(hwnd) = existing {
        request_focus_existing_window(hwnd);
        return Ok(());
    }
    let Some(generation) = generation else {
        return Ok(());
    };

    thread::Builder::new()
        .name("jinfu-search-window".to_owned())
        .spawn(move || {
            if let Err(error) = run_native_window(index, generation) {
                show_window_error(&error);
            }
            clear_window_launch_if_current(generation);
        })
        .map(|_| ())
        .map_err(|error| {
            clear_window_launch_if_current(generation);
            format!("无法启动搜索窗口线程：{error}")
        })
}

#[cfg(windows)]
fn request_focus_existing_window(raw_handle: isize) {
    use windows_sys::Win32::{Foundation::HWND, UI::WindowsAndMessaging::PostMessageW};

    let hwnd = raw_handle as HWND;
    if unsafe { PostMessageW(hwnd, FOCUS_EXISTING_SEARCH_WINDOW, 0, 0) } == 0 {
        forget_search_window_if_matches(raw_handle);
    }
}

#[cfg(not(windows))]
fn request_focus_existing_window(_: isize) {}

#[cfg(windows)]
fn show_window_error(error: &str) {
    use std::iter::once;
    use windows_sys::Win32::UI::WindowsAndMessaging::{MB_ICONERROR, MB_OK, MessageBoxW};

    let body: Vec<u16> = format!("无法打开金福搜索窗口：{error}")
        .encode_utf16()
        .chain(once(0))
        .collect();
    let title: Vec<u16> = "金福搜索".encode_utf16().chain(once(0)).collect();
    unsafe {
        MessageBoxW(
            std::ptr::null_mut(),
            body.as_ptr(),
            title.as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

#[cfg(not(windows))]
fn show_window_error(_: &str) {}

#[cfg(windows)]
mod native {
    use std::{iter::once, mem::zeroed, ptr::null_mut};

    use windows_sys::Win32::{
        Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM},
        Graphics::Gdi::{DEFAULT_GUI_FONT, GetStockObject, HBRUSH, WHITE_BRUSH},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{EnableWindow, GetFocus, SetFocus, VK_RETURN},
            WindowsAndMessaging::{
                BN_CLICKED, BS_DEFPUSHBUTTON, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
                CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL,
                GWLP_USERDATA, GetClientRect, GetMessageW, GetWindowLongPtrW, GetWindowTextLengthW,
                GetWindowTextW, IsDialogMessageW, LB_ADDSTRING, LB_ERR, LB_GETCURSEL,
                LB_RESETCONTENT, LB_SETHORIZONTALEXTENT, LBN_DBLCLK, LBS_NOINTEGRALHEIGHT,
                LBS_NOTIFY, MSG, MoveWindow, PostMessageW, PostQuitMessage, RegisterClassW,
                SendMessageW, SetWindowLongPtrW, SetWindowTextW, TranslateMessage, WM_CLOSE,
                WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_NCCREATE, WM_NCDESTROY,
                WM_SETFONT, WM_SIZE, WNDCLASSW, WS_BORDER, WS_CHILD, WS_EX_CLIENTEDGE,
                WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE, WS_VSCROLL,
            },
        },
    };

    use super::{
        FOCUS_EXISTING_SEARCH_WINDOW, SearchIndex, SearchPanelModel, c_void,
        mark_search_window_created, mark_search_window_destroying, reveal_path,
    };

    const CLASS_NAME: &str = "JinfuSearchNativeWindow";
    const WINDOW_TITLE: &str = "金福搜索";
    const EDIT_ID: i32 = 1;
    const SEARCH_BUTTON_ID: i32 = 2;
    const RESULTS_ID: i32 = 3;
    const STATUS_ID: i32 = 4;
    const INDEX_BUTTON_ID: i32 = 5;
    const INDEX_FINISHED: u32 = 0x8000 + 42;

    pub fn run(index: SearchIndex, generation: u64) -> Result<(), String> {
        let class_name = wide(CLASS_NAME);
        let title = wide(WINDOW_TITLE);
        let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
        let class = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hbrBackground: unsafe { GetStockObject(WHITE_BRUSH) } as HBRUSH,
            lpszClassName: class_name.as_ptr(),
            ..unsafe { zeroed() }
        };
        unsafe {
            // 类名已注册时仍可安全创建同类窗口；注册返回值无需作为失败条件。
            let _ = RegisterClassW(&class);
        }

        let state = Box::new(NativeState::new(index));
        let raw_state = Box::into_raw(state);
        let hwnd = unsafe {
            CreateWindowExW(
                0,
                class_name.as_ptr(),
                title.as_ptr(),
                WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                820,
                560,
                null_mut(),
                null_mut(),
                instance,
                raw_state.cast::<c_void>(),
            )
        };
        if hwnd.is_null() {
            unsafe {
                drop(Box::from_raw(raw_state));
            }
            return Err("CreateWindowExW 返回空句柄".to_owned());
        }
        mark_search_window_created(generation, hwnd as isize);

        let mut message = unsafe { zeroed::<MSG>() };
        let result = loop {
            let result = unsafe { GetMessageW(&mut message, null_mut(), 0, 0) };
            if result == -1 {
                unsafe {
                    DestroyWindow(hwnd);
                }
                break Err("GetMessageW 读取失败".to_owned());
            }
            if result == 0 {
                break Ok(());
            }
            unsafe {
                if message.message == WM_KEYDOWN && message.wParam == VK_RETURN as usize {
                    let state = &*raw_state;
                    if GetFocus() == state.results {
                        state.reveal_selected_path();
                    } else {
                        let command = SEARCH_BUTTON_ID as usize | ((BN_CLICKED as usize) << 16);
                        SendMessageW(hwnd, WM_COMMAND, command, 0);
                    }
                } else if IsDialogMessageW(hwnd, &message) == 0 {
                    TranslateMessage(&message);
                    DispatchMessageW(&message);
                }
            }
        };
        // 状态的所有权始终由此函数保留。窗口创建失败时，Windows 可能已发送
        // WM_NCDESTROY；此处统一释放可避免 WndProc 与调用者双重释放。
        unsafe {
            drop(Box::from_raw(raw_state));
        }
        result
    }

    struct NativeState {
        model: SearchPanelModel,
        input: HWND,
        button: HWND,
        index_button: HWND,
        results: HWND,
        status: HWND,
        result_paths: Vec<std::path::PathBuf>,
    }

    impl NativeState {
        fn new(index: SearchIndex) -> Self {
            Self {
                model: SearchPanelModel::new(index),
                input: null_mut(),
                button: null_mut(),
                index_button: null_mut(),
                results: null_mut(),
                status: null_mut(),
                result_paths: Vec::new(),
            }
        }

        unsafe fn create_controls(&mut self, parent: HWND) -> bool {
            let instance = unsafe { GetModuleHandleW(std::ptr::null()) };
            self.input = unsafe {
                create_control(
                    "EDIT",
                    "",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | ES_AUTOHSCROLL as u32,
                    WS_EX_CLIENTEDGE,
                    parent,
                    EDIT_ID,
                    instance,
                )
            };
            self.button = unsafe {
                create_control(
                    "BUTTON",
                    "搜索",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP | BS_DEFPUSHBUTTON as u32,
                    0,
                    parent,
                    SEARCH_BUTTON_ID,
                    instance,
                )
            };
            self.index_button = unsafe {
                create_control(
                    "BUTTON",
                    "建立/更新本机索引",
                    WS_CHILD | WS_VISIBLE | WS_TABSTOP,
                    0,
                    parent,
                    INDEX_BUTTON_ID,
                    instance,
                )
            };
            self.status = unsafe {
                create_control(
                    "STATIC",
                    &self.model.idle_hint(),
                    WS_CHILD | WS_VISIBLE,
                    0,
                    parent,
                    STATUS_ID,
                    instance,
                )
            };
            self.results = unsafe {
                create_control(
                    "LISTBOX",
                    "",
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_TABSTOP
                        | WS_VSCROLL
                        | WS_BORDER
                        | LBS_NOTIFY as u32
                        | LBS_NOINTEGRALHEIGHT as u32,
                    WS_EX_CLIENTEDGE,
                    parent,
                    RESULTS_ID,
                    instance,
                )
            };
            if self.input.is_null()
                || self.button.is_null()
                || self.index_button.is_null()
                || self.status.is_null()
                || self.results.is_null()
            {
                return false;
            }

            let font = unsafe { GetStockObject(DEFAULT_GUI_FONT) };
            for control in [
                self.input,
                self.button,
                self.index_button,
                self.status,
                self.results,
            ] {
                unsafe {
                    SendMessageW(control, WM_SETFONT, font as usize, 1);
                }
            }
            unsafe {
                SetFocus(self.input);
                self.layout(parent);
            }
            true
        }

        unsafe fn layout(&self, parent: HWND) {
            let mut client = RECT::default();
            unsafe {
                GetClientRect(parent, &mut client);
            }
            let width = (client.right - client.left).max(300);
            let height = (client.bottom - client.top).max(180);
            let padding = 14;
            let button_width = 90;
            let index_width = 170;
            let input_width = (width - padding * 4 - button_width - index_width).max(120);
            unsafe {
                MoveWindow(self.input, padding, 30, input_width, 30, 1);
                MoveWindow(
                    self.button,
                    padding * 2 + input_width,
                    30,
                    button_width,
                    30,
                    1,
                );
                MoveWindow(
                    self.index_button,
                    padding * 3 + input_width + button_width,
                    30,
                    index_width,
                    30,
                    1,
                );
                MoveWindow(self.status, padding, 72, width - padding * 2, 38, 1);
                MoveWindow(
                    self.results,
                    padding,
                    116,
                    width - padding * 2,
                    height - 130,
                    1,
                );
            }
        }

        unsafe fn refresh_results(&mut self) {
            let query = unsafe { control_text(self.input) };
            let view = self.model.search(&query);
            unsafe {
                set_control_text(self.status, &view.hint);
                SendMessageW(self.results, LB_RESETCONTENT, 0, 0);
            }
            let mut longest = 0usize;
            for row in &view.rows {
                longest = longest.max(row.encode_utf16().count());
                let text = wide(row);
                unsafe {
                    SendMessageW(self.results, LB_ADDSTRING, 0, text.as_ptr() as isize);
                }
            }
            // 为长路径保留横向滚动空间；字体差异只影响余量，不影响内容完整性。
            unsafe {
                SendMessageW(
                    self.results,
                    LB_SETHORIZONTALEXTENT,
                    longest.saturating_mul(8),
                    0,
                );
            }
            self.result_paths = view.paths;
        }

        unsafe fn reveal_selected_path(&self) {
            let selected = unsafe { SendMessageW(self.results, LB_GETCURSEL, 0, 0) };
            if selected != LB_ERR as isize
                && let Some(path) = self.result_paths.get(selected as usize)
            {
                reveal_path(path);
            }
        }

        unsafe fn start_machine_index(&self, parent: HWND) {
            unsafe {
                EnableWindow(self.index_button, 0);
                set_control_text(
                    self.status,
                    "正在索引本机固定磁盘，请保持程序运行；可继续使用电脑。",
                );
            }
            let index = self.model.index.clone();
            let parent = parent as isize;
            std::thread::spawn(move || {
                let volumes = crate::local_fixed_volumes();
                let mut reports = Vec::new();
                let mut failures = Vec::new();
                for volume in volumes {
                    let result = match index.scan_ntfs_volume(volume) {
                        Ok(report) => Ok(report),
                        Err(mft_error) => index
                            .scan_root(format!("{volume}:\\"))
                            .map_err(|scan_error| format!(
                                "{volume}: MFT 索引失败（{mft_error}）；目录扫描也失败（{scan_error}）"
                            )),
                    };
                    match result {
                        Ok(report) => reports.push(report),
                        Err(error) => failures.push(error),
                    }
                }
                let files: u64 = reports.iter().map(|report| report.indexed_files).sum();
                let directories: u64 = reports
                    .iter()
                    .map(|report| report.indexed_directories)
                    .sum();
                let message = if reports.is_empty() {
                    format!("索引失败：{}", failures.join("；"))
                } else if failures.is_empty() {
                    format!(
                        "索引完成：{files} 个文件、{directories} 个目录；现在可以搜索名称、扩展名或多个关键词。"
                    )
                } else {
                    format!(
                        "部分索引完成：{files} 个文件、{directories} 个目录；{}",
                        failures.join("；")
                    )
                };
                let message = Box::into_raw(Box::new(message));
                if unsafe { PostMessageW(parent as HWND, INDEX_FINISHED, 0, message as isize) } == 0
                {
                    unsafe {
                        drop(Box::from_raw(message));
                    }
                }
            });
        }
    }

    unsafe extern "system" fn window_proc(
        hwnd: HWND,
        message: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match message {
            WM_NCCREATE => {
                let create = unsafe { &*(lparam as *const CREATESTRUCTW) };
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
                }
                1
            }
            WM_CREATE => {
                let Some(state) = (unsafe { state_mut(hwnd) }) else {
                    return -1;
                };
                if unsafe { state.create_controls(hwnd) } {
                    0
                } else {
                    -1
                }
            }
            WM_COMMAND => {
                let control_id = (wparam & 0xffff) as i32;
                let notification = ((wparam >> 16) & 0xffff) as u32;
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    if control_id == SEARCH_BUTTON_ID && notification == BN_CLICKED {
                        unsafe {
                            state.refresh_results();
                        }
                    } else if control_id == INDEX_BUTTON_ID && notification == BN_CLICKED {
                        unsafe {
                            state.start_machine_index(hwnd);
                        }
                    } else if control_id == RESULTS_ID && notification == LBN_DBLCLK {
                        unsafe {
                            state.reveal_selected_path();
                        }
                    }
                }
                0
            }
            WM_SIZE => {
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    unsafe {
                        state.layout(hwnd);
                    }
                }
                0
            }
            WM_CLOSE => {
                unsafe {
                    DestroyWindow(hwnd);
                }
                0
            }
            FOCUS_EXISTING_SEARCH_WINDOW => {
                use windows_sys::Win32::UI::WindowsAndMessaging::{
                    SW_RESTORE, SetForegroundWindow, ShowWindow,
                };

                unsafe {
                    ShowWindow(hwnd, SW_RESTORE);
                    SetForegroundWindow(hwnd);
                }
                0
            }
            INDEX_FINISHED => {
                let message = unsafe { Box::from_raw(lparam as *mut String) };
                if let Some(state) = unsafe { state_mut(hwnd) } {
                    unsafe {
                        EnableWindow(state.index_button, 1);
                        set_control_text(state.status, &message);
                    }
                }
                0
            }
            WM_DESTROY => {
                mark_search_window_destroying(hwnd as isize);
                unsafe {
                    PostQuitMessage(0);
                }
                0
            }
            WM_NCDESTROY => {
                unsafe {
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
            }
            _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
        }
    }

    unsafe fn state_mut(hwnd: HWND) -> Option<&'static mut NativeState> {
        let pointer = unsafe { GetWindowLongPtrW(hwnd, GWLP_USERDATA) } as *mut NativeState;
        unsafe { pointer.as_mut() }
    }

    unsafe fn create_control(
        class_name: &str,
        text: &str,
        style: u32,
        extended_style: u32,
        parent: HWND,
        identifier: i32,
        instance: *mut c_void,
    ) -> HWND {
        let class_name = wide(class_name);
        let text = wide(text);
        unsafe {
            CreateWindowExW(
                extended_style,
                class_name.as_ptr(),
                text.as_ptr(),
                style,
                0,
                0,
                0,
                0,
                parent,
                identifier as usize as *mut c_void,
                instance,
                null_mut(),
            )
        }
    }

    unsafe fn control_text(control: HWND) -> String {
        let length = unsafe { GetWindowTextLengthW(control) }.max(0) as usize;
        let mut buffer = vec![0_u16; length + 1];
        unsafe {
            GetWindowTextW(control, buffer.as_mut_ptr(), buffer.len() as i32);
        }
        String::from_utf16_lossy(&buffer[..length])
    }

    unsafe fn set_control_text(control: HWND, text: &str) {
        let text = wide(text);
        unsafe {
            SetWindowTextW(control, text.as_ptr());
        }
    }

    fn wide(value: &str) -> Vec<u16> {
        value.encode_utf16().chain(once(0)).collect()
    }
}

#[cfg(windows)]
fn run_native_window(index: SearchIndex, generation: u64) -> Result<(), String> {
    native::run(index, generation)
}

#[cfg(not(windows))]
fn run_native_window(_: SearchIndex, _: u64) -> Result<(), String> {
    Err("搜索窗口只支持 Windows".to_owned())
}

fn reveal_path(path: &Path) {
    let _ = Command::new("explorer.exe")
        .args(explorer_arguments(path))
        .spawn();
}

fn explorer_arguments(path: &Path) -> Vec<OsString> {
    if path.exists() {
        vec![OsString::from("/select,"), windows_native_path(path)]
    } else {
        vec![windows_native_path(path.parent().unwrap_or(path))]
    }
}

fn windows_native_path(path: &Path) -> OsString {
    OsString::from(path.as_os_str().to_string_lossy().replace('/', "\\"))
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsString, fs, path::PathBuf};

    use tempfile::tempdir;

    use super::SearchPanelModel;
    use super::explorer_arguments;
    use crate::SearchIndex;

    #[test]
    fn empty_index_explains_that_the_user_must_create_an_index() {
        let sandbox = tempdir().unwrap();
        let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();

        let view = SearchPanelModel::new(index).search("合同");

        assert!(view.rows.is_empty());
        assert!(view.hint.contains("建立/更新本机索引"));
    }

    #[test]
    fn search_view_shows_a_full_path_for_each_indexed_match() {
        let sandbox = tempdir().unwrap();
        let root = sandbox.path().join("资料");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("季度合同.pdf"), "metadata only").unwrap();
        let index = SearchIndex::open(sandbox.path().join("index.db")).unwrap();
        index.scan_root(&root).unwrap();

        let view = SearchPanelModel::new(index).search("合同");

        assert_eq!(view.rows.len(), 1);
        assert!(view.rows[0].contains("季度合同.pdf"));
        assert!(view.rows[0].contains("资料"));
    }

    #[test]
    fn explorer_select_uses_a_separate_switch_and_native_windows_path() {
        let sandbox = tempdir().unwrap();
        let file = sandbox.path().join("中文 folder").join("验收 file.txt");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, "metadata only").unwrap();
        let storage_path = PathBuf::from(file.to_string_lossy().replace('\\', "/"));

        let arguments = explorer_arguments(&storage_path);

        assert_eq!(arguments.len(), 2);
        assert_eq!(arguments[0], OsString::from("/select,"));
        assert_eq!(arguments[1], file.as_os_str());
    }
}
