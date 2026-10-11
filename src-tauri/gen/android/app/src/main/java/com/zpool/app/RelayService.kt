package com.zpool.app

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Intent
import android.os.Build
import android.os.IBinder
import androidx.core.app.NotificationCompat

/**
 * 前台服务：把 App 进程钉在后台，反代线程（Rust tiny_http，在 tauri setup 里起的）
 * 随进程存活。安卓会杀长期后台的空进程，没有这个服务，退出界面几分钟后
 * 手机上的 8899 端口就会失联 —— 这正是"老是在等待重连"在移动端的对应物。
 *
 * 服务本体不做业务：通知常驻 + START_STICKY 足够；真正的工作线程属于 Rust 侧。
 */
class RelayService : Service() {

    override fun onCreate() {
        super.onCreate()
        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
            val ch = NotificationChannel(
                CHANNEL_ID,
                "Z·POOL 反代",
                NotificationManager.IMPORTANCE_LOW // 常驻通知不响不震
            ).apply { description = "号池反代与 Web 管理台保持运行" }
            getSystemService(NotificationManager::class.java).createNotificationChannel(ch)
        }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        val open = PendingIntent.getActivity(
            this, 0,
            Intent(this, MainActivity::class.java),
            PendingIntent.FLAG_IMMUTABLE
        )
        val n: Notification = NotificationCompat.Builder(this, CHANNEL_ID)
            .setContentTitle("Z·POOL 正在运行")
            .setContentText("反代 :8899 · 管理台 http://127.0.0.1:8899/proxy")
            .setSmallIcon(android.R.drawable.ic_menu_share)
            .setContentIntent(open)
            .setOngoing(true)
            .build()
        // Android 14+（API 34）要求前台服务声明类型；12~13 也建议带类型。
        // 反代不属于 dataSync/location 等常规类别，用 SPECIAL_USE（需配套 manifest 权限）。
        if (Build.VERSION.SDK_INT >= 34) {
            startForeground(1, n, android.content.pm.ServiceInfo.FOREGROUND_SERVICE_TYPE_SPECIAL_USE)
        } else {
            startForeground(1, n)
        }
        return START_STICKY
    }

    override fun onBind(intent: Intent?): IBinder? = null

    companion object {
        private const val CHANNEL_ID = "zpool-relay"
    }
}
