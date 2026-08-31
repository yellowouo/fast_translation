use anyhow::Result;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Sender};
use std::sync::{Mutex, OnceLock};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateCompatibleBitmap, CreateCompatibleDC, CreatePen, CreateSolidBrush, DeleteDC,
    DeleteObject, Ellipse, GetDC, GetStockObject, Rectangle, ReleaseDC, SelectObject,
    BLACK_BRUSH, BLACK_PEN, HGDIOBJ, PS_SOLID, WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    RegisterHotKey, UnregisterHotKey, HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_NOREPEAT,
    MOD_SHIFT, MOD_WIN, VIRTUAL_KEY,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW,
    DestroyMenu, DestroyWindow, DispatchMessageW, GetCursorPos, GetMessageW, LoadIconW,
    LoadImageW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, HICON, ICONINFO, IDI_APPLICATION, IMAGE_ICON,
    LR_DEFAULTCOLOR, MF_DISABLED, MF_GRAYED, MF_SEPARATOR, MF_STRING, TPM_NONOTIFY,
    TPM_RETURNCMD, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_COMMAND, WM_CONTEXTMENU,
    WM_DESTROY, WM_HOTKEY, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_OVERLAPPED,
};

use crate::config::TranslateMode;
use crate::AppEvent;

const WM_TRAY_CALLBACK: u32 = WM_APP + 100;
const WM_UPDATE_HOTKEY_MSG: u32 = WM_APP + 101;
const TRAY_ICON_ID: u32 = 1001;
const HOTKEY_TRANSLATE_ID: i32 = 100;
const HOTKEY_TOGGLE_ID: i32 = 101;

const ID_TRAY_STATUS: usize = 2001;
const ID_TRAY_TOGGLE_HOTKEYS: usize = 2002;
const ID_TRAY_MODE_GENERAL: usize = 2003;
const ID_TRAY_MODE_LLM: usize = 2004;
const ID_TRAY_SETTINGS: usize = 2005;
const ID_TRAY_EXIT: usize = 2006;

pub static HOTKEYS_ENABLED: AtomicBool = AtomicBool::new(true);
pub static SETTINGS_OPEN: AtomicBool = AtomicBool::new(false);
static CURRENT_HOTKEY_STR: LazyLock<Mutex<String>> = LazyLock::new(|| Mutex::new("Alt+Q".into()));
static CURRENT_TOGGLE_HOTKEY_STR: LazyLock<Mutex<String>> =
    LazyLock::new(|| Mutex::new("Alt+Shift+Q".into()));
static CURRENT_MODE: LazyLock<Mutex<TranslateMode>> =
    LazyLock::new(|| Mutex::new(TranslateMode::General));
static APP_EVENT_SENDER: OnceLock<Sender<AppEvent>> = OnceLock::new();
static TRAY_HWND: OnceLock<isize> = OnceLock::new();

use std::sync::LazyLock;

