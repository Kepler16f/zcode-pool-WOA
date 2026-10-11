package com.zpool.app

import android.content.Intent
import android.os.Build
import android.os.Bundle
import androidx.activity.enableEdgeToEdge

class MainActivity : TauriActivity() {
  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)
    // App 一起来就把前台服务挂上：反代（Rust 线程）要随进程常驻，
    // 通知栏可见也方便用户知道它活着、点通知回到管理界面。
    val svc = Intent(this, RelayService::class.java)
    if (Build.VERSION.SDK_INT >= 26) {
      startForegroundService(svc)
    } else {
      startService(svc)
    }
  }
}
