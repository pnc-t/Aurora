//! macOS CGEvent による入力注入。
//!
//! プロトコルの `InputEvent::Key { vk }` は Windows 仮想キーコードなので、
//! macOS の仮想キーコード (Carbon `kVK_*`) へ変換してから注入する。
//! クライアント (`input.ts`) が送る VK 集合のみを対象とする。

use anyhow::{Result, anyhow};
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGMouseButton, ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;
use ddd_common::proto::{InputEvent, MouseButton};

pub struct CgEventInjector {
    source: CGEventSource,
    width: f64,
    height: f64,
    /// 直近のカーソル位置。ボタン/ホイールイベントは座標を要求するため保持する。
    last_x: f64,
    last_y: f64,
    /// 押下中の左ボタン状態 (ドラッグ判定用)。
    left_down: bool,
}

impl CgEventInjector {
    pub fn new(geom: super::DisplayGeom) -> Self {
        // HIDSystemState: 物理デバイス相当のイベント源。修飾キーの状態も追跡される。
        let source =
            CGEventSource::new(CGEventSourceStateID::HIDSystemState).expect("create CGEventSource");
        Self {
            source,
            width: (geom.width.max(1)) as f64,
            height: (geom.height.max(1)) as f64,
            last_x: 0.0,
            last_y: 0.0,
            left_down: false,
        }
    }

    fn post_mouse(&self, ty: CGEventType, button: CGMouseButton) -> Result<()> {
        let point = CGPoint::new(self.last_x, self.last_y);
        let ev = CGEvent::new_mouse_event(self.source.clone(), ty, point, button)
            .map_err(|_| anyhow!("CGEvent::new_mouse_event failed"))?;
        ev.post(CGEventTapLocation::HID);
        Ok(())
    }
}

impl super::InputInjector for CgEventInjector {
    fn set_display(&mut self, geom: super::DisplayGeom) {
        // macOS は MVP では primary 固定 (原点 0,0)。寸法のみ更新する。
        self.width = (geom.width.max(1)) as f64;
        self.height = (geom.height.max(1)) as f64;
    }

    fn inject(&mut self, ev: &InputEvent) -> Result<()> {
        match *ev {
            InputEvent::MouseMove { x, y } => {
                self.last_x = (x as f64).clamp(0.0, self.width - 1.0);
                self.last_y = (y as f64).clamp(0.0, self.height - 1.0);
                // ドラッグ中は LeftMouseDragged を送らないと多くのアプリが追従しない。
                let ty = if self.left_down {
                    CGEventType::LeftMouseDragged
                } else {
                    CGEventType::MouseMoved
                };
                self.post_mouse(ty, CGMouseButton::Left)
            }
            InputEvent::MouseMoveRelative { dx, dy } => {
                // ポインタロック中の相対移動。CGEvent は絶対座標モデルのため、保持中の
                // last_x/last_y に加算してクランプし MouseMoved(押下中は Dragged) を post。
                // (真の無限キャプチャにはならない best-effort。mac は未検証扱い)
                self.last_x = (self.last_x + dx as f64).clamp(0.0, self.width - 1.0);
                self.last_y = (self.last_y + dy as f64).clamp(0.0, self.height - 1.0);
                let ty = if self.left_down {
                    CGEventType::LeftMouseDragged
                } else {
                    CGEventType::MouseMoved
                };
                self.post_mouse(ty, CGMouseButton::Left)
            }
            InputEvent::MouseButton { button, down } => {
                let (ty, cg_btn) = match (button, down) {
                    (MouseButton::Left, true) => {
                        self.left_down = true;
                        (CGEventType::LeftMouseDown, CGMouseButton::Left)
                    }
                    (MouseButton::Left, false) => {
                        self.left_down = false;
                        (CGEventType::LeftMouseUp, CGMouseButton::Left)
                    }
                    (MouseButton::Right, true) => {
                        (CGEventType::RightMouseDown, CGMouseButton::Right)
                    }
                    (MouseButton::Right, false) => {
                        (CGEventType::RightMouseUp, CGMouseButton::Right)
                    }
                    (MouseButton::Middle, true) => {
                        (CGEventType::OtherMouseDown, CGMouseButton::Center)
                    }
                    (MouseButton::Middle, false) => {
                        (CGEventType::OtherMouseUp, CGMouseButton::Center)
                    }
                };
                self.post_mouse(ty, cg_btn)
            }
            InputEvent::MouseWheel { delta, horizontal } => {
                // WHEEL_DELTA(120) 単位 → 行数。最低 1 行は動かす。
                let lines = (delta / 120).clamp(-127, 127);
                let lines = if lines == 0 {
                    if delta > 0 { 1 } else { -1 }
                } else {
                    lines
                };
                let (wheel1, wheel2) = if horizontal { (0, lines) } else { (lines, 0) };
                let ev = CGEvent::new_scroll_event(
                    self.source.clone(),
                    ScrollEventUnit::LINE,
                    2,
                    wheel1,
                    wheel2,
                    0,
                )
                .map_err(|_| anyhow!("CGEvent::new_scroll_event failed"))?;
                ev.post(CGEventTapLocation::HID);
                Ok(())
            }
            InputEvent::Key { vk, down } => {
                let keycode =
                    map_vk_to_macos(vk).ok_or_else(|| anyhow!("unmapped VK on macOS: {vk:#x}"))?;
                let ev = CGEvent::new_keyboard_event(self.source.clone(), keycode, down)
                    .map_err(|_| anyhow!("CGEvent::new_keyboard_event failed"))?;
                ev.post(CGEventTapLocation::HID);
                Ok(())
            }
        }
    }
}