pub struct TrayIcon {
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

unsafe impl Send for TrayIcon {}
unsafe impl Sync for TrayIcon {}

impl TrayIcon {
    pub fn start(
        tx: Sender<AppEvent>,
        initial_hotkey: String,
        initial_toggle_hotkey: String,
        initial_enabled: bool,
        initial_mode: TranslateMode,
    ) -> Result<Self> {
        let _ = APP_EVENT_SENDER.set(tx);
        HOTKEYS_ENABLED.store(initial_enabled, Ordering::SeqCst);
        *CURRENT_HOTKEY_STR.lock().unwrap() = initial_hotkey.clone();
        *CURRENT_TOGGLE_HOTKEY_STR.lock().unwrap() = initial_toggle_hotkey.clone();
        *CURRENT_MODE.lock().unwrap() = initial_mode;

        let (init_tx, init_rx) = mpsc::channel::<isize>();

        let thread_handle = std::thread::Builder::new()
            .name("tray_message_pump".into())
            .spawn(move || {
                unsafe {
                    let instance = match GetModuleHandleW(None) {
                        Ok(inst) => inst,
                        Err(e) => {
                            eprintln!("[Tray] GetModuleHandleW failed: {e}");
                            return;
                        }
                    };

                    let class_name = w!("FastTranslationTrayWindowClass");

                    let wnd_class = WNDCLASSW {
                        lpfnWndProc: Some(tray_wnd_proc),
                        hInstance: instance.into(),
                        lpszClassName: class_name,
                        ..Default::default()
                    };

                    RegisterClassW(&wnd_class);

                    let hwnd = match CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        class_name,
                        w!("FastTranslationTrayWindow"),
                        WS_OVERLAPPED,
                        0,
                        0,
                        0,
                        0,
                        HWND::default(),
                        None,
                        instance,
                        None,
                    ) {
                        Ok(h) => h,
                        Err(e) => {
                            eprintln!("[Tray] CreateWindowExW failed: {e}");
                            return;
                        }
                    };

                    let _ = TRAY_HWND.set(hwnd.0 as isize);

                    let icon = get_tray_icon(instance.into());

                    let mut nid = NOTIFYICONDATAW {
                        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
                        hWnd: hwnd,
                        uID: TRAY_ICON_ID,
                        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP,
                        uCallbackMessage: WM_TRAY_CALLBACK,
                        hIcon: icon,
                        ..Default::default()
                    };

                    let tip_text = if initial_enabled {
                        "Fast Translation 划词翻译 (运行中)\0"
                    } else {
                        "Fast Translation 划词翻译 (禁用中)\0"
                    };
                    let tip_encoded: Vec<u16> = tip_text.encode_utf16().collect();
                    let copy_len = tip_encoded.len().min(nid.szTip.len());
                    nid.szTip[..copy_len].copy_from_slice(&tip_encoded[..copy_len]);

                    if !Shell_NotifyIconW(NIM_ADD, &nid).as_bool() {
                        eprintln!("[Tray] Shell_NotifyIconW(NIM_ADD) failed");
                    } else {
                        println!("[Tray] System tray icon initialized.");
                    }

                    // 注册全局启停快捷键（无论程序是否禁用，始终监听此快捷键以便重新唤醒）
                    register_global_hotkey(hwnd, HOTKEY_TOGGLE_ID, &initial_toggle_hotkey);

                    // 注册初始全局翻译快捷键
                    if initial_enabled {
                        register_global_hotkey(hwnd, HOTKEY_TRANSLATE_ID, &initial_hotkey);
                    }

                    let _ = init_tx.send(hwnd.0 as isize);

                    let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
                    while GetMessageW(&mut msg, HWND::default(), 0, 0).into() {
                        let _ = TranslateMessage(&msg);
                        DispatchMessageW(&msg);
                    }

                    // 退出消息循环时清理托盘图标与快捷键
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TRANSLATE_ID);
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE_ID);
                    let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
                    println!("[Tray] Tray message pump exited.");
                }
            })?;

        let _ = init_rx.recv()?;

        Ok(Self {
            thread_handle: Some(thread_handle),
        })
    }

    /// 更新当前翻译引擎模式
    pub fn update_mode(mode: TranslateMode) {
        *CURRENT_MODE.lock().unwrap() = mode;
        println!("[Tray] Updated tray translate mode to: {}", mode);
    }

    /// 更新快捷键设置并重新注册（翻译唤醒与启停快捷键）
    pub fn update_hotkeys(new_hotkey: &str, new_toggle_hotkey: &str, enabled: bool) {
        *CURRENT_HOTKEY_STR.lock().unwrap() = new_hotkey.to_string();
        *CURRENT_TOGGLE_HOTKEY_STR.lock().unwrap() = new_toggle_hotkey.to_string();
        HOTKEYS_ENABLED.store(enabled, Ordering::SeqCst);
        if let Some(&raw_hwnd) = TRAY_HWND.get() {
            unsafe {
                let hwnd = HWND(raw_hwnd as *mut _);
                let _ = PostMessageW(hwnd, WM_UPDATE_HOTKEY_MSG, WPARAM(0), LPARAM(0));
            }
        }
    }

    /// 打开/关闭设置窗口时设置状态并临时注销快捷键，防止按键冲突
    pub fn set_settings_open(open: bool) {
        SETTINGS_OPEN.store(open, Ordering::SeqCst);
        if let Some(&raw_hwnd) = TRAY_HWND.get() {
            unsafe {
                let hwnd = HWND(raw_hwnd as *mut _);
                if open {
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TRANSLATE_ID);
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE_ID);
                    println!("[Tray] Hotkeys temporarily unregistered while settings window is open.");
                } else {
                    let hotkey = CURRENT_HOTKEY_STR.lock().unwrap().clone();
                    let toggle_hotkey = CURRENT_TOGGLE_HOTKEY_STR.lock().unwrap().clone();
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TRANSLATE_ID);
                    let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE_ID);
                    register_global_hotkey(hwnd, HOTKEY_TOGGLE_ID, &toggle_hotkey);
                    if HOTKEYS_ENABLED.load(Ordering::Relaxed) {
                        register_global_hotkey(hwnd, HOTKEY_TRANSLATE_ID, &hotkey);
                    }
                    println!("[Tray] Hotkeys restored after settings window closed.");
                }
            }
        }
    }

    /// 获取当前设置窗口是否处于打开状态
    pub fn is_settings_open() -> bool {
        SETTINGS_OPEN.load(Ordering::Relaxed)
    }

    /// 获取当前快捷键是否启用
    pub fn is_enabled() -> bool {
        HOTKEYS_ENABLED.load(Ordering::Relaxed)
    }
}

