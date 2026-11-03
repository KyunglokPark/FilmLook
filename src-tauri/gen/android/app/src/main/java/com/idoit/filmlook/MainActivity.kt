package com.idoit.filmlook

import android.graphics.Color
import android.os.Bundle
import android.view.View
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // 엣지투엣지 강제(targetSdk 35+) 환경에서 상태바/네비바가 웹뷰를 덮지 않도록
    // 시스템 바 인셋만큼 콘텐츠에 패딩을 주고, 바 뒤 배경은 앱 배경색과 맞춘다.
    val content = findViewById<View>(android.R.id.content)
    content.setBackgroundColor(Color.parseColor("#FF161618"))
    ViewCompat.setOnApplyWindowInsetsListener(content) { v, insets ->
      val bars = insets.getInsets(
        WindowInsetsCompat.Type.systemBars() or WindowInsetsCompat.Type.displayCutout()
      )
      v.setPadding(bars.left, bars.top, bars.right, bars.bottom)
      WindowInsetsCompat.CONSUMED
    }
  }
}
