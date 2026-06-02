//! Linux uinput (evdev) による入力注入。
//!
//! 仮想入力デバイスを 1 つ作り、絶対座標マウス + キーボード + ホイールを再生する。
//! プロトコルの `vk` は Windows 仮想キーコードなので Linux の `KEY_*` コードへ変換する。
//!
//! 前提: `/dev/uinput` への書き込み権限が必要 (root もしくは uinput グループ)。
//! 権限がない場合は `new` が失敗するが、入力スレッドのみが停止し映像は継続する。

use anyhow::{Context, Result, anyhow};
use ddd_common::proto::{InputEvent as ProtoInput, MouseButton};
use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{
    AbsInfo, AbsoluteAxisType, AttributeSet, EventType, InputEvent, Key, RelativeAxisType,
    UinputAbsSetup,
};

// マウスボタン (Linux input-event-codes)
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

pub struct UinputInjector {
    device: VirtualDevice,
    width: i32,
    height: i32,
}

impl UinputInjector {
    pub fn new(geom: super::DisplayGeom) -> Self {
        match Self::try_new(geom.width, geom.height) {
            Ok(injector) => injector,
            Err(e) => {
                // PlatformInjector::new はインフォリブルな契約 (Windows 実装に合わせる)。
                // uinput が開けない環境では入力スレッドを諦めるしかないため panic させ、
                // 呼び出し側 (input_loop) のスレッドだけが落ちるようにする。
                panic!("uinput init failed: {e}");
            }
        }
    }

    fn try_new(display_width: u32, display_height: u32) -> Result<Self> {
        let width = display_width.max(1) as i32;
        let height = display_height.max(1) as i32;

        // キー: 0..=248 の主要コード + マウスボタンを全て有効化する。
        let mut keys = AttributeSet::<Key>::new();
        for code in 1u16..=248 {
            keys.insert(Key(code));
        }
        keys.insert(Key(BTN_LEFT));
        keys.insert(Key(BTN_RIGHT));
        keys.insert(Key(BTN_MIDDLE));

        // 相対軸: ホイール (縦/横)。
        let mut rel = AttributeSet::<RelativeAxisType>::new();
        rel.insert(RelativeAxisType::REL_WHEEL);
        rel.insert(RelativeAxisType::REL_HWHEEL);

        let abs_x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_X,
            AbsInfo::new(0, 0, width - 1, 0, 0, 0),
        );
        let abs_y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_Y,
            AbsInfo::new(0, 0, height - 1, 0, 0, 0),
        );

        let device = VirtualDeviceBuilder::new()
            .context("open /dev/uinput")?
            .name("ddd-virtual-input")
            .with_keys(&keys)
            .context("with_keys")?
            .with_relative_axes(&rel)
            .context("with_relative_axes")?
            .with_absolute_axis(&abs_x)
            .context("with_absolute_axis(X)")?
            .with_absolute_axis(&abs_y)
            .context("with_absolute_axis(Y)")?
            .build()
            .context("build uinput device")?;

        Ok(Self {
            device,
            width,
            height,
        })
    }
}

impl super::InputInjector for UinputInjector {
    fn set_display(&mut self, geom: super::DisplayGeom) {
        // uinput の絶対軸レンジは生成時に固定のため寸法変更は完全には反映できない。
        // Linux は単一ディスプレイ前提 (enumerate が 1 枚) なので clamp 値のみ更新する。
        self.width = (geom.width.max(1)) as i32;
        self.height = (geom.height.max(1)) as i32;
    }

