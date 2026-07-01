//! クライアント側のファイル共有: ダウンロード受信と、ホストへ見せる FS の列挙/読み書き。
use super::*;

/// ダウンロード受信状態 (ホスト → クライアント)。1 セッション 1 転送のみ。
pub(crate) struct DownloadState {
    pub(crate) id: u32,
    pub(crate) file: tokio::fs::File,
    pub(crate) written: u64,
    /// FileOffer で告知された総サイズ。これを超える書き込みは中断する
    /// (悪意あるホストが告知より多く送ってディスクを埋めるのを防ぐ)。
    pub(crate) total: u64,
    pub(crate) path: String,
}

/// ダウンロード保存ディレクトリ (`%USERPROFILE%\Aurora\downloads`)。
///
/// インストール版アプリはショートカット起動で CWD が不定 (書き込み不可の場所もある) の
/// ため、CWD 相対では保存に失敗したり場所が予測不能になる。host 側 `upload_dir` と対称に、
/// ユーザープロファイル配下の固定パスへ絶対化して discoverable にする。
/// `USERPROFILE` (非 Windows では `HOME`) が無ければ最後の手段としてカレントディレクトリ。
fn download_dir() -> PathBuf {
    let base = std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    base.join("Aurora").join("downloads")
}

/// `download_dir()` 配下に保存先ファイルを作る。ファイル名はパス区切りを除去して安全化する。
pub(crate) async fn open_download(name: &str) -> anyhow::Result<(tokio::fs::File, String)> {
    let dir = download_dir();
    tokio::fs::create_dir_all(&dir).await?;
    // パス区切り・ADS (`:`) を中和し、ファイル名成分のみを採用する (host open_upload と対称)。
    let safe = aurora_common::fsname::sanitize_filename(name, "download.bin");
    let path = dir.join(&safe);
    let file = tokio::fs::File::create(&path).await?;
    Ok((file, path.display().to_string()))
}
/// このセッションのファイル共有スコープ。
#[derive(Clone, Default)]
pub enum ShareScope {
    /// 共有無効 (既定)。すべての FS 要求を拒否する。
    #[default]
    Disabled,
    /// This PC 全体。先頭コンポーネントをドライブ文字とみなし、全ドライブを 1 つの合成ルートに
    /// 公開する。ホスト側は登録端末のときだけ実際にドライブを mount する (net.rs の client_trusted)。
    WholePc,
}

/// 1 ページの一覧結果。
pub(crate) struct ListPage {
    entries: Vec<FsEntry>,
    done: bool,
    error: Option<String>,
}

impl ListPage {
    fn err(msg: String) -> Self {
        Self {
            entries: Vec::new(),
            done: true,
            error: Some(msg),
        }
    }
    pub(crate) fn into_msg(self, req_id: u64) -> ControlMsg {
        ControlMsg::FsListReply {
            req_id,
            entries: self.entries,
            done: self.done,
            error: self.error,
        }
    }
}

/// 制御フレーム 64KB 上限に収めるための 1 ページあたりエントリ数上限。
const LIST_PAGE: usize = 128;
/// FsRead 1 回の上限。host は 32KB ずつ要求するが、悪意あるホストが大きい length を
/// 送ってきても `FsReadReply` (data + bincode + AEAD タグ) が TCP フレーム上限
/// (`u16::MAX`) を超えて自己切断しないよう、オーバーヘッド分を残してクランプする。
const MAX_READ: u32 = 60 * 1024;

/// 読み取り系 (一覧/読込) のパス解決。`Ok(None)` は合成ルート (= ドライブ一覧を出す) を表す。
fn resolve_read(scope: &ShareScope, rel: &str) -> Result<Option<PathBuf>, String> {
    match scope {
        ShareScope::Disabled => Err("sharing disabled".into()),
        ShareScope::WholePc => whole_pc_resolve(rel),
    }
}

/// 書き戻し系 (作成/書込/削除/改名) のパス解決。ドライブ/合成ルート自身への作用は拒否する。
fn resolve_write(scope: &ShareScope, rel: &str) -> Result<PathBuf, String> {
    match scope {
        ShareScope::Disabled => Err("sharing disabled".into()),
        ShareScope::WholePc => whole_pc_resolve_write(rel),
    }
}

