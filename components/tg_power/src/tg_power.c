#include "tg_power.h"

/* Placeholder until the board provides mains-loss detection and rail sampling. */
esp_err_t tg_power_init(void)
{
    return ESP_OK;
}

bool tg_power_mains_lost(void)
{
    return false;
}

float tg_power_rail_voltage(void)
{
    return 5.0f;
}