    fn inject(&mut self, ev: &ProtoInput) -> Result<()> {
        match *ev {
            ProtoInput::MouseMove { x, y } => {
                let cx = x.clamp(0, self.width - 1);
                let cy = y.clamp(0, self.height - 1);
                self.device
                    .emit(&[
                        InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_X.0, cx),
                        InputEvent::new(EventType::ABSOLUTE, AbsoluteAxisType::ABS_Y.0, cy),
                    ])
                    .context("emit mouse move")
            }
            ProtoInput::MouseButton { button, down } => {
                let code = match button {
                    MouseButton::Left => BTN_LEFT,
                    MouseButton::Right => BTN_RIGHT,
                    MouseButton::Middle => BTN_MIDDLE,
                };
                let val = if down { 1 } else { 0 };
                self.device
                    .emit(&[InputEvent::new(EventType::KEY, code, val)])
                    .context("emit mouse button")
            }
            ProtoInput::MouseWheel { delta, horizontal } => {
                // WHEEL_DELTA(120) 単位 → ノッチ数。最低 1 ノッチは動かす。
                let mut notches = delta / 120;
                if notches == 0 {
                    notches = if delta > 0 { 1 } else { -1 };
                }
                let axis = if horizontal {
                    RelativeAxisType::REL_HWHEEL
                } else {
                    RelativeAxisType::REL_WHEEL
                };
                self.device
                    .emit(&[InputEvent::new(EventType::RELATIVE, axis.0, notches)])
                    .context("emit wheel")
            }
            ProtoInput::Key { vk, down } => {
                let code =
                    map_vk_to_linux(vk).ok_or_else(|| anyhow!("unmapped VK on Linux: {vk:#x}"))?;
                let val = if down { 1 } else { 0 };
                self.device
                    .emit(&[InputEvent::new(EventType::KEY, code, val)])
                    .context("emit key")
            }
        }
    }
}

/// Windows 仮想キーコード → Linux input-event `KEY_*` コード。
fn map_vk_to_linux(vk: u32) -> Option<u16> {
    let code: u16 = match vk {
        // 英字 A-Z
        0x41 => 30, // A
        0x42 => 48, // B
        0x43 => 46, // C
        0x44 => 32, // D
        0x45 => 18, // E
        0x46 => 33, // F
        0x47 => 34, // G
        0x48 => 35, // H
        0x49 => 23, // I
        0x4A => 36, // J
        0x4B => 37, // K
        0x4C => 38, // L
        0x4D => 50, // M
        0x4E => 49, // N
        0x4F => 24, // O
        0x50 => 25, // P
        0x51 => 16, // Q
        0x52 => 19, // R
        0x53 => 31, // S
        0x54 => 20, // T
        0x55 => 22, // U
        0x56 => 47, // V
        0x57 => 17, // W
        0x58 => 45, // X
        0x59 => 21, // Y
        0x5A => 44, // Z
        // 数字 0-9
        0x30 => 11,
        0x31 => 2,
        0x32 => 3,
        0x33 => 4,
        0x34 => 5,
        0x35 => 6,
        0x36 => 7,
        0x37 => 8,
        0x38 => 9,
        0x39 => 10,
        // テンキー 0-9
        0x60 => 82,
        0x61 => 79,
        0x62 => 80,
        0x63 => 81,
        0x64 => 75,
        0x65 => 76,
        0x66 => 77,
        0x67 => 71,
        0x68 => 72,
        0x69 => 73,
        0x6A => 55, // KP *
        0x6B => 78, // KP +
        0x6D => 74, // KP -
        0x6E => 83, // KP .
        0x6F => 98, // KP /
        // ファンクション F1-F12
        0x70 => 59,
        0x71 => 60,
        0x72 => 61,
        0x73 => 62,
        0x74 => 63,
        0x75 => 64,
        0x76 => 65,
        0x77 => 66,
        0x78 => 67,
        0x79 => 68,
        0x7A => 87,
        0x7B => 88,
        // 制御・記号キー
        0x08 => 14,  // Backspace
        0x09 => 15,  // Tab
        0x0D => 28,  // Enter
        0x14 => 58,  // CapsLock
        0x1B => 1,   // Escape
        0x20 => 57,  // Space
        0x21 => 104, // PageUp
        0x22 => 109, // PageDown
        0x23 => 107, // End
        0x24 => 102, // Home
        0x25 => 105, // Left
        0x26 => 103, // Up
        0x27 => 106, // Right
        0x28 => 108, // Down
        0x2D => 110, // Insert
        0x2E => 111, // Delete
        0xA0 => 42,  // LShift
        0xA1 => 54,  // RShift
        0xA2 => 29,  // LCtrl
        0xA3 => 97,  // RCtrl
        0xA4 => 56,  // LAlt
        0xA5 => 100, // RAlt
        0x5B => 125, // LMeta
        0x5C => 126, // RMeta
        0xBA => 39,  // ;
        0xBB => 13,  // =
        0xBC => 51,  // ,
        0xBD => 12,  // -
        0xBE => 52,  // .
        0xBF => 53,  // /
        0xC0 => 41,  // `
        0xDB => 26,  // [
        0xDC => 43,  // backslash
        0xDD => 27,  // ]
        0xDE => 40,  // '
        _ => return None,
    };
    Some(code)
}
