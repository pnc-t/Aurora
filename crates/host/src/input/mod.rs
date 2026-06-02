//! 入力注入の抽象 (Port)。OS別の Adapter は cfg で差し替える。

use anyhow::Result;
use ddd_common::proto::InputEvent;

/// 入力注入対象ディスプレイのデスクトップ上の配置 (原点 + 寸法)。
/// マルチディスプレイ環境で非プライマリを選んだときの座標マッピングに使う。
#[derive(Debug, Clone, Copy)]
pub struct DisplayGeom {
    pub x: i32,
    pub y: i32,
    pub width: u32,
    pub height: u32,
}

pub trait InputInjector: Send {
    fn inject(&mut self, ev: &InputEvent) -> Result<()>;
    /// キャプチャ対象ディスプレイが切り替わったときに座標系を更新する。
    fn set_display(&mut self, geom: DisplayGeom);
}

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WinInjector as PlatformInjector;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::UinputInjector as PlatformInjector;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::CgEventInjector as PlatformInjector;
