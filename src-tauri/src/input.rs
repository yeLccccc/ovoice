// 全局按键状态读取（仅 Windows）。
// 用 GetAsyncKeyState 轮询，而非 SetWindowsHookEx / rdev——
// 因为 Tauri/tao 自带一个 WH_KEYBOARD_LL 钩子，窗口聚焦时会抢先吞键，
// 导致 rdev 失灵（tauri#14770）。GetAsyncKeyState 读的是 OS 维护的全局键状态，
// 走另一条通道，不受 tao 钩子影响，聚焦/失焦/最小化都稳。
#[cfg(target_os = "windows")]
mod imp {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetAsyncKeyState;

    // VK_LCONTROL = 0xA2，VK_RCONTROL = 0xA3。
    const VK_LCONTROL: i32 = 0xA2;
    const VK_RCONTROL: i32 = 0xA3;

    /// 当前是否按下了 Ctrl（左或右）。
    pub fn ctrl_down() -> bool {
        unsafe {
            // 返回值最高位（0x8000）置 1 表示该键当前处于按下状态。
            is_down(VK_LCONTROL) || is_down(VK_RCONTROL)
        }
    }

    #[inline]
    unsafe fn is_down(vk: i32) -> bool {
        (GetAsyncKeyState(vk) as u16) & 0x8000 != 0
    }
}

#[cfg(not(target_os = "windows"))]
mod imp {
    pub fn ctrl_down() -> bool {
        false
    }
}

pub fn ctrl_down() -> bool {
    imp::ctrl_down()
}
