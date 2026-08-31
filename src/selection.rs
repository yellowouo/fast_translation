use std::cell::OnceCell;
use windows::Win32::Foundation::{POINT, RECT};
use windows::Win32::UI::Accessibility::{
    IUIAutomation, IUIAutomationElement, IUIAutomationTextPattern,
    CUIAutomation, UIA_TextPatternId,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_ALL, COINIT_APARTMENTTHREADED,
};

pub struct SelectionReader;

impl SelectionReader {
    pub fn new() -> Self {
        Self
    }

    /// Initialize COM on the calling thread (once).
    fn ensure_com() {
        thread_local! {
            static INIT: OnceCell<()> = OnceCell::new();
        }
        INIT.with(|c| {
            c.get_or_init(|| unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            });
        });
    }

    /// Try to get selected text via UI Automation
    pub fn get_selection(&self) -> Option<(String, RECT)> {
        Self::ensure_com();

        let auto: IUIAutomation = match unsafe {
            CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_ALL)
        } {
            Ok(a) => a,
            Err(e) => {
                eprintln!("[Selection] CoCreateInstance(UIAutomation) failed: {e}");
                return None;
            }
        };

        // Cursor position (screen coords)
        let mut pt = POINT::default();
        if unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut pt) }.is_err() {
            eprintln!("[Selection] GetCursorPos failed");
            return None;
        }

        // Element under cursor
        let mut elem: IUIAutomationElement = match unsafe { auto.ElementFromPoint(pt) } {
            Ok(e) => e,
            Err(e) => {
                eprintln!("[Selection] ElementFromPoint failed: {e}");
                return None;
            }
        };

        // Walk up the tree (max 20 hops) looking for TextPattern
        for depth in 0..20u32 {
            match Self::try_text_pattern(&auto, &elem, pt) {
                Some(result) => {
                    println!("[Selection] Found text via UIA at depth {depth}: {:.60}...", result.0);
                    return Some(result);
                }
                None => { /* try parent */ }
            }
            // Move to parent element
            match unsafe { auto.ControlViewWalker() } {
                Ok(walker) => match unsafe { walker.GetParentElement(&elem) } {
                    Ok(parent) => elem = parent,
                    Err(_) => break,
                },
                Err(_) => break,
            }
        }

        // Fallback: try GetFocusedElement
        match unsafe { auto.GetFocusedElement() } {
            Ok(focused) => {
                if let Some(result) = Self::try_text_pattern(&auto, &focused, pt) {
                    println!("[Selection] Found text via focused element: {:.60}...", result.0);
                    return Some(result);
                }
            }
            Err(e) => {
                eprintln!("[Selection] GetFocusedElement failed: {e}");
            }
        }

        None
    }

    /// 混合取词：UIA 优先；若 UIA 无法获取且鼠标发生拖拽（或强制取词），降级为模拟 Ctrl+C
    pub fn get_selection_hybrid(&self, dragged: bool) -> Option<String> {
        if let Some((text, _)) = self.get_selection() {
            let trimmed = text.trim();
            if trimmed.len() >= 2 {
                return Some(trimmed.to_string());
            }
        }
        if dragged {
            if let Some(text) = get_selection_by_copy() {
                let trimmed = text.trim();
                if trimmed.len() >= 2 {
                    println!("[Selection] Found text via simulated copy: {:.60}...", trimmed);
                    return Some(trimmed.to_string());
                }
            }
        }
        None
    }

    /// Try to extract selected text from an element via TextPattern.
    fn try_text_pattern(
        _auto: &IUIAutomation,
        elem: &IUIAutomationElement,
        cursor: POINT,
    ) -> Option<(String, RECT)> {
        unsafe {
            let iunknown = match elem.GetCurrentPattern(UIA_TextPatternId) {
                Ok(p) => p,
                Err(_) => return None, // Normal — element doesn't support TextPattern
            };
            let tp: IUIAutomationTextPattern = match windows::core::Interface::cast(&iunknown) {
                Ok(t) => t,
                Err(_) => return None,
            };

            let arr = match tp.GetSelection() {
                Ok(a) => a,
                Err(_) => return None,
            };
            let count = arr.Length().unwrap_or(0);
            if count == 0 {
                return None;
            }

            let range = match arr.GetElement(0) {
                Ok(r) => r,
                Err(_) => return None,
            };
            let text = match range.GetText(-1) {
                Ok(t) => t.to_string(),
                Err(_) => return None,
            };
            if text.trim().len() < 2 {
                return None;
            }

            let rect = elem.CurrentBoundingRectangle().unwrap_or(RECT {
                left: cursor.x,
                top: cursor.y - 20,
                right: cursor.x + 100,
                bottom: cursor.y,
            });

            Some((text, rect))
        }
    }
}

