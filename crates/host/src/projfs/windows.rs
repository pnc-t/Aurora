//! ProjFS の Windows 実装。
//!
//! `aurora-remote/` をプレースホルダ化して `PrjStartVirtualizing` を呼び、列挙 / メタ情報 /
//! ファイル内容のコールバックを [`FsBridge`] 経由でクライアントへ転送する (読み取り方向)。

use std::collections::HashMap;
use std::ffi::c_void;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};
use windows::Win32::Foundation::{
    BOOLEAN, E_OUTOFMEMORY, ERROR_FILE_NOT_FOUND, ERROR_IO_DEVICE, FreeLibrary, S_OK,
};
use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
use windows::Win32::Storage::ProjectedFileSystem::{
    PRJ_CALLBACK_DATA, PRJ_CALLBACKS, PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN,
    PRJ_DIR_ENTRY_BUFFER_HANDLE, PRJ_FILE_BASIC_INFO, PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    PRJ_NOTIFICATION, PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED,
    PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED, PRJ_NOTIFICATION_FILE_RENAMED,
    PRJ_NOTIFICATION_MAPPING, PRJ_NOTIFICATION_NEW_FILE_CREATED, PRJ_NOTIFICATION_PARAMETERS,
    PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED, PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED,
    PRJ_NOTIFY_FILE_RENAMED, PRJ_NOTIFY_NEW_FILE_CREATED, PRJ_PLACEHOLDER_INFO,
    PRJ_STARTVIRTUALIZING_OPTIONS, PrjAllocateAlignedBuffer, PrjFileNameCompare, PrjFileNameMatch,
    PrjFillDirEntryBuffer, PrjFreeAlignedBuffer, PrjMarkDirectoryAsPlaceholder,
    PrjStartVirtualizing, PrjStopVirtualizing, PrjWriteFileData, PrjWritePlaceholderInfo,
};
use windows::Win32::System::LibraryLoader::LoadLibraryW;
use windows::core::{GUID, HRESULT, PCWSTR};

use super::FsBridge;
use aurora_common::proto::{ControlMsg, FsEntry};

/// FsRead 1 回の最大バイト数 (制御フレーム 64KB 上限に収める)。
const READ_CHUNK: u32 = 32 * 1024;

/// 列挙セッション状態 (enumeration GUID をキーに保持)。
struct EnumSession {
    entries: Vec<FsEntry>,
    index: usize,
    loaded: bool,
    /// 検索式 (ワイルドカード)。null / 空なら全件。
    pattern: Option<Vec<u16>>,
}

/// コールバックの instance context として渡す本体。アドレス安定のため Box で確保する。
struct Provider {
    bridge: Arc<FsBridge>,
    enums: Mutex<HashMap<u128, EnumSession>>,
    /// 仮想化ルートの実パス。書き戻し時に materialize 済みのファイルを読むのに使う。
    root: PathBuf,
}

/// 投影セッション。Drop で `PrjStopVirtualizing`。
pub struct WinVirtualization {
    ctx: PRJ_NAMESPACE_VIRTUALIZATION_CONTEXT,
    // instance context の実体。callbacks 実行中は生存している必要がある。
    _provider: Box<Provider>,
}

impl Drop for WinVirtualization {
    fn drop(&mut self) {
        // SAFETY: ctx は start() で得た有効な仮想化コンテキスト。停止は drop で 1 回のみ。
        unsafe { PrjStopVirtualizing(self.ctx) };
    }
}

// SAFETY: ctx はスレッド間で移動しても安全 (ProjFS が内部同期する)。
unsafe impl Send for WinVirtualization {}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn guid_key(g: &GUID) -> u128 {
    ((g.data1 as u128) << 96)
        | ((g.data2 as u128) << 80)
        | ((g.data3 as u128) << 64)
        | (u64::from_be_bytes(g.data4) as u128)
}

