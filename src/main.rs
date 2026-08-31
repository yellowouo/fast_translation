#![windows_subsystem = "windows"]

mod config;
mod input;
mod selection;
mod translator;
mod tray;
mod tts;

use anyhow::Result;
use config::AppConfig;
use input::MouseHook;
use selection::SelectionReader;
use std::sync::{Arc, Mutex, mpsc};
use tokio::sync::Mutex as AsyncMutex;
use translator::BaiduTranslator;
use tray::TrayIcon;
use tts::WindowsTts;

slint::include_modules!();

pub enum AppEvent {
    MouseRelease {
        down_x: i32,
        down_y: i32,
        x: i32,
        y: i32,
        dragged: bool,
    },
    HotKeyTriggered,
    OpenSettings,
    ToggleHotkeys(bool),
    SetTranslateMode(crate::config::TranslateMode),
    Exit,
}

struct SendWin(TranslationPopup);
unsafe impl Send for SendWin {}
unsafe impl Sync for SendWin {}

struct SendBall(FloatingBall);
unsafe impl Send for SendBall {}
unsafe impl Sync for SendBall {}

struct SendSettings(HotkeySettingsWindow);
unsafe impl Send for SendSettings {}
unsafe impl Sync for SendSettings {}

static WIN: once_cell::sync::Lazy<Mutex<Option<SendWin>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));
static BALL: once_cell::sync::Lazy<Mutex<Option<SendBall>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));
static SETTINGS_WIN: once_cell::sync::Lazy<Mutex<Option<SendSettings>>> =
    once_cell::sync::Lazy::new(|| Mutex::new(None));

static TEXT: once_cell::sync::Lazy<Mutex<String>> =
    once_cell::sync::Lazy::new(|| Mutex::new(String::new()));
/// 最近一次翻译结果；译文卡的复制/朗读都取自这里
static TRANSLATED_TEXT: once_cell::sync::Lazy<Mutex<String>> =
    once_cell::sync::Lazy::new(|| Mutex::new(String::new()));

static DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static DRAG_ANCHOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static DRAG_ANCHOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static WIN_ANCHOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static WIN_ANCHOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

static SETTINGS_DRAGGING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static SETTINGS_DRAG_ANCHOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SETTINGS_DRAG_ANCHOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SETTINGS_WIN_ANCHOR_X: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);
static SETTINGS_WIN_ANCHOR_Y: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