impl Drop for TrayIcon {
    fn drop(&mut self) {
        if let Some(&raw_hwnd) = TRAY_HWND.get() {
            unsafe {
                let hwnd = HWND(raw_hwnd as *mut _);
                let _ = PostMessageW(hwnd, WM_DESTROY, WPARAM(0), LPARAM(0));
            }
        }
        if let Some(handle) = self.thread_handle.take() {
            let _ = handle.join();
        }
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    match msg {
        WM_TRAY_CALLBACK => {
            let event = l_param.0 as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            }
            LRESULT(0)
        }
        WM_HOTKEY => {
            let hotkey_id = w_param.0 as i32;
            if hotkey_id == HOTKEY_TOGGLE_ID {
                toggle_program_state(hwnd);
            } else if hotkey_id == HOTKEY_TRANSLATE_ID {
                if SETTINGS_OPEN.load(Ordering::Relaxed) || !HOTKEYS_ENABLED.load(Ordering::Relaxed) {
                    return LRESULT(0);
                }
                println!("[Hotkey] Global hotkey triggered!");
                if let Some(tx) = APP_EVENT_SENDER.get() {
                    let _ = tx.send(AppEvent::HotKeyTriggered);
                }
            }
            LRESULT(0)
        }
        WM_UPDATE_HOTKEY_MSG => {
            if !SETTINGS_OPEN.load(Ordering::Relaxed) {
                let enabled = HOTKEYS_ENABLED.load(Ordering::SeqCst);
                let hotkey = CURRENT_HOTKEY_STR.lock().unwrap().clone();
                let toggle_hotkey = CURRENT_TOGGLE_HOTKEY_STR.lock().unwrap().clone();
                let _ = UnregisterHotKey(hwnd, HOTKEY_TRANSLATE_ID);
                let _ = UnregisterHotKey(hwnd, HOTKEY_TOGGLE_ID);
                register_global_hotkey(hwnd, HOTKEY_TOGGLE_ID, &toggle_hotkey);
                if enabled {
                    register_global_hotkey(hwnd, HOTKEY_TRANSLATE_ID, &hotkey);
                }
                update_tray_tooltip(hwnd, enabled);
            }
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = (w_param.0 & 0xFFFF) as usize;
            handle_menu_command(hwnd, id);
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, w_param, l_param),
    }
}

/// 切换程序全局启用/禁用状态，并同步刷新热键注册、托盘 Tooltip 与事件通知
unsafe fn toggle_program_state(hwnd: HWND) {
    let current = HOTKEYS_ENABLED.load(Ordering::SeqCst);
    let new_state = !current;
    HOTKEYS_ENABLED.store(new_state, Ordering::SeqCst);

    let hotkey = CURRENT_HOTKEY_STR.lock().unwrap().clone();
    let _ = UnregisterHotKey(hwnd, HOTKEY_TRANSLATE_ID);
    if new_state && !SETTINGS_OPEN.load(Ordering::Relaxed) {
        register_global_hotkey(hwnd, HOTKEY_TRANSLATE_ID, &hotkey);
        println!("[Tray] 程序已启用 (运行中)");
    } else {
        println!("[Tray] 程序已禁用 (禁用中)");
    }

    update_tray_tooltip(hwnd, new_state);

    if let Some(tx) = APP_EVENT_SENDER.get() {
        let _ = tx.send(AppEvent::ToggleHotkeys(new_state));
    }
}