/// PRJ_CALLBACK_DATA.FilePathName を共有ルート相対パス (区切り `/`) に変換する。
fn rel_path(data: &PRJ_CALLBACK_DATA) -> String {
    // SAFETY: FilePathName は ProjFS が設定する null 終端ワイド文字列。
    let raw = unsafe { data.FilePathName.to_string() }.unwrap_or_default();
    raw.replace('\\', "/")
}

/// InstanceContext から Provider 参照を復元する。
fn provider_of(data: &PRJ_CALLBACK_DATA) -> &Provider {
    // SAFETY: InstanceContext には start() で Provider の生ポインタを設定済み。
    // WinVirtualization が provider を保持する間 (= 仮想化中) 有効。
    unsafe { &*(data.InstanceContext as *const Provider) }
}

fn basic_info(entry: &FsEntry) -> PRJ_FILE_BASIC_INFO {
    PRJ_FILE_BASIC_INFO {
        IsDirectory: BOOLEAN(entry.is_dir as u8),
        FileSize: entry.size as i64,
        FileAttributes: if entry.is_dir {
            FILE_ATTRIBUTE_DIRECTORY.0
        } else {
            FILE_ATTRIBUTE_NORMAL.0
        },
        ..Default::default()
    }
}

/// 名前を ProjFS 規約の順序で比較する。
fn name_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let aw = wide(a);
    let bw = wide(b);
    // SAFETY: aw/bw は null 終端の有効な PCWSTR。
    let n = unsafe { PrjFileNameCompare(PCWSTR(aw.as_ptr()), PCWSTR(bw.as_ptr())) };
    n.cmp(&0)
}

/// `projectedfslib.dll` をロードできるか確認する。
///
/// このDLLは Windows のオプション機能 "Projected File System" (Client-ProjFS) が
/// 有効なときだけ存在する。`build.rs` で遅延ロードにしているため、ここで事前に
/// プローブしておかないと、無効環境では最初の `Prj*` 呼び出しが SEH 例外で abort
/// する。存在すれば `Err` でなく net 側の graceful フォールバックに委ねる。
fn projfs_available() -> bool {
    let name = wide("projectedfslib.dll");
    // SAFETY: name は null 終端の有効な PCWSTR。成功時の HMODULE は直後に FreeLibrary する。
    unsafe {
        match LoadLibraryW(PCWSTR(name.as_ptr())) {
            Ok(h) if !h.is_invalid() => {
                let _ = FreeLibrary(h);
                true
            }
            _ => false,
        }
    }
}