fn with_tw<R>(f: impl FnOnce(&TranslationPopup) -> R) -> R {
    f(&WIN.lock().unwrap().as_ref().unwrap().0)
}
fn with_fb<R>(f: impl FnOnce(&FloatingBall) -> R) -> R {
    f(&BALL.lock().unwrap().as_ref().unwrap().0)
}
fn with_sw<R>(f: impl FnOnce(&HotkeySettingsWindow) -> R) -> R {
    f(&SETTINGS_WIN.lock().unwrap().as_ref().unwrap().0)
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("[Fast Translation] Starting in background...");

    let app_config = Arc::new(Mutex::new(AppConfig::load()?));
    println!("[Fast Translation] Configuration loaded");

    let translator = Arc::new(AsyncMutex::new(BaiduTranslator::new(&app_config.lock().unwrap())?));
    println!("[Fast Translation] Translator ready (cache {})", app_config.lock().unwrap().app.cache_size);

    let tts = Arc::new(WindowsTts::new());
    println!("[Fast Translation] TTS ready");

    let (tx, rx) = mpsc::channel::<AppEvent>();
    let _hook = MouseHook::start(tx.clone())?;
    println!("[Fast Translation] Mouse hook started");

    let initial_hotkey = app_config.lock().unwrap().hotkey.translate.clone();
    let initial_toggle_hotkey = app_config.lock().unwrap().hotkey.toggle.clone();
    let initial_hotkey_enabled = app_config.lock().unwrap().hotkey.enabled;
    let initial_mode = app_config.lock().unwrap().baidu.mode;
    let _tray = TrayIcon::start(
        tx.clone(),
        initial_hotkey,
        initial_toggle_hotkey,
        initial_hotkey_enabled,
        initial_mode,
    )?;
    println!("[Fast Translation] Tray icon started");

    let _keep_alive = KeepAlive::new()?;
    _keep_alive.window().set_position(slint::PhysicalPosition::new(-9999, -9999));
    let _ = _keep_alive.window().show();

    let translation_win = TranslationPopup::new()?;
    let floating_ball = FloatingBall::new()?;
    let settings_win = HotkeySettingsWindow::new()?;

    {
        let cfg = app_config.lock().unwrap();
        settings_win.set_current_hotkey(slint::SharedString::from(&cfg.hotkey.translate));
        settings_win.set_current_toggle_hotkey(slint::SharedString::from(&cfg.hotkey.toggle));
        settings_win.set_hotkeys_enabled(cfg.hotkey.enabled);
        settings_win.set_drag_copy_fallback(cfg.hotkey.drag_copy_fallback);
    }

    *WIN.lock().unwrap() = Some(SendWin(translation_win.clone_strong()));
    *BALL.lock().unwrap() = Some(SendBall(floating_ball.clone_strong()));
    *SETTINGS_WIN.lock().unwrap() = Some(SendSettings(settings_win.clone_strong()));

    let sr = Arc::new(SelectionReader::new());
    println!("[Fast Translation] Ready.");

    // ── Event router / Poll thread ──
    {
        let sr = sr.clone();
        let tr = translator.clone();
        let cfg = app_config.clone();
        std::thread::Builder::new().name("poll".into()).spawn(move || {
            while let Ok(ev) = rx.recv() {
                match ev {
                    AppEvent::MouseRelease {
                        down_x,
                        down_y,
                        x,
                        y,
                        dragged,
                    } => {
                        // 1. 如果处于窗口拖动状态，释放拖拽状态并忽略此释放事件
                        if DRAGGING.swap(false, std::sync::atomic::Ordering::Relaxed)
                            || SETTINGS_DRAGGING.swap(false, std::sync::atomic::Ordering::Relaxed)
                        {
                            continue;
                        }

                        // 2. 检测鼠标释放点是否在当前进程的任何窗口（弹窗、设置窗口、悬浮球）内部
                        let pt = windows::Win32::Foundation::POINT { x, y };
                        let hit_hwnd = unsafe { windows::Win32::UI::WindowsAndMessaging::WindowFromPoint(pt) };
                        let mut hit_pid = 0;
                        if !hit_hwnd.is_invalid() && hit_hwnd.0 != std::ptr::null_mut() {
                            unsafe {
                                windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId(
                                    hit_hwnd,
                                    Some(&mut hit_pid),
                                );
                            }
                        }
                        let is_click_inside_our_app = hit_pid == std::process::id();

                        if is_click_inside_our_app {
                            // 用户点击在我们自己的窗口内部（如点击复制/朗读按钮、输入框、标题栏拖拽区域等），绝不关闭窗口
                            continue;
                        }

                        // 3. 用户点击在外部其他应用或桌面上：隐藏翻译弹窗与悬浮球
                        with_tw(|tw| { let _ = tw.window().hide(); });

                        if !TrayIcon::is_enabled() {
                            with_fb(|fb| { let _ = fb.window().hide(); });
                            continue;
                        }

                        // 4. 过滤非客户区拖拽（例如拖拽外部窗口标题栏、边框、滚动条、任务栏或桌面）
                        let is_client = is_valid_client_drag(down_x, down_y, x, y);
                        if dragged && !is_client {
                            with_fb(|fb| { let _ = fb.window().hide(); });
                            continue;
                        }

                        // 5. 进行选中文本获取
                        let drag_fallback = cfg.lock().unwrap().hotkey.drag_copy_fallback;
                        let text = sr.get_selection_hybrid(dragged && drag_fallback && is_client);

                        let _ = slint::invoke_from_event_loop(move || {
                            match text {
                                Some(ref t) => {
                                    *TEXT.lock().unwrap() = t.clone();
                                    with_fb(|fb| {
                                        fb.set_visible_ball(true);
                                        let scale = fb.window().scale_factor();
                                        let ball_size = (20.0 * scale).round() as i32;
                                        let (ball_x, ball_y) = clamp_window_position(
                                            x + (12.0 * scale).round() as i32,
                                            y - (14.0 * scale).round() as i32,
                                            ball_size,
                                            ball_size,
                                        );
                                        fb.window().set_position(slint::PhysicalPosition::new(
                                            ball_x, ball_y,
                                        ));
                                        configure_tool_windows();
                                        let _ = fb.window().show();
                                    });
                                }
                                None => { with_fb(|fb| { let _ = fb.window().hide(); }); }
                            }
                        });
                    }

                    AppEvent::HotKeyTriggered => {
                        if !TrayIcon::is_enabled() || TrayIcon::is_settings_open() {
                            continue;
                        }
                        let settings_open = with_sw(|sw| sw.window().is_visible());
                        if settings_open {
                            continue;
                        }
                        println!("[App] Global hotkey: open empty translation window at cursor...");
                        let _ = slint::invoke_from_event_loop(move || {
                            show_empty_translation_at_cursor();
                        });
                    }

                    AppEvent::OpenSettings => {
                        TrayIcon::set_settings_open(true);
                        let cfg_clone = cfg.clone();
                        let _ = slint::invoke_from_event_loop(move || {
                            let mut cursor = windows::Win32::Foundation::POINT::default();
                            unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor).ok(); }
                            let (hotkey, toggle, enabled, drag) = {
                                let c = cfg_clone.lock().unwrap();
                                (
                                    c.hotkey.translate.clone(),
                                    c.hotkey.toggle.clone(),
                                    c.hotkey.enabled,
                                    c.hotkey.drag_copy_fallback,
                                )
                            };
                            with_sw(|sw| {
                                sw.set_current_hotkey(slint::SharedString::from(&hotkey));
                                sw.set_current_toggle_hotkey(slint::SharedString::from(&toggle));
                                sw.set_hotkeys_enabled(enabled);
                                sw.set_drag_copy_fallback(drag);
                                sw.set_recording_target(0);
                                sw.set_tip_message(slint::SharedString::default());

                                let scale = sw.window().scale_factor();
                                let win_w = (380.0 * scale).round() as i32;
                                let win_h = (350.0 * scale).round() as i32;
                                let (sw_x, sw_y) = clamp_window_position(
                                    cursor.x - win_w / 2,
                                    cursor.y - win_h - (20.0 * scale).round() as i32,
                                    win_w,
                                    win_h,
                                );
                                sw.window().set_position(slint::PhysicalPosition::new(
                                    sw_x, sw_y,
                                ));
                                configure_tool_windows();
                                let _ = sw.window().show();
                            });
                        });
                    }

                    AppEvent::ToggleHotkeys(enabled) => {
                        {
                            let mut c = cfg.lock().unwrap();
                            c.hotkey.enabled = enabled;
                            let _ = c.save();
                        }
                        let _ = slint::invoke_from_event_loop(move || {
                            with_sw(|sw| {
                                sw.set_hotkeys_enabled(enabled);
                            });
                        });
                    }

                    AppEvent::SetTranslateMode(mode) => {
                        {
                            let mut c = cfg.lock().unwrap();
                            c.baidu.mode = mode;
                            let _ = c.save();
                        }
                        tr.blocking_lock().set_mode(mode);
                        TrayIcon::update_mode(mode);
                    }

                    AppEvent::Exit => {
                        let _ = slint::invoke_from_event_loop(|| slint::quit_event_loop().unwrap());
                        break;
                    }
                }
            }
        })?;
    }

    // ── Hover / Click → translate ──
    {
        let tr = translator.clone();
        floating_ball.on_hovered(move || { show_translation(&tr); });
    }
    {
        let tr = translator.clone();
        floating_ball.on_clicked(move || { show_translation(&tr); });
    }

    // ── TTS ──
    // 译文卡的 🔊 朗读译文
    {
        let tts = tts.clone();
        translation_win.on_play_pronunciation(move || {
            let text = TRANSLATED_TEXT.lock().unwrap().clone();
            if !text.is_empty() {
                let tts = tts.clone();
                std::thread::spawn(move || { let _ = tts.speak_async(&text); });
            }
        });
    }
    // 原文卡的 🔊 朗读原文
    {
        let tts = tts.clone();
        translation_win.on_play_source(move || {
            let text = TEXT.lock().unwrap().clone();
            if !text.is_empty() {
                let tts = tts.clone();
                std::thread::spawn(move || { let _ = tts.speak_async(&text); });
            }
        });
    }

    // ── Copy ──
    translation_win.on_copy_result(|| {
        let text = TRANSLATED_TEXT.lock().unwrap().clone();
        if !text.is_empty() { copy_to_clipboard(&text); }
    });
    translation_win.on_copy_source(|| {
        let text = TEXT.lock().unwrap().clone();
        if !text.is_empty() { copy_to_clipboard(&text); }
    });

    // ── Header drag (Translation Popup) ──
    translation_win.on_header_moved(|| {
        use std::sync::atomic::Ordering;
        let mut cursor = windows::Win32::Foundation::POINT::default();
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor).ok(); }
        if !DRAGGING.load(Ordering::Relaxed) {
            DRAGGING.store(true, Ordering::Relaxed);
            DRAG_ANCHOR_X.store(cursor.x, Ordering::Relaxed);
            DRAG_ANCHOR_Y.store(cursor.y, Ordering::Relaxed);
            let pos = WIN.lock().unwrap().as_ref().unwrap().0.window().position();
            WIN_ANCHOR_X.store(pos.x, Ordering::Relaxed);
            WIN_ANCHOR_Y.store(pos.y, Ordering::Relaxed);
        } else {
            let dx = cursor.x - DRAG_ANCHOR_X.load(Ordering::Relaxed);
            let dy = cursor.y - DRAG_ANCHOR_Y.load(Ordering::Relaxed);
            let nx = WIN_ANCHOR_X.load(Ordering::Relaxed) + dx;
            let ny = WIN_ANCHOR_Y.load(Ordering::Relaxed) + dy;
            with_tw(|tw| {
                tw.window().set_position(slint::PhysicalPosition::new(nx, ny));
            });
        }
    });

    // ── Close popup ──
    translation_win.on_close_popup(|| {
        DRAGGING.store(false, std::sync::atomic::Ordering::Relaxed);
        with_tw(|tw| { let _ = tw.window().hide(); });
        with_fb(|fb| { fb.set_visible_ball(true); });
    });

    // ── Editable source text input callback ──
    {
        let tr = translator.clone();
        translation_win.on_source_edited(move |new_text| {
            let text_str = new_text.to_string();
            {
                let cur = TEXT.lock().unwrap();
                if *cur == text_str {
                    return;
                }
            }
            *TEXT.lock().unwrap() = text_str.clone();
            if text_str.trim().is_empty() {
                with_tw(|tw| {
                    tw.set_translated_text(slint::SharedString::default());
                    tw.set_is_loading(false);
                });
                return;
            }
            let tr = tr.clone();
            slint::spawn_local(async move {
                with_tw(|tw| {
                    tw.set_is_loading(true);
                    tw.set_error_message(slint::SharedString::default());
                });
                let result = tr.lock().await.translate(&text_str).await;
                with_tw(|tw| {
                    match result {
                        Ok(translated) => {
                            *TRANSLATED_TEXT.lock().unwrap() = translated.clone();
                            tw.set_translated_text(slint::SharedString::from(&translated));
                            tw.set_is_loading(false);
                        }
                        Err(e) => {
                            tw.set_error_message(slint::SharedString::from(&e.to_string()));
                            tw.set_is_loading(false);
                        }
                    }
                });
            }).unwrap();
        });
    }

    // ── Key Recording Callback ──
    settings_win.on_record_key_pressed(|target, key, ctrl, alt, shift, meta| {
        let key_str = key.to_string();
        if let Some(combo) = format_recorded_hotkey(&key_str, ctrl, alt, shift, meta) {
            with_sw(|sw| {
                if target == 1 {
                    sw.set_current_hotkey(slint::SharedString::from(&combo));
                    sw.set_tip_message(slint::SharedString::from(format!("已录制翻译快捷键：{}", combo)));
                } else if target == 2 {
                    sw.set_current_toggle_hotkey(slint::SharedString::from(&combo));
                    sw.set_tip_message(slint::SharedString::from(format!("已录制启停快捷键：{}", combo)));
                }
                sw.set_recording_target(0);
            });
        }
    });

    settings_win.on_start_recording(|target| {
        println!("[Settings] Hotkey recording started for target {}...", target);
        TrayIcon::set_settings_open(true);
    });

    settings_win.on_stop_recording(|| {
        println!("[Settings] Hotkey recording canceled/stopped.");
    });

    // ── Settings Window Callbacks ──
    {
        let cfg = app_config.clone();
        settings_win.on_save_settings(move |hotkey, toggle_hotkey, enabled, drag_fallback| {
            let hotkey_str = hotkey.to_string();
            let toggle_str = toggle_hotkey.to_string();
            println!(
                "[Settings] Saving settings: hotkey={}, toggle={}, enabled={}, drag_fallback={}",
                hotkey_str, toggle_str, enabled, drag_fallback
            );
            {
                let mut c = cfg.lock().unwrap();
                c.hotkey.translate = hotkey_str.clone();
                c.hotkey.toggle = toggle_str.clone();
                c.hotkey.enabled = enabled;
                c.hotkey.drag_copy_fallback = drag_fallback;
                let _ = c.save();
            }
            TrayIcon::update_hotkeys(&hotkey_str, &toggle_str, enabled);

            with_sw(|sw| {
                sw.set_recording_target(0);
                sw.set_tip_message(slint::SharedString::from("设置已保存生效！"));
            });

            // 600ms 后自动隐藏设置窗口并恢复全局热键
            let t: &'static slint::Timer = Box::leak(Box::new(slint::Timer::default()));
            t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(600), || {
                TrayIcon::set_settings_open(false);
                with_sw(|sw| {
                    sw.set_tip_message(slint::SharedString::default());
                    let _ = sw.window().hide();
                });
            });
        });
    }

    settings_win.on_close_window(|| {
        SETTINGS_DRAGGING.store(false, std::sync::atomic::Ordering::Relaxed);
        TrayIcon::set_settings_open(false);
        with_sw(|sw| {
            sw.set_recording_target(0);
            sw.set_tip_message(slint::SharedString::default());
            let _ = sw.window().hide();
        });
    });

    // ── Header drag (Settings Window) ──
    settings_win.on_header_moved(|| {
        use std::sync::atomic::Ordering;
        let mut cursor = windows::Win32::Foundation::POINT::default();
        unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor).ok(); }
        if !SETTINGS_DRAGGING.load(Ordering::Relaxed) {
            SETTINGS_DRAGGING.store(true, Ordering::Relaxed);
            SETTINGS_DRAG_ANCHOR_X.store(cursor.x, Ordering::Relaxed);
            SETTINGS_DRAG_ANCHOR_Y.store(cursor.y, Ordering::Relaxed);
            let pos = SETTINGS_WIN.lock().unwrap().as_ref().unwrap().0.window().position();
            SETTINGS_WIN_ANCHOR_X.store(pos.x, Ordering::Relaxed);
            SETTINGS_WIN_ANCHOR_Y.store(pos.y, Ordering::Relaxed);
        } else {
            let dx = cursor.x - SETTINGS_DRAG_ANCHOR_X.load(Ordering::Relaxed);
            let dy = cursor.y - SETTINGS_DRAG_ANCHOR_Y.load(Ordering::Relaxed);
            let nx = SETTINGS_WIN_ANCHOR_X.load(Ordering::Relaxed) + dx;
            let ny = SETTINGS_WIN_ANCHOR_Y.load(Ordering::Relaxed) + dy;
            with_sw(|sw| {
                sw.window().set_position(slint::PhysicalPosition::new(nx, ny));
            });
        }
    });

    // ── Release drag on mouse up ──
    {
        let t: &'static slint::Timer = Box::leak(Box::new(slint::Timer::default()));
        t.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(30), || {
            if DRAGGING.load(std::sync::atomic::Ordering::Relaxed)
                || SETTINGS_DRAGGING.load(std::sync::atomic::Ordering::Relaxed)
            {
                use windows::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;
                let pressed = unsafe { GetAsyncKeyState(0x01) } & 0x8000u16 as i16 != 0;
                if !pressed {
                    DRAGGING.store(false, std::sync::atomic::Ordering::Relaxed);
                    SETTINGS_DRAGGING.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        });
    }

    // ── Hide on startup ──
    {
        let t: &'static slint::Timer = Box::leak(Box::new(slint::Timer::default()));
        t.start(slint::TimerMode::SingleShot, std::time::Duration::from_millis(50), || {
            configure_tool_windows();
            with_tw(|tw| { let _ = tw.window().hide(); });
            with_fb(|fb| { let _ = fb.window().hide(); });
            with_sw(|sw| { let _ = sw.window().hide(); });
        });
    }

    slint::run_event_loop()?;
    println!("[Fast Translation] Exiting...");
    Ok(())
}