/// Windows 仮想キーコード → macOS 仮想キーコード (Carbon `kVK_*` / ANSI レイアウト)。
fn map_vk_to_macos(vk: u32) -> Option<u16> {
    let code: u16 = match vk {
        // 英字 A-Z (VK 0x41-0x5A)
        0x41 => 0,  // A
        0x42 => 11, // B
        0x43 => 8,  // C
        0x44 => 2,  // D
        0x45 => 14, // E
        0x46 => 3,  // F
        0x47 => 5,  // G
        0x48 => 4,  // H
        0x49 => 34, // I
        0x4A => 38, // J
        0x4B => 40, // K
        0x4C => 37, // L
        0x4D => 46, // M
        0x4E => 45, // N
        0x4F => 31, // O
        0x50 => 35, // P
        0x51 => 12, // Q
        0x52 => 15, // R
        0x53 => 1,  // S
        0x54 => 17, // T
        0x55 => 32, // U
        0x56 => 9,  // V
        0x57 => 13, // W
        0x58 => 7,  // X
        0x59 => 16, // Y
        0x5A => 6,  // Z
        // 数字 0-9 (VK 0x30-0x39)
        0x30 => 29,
        0x31 => 18,
        0x32 => 19,
        0x33 => 20,
        0x34 => 21,
        0x35 => 23,
        0x36 => 22,
        0x37 => 26,
        0x38 => 28,
        0x39 => 25,
        // テンキー 0-9 (VK 0x60-0x69)
        0x60 => 82,
        0x61 => 83,
        0x62 => 84,
        0x63 => 85,
        0x64 => 86,
        0x65 => 87,
        0x66 => 88,
        0x67 => 89,
        0x68 => 91,
        0x69 => 92,
        0x6A => 67, // Numpad *
        0x6B => 69, // Numpad +
        0x6D => 78, // Numpad -
        0x6E => 65, // Numpad .
        0x6F => 75, // Numpad /
        // ファンクション F1-F12 (VK 0x70-0x7B)
        0x70 => 122,
        0x71 => 120,
        0x72 => 99,
        0x73 => 118,
        0x74 => 96,
        0x75 => 97,
        0x76 => 98,
        0x77 => 100,
        0x78 => 101,
        0x79 => 109,
        0x7A => 103,
        0x7B => 111,
        // 制御・記号キー
        0x08 => 51,  // Backspace
        0x09 => 48,  // Tab
        0x0D => 36,  // Enter
        0x14 => 57,  // CapsLock
        0x1B => 53,  // Escape
        0x20 => 49,  // Space
        0x21 => 116, // PageUp
        0x22 => 121, // PageDown
        0x23 => 119, // End
        0x24 => 115, // Home
        0x25 => 123, // Left
        0x26 => 126, // Up
        0x27 => 124, // Right
        0x28 => 125, // Down
        0x2E => 117, // Delete (forward)
        0xA0 => 56,  // LShift
        0xA1 => 60,  // RShift
        0xA2 => 59,  // LCtrl
        0xA3 => 62,  // RCtrl
        0xA4 => 58,  // LAlt (Option)
        0xA5 => 61,  // RAlt (Option)
        0x5B => 55,  // LMeta (Command)
        0x5C => 54,  // RMeta (Command)
        0xBA => 41,  // ;
        0xBB => 24,  // =
        0xBC => 43,  // ,
        0xBD => 27,  // -
        0xBE => 47,  // .
        0xBF => 44,  // /
        0xC0 => 50,  // `
        0xDB => 33,  // [
        0xDC => 42,  // backslash
        0xDD => 30,  // ]
        0xDE => 39,  // '
        _ => return None,
    };
    Some(code)
}