pub fn start(bridge: Arc<FsBridge>, root: &Path) -> Result<WinVirtualization> {
    if !projfs_available() {
        anyhow::bail!("projectedfslib.dll をロードできません (Windows 機能 Client-ProjFS が無効)");
    }

    std::fs::create_dir_all(root).with_context(|| format!("create projfs root {root:?}"))?;

    // 仮想化インスタンス GUID をルート直下に永続化し、再起動でも再利用する。
    let id_path = root.join(".ddd-instance");
    let instance_id = match std::fs::read(&id_path) {
        Ok(bytes) if bytes.len() == 16 => {
            GUID::from_u128(u128::from_le_bytes(bytes.try_into().unwrap()))
        }
        _ => {
            let g = GUID::from_u128(rand::random::<u128>());
            let _ = std::fs::write(&id_path, g.to_u128().to_le_bytes());
            g
        }
    };

    let root_w = wide(&root.to_string_lossy());
    // プレースホルダ化。既にマーク済みなら失敗するが投影は続行できるためログのみ。
    // SAFETY: root_w は null 終端の有効な PCWSTR、instance_id は有効な GUID。
    if let Err(e) = unsafe {
        PrjMarkDirectoryAsPlaceholder(PCWSTR(root_w.as_ptr()), PCWSTR::null(), None, &instance_id)
    } {
        tracing::debug!(error = %e, "PrjMarkDirectoryAsPlaceholder (likely already a placeholder)");
    }

    let provider = Box::new(Provider {
        bridge,
        enums: Mutex::new(HashMap::new()),
        root: root.to_path_buf(),
    });

    let callbacks = PRJ_CALLBACKS {
        StartDirectoryEnumerationCallback: Some(start_enum_cb),
        EndDirectoryEnumerationCallback: Some(end_enum_cb),
        GetDirectoryEnumerationCallback: Some(get_enum_cb),
        GetPlaceholderInfoCallback: Some(get_placeholder_cb),
        GetFileDataCallback: Some(get_file_data_cb),
        NotificationCallback: Some(notification_cb),
        ..Default::default()
    };

    // 書き戻し用の通知を root ツリー全体で購読する (作成 / 変更 / 削除 / リネーム)。
    let mut notify_root = wide("");
    let mut mappings = [PRJ_NOTIFICATION_MAPPING {
        NotificationBitMask: PRJ_NOTIFY_NEW_FILE_CREATED
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_MODIFIED
            | PRJ_NOTIFY_FILE_HANDLE_CLOSED_FILE_DELETED
            | PRJ_NOTIFY_FILE_RENAMED,
        NotificationRoot: PCWSTR(notify_root.as_mut_ptr()),
    }];
    let options = PRJ_STARTVIRTUALIZING_OPTIONS {
        NotificationMappings: mappings.as_mut_ptr(),
        NotificationMappingsCount: mappings.len() as u32,
        ..Default::default()
    };

    let provider_ptr = provider.as_ref() as *const Provider as *const c_void;

    // SAFETY: root_w/notify_root は有効な PCWSTR、callbacks/options/mappings は本関数で
    // 構築し呼び出し完了まで生存する、provider_ptr は WinVirtualization が provider を
    // 保持する限り有効。
    let ctx = unsafe {
        PrjStartVirtualizing(
            PCWSTR(root_w.as_ptr()),
            &callbacks,
            Some(provider_ptr),
            Some(&options),
        )
    }
    .context("PrjStartVirtualizing (Client-ProjFS feature が無効の可能性)")?;

    tracing::info!(root = %root.display(), "ProjFS virtualization started");

    Ok(WinVirtualization {
        ctx,
        _provider: provider,
    })
}

unsafe extern "system" fn start_enum_cb(
    callbackdata: *const PRJ_CALLBACK_DATA,
    enumerationid: *const GUID,
) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let (data, key) = unsafe { (&*callbackdata, guid_key(&*enumerationid)) };
    let provider = provider_of(data);
    if let Ok(mut map) = provider.enums.lock() {
        map.insert(
            key,
            EnumSession {
                entries: Vec::new(),
                index: 0,
                loaded: false,
                pattern: None,
            },
        );
    }
    S_OK
}

unsafe extern "system" fn end_enum_cb(
    callbackdata: *const PRJ_CALLBACK_DATA,
    enumerationid: *const GUID,
) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let (data, key) = unsafe { (&*callbackdata, guid_key(&*enumerationid)) };
    let provider = provider_of(data);
    if let Ok(mut map) = provider.enums.lock() {
        map.remove(&key);
    }
    S_OK
}