fn show_translation(tr: &Arc<AsyncMutex<BaiduTranslator>>) {
    with_fb(|fb| { fb.set_visible_ball(false); });
    let text = TEXT.lock().unwrap().clone();
    if text.is_empty() { return; }
    *TRANSLATED_TEXT.lock().unwrap() = String::new();

    let tw = WIN.lock().unwrap().as_ref().unwrap().0.clone_strong();
    let pos = BALL.lock().unwrap().as_ref().unwrap().0.window().position();

    tw.set_source_expanded(false);
    tw.set_original_text(slint::SharedString::from(&text));
    tw.set_translated_text(slint::SharedString::default());
    tw.set_is_loading(true);
    tw.set_error_message(slint::SharedString::default());

    let scale = tw.window().scale_factor();
    let win_w = if tw.window().size().width > 0 {
        tw.window().size().width as i32
    } else {
        (440.0 * scale).round() as i32
    };
    let win_h = if tw.window().size().height > 0 {
        tw.window().size().height as i32
    } else {
        (300.0 * scale).round() as i32
    };
    let (clamped_x, clamped_y) = clamp_window_position(
        pos.x + (24.0 * scale).round() as i32,
        pos.y,
        win_w,
        win_h,
    );
    tw.window().set_position(slint::PhysicalPosition::new(clamped_x, clamped_y));
    configure_tool_windows();
    let _ = tw.window().show();

    let tr = tr.clone();
    slint::spawn_local(async move {
        let result = tr.lock().await.translate(&text).await;
        match result {
            Ok(translated) => {
                *TRANSLATED_TEXT.lock().unwrap() = translated.clone();
                tw.set_translated_text(slint::SharedString::from(&translated));
                tw.set_is_loading(false);
            }
            Err(e) => {
                tw.set_error_message(slint::SharedString::from(&e.to_string()));
                tw.set_is_loading(false);
            }
        }
    }).unwrap();
}

