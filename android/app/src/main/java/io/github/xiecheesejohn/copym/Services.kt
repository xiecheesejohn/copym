package io.github.xiecheesejohn.copym

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.IBinder
import android.service.quicksettings.TileService
import android.widget.Toast
import androidx.core.content.ContextCompat
import androidx.core.content.IntentCompat
import kotlin.concurrent.thread

/**
 * Optional foreground service that keeps the process (and so the sync
 * engine) alive while the app is in the background.
 */
class SyncService : Service() {
    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_QUIT) {
            // "退出" in the notification: stop syncing and end the process.
            stopForeground(STOP_FOREGROUND_REMOVE)
            stopSelf()
            android.os.Process.killProcess(android.os.Process.myPid())
            return START_NOT_STICKY
        }
        Core.init(this)
        val nm = getSystemService(NotificationManager::class.java)
        nm.createNotificationChannel(NotificationChannel(CHANNEL, "后台同步", NotificationManager.IMPORTANCE_MIN))
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val n = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_stat)
            .setContentTitle("copym 正在同步")
            .setContentText("局域网剪贴板同步在后台运行")
            .setContentIntent(open)
            .addAction(
                Notification.Action.Builder(
                    null,
                    "退出",
                    PendingIntent.getService(
                        this, 1, Intent(this, SyncService::class.java).setAction(ACTION_QUIT), PendingIntent.FLAG_IMMUTABLE,
                    ),
                ).build(),
            )
            .setOngoing(true)
            .build()
        try {
            if (Build.VERSION.SDK_INT >= 29) {
                startForeground(1, n, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
            } else {
                startForeground(1, n)
            }
        } catch (e: Exception) {
            stopSelf() // e.g. restart from background not allowed
        }
        return START_STICKY
    }

    /** Android 15 limits dataSync services to 6 h a day. */
    override fun onTimeout(startId: Int, fgsType: Int) {
        stopSelf()
    }

    companion object {
        private const val CHANNEL = "sync"
        private const val ACTION_QUIT = "io.github.xiecheesejohn.copym.QUIT"
        fun start(ctx: Context) {
            try {
                ContextCompat.startForegroundService(ctx, Intent(ctx, SyncService::class.java))
            } catch (_: Exception) {
            }
        }

        fun stop(ctx: Context) {
            ctx.stopService(Intent(ctx, SyncService::class.java))
        }
    }
}

/** Quick-settings tile: save the current clipboard into copym. */
class CaptureTileService : TileService() {
    override fun onClick() {
        val intent = Intent(this, CaptureActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE))
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}

/**
 * Invisible activity. Handles "share to copym" (text or image) and, when
 * started from the tile, reads the clipboard once it has window focus.
 */
class CaptureActivity : Activity() {
    private var waitingForFocus = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Core.init(this)
        if (intent?.action == Intent.ACTION_SEND) {
            val text = intent.getStringExtra(Intent.EXTRA_TEXT)
            val uri = IntentCompat.getParcelableExtra(intent, Intent.EXTRA_STREAM, Uri::class.java)
            val app = applicationContext
            thread {
                val msg = when {
                    uri != null && intent.type?.startsWith("image/") == true -> Clip.saveImage(app, uri)
                    !text.isNullOrEmpty() -> Clip.saveText(text)
                    else -> "没有可保存的内容"
                }
                // Finish only afterwards: the URI grant lives as long as this activity.
                runOnUiThread {
                    Toast.makeText(app, msg, Toast.LENGTH_SHORT).show()
                    finish()
                }
            }
        } else {
            waitingForFocus = true
        }
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (!hasFocus || !waitingForFocus) return
        waitingForFocus = false
        val snap = Clip.snapshot(this)
        val app = applicationContext
        thread {
            val msg = Clip.save(app, snap, force = true)
            if (msg != null) android.os.Handler(app.mainLooper).post { Toast.makeText(app, msg, Toast.LENGTH_SHORT).show() }
        }
        finish()
    }
}
