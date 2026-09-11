#ifndef TP_MOBILE_FFI_H
#define TP_MOBILE_FFI_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

enum {
    TP_MOBILE_OK = 0,
    TP_MOBILE_INVALID_ARGUMENT = -1,
    TP_MOBILE_INVALID_JSON = -2,
    TP_MOBILE_INVALID_CONFIG = -3,
    TP_MOBILE_ALREADY_RUNNING = -4,
    TP_MOBILE_START_FAILED = -5,
};

int32_t tp_mobile_start_proxy(const char *json);
int32_t tp_mobile_stop_proxy(void);
char *tp_mobile_status_json(void);
char *tp_mobile_logs_json(size_t limit);
int32_t tp_mobile_clear_logs(void);
int32_t tp_mobile_set_log_level(const char *level);
char *tp_mobile_log_config_json(void);
char *tp_mobile_clash_overlay_yaml(void);
char *tp_mobile_runtime_config_json(void);

/*
 * Platform account panel. Every one of these blocks on a single HTTP round
 * trip and returns a JSON string the caller releases with
 * tp_mobile_free_string, so they must be called off the main thread.
 *
 * Nothing here stores the access token. The app keeps it in the Keychain and
 * passes it back in, so signing out is one delete rather than two.
 */
char *tp_mobile_platform_start_sign_in(const char *platform_url);
char *tp_mobile_platform_poll_sign_in(const char *platform_url,
                                      const char *device_code);
char *tp_mobile_platform_list_tunnels(const char *platform_url,
                                      const char *access_token,
                                      uint64_t expires_at_unix);
char *tp_mobile_platform_list_peers(const char *platform_url,
                                    const char *access_token,
                                    uint64_t expires_at_unix,
                                    const char *tunnel_id);
char *tp_mobile_platform_import_peer(const char *platform_url,
                                     const char *access_token,
                                     uint64_t expires_at_unix,
                                     const char *tunnel_id,
                                     const char *peer_id);
char *tp_mobile_platform_create_peer(const char *platform_url,
                                     const char *access_token,
                                     uint64_t expires_at_unix,
                                     const char *tunnel_id,
                                     const char *name);

void tp_mobile_free_string(char *ptr);

#ifdef __cplusplus
}
#endif

#endif