fn show_empty_translation_at_cursor() {
    with_fb(|fb| { fb.set_visible_ball(false); });
    *TEXT.lock().unwrap() = String::new();
    *TRANSLATED_TEXT.lock().unwrap() = String::new();

    let tw = WIN.lock().unwrap().as_ref().unwrap().0.clone_strong();

    let mut cursor = windows::Win32::Foundation::POINT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor).ok(); }

    tw.set_source_expanded(false);
    tw.set_original_text(slint::SharedString::default());
    tw.set_translated_text(slint::SharedString::default());
    tw.set_is_loading(false);
    tw.set_error_message(slint::SharedString::default());

    let scale = tw.window().scale_factor();
    let win_w = (440.0 * scale).round() as i32;
    let win_h = (220.0 * scale).round() as i32;
    let (clamped_x, clamped_y) = clamp_window_position(
        cursor.x + (16.0 * scale).round() as i32,
        cursor.y - (16.0 * scale).round() as i32,
        win_w,
        win_h,
    );
    tw.window().set_position(slint::PhysicalPosition::new(clamped_x, clamped_y));
    configure_tool_windows();
    let _ = tw.window().show();
}

fn copy_to_clipboard(text: &str) {
    selection::write_clipboard_text(text);
}

/// 将当前 UI 线程中的浮窗设置扩展样式 WS_EX_TOOLWINDOW，防止在任务栏中出现；
/// 针对悬浮球和保活窗口附加 WS_EX_NOACTIVATE（不抢前台焦点），而弹窗与设置窗口保留交互激活能力
fn configure_tool_windows() {
    use windows::Win32::Foundation::{BOOL, HWND, LPARAM};
    use windows::Win32::System::Threading::GetCurrentThreadId;
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumThreadWindows, GetClassNameW, GetWindowLongW, GetWindowTextW, SetWindowLongW,
        GWL_EXSTYLE, WS_EX_APPWINDOW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
    };

    unsafe extern "system" fn enum_proc(hwnd: HWND, _: LPARAM) -> BOOL {
        let mut class_name = [0u16; 256];
        let len = GetClassNameW(hwnd, &mut class_name);
        if len > 0 {
            let name = String::from_utf16_lossy(&class_name[..len as usize]);
            if !name.contains("Tray") {
                let mut title_buf = [0u16; 256];
                let title_len = GetWindowTextW(hwnd, &mut title_buf);
                let title = if title_len > 0 {
                    String::from_utf16_lossy(&title_buf[..title_len as usize])
                } else {
                    String::new()
                };

                let style = GetWindowLongW(hwnd, GWL_EXSTYLE);
                let is_non_interactive = title.contains("Ball") || title.contains("KeepAlive");

                let target = if is_non_interactive {
                    (style & !(WS_EX_APPWINDOW.0 as i32))
                        | (WS_EX_TOOLWINDOW.0 as i32)
                        | (WS_EX_NOACTIVATE.0 as i32)
                } else {
                    (style & !(WS_EX_APPWINDOW.0 as i32 | WS_EX_NOACTIVATE.0 as i32))
                        | (WS_EX_TOOLWINDOW.0 as i32)
                };

                if style != target {
                    SetWindowLongW(hwnd, GWL_EXSTYLE, target);
                }
            }
        }
        BOOL(1)
    }

    unsafe {
        let tid = GetCurrentThreadId();
        let _ = EnumThreadWindows(tid, Some(enum_proc), LPARAM(0));
    }
}

