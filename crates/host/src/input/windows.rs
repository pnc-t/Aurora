//! Windows SendInput による入力注入。

use anyhow::{Result, anyhow};
use aurora_common::proto::{InputEvent, MouseButton};
use windows::Win32::Foundation::POINT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT,
    KEYEVENTF_EXTENDEDKEY, KEYEVENTF_KEYUP, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN,
    MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP,
    MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

use super::{DisplayGeom, InputInjector};

pub struct WinInjector {
    /// キャプチャ対象ディスプレイのデスクトップ配置 (原点 + 寸法)。
    geom: DisplayGeom,
    /// 仮想デスクトップ境界 (左上原点 + 幅高)。MOUSEEVENTF_VIRTUALDESK の正規化基準。
    vx: i32,
    vy: i32,
    vw: i32,
    vh: i32,
}

impl WinInjector {
    pub fn new(geom: DisplayGeom) -> Self {
        let mut s = Self {
            geom,
            vx: 0,
            vy: 0,
            vw: 1,
            vh: 1,
        };
        s.refresh_virtual();
        s
    }

    /// 仮想デスクトップの境界を取得し直す。ディスプレイ構成変更時に呼ぶ。
    fn refresh_virtual(&mut self) {
        // SAFETY: GetSystemMetrics は副作用なしの値取得。
        unsafe {
            self.vx = GetSystemMetrics(SM_XVIRTUALSCREEN);
            self.vy = GetSystemMetrics(SM_YVIRTUALSCREEN);
            self.vw = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
            self.vh = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);
        }
    }

    fn send_one(&self, input: INPUT) -> Result<()> {
        let arr = [input];
        // SAFETY: arr は 1 要素の有効な INPUT 配列。サイズ引数は INPUT の実サイズ。
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
    fn set_display(&mut self, geom: DisplayGeom) {
        self.geom = geom;
        self.refresh_virtual();
    }

    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                // クライアントは選択ディスプレイ内の座標 (0..width, 0..height) を送る。
                // まずデスクトップ仮想座標へ平行移動し、仮想デスクトップ全域を 0..65535 に
                // 正規化して MOUSEEVENTF_VIRTUALDESK で注入する (非プライマリでも正しく届く)。
                let w = self.geom.width.max(1) as i32;
                let h = self.geom.height.max(1) as i32;
                let abs_x = self.geom.x + x.clamp(0, w - 1);
                let abs_y = self.geom.y + y.clamp(0, h - 1);
                let cx = ((abs_x - self.vx) as i64 * 65535 / (self.vw as i64 - 1).max(1)) as i32;
                let cy = ((abs_y - self.vy) as i64 * 65535 / (self.vh as i64 - 1).max(1)) as i32;
                let mi = MOUSEINPUT {
                    dx: cx,
                    dy: cy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
                    time: 0,
                    dwExtraInfo: 0,
                };
                self.send_one(INPUT {
                    r#type: INPUT_MOUSE,
                    Anonymous: INPUT_0 { mi },
                })
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                // 相対移動も「現在位置 + 差分」の絶対座標に直してから注入する。
                // MOUSEEVENTF_MOVE 単体 (ABSOLUTE 無し) だと OS のポインタ速度と「ポインターの
                // 精度を高める」(既定 ON の加速) が掛かり、送った量と実際の移動量が一致しない。
                // クライアントは自前で描くカーソルを指の移動量ぶん動かすため、加速が入ると
                // 動かすほど両者がずれていく (モバイルの相対追従で実測。速度依存の係数なので
                // クライアント側の補正では合わせられない)。絶対座標なら OS の加工を受けない。
                let mut pt = POINT::default();
                // SAFETY: 出力先は自前のスタック変数。失敗時は Err が返るだけで副作用は無い。
                unsafe { GetCursorPos(&mut pt) }?;
                // dx/dy はクライアント任意の i32。加算のあふれは画面端へのクランプで吸収する。
                let abs_x =
                    pt.x.saturating_add(dx)
                        .clamp(self.vx, self.vx + self.vw - 1);
                let abs_y =
                    pt.y.saturating_add(dy)
                        .clamp(self.vy, self.vy + self.vh - 1);
                let cx = ((abs_x - self.vx) as i64 * 65535 / (self.vw as i64 - 1).max(1)) as i32;
                let cy = ((abs_y - self.vy) as i64 * 65535 / (self.vh as i64 - 1).max(1)) as i32;
                let mi = MOUSEINPUT {
                    dx: cx,
                    dy: cy,
                    mouseData: 0,
                    dwFlags: MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK,
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