unsafe extern "system" fn get_enum_cb(
    callbackdata: *const PRJ_CALLBACK_DATA,
    enumerationid: *const GUID,
    searchexpression: PCWSTR,
    direntrybufferhandle: PRJ_DIR_ENTRY_BUFFER_HANDLE,
) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let (data, key) = unsafe { (&*callbackdata, guid_key(&*enumerationid)) };
    let provider = provider_of(data);
    let restart = (data.Flags.0 & PRJ_CB_DATA_FLAG_ENUM_RESTART_SCAN.0) != 0;

    // --- フェーズ 1: ロード要否を判定 (短時間ロック) ---
    let need_load = match provider.enums.lock() {
        Ok(map) => match map.get(&key) {
            Some(s) => !s.loaded || restart,
            None => return ERROR_FILE_NOT_FOUND.to_hresult(),
        },
        Err(_) => return ERROR_IO_DEVICE.to_hresult(),
    };

    // --- フェーズ 2: ネットワーク取得はロック外で ---
    if need_load {
        let path = rel_path(data);
        let mut entries = match provider.bridge.list_all(&path) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "FsList failed");
                return ERROR_IO_DEVICE.to_hresult();
            }
        };
        entries.sort_by(|a, b| name_compare(&a.name, &b.name));
        let pattern = if searchexpression.is_null() {
            None
        } else {
            // SAFETY: null でない場合 ProjFS が渡す有効なワイド文字列。
            let s = unsafe { searchexpression.to_string() }.unwrap_or_default();
            if s.is_empty() { None } else { Some(wide(&s)) }
        };
        if let Ok(mut map) = provider.enums.lock() {
            if let Some(sess) = map.get_mut(&key) {
                sess.entries = entries;
                sess.index = 0;
                sess.loaded = true;
                sess.pattern = pattern;
            }
        }
    }

    // --- フェーズ 3: バッファ充填 (ネットワークなし) ---
    if let Ok(mut map) = provider.enums.lock() {
        if let Some(sess) = map.get_mut(&key) {
            while sess.index < sess.entries.len() {
                let entry = &sess.entries[sess.index];
                let name_w = wide(&entry.name);
                if let Some(pat) = &sess.pattern {
                    // SAFETY: name_w / pat は null 終端の有効な PCWSTR。
                    let m =
                        unsafe { PrjFileNameMatch(PCWSTR(name_w.as_ptr()), PCWSTR(pat.as_ptr())) };
                    if m.0 == 0 {
                        sess.index += 1;
                        continue;
                    }
                }
                let bi = basic_info(entry);
                // SAFETY: name_w は有効な PCWSTR、bi は有効、dirhandle は ProjFS 提供。
                let res = unsafe {
                    PrjFillDirEntryBuffer(PCWSTR(name_w.as_ptr()), Some(&bi), direntrybufferhandle)
                };
                if res.is_err() {
                    // バッファ満杯。index は進めず次回呼び出しで続きを返す。
                    break;
                }
                sess.index += 1;
            }
        }
    }

    S_OK
}

unsafe extern "system" fn get_placeholder_cb(callbackdata: *const PRJ_CALLBACK_DATA) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let data = unsafe { &*callbackdata };
    let provider = provider_of(data);
    let rel = rel_path(data);

    let (parent, name) = match rel.rsplit_once('/') {
        Some((p, n)) => (p.to_string(), n.to_string()),
        None => (String::new(), rel.clone()),
    };

    let entries = match provider.bridge.list_all(&parent) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(path = %parent, error = %e, "FsList (placeholder) failed");
            return ERROR_IO_DEVICE.to_hresult();
        }
    };
    let Some(entry) = entries.iter().find(|e| e.name.eq_ignore_ascii_case(&name)) else {
        return ERROR_FILE_NOT_FOUND.to_hresult();
    };

    let info = PRJ_PLACEHOLDER_INFO {
        FileBasicInfo: basic_info(entry),
        ..Default::default()
    };
    let dest_w = wide(&rel.replace('/', "\\"));

    // SAFETY: ctx は ProjFS 提供、dest_w は有効な PCWSTR、info は有効な構造体。
    let res = unsafe {
        PrjWritePlaceholderInfo(
            data.NamespaceVirtualizationContext,
            PCWSTR(dest_w.as_ptr()),
            &info,
            std::mem::size_of::<PRJ_PLACEHOLDER_INFO>() as u32,
        )
    };
    match res {
        Ok(()) => S_OK,
        Err(e) => e.code(),
    }
}