/// 全ドライブ合成ルート: ホスト相対パス (`/` 区切り) を実パスへ写す。
/// 先頭コンポーネント = ドライブ文字 (例 "C") → `C:\`、以降は通常の子コンポーネント。
/// `Ok(None)` は合成ルート自身 (= ドライブ一覧)。`..` / 区切り混入などの危険成分は拒否する。
/// This PC 全体を公開する仕様上「ルート外脱出」概念は無く、全ドライブが共有対象なので
/// canonicalize での封じ込めは行わない (脱出先も同じく共有範囲内)。
fn whole_pc_resolve(rel: &str) -> Result<Option<PathBuf>, String> {
    let mut comps = rel.split('/').filter(|c| !c.is_empty() && *c != ".");
    let Some(first) = comps.next() else {
        return Ok(None); // 合成ルート
    };
    // ドライブ文字は 1 文字の英字のみ。
    let mut chars = first.chars();
    let letter = chars.next();
    if first.len() != 1
        || chars.next().is_some()
        || !letter.is_some_and(|c| c.is_ascii_alphabetic())
    {
        return Err(format!("invalid drive: {first}"));
    }
    let mut p = PathBuf::from(format!("{}:\\", letter.unwrap().to_ascii_uppercase()));
    for comp in comps {
        if comp == ".." || comp.contains('\\') || comp.contains(':') {
            return Err(format!("unsafe path component: {comp}"));
        }
        p.push(comp);
    }
    Ok(Some(p))
}

/// 書き戻し用の解決。合成ルート (0 成分) とドライブのルート (1 成分) への破壊的操作を拒否する
/// (悪意あるホストが `FsDelete{path:"C"}` 1 通でドライブごと消す/改名する事故を防ぐ)。
fn whole_pc_resolve_write(rel: &str) -> Result<PathBuf, String> {
    let n = rel
        .split('/')
        .filter(|c| !c.is_empty() && *c != ".")
        .count();
    if n < 2 {
        return Err("refusing operation on drive or synthetic root".into());
    }
    whole_pc_resolve(rel)?.ok_or_else(|| "refusing operation on synthetic root".into())
}

/// 存在する論理ドライブを `FsEntry` (ディレクトリ扱い) で返す。合成ルートの一覧に使う。
/// ドライブ名は `:` を含むとホストのファイル名として不正になるため、裸の文字 ("C") にする。
#[cfg(windows)]
fn list_drives() -> Vec<FsEntry> {
    // SAFETY: GetLogicalDrives は引数を取らず、存在ドライブのビットマスクを返すだけの純粋関数。
    let mask = unsafe { windows::Win32::Storage::FileSystem::GetLogicalDrives() };
    (0..26u32)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| FsEntry {
            name: ((b'A' + i as u8) as char).to_string(),
            is_dir: true,
            size: 0,
        })
        .collect()
}
#[cfg(not(windows))]
fn list_drives() -> Vec<FsEntry> {
    Vec::new()
}

/// `all` を名前順に整列し `offset` から 1 ページ (最大 `LIST_PAGE` 件) 切り出す。
fn paginate(mut all: Vec<FsEntry>, offset: u32) -> ListPage {
    // ページ境界の安定のため名前で整列 (host 側でも ProjFS 規約で再整列する)。
    all.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    let total = all.len();
    let start = (offset as usize).min(total);
    let entries: Vec<FsEntry> = all.into_iter().skip(start).take(LIST_PAGE).collect();
    let done = start + entries.len() >= total;
    ListPage {
        entries,
        done,
        error: None,
    }
}

/// スコープに従いディレクトリ (または合成ルート) を `offset` から最大 `LIST_PAGE` 件列挙する。
pub(crate) async fn fs_list(scope: &ShareScope, rel: &str, offset: u32) -> ListPage {
    let dir = match resolve_read(scope, rel) {
        Ok(Some(p)) => p,
        Ok(None) => return paginate(list_drives(), offset), // 合成ルート = ドライブ一覧
        Err(e) => return ListPage::err(e),
    };
    let mut rd = match tokio::fs::read_dir(&dir).await {
        Ok(rd) => rd,
        Err(e) => return ListPage::err(format!("read_dir: {e}")),
    };
    let mut all: Vec<FsEntry> = Vec::new();
    loop {
        match rd.next_entry().await {
            Ok(Some(ent)) => {
                let name = ent.file_name().to_string_lossy().to_string();
                let (is_dir, size) = match ent.metadata().await {
                    Ok(m) => (m.is_dir(), m.len()),
                    Err(_) => continue, // メタ取得不能なエントリはスキップ
                };
                all.push(FsEntry { name, is_dir, size });
            }
            Ok(None) => break,
            Err(e) => return ListPage::err(format!("read entry: {e}")),
        }
    }
    paginate(all, offset)
}

