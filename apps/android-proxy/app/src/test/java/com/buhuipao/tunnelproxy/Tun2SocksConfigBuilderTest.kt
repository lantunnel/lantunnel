package com.buhuipao.tunnelproxy

import org.junit.Assert.assertFalse
import org.junit.Assert.assertTrue
import org.junit.Test

class Tun2SocksConfigBuilderTest {
    @Test
    fun buildOmitsCredentialsWhenLocalProxyAuthIsDisabled() {
        val yaml = Tun2SocksConfigBuilder.build(
            runtimeConfigJson = """
                {
                  "local_socks5": {
                    "host": "127.0.0.1",
                    "port": 1080,
                    "auth_enabled": false
                  }
                }
            """.trimIndent(),
            tunMtu = 8500,
            tunAddress = "10.255.0.2",
        )

        assertTrue(yaml.contains("  port: 1080"))
        assertTrue(yaml.contains("  address: 127.0.0.1"))
        assertFalse(yaml.contains("username:"))
        assertFalse(yaml.contains("password:"))
    }

    /**
     * The flag decides, not the presence of the strings.
     *
     * A config that carries a credential pair but never turns auth on is not
     * asking for auth — writing the pair anyway would make tun2socks offer
     * credentials the local SOCKS listener never asked for.
     */
    @Test
    fun buildOmitsCredentialsWhenAuthEnabledIsAbsent() {
        val yaml = Tun2SocksConfigBuilder.build(
            runtimeConfigJson = """
                {
                  "local_socks5": {
                    "host": "127.0.0.1",
                    "port": 1080,
                    "username": "group-1",
                    "password": "secret-1"
                  }
                }
            """.trimIndent(),
            tunMtu = 8500,
            tunAddress = "10.255.0.2",
        )

        assertFalse(yaml.contains("username:"))
        assertFalse(yaml.contains("password:"))
    }

    /**
     * Auth on with nothing to send must not throw.
     *
     * This ran on the VPN start thread, so a JSONException here did not
     * surface as an error — it left the tunnel silently unestablished.
     */
    @Test
    fun buildDoesNotThrowWhenAuthEnabledButCredentialsMissing() {
        val yaml = Tun2SocksConfigBuilder.build(
            runtimeConfigJson = """
                {
                  "local_socks5": {
                    "host": "127.0.0.1",
                    "port": 1080,
                    "auth_enabled": true
                  }
                }
            """.trimIndent(),
            tunMtu = 8500,
            tunAddress = "10.255.0.2",
        )

        assertTrue(yaml.contains("  username: \"\""))
        assertTrue(yaml.contains("  port: 1080"))
    }

    /** Auth on with a real pair still carries it. */
    @Test
    fun buildIncludesCredentialsWhenAuthEnabled() {
        val yaml = Tun2SocksConfigBuilder.build(
            runtimeConfigJson = """
                {
                  "local_socks5": {
                    "host": "127.0.0.1",
                    "port": 1080,
                    "auth_enabled": true,
                    "username": "group-1",
                    "password": "secret-1"
                  }
                }
            """.trimIndent(),
            tunMtu = 8500,
            tunAddress = "10.255.0.2",
        )

        assertTrue(yaml.contains("  username: \"group-1\""))
        assertTrue(yaml.contains("  password: \"secret-1\""))
    }
}