/// 更新托盘图标的 Tooltip 文本提示
unsafe fn update_tray_tooltip(hwnd: HWND, enabled: bool) {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_TIP,
        ..Default::default()
    };

    let tip_text = if enabled {
        "Fast Translation 划词翻译 (运行中)\0"
    } else {
        "Fast Translation 划词翻译 (禁用中)\0"
    };
    let tip_encoded: Vec<u16> = tip_text.encode_utf16().collect();
    let copy_len = tip_encoded.len().min(nid.szTip.len());
    nid.szTip[..copy_len].copy_from_slice(&tip_encoded[..copy_len]);

    let _ = Shell_NotifyIconW(windows::Win32::UI::Shell::NIM_MODIFY, &nid);
}

unsafe fn handle_menu_command(hwnd: HWND, id: usize) {
    match id {
        ID_TRAY_TOGGLE_HOTKEYS => {
            toggle_program_state(hwnd);
        }
        ID_TRAY_MODE_GENERAL => {
            *CURRENT_MODE.lock().unwrap() = TranslateMode::General;
            println!("[Tray] 菜单切换: 通用文本翻译");
            if let Some(tx) = APP_EVENT_SENDER.get() {
                let _ = tx.send(AppEvent::SetTranslateMode(TranslateMode::General));
            }
        }
        ID_TRAY_MODE_LLM => {
            *CURRENT_MODE.lock().unwrap() = TranslateMode::Llm;
            println!("[Tray] 菜单切换: 大模型文本翻译");
            if let Some(tx) = APP_EVENT_SENDER.get() {
                let _ = tx.send(AppEvent::SetTranslateMode(TranslateMode::Llm));
            }
        }
        ID_TRAY_SETTINGS => {
            println!("[Tray] Opening hotkey settings...");
            TrayIcon::set_settings_open(true);
            if let Some(tx) = APP_EVENT_SENDER.get() {
                let _ = tx.send(AppEvent::OpenSettings);
            }
        }
        ID_TRAY_EXIT => {
            println!("[Tray] Exit clicked from tray menu.");
            let _ = slint::invoke_from_event_loop(|| {
                let _ = slint::quit_event_loop();
            });
            let _ = DestroyWindow(hwnd);
        }
        _ => {}
    }
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let hmenu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };

    let is_enabled = HOTKEYS_ENABLED.load(Ordering::Relaxed);
    let current_mode = *CURRENT_MODE.lock().unwrap();

    // 动态生成菜单顶部标题（展示 (运行中) 或 (禁用中)）
    let title_str = if is_enabled {
        "Fast Translation 划词翻译 (运行中)\0"
    } else {
        "Fast Translation 划词翻译 (禁用中)\0"
    };
    let title_wide: Vec<u16> = title_str.encode_utf16().collect();

    // 动态生成启用/禁用程序文本与快捷键提示
    let toggle_key = CURRENT_TOGGLE_HOTKEY_STR.lock().unwrap().clone();
    let toggle_text_str = if is_enabled {
        format!("⏸ 禁用程序 ({})\0", toggle_key)
    } else {
        format!("▶ 启用程序 ({})\0", toggle_key)
    };
    let toggle_wide: Vec<u16> = toggle_text_str.encode_utf16().collect();

    // 动态生成翻译模式菜单项（带选中圆点提示）
    let mode_general_str = if current_mode == TranslateMode::General {
        "● 通用文本翻译 (极速响应)\0"
    } else {
        "○ 通用文本翻译 (极速响应)\0"
    };
    let mode_general_wide: Vec<u16> = mode_general_str.encode_utf16().collect();

    let mode_llm_str = if current_mode == TranslateMode::Llm {
        "● 大模型文本翻译 (AI 润色)\0"
    } else {
        "○ 大模型文本翻译 (AI 润色)\0"
    };
    let mode_llm_wide: Vec<u16> = mode_llm_str.encode_utf16().collect();

    let settings_text = w!("⚙ 快捷键设置...");
    let exit_text = w!("✕ 退出 (Exit)");

    let _ = AppendMenuW(
        hmenu,
        MF_STRING | MF_GRAYED | MF_DISABLED,
        ID_TRAY_STATUS,
        PCWSTR(title_wide.as_ptr()),
    );
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_TRAY_TOGGLE_HOTKEYS,
        PCWSTR(toggle_wide.as_ptr()),
    );
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_TRAY_MODE_GENERAL,
        PCWSTR(mode_general_wide.as_ptr()),
    );
    let _ = AppendMenuW(
        hmenu,
        MF_STRING,
        ID_TRAY_MODE_LLM,
        PCWSTR(mode_llm_wide.as_ptr()),
    );
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(hmenu, MF_STRING, ID_TRAY_SETTINGS, settings_text);
    let _ = AppendMenuW(hmenu, MF_SEPARATOR, 0, PCWSTR::null());
    let _ = AppendMenuW(hmenu, MF_STRING, ID_TRAY_EXIT, exit_text);

    let mut cursor = POINT::default();
    let _ = GetCursorPos(&mut cursor);

    // 关键：Windows 托盘菜单必须将目标窗口置于前台，否则点击菜单外区域无法自动关闭菜单
    let _ = SetForegroundWindow(hwnd);

    let cmd = TrackPopupMenu(
        hmenu,
        TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_NONOTIFY,
        cursor.x,
        cursor.y,
        0,
        hwnd,
        None,
    );

    let _ = DestroyMenu(hmenu);
    let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));

    if cmd.0 > 0 {
        handle_menu_command(hwnd, cmd.0 as usize);
    }
}