/// スコープに従いファイルを `offset` から最大 `length` バイト読む。
/// 戻り (data, error)。空 data かつ error=None は EOF。
pub(crate) async fn fs_read(
    scope: &ShareScope,
    rel: &str,
    offset: u64,
    length: u32,
) -> (Vec<u8>, Option<String>) {
    let path = match resolve_read(scope, rel) {
        Ok(Some(p)) => p,
        Ok(None) => return (Vec::new(), Some("is a directory".into())),
        Err(e) => return (Vec::new(), Some(e)),
    };
    let mut file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return (Vec::new(), Some(format!("open: {e}"))),
    };
    if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
        return (Vec::new(), Some(format!("seek: {e}")));
    }
    let want = length.min(MAX_READ) as usize;
    let mut buf = vec![0u8; want];
    let mut filled = 0;
    // 要求長を満たすか EOF まで読む (短い read を埋める)。
    while filled < want {
        match file.read(&mut buf[filled..]).await {
            Ok(0) => break, // EOF
            Ok(n) => filled += n,
            Err(e) => return (Vec::new(), Some(format!("read: {e}"))),
        }
    }
    buf.truncate(filled);
    (buf, None)
}

/// ファイル / ディレクトリを作成する (ファイルは 0 バイトに truncate)。
pub(crate) async fn fs_create(scope: &ShareScope, rel: &str, is_dir: bool) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    if is_dir {
        tokio::fs::create_dir_all(&p)
            .await
            .map_err(|e| format!("create_dir: {e}"))
    } else {
        if let Some(parent) = p.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        tokio::fs::File::create(&p)
            .await
            .map(|_| ())
            .map_err(|e| format!("create_file: {e}"))
    }
}

/// ファイルの `offset` 以降にバイトを書き込む (なければ作成、truncate はしない)。
pub(crate) async fn fs_write(
    scope: &ShareScope,
    rel: &str,
    offset: u64,
    data: &[u8],
) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    let mut file = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&p)
        .await
        .map_err(|e| format!("open: {e}"))?;
    file.seek(std::io::SeekFrom::Start(offset))
        .await
        .map_err(|e| format!("seek: {e}"))?;
    file.write_all(data)
        .await
        .map_err(|e| format!("write: {e}"))
}

/// ファイル / ディレクトリを削除する。
pub(crate) async fn fs_delete(scope: &ShareScope, rel: &str) -> Result<(), String> {
    let p = resolve_write(scope, rel)?;
    match tokio::fs::metadata(&p).await {
        Ok(m) if m.is_dir() => tokio::fs::remove_dir_all(&p)
            .await
            .map_err(|e| format!("remove_dir: {e}")),
        Ok(_) => tokio::fs::remove_file(&p)
            .await
            .map_err(|e| format!("remove_file: {e}")),
        Err(_) => Ok(()), // 既に無いなら成功扱い
    }
}

/// `from` を `to` にリネーム / 移動する。
pub(crate) async fn fs_rename(scope: &ShareScope, from: &str, to: &str) -> Result<(), String> {
    let src = resolve_write(scope, from)?;
    let dst = resolve_write(scope, to)?;
    if let Some(parent) = dst.parent() {
        let _ = tokio::fs::create_dir_all(parent).await;
    }
    tokio::fs::rename(&src, &dst)
        .await
        .map_err(|e| format!("rename: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 合成ルート (0 成分) とドライブのルート (1 成分) への破壊系操作は拒否する
    /// (ドライブごと remove_dir_all / rename される回帰を防ぐ)。
    #[test]
    fn whole_pc_write_rejects_drive_and_synthetic_root() {
        for rel in ["", ".", "/", "C", "C/", "/C/.", "//"] {
            assert!(
                whole_pc_resolve_write(rel).is_err(),
                "expected rejection for {rel:?}"
            );
        }
    }

    /// `..` / 区切り混入 / 不正なドライブ文字はコンポーネント単位で拒否する。
    #[test]
    fn whole_pc_resolve_rejects_traversal_and_bad_drive() {
        for rel in ["C/..", "C/a/../b", "C/a\\b", "C/x:y", "CC/x", "1/x"] {
            assert!(whole_pc_resolve(rel).is_err(), "should reject {rel:?}");
        }
    }

    /// 合成ルートは `None`、ドライブ配下は実パスへ写す (ドライブ文字は大文字化)。
    #[cfg(windows)]
    #[test]
    fn whole_pc_resolve_maps_paths() {
        assert!(matches!(whole_pc_resolve(""), Ok(None)));
        assert!(matches!(whole_pc_resolve("."), Ok(None)));
        let p = whole_pc_resolve("c/Users/name").unwrap().unwrap();
        assert_eq!(p, std::path::PathBuf::from("C:\\Users\\name"));
    }
}
