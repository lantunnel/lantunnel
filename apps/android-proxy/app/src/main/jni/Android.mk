LOCAL_PATH := $(call my-dir)
TUN2SOCKS_JNI_PATH := $(LOCAL_PATH)

ifndef NDK_MODULE_PATH
NDK_MODULE_PATH := $(TUN2SOCKS_JNI_PATH)/hev-socks5-tunnel/third-part
endif

include $(TUN2SOCKS_JNI_PATH)/hev-socks5-tunnel/Android.mk

LOCAL_PATH := $(TUN2SOCKS_JNI_PATH)

include $(CLEAR_VARS)
LOCAL_MODULE := tun2socks
LOCAL_SRC_FILES := tun2socks_jni.c
LOCAL_C_INCLUDES := \
	$(LOCAL_PATH)/hev-socks5-tunnel/src
LOCAL_CFLAGS += -Wall -Wextra -Werror
LOCAL_SHARED_LIBRARIES := hev-socks5-tunnel
LOCAL_LDLIBS := -llog
LOCAL_LDFLAGS += -Wl,-z,max-page-size=16384
LOCAL_LDFLAGS += -Wl,-z,common-page-size=16384
include $(BUILD_SHARED_LIBRARY)
