#include <jni.h>
#include <stdatomic.h>
#include <string.h>
#include <unistd.h>

#include "hev-main.h"

static atomic_int tun2socks_running = 0;

JNIEXPORT jint JNICALL
Java_com_buhuipao_tunnelproxy_Tun2SocksNative_start(JNIEnv *env, jobject thiz,
                                                    jstring config,
                                                    jint tun_fd)
{
    const char *config_chars;
    int tunnel_fd;
    int result;

    (void)thiz;

    if (config == NULL)
        return -1;

    tunnel_fd = dup(tun_fd);
    if (tunnel_fd < 0)
        return -1;

    config_chars = (*env)->GetStringUTFChars(env, config, NULL);
    if (config_chars == NULL) {
        close(tunnel_fd);
        return -1;
    }

    if (atomic_exchange(&tun2socks_running, 1) != 0) {
        (*env)->ReleaseStringUTFChars(env, config, config_chars);
        close(tunnel_fd);
        return -2;
    }

    result = hev_socks5_tunnel_main_from_str(
        (const unsigned char *)config_chars, (unsigned int)strlen(config_chars),
        tunnel_fd);

    atomic_store(&tun2socks_running, 0);

    (*env)->ReleaseStringUTFChars(env, config, config_chars);
    close(tunnel_fd);

    return result;
}

JNIEXPORT void JNICALL
Java_com_buhuipao_tunnelproxy_Tun2SocksNative_stop(JNIEnv *env, jobject thiz)
{
    (void)env;
    (void)thiz;

    if (!atomic_load(&tun2socks_running))
        return;

    hev_socks5_tunnel_quit();
}

JNIEXPORT jboolean JNICALL
Java_com_buhuipao_tunnelproxy_Tun2SocksNative_isRunning(JNIEnv *env,
                                                        jobject thiz)
{
    (void)env;
    (void)thiz;

    return atomic_load(&tun2socks_running) ? JNI_TRUE : JNI_FALSE;
}
