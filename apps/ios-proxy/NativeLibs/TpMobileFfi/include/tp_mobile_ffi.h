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
void tp_mobile_free_string(char *ptr);

#ifdef __cplusplus
}
#endif

#endif
