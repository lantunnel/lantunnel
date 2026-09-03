package com.buhuipao.tunnelproxy

object TunnelProxyNative {
    const val OK: Int = 0
    const val INVALID_ARGUMENT: Int = -1
    const val INVALID_JSON: Int = -2
    const val INVALID_CONFIG: Int = -3
    const val ALREADY_RUNNING: Int = -4
    const val START_FAILED: Int = -5

    init {
        System.loadLibrary("tp_mobile_ffi")
    }

    external fun startProxy(requestJson: String): Int

    external fun stopProxy(): Int

    external fun statusJson(): String

    external fun logsJson(limit: Int): String

    external fun clearNativeLogs(): Int

    external fun setLogLevel(level: String): Int

    external fun logConfigJson(): String

    external fun clashOverlayYaml(): String

    external fun runtimeConfigJson(): String
}
