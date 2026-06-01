//! Windows SendInput による入力注入。

use anyhow::{Result, anyhow};
use ddd_common::proto::{InputEvent, MouseButton};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};

use super::InputInjector;

pub struct WinInjector {
    width: i32,
    height: i32,
}

impl WinInjector {
    pub fn new(display_width: u32, display_height: u32) -> Self {
        Self {
            width: display_width.max(1) as i32,
            height: display_height.max(1) as i32,
        }
    }

    fn send_one(&self, input: INPUT) -> Result<()> {
        let arr = [input];
        let n = unsafe { SendInput(&arr, std::mem::size_of::<INPUT>() as i32) };
        if n != 1 {
            return Err(anyhow!("SendInput returned {n}"));
        }
        Ok(())
    }
}

/// 拡張キー扱いが必要な VK_*。0xE0 プレフィックスを伴うスキャンを生成させる。
fn is_extended_vk(vk: u32) -> bool {
    matches!(
        vk,
        0x21 | 0x22 // PageUp/Down
        | 0x23 | 0x24 // End / Home
        | 0x25 | 0x26 | 0x27 | 0x28 // 矢印
        | 0x2D | 0x2E // Insert / Delete
        | 0x5B | 0x5C // LWin / RWin
        | 0xA3 | 0xA5 // RCtrl / RAlt
        | 0x6F // NumpadDivide
    )
}

impl InputInjector for WinInjector {
    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                // MOUSEEVENTF_ABSOLUTE はプライマリモニタ全域を 0..65535 に正規化する。
                // ホストが primary ディスプレイ全体をキャプチャしている前提でスケール。
                let max_x = (self.width as i64 - 1).max(1);
                let max_y = (self.height as i64 - 1).max(1);
                let cx = (x.clamp(0, self.width - 1) as i64 * 65535 / max_x) as i32;
                let cy = (y.clamp(0, self.height - 1) as i64 * 65535 / max_y) as i32;
                let mi = MOUSEINPUT {
                    dx: cx,
                    dy: cy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send_one(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi },
                })
            }
            InputEvent::MouseButton { button, down } => {
                let flag: MOUSE_EVENT_FLAGS = match (button, down) {
                    (MouseButton::Left, true) => MOUSEEVENTF_LEFTDOWN,
                    (MouseButton::Left, false) => MOUSEEVENTF_LEFTUP,
                    (MouseButton::Right, true) => MOUSEEVENTF_RIGHTDOWN,
                    (MouseButton::Right, false) => MOUSEEVENTF_RIGHTUP,
                    (MouseButton::Middle, true) => MOUSEEVENTF_MIDDLEDOWN,
                    (MouseButton::Middle, false) => MOUSEEVENTF_MIDDLEUP,
                };
                let mi = MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: 0,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send_one(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi },
                })
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                let flag = if horizontal {
                    MOUSEEVENTF_HWHEEL
                } else {
                    MOUSEEVENTF_WHEEL
                };
                // mouseData は WHEEL_DELTA (120) 単位の符号付き整数。
                let mi = MOUSEINPUT {
                    dx: 0,
                    dy: 0,
                    mouseData: delta as u32,
                    dwFlags: flag,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send_one(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi },
                })
            }
            InputEvent::Key { vk, down } => {
                if vk > 0xFF {
                    return Err(anyhow!("invalid VK: {vk}"));
                }
                let mut flags = KEYBD_EVENT_FLAGS(0);
                if !down {
                    flags |= KEYEVENTF_KEYUP;
                }
                if is_extended_vk(vk) {
                    flags |= KEYEVENTF_EXTENDEDKEY;
                }
                let ki = KEYBDINPUT {
                    wVk: VIRTUAL_KEY(vk as u16),
                    wScan: 0,
                    dwFlags: flags,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send_one(INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 { ki },
                })
            }
        }
    }
}
