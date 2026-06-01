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
