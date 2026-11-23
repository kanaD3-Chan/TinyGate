#pragma once

#include <stdbool.h>
#include "esp_err.h"

#ifdef __cplusplus
extern "C" {
#endif

/* Supercapacitor hold-up interface. Board-level sensing is pending. */
esp_err_t tg_power_init(void);
bool tg_power_mains_lost(void);
float tg_power_rail_voltage(void);

#ifdef __cplusplus
}
#endif
