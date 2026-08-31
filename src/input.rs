use anyhow::Result;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::OnceLock;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, PostThreadMessageW, SetWindowsHookExW,
    UnhookWindowsHookEx, MSG, MSLLHOOKSTRUCT, WH_MOUSE_LL,
};

use crate::AppEvent;

static HOOK_SENDER: OnceLock<Sender<AppEvent>> = OnceLock::new();
/// Thread id of the hook thread — used to wake it up so it can exit
static HOOK_THREAD_ID: OnceLock<u32> = OnceLock::new();
/// 记录鼠标按下时的坐标 (x, y)
static DOWN_POS: AtomicI64 = AtomicI64::new(0);

pub struct MouseHook {
    // The hook is installed on a background thread; we unhook via the thread id
}

impl MouseHook {
    /// Install the low-level mouse hook on a **new thread** that also runs the
    /// message pump.  `WH_MOUSE_LL` callbacks fire on the thread that installed
    /// the hook, so both must be the same thread.
    pub fn start(tx: Sender<AppEvent>) -> Result<Self> {
        HOOK_SENDER
            .set(tx)
            .map_err(|_| anyhow::anyhow!("Hook channel already set"))?;

        // Signal channel: hook thread tells us when it's installed
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();

        std::thread::spawn(move || {
            unsafe {
                let hook = match SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_hook_proc), None, 0) {
                    Ok(h) => h,
                    Err(e) => {
                        eprintln!("[Hook] SetWindowsHookExW failed: {e}");
                        let _ = ready_tx.send(());
                        return;
                    }
                };

                let tid = windows::Win32::System::Threading::GetCurrentThreadId();
                let _ = HOOK_THREAD_ID.set(tid);

                // Signal that the hook is ready
                let _ = ready_tx.send(());

                println!("[Hook] Mouse hook installed, pumping messages...");

                let mut msg = MSG::default();
                while GetMessageW(&mut msg, None, 0, 0).into() {}

                let _ = UnhookWindowsHookEx(hook);
                println!("[Hook] Message pump exited, hook removed.");
            }
        });

        // Wait for the hook to be installed
        ready_rx.recv()?;

        Ok(Self {})
    }
}

impl Drop for MouseHook {
    fn drop(&mut self) {
        // Wake the hook thread so GetMessageW returns and the thread exits
        if let Some(&tid) = HOOK_THREAD_ID.get() {
            unsafe {
                let _ = PostThreadMessageW(tid, 0x0012 /* WM_QUIT */, WPARAM(0), LPARAM(0));
            }
        }
    }
}

unsafe extern "system" fn mouse_hook_proc(
    n_code: i32,
    w_param: WPARAM,
    l_param: LPARAM,
) -> LRESULT {
    if n_code >= 0 {
        let event = w_param.0 as u32;
        let ms = &*(l_param.0 as *const MSLLHOOKSTRUCT);

        // WM_LBUTTONDOWN = 0x0201
        if event == 0x0201 {
            let packed = ((ms.pt.x as u32 as u64) << 32) | (ms.pt.y as u32 as u64);
            DOWN_POS.store(packed as i64, Ordering::Relaxed);
        }

        // WM_LBUTTONUP = 0x0202
        if event == 0x0202 {
            let packed = DOWN_POS.load(Ordering::Relaxed) as u64;
            let down_x = (packed >> 32) as u32 as i32;
            let down_y = (packed & 0xFFFF_FFFF) as u32 as i32;
            let dx = (ms.pt.x - down_x).abs();
            let dy = (ms.pt.y - down_y).abs();
            let dragged = dx > 7 || dy > 7;

            if let Some(tx) = HOOK_SENDER.get() {
                let _ = tx.send(AppEvent::MouseRelease {
                    down_x,
                    down_y,
                    x: ms.pt.x,
                    y: ms.pt.y,
                    dragged,
                });
            }
        }
    }
    CallNextHookEx(None, n_code, w_param, l_param)
}
