//! 入力注入の抽象 (Port)。OS別の Adapter は cfg で差し替える。

use anyhow::Result;
use ddd_common::proto::InputEvent;

pub trait InputInjector: Send {
    fn inject(&mut self, ev: &InputEvent) -> Result<()>;
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
