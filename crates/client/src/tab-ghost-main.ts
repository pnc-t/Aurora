// タブのドラッグ用ゴースト窓のエントリ。表示するのはタブ名だけで、IPC 権限は持たない
// (位置の追従・破棄は Rust の tab_drag が行う)。
import "./styles/base.css";
import "./ui/tab-ghost.css";

const label = new URLSearchParams(location.search).get("label") ?? "";
const el = document.getElementById("ghost");
// ホスト由来の表示名なので innerHTML ではなく textContent で入れる。
if (el) el.textContent = label;