/// 将 Slint 接收到的按键事件格式化为类似 "Alt+Q", "Ctrl+Shift+D", "F10" 的规范字符串
fn format_recorded_hotkey(
    key: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    win: bool,
) -> Option<String> {
    let mut mods = Vec::new();
    if ctrl { mods.push("Ctrl"); }
    if alt { mods.push("Alt"); }
    if shift { mods.push("Shift"); }
    if win { mods.push("Win"); }

    let trimmed = key.trim();
    if trimmed.is_empty() {
        return None;
    }

    let main_key = match trimmed {
        " " => "Space".to_string(),
        "\t" => "Tab".to_string(),
        "\r" | "\n" => "Enter".to_string(),
        "\u{001b}" => return None,
        s if s.chars().count() == 1 => {
            let c = s.chars().next().unwrap();
            match c {
                '\u{f704}' => "F1".to_string(),
                '\u{f705}' => "F2".to_string(),
                '\u{f706}' => "F3".to_string(),
                '\u{f707}' => "F4".to_string(),
                '\u{f708}' => "F5".to_string(),
                '\u{f709}' => "F6".to_string(),
                '\u{f70a}' => "F7".to_string(),
                '\u{f70b}' => "F8".to_string(),
                '\u{f70c}' => "F9".to_string(),
                '\u{f70d}' => "F10".to_string(),
                '\u{f70e}' => "F11".to_string(),
                '\u{f70f}' => "F12".to_string(),
                '\u{f700}'..='\u{f73f}' => return None, // 其他功能键
                c if c.is_alphanumeric() => c.to_uppercase().to_string(),
                _ => return None,
            }
        }
        s if s.eq_ignore_ascii_case("Control")
            || s.eq_ignore_ascii_case("Alt")
            || s.eq_ignore_ascii_case("Shift")
            || s.eq_ignore_ascii_case("Meta") =>
        {
            return None;
        }
        s => s.to_uppercase(),
    };

    if mods.is_empty() && !main_key.starts_with('F') {
        mods.push("Alt");
    }

    mods.push(&main_key);
    Some(mods.join("+"))
}