/// 注册全局快捷键
unsafe fn register_global_hotkey(hwnd: HWND, id: i32, hotkey_str: &str) {
    if let Some((mods, vk)) = parse_hotkey(hotkey_str) {
        let res = RegisterHotKey(hwnd, id, mods, vk.0 as u32);
        if res.is_ok() {
            println!("[Hotkey] Successfully registered hotkey (ID {}): {}", id, hotkey_str);
        } else {
            eprintln!("[Hotkey] Failed to register hotkey (ID {}): {}", id, hotkey_str);
        }
    } else {
        eprintln!("[Hotkey] Invalid hotkey string: {}", hotkey_str);
    }
}

/// 解析类似 "Alt+Q", "Ctrl+Shift+D", "Alt+W" 的快捷键字符串
pub fn parse_hotkey(s: &str) -> Option<(HOT_KEY_MODIFIERS, VIRTUAL_KEY)> {
    let mut mods = MOD_NOREPEAT.0;
    let mut vk_opt = None;

    for part in s.split('+') {
        let p = part.trim().to_uppercase();
        match p.as_str() {
            "ALT" => mods |= MOD_ALT.0,
            "CTRL" | "CONTROL" => mods |= MOD_CONTROL.0,
            "SHIFT" => mods |= MOD_SHIFT.0,
            "WIN" | "WINDOWS" => mods |= MOD_WIN.0,
            s if s.len() == 1 => {
                let c = s.chars().next().unwrap();
                if c.is_ascii_alphabetic() {
                    vk_opt = Some(VIRTUAL_KEY(c as u16));
                } else if c.is_ascii_digit() {
                    vk_opt = Some(VIRTUAL_KEY(c as u16));
                }
            }
            "F1" => vk_opt = Some(VIRTUAL_KEY(0x70)),
            "F2" => vk_opt = Some(VIRTUAL_KEY(0x71)),
            "F3" => vk_opt = Some(VIRTUAL_KEY(0x72)),
            "F4" => vk_opt = Some(VIRTUAL_KEY(0x73)),
            "F5" => vk_opt = Some(VIRTUAL_KEY(0x74)),
            "F6" => vk_opt = Some(VIRTUAL_KEY(0x75)),
            "F7" => vk_opt = Some(VIRTUAL_KEY(0x76)),
            "F8" => vk_opt = Some(VIRTUAL_KEY(0x77)),
            "F9" => vk_opt = Some(VIRTUAL_KEY(0x78)),
            "F10" => vk_opt = Some(VIRTUAL_KEY(0x79)),
            "F11" => vk_opt = Some(VIRTUAL_KEY(0x7A)),
            "F12" => vk_opt = Some(VIRTUAL_KEY(0x7B)),
            "SPACE" => vk_opt = Some(VIRTUAL_KEY(0x20)),
            "TAB" => vk_opt = Some(VIRTUAL_KEY(0x09)),
            "ESC" | "ESCAPE" => vk_opt = Some(VIRTUAL_KEY(0x1B)),
            _ => {}
        }
    }

    vk_opt.map(|vk| (HOT_KEY_MODIFIERS(mods), vk))
}

