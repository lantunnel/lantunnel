package com.buhuipao.tunnelproxy

object Tun2SocksNative {
    init {
        System.loadLibrary("tun2socks")
    }

    external fun start(config: String, tunFd: Int): Int
    external fun stop()
    external fun isRunning(): Boolean
}