/// 检查鼠标拖动的起点和终点是否均位于合法的窗口客户区（有效排除标题栏拖拽、边框缩放、滚动条、任务栏和桌面框选）
fn is_valid_client_drag(down_x: i32, down_y: i32, up_x: i32, up_y: i32) -> bool {
    use windows::Win32::Foundation::{LPARAM, POINT, WPARAM};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetClassNameW, SendMessageW, WindowFromPoint, WM_NCHITTEST,
    };

    let check_point = |px: i32, py: i32| -> (bool, isize) {
        let pt = POINT { x: px, y: py };
        let hwnd = unsafe { WindowFromPoint(pt) };
        if hwnd.is_invalid() || hwnd.0 == std::ptr::null_mut() {
            return (false, 0);
        }

        // 1. 过滤任务栏与桌面背景
        let mut class_name = [0u16; 64];
        let len = unsafe { GetClassNameW(hwnd, &mut class_name) };
        if len > 0 {
            let name = String::from_utf16_lossy(&class_name[..len as usize]);
            if name.contains("TrayWnd")
                || name == "Progman"
                || name == "WorkerW"
                || name.contains("Shell")
            {
                return (false, hwnd.0 as isize);
            }
        }

        // 2. 发送 WM_NCHITTEST 检测鼠标命中区域
        let l_param = LPARAM(((py as isize & 0xFFFF) << 16) | (px as isize & 0xFFFF));
        let hit = unsafe { SendMessageW(hwnd, WM_NCHITTEST, WPARAM(0), l_param) }.0;

        // HTCLIENT = 1（客户区，真正的文档/网页/代码文本区）
        // 标题栏(2)、滚动条(6,7)、缩放边框(10..18)、系统按钮(8,9,20)等均不是文本区
        (hit == 1, hwnd.0 as isize)
    };

    let (down_valid, down_hwnd) = check_point(down_x, down_y);
    if !down_valid {
        return false;
    }
    let (up_valid, up_hwnd) = check_point(up_x, up_y);
    if !up_valid {
        return false;
    }

    // 跨窗口拖拽（如从一个窗口拖到另一个窗口或桌面）通常不是划词选择
    if down_hwnd != up_hwnd {
        return false;
    }

    true
}

/// 将窗口物理坐标限制在当前显示器的工作区（排除任务栏）内，防止窗口在屏幕边缘或多显示器边界处被截断
fn clamp_window_position(x: i32, y: i32, width: i32, height: i32) -> (i32, i32) {
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromPoint, MONITORINFO, MONITOR_DEFAULTTONEAREST,
    };

    let pt = POINT { x, y };
    let hmon = unsafe { MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST) };
    let mut mi = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };

    let work_area = if unsafe { GetMonitorInfoW(hmon, &mut mi) }.as_bool() {
        mi.rcWork
    } else {
        RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1080,
        }
    };

    let margin = 12;
    let min_x = work_area.left + margin;
    let max_x = (work_area.right - width - margin).max(min_x);
    let min_y = work_area.top + margin;
    let max_y = (work_area.bottom - height - margin).max(min_y);

    let clamped_x = x.clamp(min_x, max_x);
    let clamped_y = y.clamp(min_y, max_y);

    (clamped_x, clamped_y)
}
