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

    // The Platform account panel. Each of these blocks on one HTTP round trip,
    // so every caller has to be off the main thread.
    external fun platformStartSignIn(platformUrl: String): String

    external fun platformPollSignIn(platformUrl: String, deviceCode: String): String

    external fun platformListTunnels(
        platformUrl: String,
        accessToken: String,
        expiresAtUnix: Long,
    ): String

    external fun platformListPeers(
        platformUrl: String,
        accessToken: String,
        expiresAtUnix: Long,
        tunnelId: String,
    ): String

    external fun platformImportPeer(
        platformUrl: String,
        accessToken: String,
        expiresAtUnix: Long,
        tunnelId: String,
        peerId: String,
    ): String

    external fun platformCreatePeer(
        platformUrl: String,
        accessToken: String,
        expiresAtUnix: Long,
        tunnelId: String,
        name: String,
    ): String
}