/// 获取托盘图标：优先从 exe 嵌入的 Win32 资源中读取高清图标，若无则使用备用绘制
unsafe fn get_tray_icon(instance: windows::Win32::Foundation::HMODULE) -> HICON {
    // 尝试从 exe 嵌入资源加载 1 号图标（winres 默认 ID 为 1）
    if let Ok(h) = LoadImageW(
        instance,
        PCWSTR(1 as *const u16),
        IMAGE_ICON,
        32,
        32,
        LR_DEFAULTCOLOR,
    ) {
        if !h.is_invalid() && h.0 != std::ptr::null_mut() {
            return HICON(h.0);
        }
    }

    // 备用：从 GDI 动态生成与主题一致的矢量图标
    if let Some(hicon) = create_app_icon() {
        return hicon;
    }

    // 终极备用：系统默认程序图标
    LoadIconW(None, IDI_APPLICATION).unwrap_or_default()
}

/// 创建带粉色渐变质感的小图标，与悬浮球的色彩主题保持一致
unsafe fn create_app_icon() -> Option<HICON> {
    let hdc_screen = GetDC(HWND::default());
    if hdc_screen.is_invalid() {
        return None;
    }

    let hdc = CreateCompatibleDC(hdc_screen);
    let hbm_color = CreateCompatibleBitmap(hdc_screen, 32, 32);
    let hbm_mask = CreateCompatibleBitmap(hdc_screen, 32, 32);

    if hdc.is_invalid() || hbm_color.is_invalid() || hbm_mask.is_invalid() {
        let _ = ReleaseDC(HWND::default(), hdc_screen);
        return None;
    }

    let old_bm = SelectObject(hdc, HGDIOBJ(hbm_color.0));

    // 绘制彩色圆形底色 (#db5590 -> BGR: 0x9055DB)
    let brush = CreateSolidBrush(COLORREF(0x009055DB));
    let old_brush = SelectObject(hdc, HGDIOBJ(brush.0));
    let pen = CreatePen(PS_SOLID, 1, COLORREF(0x009055DB));
    let old_pen = SelectObject(hdc, HGDIOBJ(pen.0));

    let _ = Ellipse(hdc, 3, 3, 29, 29);

    // 绘制内层高光圈
    let inner_brush = CreateSolidBrush(COLORREF(0x00C4A4F0));
    let _ = SelectObject(hdc, HGDIOBJ(inner_brush.0));
    let _ = Ellipse(hdc, 8, 8, 24, 24);

    SelectObject(hdc, old_brush);
    SelectObject(hdc, old_pen);
    SelectObject(hdc, old_bm);
    let _ = DeleteObject(HGDIOBJ(brush.0));
    let _ = DeleteObject(HGDIOBJ(inner_brush.0));
    let _ = DeleteObject(HGDIOBJ(pen.0));
    let _ = DeleteDC(hdc);

    // 绘制透明 Mask（白色为透明，黑色为不透明）
    let hdc_mask = CreateCompatibleDC(hdc_screen);
    let old_mask_bm = SelectObject(hdc_mask, HGDIOBJ(hbm_mask.0));

    let white_brush = GetStockObject(WHITE_BRUSH);
    let _ = SelectObject(hdc_mask, white_brush);
    let _ = Rectangle(hdc_mask, 0, 0, 32, 32);

    let black_brush = GetStockObject(BLACK_BRUSH);
    let _ = SelectObject(hdc_mask, black_brush);
    let black_pen = GetStockObject(BLACK_PEN);
    let _ = SelectObject(hdc_mask, black_pen);
    let _ = Ellipse(hdc_mask, 3, 3, 29, 29);

    SelectObject(hdc_mask, old_mask_bm);
    let _ = DeleteDC(hdc_mask);
    let _ = ReleaseDC(HWND::default(), hdc_screen);

    let mut icon_info = ICONINFO {
        fIcon: BOOL(1),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: hbm_mask,
        hbmColor: hbm_color,
    };

    let hicon = CreateIconIndirect(&mut icon_info).ok();

    let _ = DeleteObject(HGDIOBJ(hbm_color.0));
    let _ = DeleteObject(HGDIOBJ(hbm_mask.0));

    hicon
}
