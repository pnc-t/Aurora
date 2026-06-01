//! クライアント側ネットワーク (実装はステップ4で本格化)。

#[tauri::command]
pub async fn connect_to_host(host: String, password: String) -> Result<String, String> {
    tracing::info!(host = %host, "connect_to_host called (skeleton)");
    let _ = password;
    Ok(format!("would connect to {host} (not implemented)"))
}