/// 通过向当前窗口发送 Ctrl+C 获取选中文本，并自动备份和恢复剪贴板
pub fn get_selection_by_copy() -> Option<String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, GetClipboardSequenceNumber, OpenClipboard,
    };
    use windows::Win32::UI::Input::KeyboardAndMouse::{SendInput, INPUT, VK_C, VK_CONTROL};

    // 1. 备份原剪贴板文本
    let original = read_clipboard_text();

    // 2. 清空剪贴板并记录清空后的序列号，确保只识别目标程序新写入的数据
    unsafe {
        if OpenClipboard(HWND::default()).is_ok() {
            let _ = EmptyClipboard();
            let _ = CloseClipboard();
        }
    }
    let seq_empty = unsafe { GetClipboardSequenceNumber() };

    // 3. 模拟发送 Ctrl + C
    unsafe {
        let inputs = [
            make_key_input(VK_CONTROL.0, false),
            make_key_input(VK_C.0, false),
            make_key_input(VK_C.0, true),
            make_key_input(VK_CONTROL.0, true),
        ];
        SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
    }

    // 4. 轮询等待目标程序响应复制操作（最大等待 50ms，每 5ms 检测一次剪贴板序列号变化）
    let mut copied = false;
    for _ in 0..10 {
        std::thread::sleep(std::time::Duration::from_millis(5));
        if unsafe { GetClipboardSequenceNumber() } != seq_empty {
            copied = true;
            break;
        }
    }

    // 5. 仅当目标应用确实写入了新剪贴板数据且不是代码编辑器的空选区整行复制时才读取
    let new_text = if copied {
        if is_empty_line_copy() {
            println!("[Selection] Ignored empty-selection line copy from editor.");
            None
        } else {
            read_clipboard_text().filter(|t| t.trim().len() >= 2)
        }
    } else {
        None
    };

    // 6. 恢复原本的剪贴板（避免污染用户的剪贴板历史）
    if let Some(ref old_text) = original {
        write_clipboard_text(old_text);
    }

    new_text
}

fn make_key_input(vk: u16, keyup: bool) -> windows::Win32::UI::Input::KeyboardAndMouse::INPUT {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, VIRTUAL_KEY,
    };
    let mut input = INPUT {
        r#type: INPUT_KEYBOARD,
        ..Default::default()
    };
    input.Anonymous.ki = KEYBDINPUT {
        wVk: VIRTUAL_KEY(vk),
        dwFlags: if keyup { KEYEVENTF_KEYUP } else { Default::default() },
        ..Default::default()
    };
    input
}

pub fn read_clipboard_text() -> Option<String> {
    use windows::Win32::Foundation::{HGLOBAL, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};

    unsafe {
        // CF_UNICODETEXT = 13
        if OpenClipboard(HWND::default()).is_err() {
            return None;
        }
        let res = if IsClipboardFormatAvailable(13).is_ok() {
            if let Ok(handle) = GetClipboardData(13) {
                let hglobal = HGLOBAL(handle.0 as *mut _);
                let ptr = GlobalLock(hglobal);
                if !ptr.is_null() {
                    let u16_ptr = ptr as *const u16;
                    let mut len = 0;
                    while *u16_ptr.add(len) != 0 {
                        len += 1;
                    }
                    let slice = std::slice::from_raw_parts(u16_ptr, len);
                    let text = String::from_utf16_lossy(slice);
                    let _ = GlobalUnlock(hglobal);
                    Some(text)
                } else {
                    None
                }
            } else {
                None
            }
        } else {
            None
        };
        let _ = CloseClipboard();
        res
    }
}

pub fn write_clipboard_text(text: &str) {
    use windows::Win32::Foundation::{HANDLE, HWND};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};

    unsafe {
        if OpenClipboard(HWND::default()).is_err() {
            return;
        }
        let _ = EmptyClipboard();
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
        if let Ok(h) = GlobalAlloc(GMEM_MOVEABLE, wide.len() * 2) {
            let ptr = GlobalLock(h) as *mut u16;
            if !ptr.is_null() {
                core::ptr::copy_nonoverlapping(wide.as_ptr(), ptr, wide.len());
                let _ = GlobalUnlock(h);
                let _ = SetClipboardData(13, HANDLE(h.0));
            }
        }
        let _ = CloseClipboard();
    }
}

/// 检查当前剪贴板中是否包含代码编辑器（如 VS Code、Visual Studio）在无选中文本时自动复制整行的特征标记
pub fn is_empty_line_copy() -> bool {
    use windows::Win32::System::DataExchange::{
        IsClipboardFormatAvailable, RegisterClipboardFormatW,
    };
    use windows::core::w;

    let vscode_tag = unsafe { RegisterClipboardFormatW(w!("VSCodeClipboardFormat")) };
    let msdev_tag = unsafe { RegisterClipboardFormatW(w!("MSDEVColumnSelect")) };
    let vs_tag = unsafe {
        RegisterClipboardFormatW(w!("VisualStudioEditorOperationsLineCutCopyClipboardTag"))
    };

    if vscode_tag != 0 && unsafe { IsClipboardFormatAvailable(vscode_tag) }.is_ok() {
        return true;
    }
    if msdev_tag != 0 && unsafe { IsClipboardFormatAvailable(msdev_tag) }.is_ok() {
        return true;
    }
    if vs_tag != 0 && unsafe { IsClipboardFormatAvailable(vs_tag) }.is_ok() {
        return true;
    }

    false
}