unsafe extern "system" fn get_file_data_cb(
    callbackdata: *const PRJ_CALLBACK_DATA,
    byteoffset: u64,
    length: u32,
) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let data = unsafe { &*callbackdata };
    let provider = provider_of(data);
    let path = rel_path(data);
    let ctx = data.NamespaceVirtualizationContext;
    let stream_id = data.DataStreamId;

    let mut remaining = length;
    let mut cur = byteoffset;
    while remaining > 0 {
        let want = remaining.min(READ_CHUNK);
        let bytes = match provider.bridge.read(&path, cur, want) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(path = %path, error = %e, "FsRead failed");
                return ERROR_IO_DEVICE.to_hresult();
            }
        };
        if bytes.is_empty() {
            break; // EOF
        }
        let got = bytes.len();
        // SAFETY: ctx は有効。確保失敗時は null。
        let buf = unsafe { PrjAllocateAlignedBuffer(ctx, got) };
        if buf.is_null() {
            return E_OUTOFMEMORY;
        }
        // SAFETY: buf は got バイト確保済み、bytes も got バイト。
        unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), buf as *mut u8, got) };
        // SAFETY: ctx/stream_id は有効、buf は got バイトの有効領域。
        let res = unsafe { PrjWriteFileData(ctx, &stream_id, buf, cur, got as u32) };
        // SAFETY: buf は直前に確保したもの。
        unsafe { PrjFreeAlignedBuffer(buf) };
        if let Err(e) = res {
            return e.code();
        }
        cur += got as u64;
        remaining -= got as u32;
    }

    S_OK
}

/// 書き戻し通知。ホストの Explorer が root 内で行った作成 / 変更 / 削除 / リネームを
/// クライアントへ一方向に反映する。応答は待たない (ベストエフォート)。
unsafe extern "system" fn notification_cb(
    callbackdata: *const PRJ_CALLBACK_DATA,
    isdirectory: BOOLEAN,
    notification: PRJ_NOTIFICATION,
    destinationfilename: PCWSTR,
    _operationparameters: *mut PRJ_NOTIFICATION_PARAMETERS,
) -> HRESULT {
    // SAFETY: ProjFS が渡す有効なポインタ。
    let data = unsafe { &*callbackdata };
    let provider = provider_of(data);
    let rel = rel_path(data);
    let is_dir = isdirectory.0 != 0;

    match notification {
        PRJ_NOTIFICATION_NEW_FILE_CREATED => {
            // 空ファイル / ディレクトリをまず作る (ファイル内容は後続の MODIFIED で流す)。
            provider
                .bridge
                .push(ControlMsg::FsCreate { path: rel, is_dir });
        }
        PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_MODIFIED => {
            if !is_dir {
                push_file_content(provider, &rel);
            }
        }
        PRJ_NOTIFICATION_FILE_HANDLE_CLOSED_FILE_DELETED => {
            provider.bridge.push(ControlMsg::FsDelete { path: rel });
        }
        PRJ_NOTIFICATION_FILE_RENAMED => {
            // SAFETY: destinationfilename は ProjFS が渡す null 終端ワイド文字列。
            let to = unsafe { destinationfilename.to_string() }
                .unwrap_or_default()
                .replace('\\', "/");
            provider.bridge.push(ControlMsg::FsRename { from: rel, to });
        }
        _ => {}
    }

    S_OK
}

/// materialize 済みのバッキングファイルを読み、FsCreate + FsWrite でクライアントへ送る。
fn push_file_content(provider: &Provider, rel: &str) {
    let full = provider.root.join(rel);
    let mut f = match std::fs::File::open(&full) {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(path = %rel, error = %e, "writeback open failed");
            return;
        }
    };
    // 既存内容を 0 に truncate してから書き直す。
    provider.bridge.push(ControlMsg::FsCreate {
        path: rel.to_string(),
        is_dir: false,
    });
    let mut offset: u64 = 0;
    let mut buf = vec![0u8; READ_CHUNK as usize];
    loop {
        let n = match f.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(path = %rel, error = %e, "writeback read failed");
                return;
            }
        };
        provider.bridge.push(ControlMsg::FsWrite {
            path: rel.to_string(),
            offset,
            data: buf[..n].to_vec(),
        });
        offset += n as u64;
    }
    tracing::info!(path = %rel, size = offset, "writeback streamed to client");
}
