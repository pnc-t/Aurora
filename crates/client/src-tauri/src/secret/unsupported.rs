//! Windows / Android 以外の Adapter。暗号化保存に未対応 (Windows/Android 優先方針)。

pub(super) fn protect(_plain: &[u8]) -> Result<Vec<u8>, String> {
    Err("この OS では秘密の暗号化保存に対応していません".into())
}

pub(super) fn unprotect(_enc: &[u8]) -> Option<Vec<u8>> {
    None
}
