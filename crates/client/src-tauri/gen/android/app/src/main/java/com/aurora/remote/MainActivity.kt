package com.aurora.remote

import android.content.ClipData
import android.content.ClipDescription
import android.content.ClipboardManager
import android.content.Context
import android.graphics.Color
import android.os.Build
import android.os.Bundle
import android.os.PersistableBundle
import androidx.activity.SystemBarStyle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  companion object {
    // クリップボード書き込みは ClipboardManager が System Service であり Activity を必要と
    // しないため、application context だけで足りる (読み取りは Android 10+ でフォアグラウンド
    // 制約があるためスコープ外。書き込み専用)。
    private var appContext: Context? = null

    /** ホスト→端末のクリップボード書き込み。Rust (mobile_clipboard) から JNI で呼ぶ。 */
    @JvmStatic
    fun setClipboardText(text: String): Boolean {
      val context = appContext ?: return false
      return try {
        val cm = context.getSystemService(Context.CLIPBOARD_SERVICE) as ClipboardManager
        val clip = ClipData.newPlainText("aurora", text)
        // Android 13+ は setPrimaryClip のたびに中身のプレビューを画面に出す。クリップボード同期は
        // 既定 ON なので、ホスト PC でパスワードをコピーしただけで端末画面に平文が表示されてしまう
        // (覗き見・画面録画・キャストに乗る)。EXTRA_IS_SENSITIVE を立てるとプレビューが伏字になる。
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
          clip.description.extras = PersistableBundle().apply {
            putBoolean(ClipDescription.EXTRA_IS_SENSITIVE, true)
          }
        }
        cm.setPrimaryClip(clip)
        true
      } catch (e: Exception) {
        false
      }
    }
  }

  override fun onCreate(savedInstanceState: Bundle?) {
    // Aurora の Web UI は既定で暗い配色。enableEdgeToEdge() を引数無しで呼ぶと
    // SystemBarStyle.auto(...) になり、端末のダーク/ライト設定に応じて実行時にシステムバーの
    // アイコン色を決めてしまう (values/themes.xml の android:windowLightStatusBar 指定は
    // ここで上書きされ勝てない)。ライトモード端末だと「黒アイコン + 明るいナビゲーションバー」
    // になり暗い UI に埋もれて読めなくなる (実機 SM-X230 で確認)。そのため明示的に
    // dark (= 暗い背景向け = 白アイコン) を固定する。
    // themes.xml 側の指定は edge-to-edge 適用前の一瞬 (起動スプラッシュ等) に効くため残すが、
    // 実行時はここの指定が優先されるので両方揃えておく必要がある。
    enableEdgeToEdge(
      statusBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
      navigationBarStyle = SystemBarStyle.dark(Color.TRANSPARENT),
    )
    super.onCreate(savedInstanceState)
    appContext = applicationContext
  }
}
