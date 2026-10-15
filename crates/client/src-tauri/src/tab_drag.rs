//! タブのウィンドウ間ドラッグ (分割/合流) の追跡とゴースト表示。
//!
//! DOM の pointer イベントでは、ポインタが窓の外に出るとドラッグ中のタブを描けず (WebView は自窓の
//! 外に描画できない)、合流先が分かりにくい。またタブ 1 枚の窓は自分自身が合流先に重なって見えない
//! が、隠すと pointerup が届かなくなる。そこでドラッグ開始以降の追跡を Rust 側へ移し、カーソル位置と
//! ボタン状態をポーリングして小さなゴースト窓 (タブ名表示) を追従させる。離した座標は元窓へ
//! `aurora://tab-drag-drop` で返し、合流/分割の確定は既存の JS (`splitTab`) が行う。
//!
//! Windows 専用 (ボタン状態の取得に GetAsyncKeyState を使う)。他 OS はエラーを返し、呼び出し側は
//! 従来の DOM ドラッグにフォールバックする。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use tauri::{Emitter, PhysicalPosition, WebviewUrl, WebviewWindow, WebviewWindowBuilder};

/// ゴースト窓のラベル。capture-* ではないので capability (IPC 権限) を一切持たない。
const GHOST_LABEL: &str = "tab-ghost";
/// カーソルとゴーストのずれ。カーソルの真下に置くとゴースト自身がドロップ先の判定や表示を隠す。
const GHOST_OFFSET: i32 = 14;
const POLL: Duration = Duration::from_millis(16);
/// ボタンの解放を取りこぼした場合 (フォーカス喪失等) に追跡を打ち切る上限。
const MAX_DRAG: Duration = Duration::from_secs(60);

/// 同時に 1 本だけ追跡する (ゴースト窓のラベルも 1 つ)。
static ACTIVE: AtomicBool = AtomicBool::new(false);

#[derive(Clone, serde::Serialize)]
struct DropPayload {
    session_id: String,
    /// ドロップ点 (論理 px)。JS の `window.screenX + clientX` と同じ座標系。
    x: f64,
    y: f64,
}

#[derive(Clone, serde::Serialize)]
struct CancelPayload {
    session_id: String,
}

/// タブのドラッグ追跡を開始する。`hide_source` はタブ 1 枚の窓で、ドラッグ中は元窓を隠して
/// 合流先を見えるようにする (キャンセル時は Rust が再表示、確定時は JS 側が後始末する)。
///
/// 必ず async にする: 同期コマンドはメインスレッドで実行され、その中で窓を生成すると Windows では
/// 生成がメインスレッドを待ってデッドロックする (実機で client が固まった)。
#[tauri::command]
pub async fn begin_tab_drag(
    app: tauri::AppHandle,
    window: WebviewWindow,
    session_id: String,
    label: String,
    hide_source: bool,
) -> Result<(), String> {
    #[cfg(not(target_os = "windows"))]
    {
        let _ = (app, window, session_id, label, hide_source);
        Err("tab drag tracking is not supported on this OS".into())
    }
    #[cfg(target_os = "windows")]
    {
        if ACTIVE.swap(true, Ordering::AcqRel) {
            return Err("another tab drag is in progress".into());
        }
        let start = app.cursor_position().map_err(|e| {
            ACTIVE.store(false, Ordering::Release);
            format!("cursor position: {e}")
        })?;
        let url = format!("tab-ghost.html?label={}", crate::net::q(&label));
        let ghost = WebviewWindowBuilder::new(&app, GHOST_LABEL, WebviewUrl::App(url.into()))
            .title("Aurora")
            .inner_size(220.0, 34.0)
            .decorations(false)
            .resizable(false)
            .always_on_top(true)
            .skip_taskbar(true)
            .focused(false)
            .shadow(false)
            .position(
                start.x + f64::from(GHOST_OFFSET),
                start.y + f64::from(GHOST_OFFSET),
            )
            .build()
            .map_err(|e| {
                ACTIVE.store(false, Ordering::Release);
                format!("ghost window: {e}")
            })?;
        // builder の position は論理 px 扱いのため、追従は物理 px で上書きする (混在 DPI 対策)。
        let _ = ghost.set_position(PhysicalPosition::new(
            start.x as i32 + GHOST_OFFSET,
            start.y as i32 + GHOST_OFFSET,
        ));
        if hide_source {
            let _ = window.hide();
        }
        std::thread::Builder::new()
            .name("tab-drag".into())
            .spawn(move || track(app, window, ghost, session_id, hide_source))
            .map_err(|e| {
                ACTIVE.store(false, Ordering::Release);
                format!("spawn tab drag thread: {e}")
            })?;
        Ok(())
    }
}

#[cfg(target_os = "windows")]
fn track(
    app: tauri::AppHandle,
    source: WebviewWindow,
    ghost: WebviewWindow,
    session_id: String,
    hide_source: bool,
) {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE, VK_LBUTTON};

    // SAFETY: 引数はキーコードのみで、戻り値の最上位ビットで現在の押下状態を読むだけ。
    let pressed = |vk: u16| unsafe { GetAsyncKeyState(i32::from(vk)) } < 0;
    let started = Instant::now();
    let mut last = None;
    let dropped = loop {
        if pressed(VK_ESCAPE.0) || started.elapsed() > MAX_DRAG {
            break None;
        }
        let Ok(pos) = app.cursor_position() else {
            break None;
        };
        if last != Some((pos.x as i32, pos.y as i32)) {
            last = Some((pos.x as i32, pos.y as i32));
            let _ = ghost.set_position(PhysicalPosition::new(
                pos.x as i32 + GHOST_OFFSET,
                pos.y as i32 + GHOST_OFFSET,
            ));
        }
        if !pressed(VK_LBUTTON.0) {
            break Some(pos);
        }
        std::thread::sleep(POLL);
    };
    let _ = ghost.destroy();

    let drop_point = dropped.and_then(|pos| {
        let scale = source.scale_factor().ok()?;
        let logical = pos.to_logical::<f64>(scale);
        // 元窓が見えている (= タブ 2 枚以上) ときに自窓の上で離したのは取り消し (従来どおり)。
        if !hide_source && within(&source, scale, logical.x, logical.y) {
            return None;
        }
        Some((logical.x, logical.y))
    });
    match drop_point {
        Some((x, y)) => {
            let _ = source.emit_to(
                source.label(),
                "aurora://tab-drag-drop",
                DropPayload { session_id, x, y },
            );
        }
        None => {
            if hide_source {
                let _ = source.show();
            }
            let _ = source.emit_to(
                source.label(),
                "aurora://tab-drag-cancel",
                CancelPayload { session_id },
            );
        }
    }
    ACTIVE.store(false, Ordering::Release);
}

#[cfg(target_os = "windows")]
fn within(w: &WebviewWindow, scale: f64, x: f64, y: f64) -> bool {
    let (Ok(pos), Ok(size)) = (w.outer_position(), w.outer_size()) else {
        return false;
    };
    let pos = pos.to_logical::<f64>(scale);
    let size = size.to_logical::<f64>(scale);
    x >= pos.x && x < pos.x + size.width && y >= pos.y && y < pos.y + size.height
}
