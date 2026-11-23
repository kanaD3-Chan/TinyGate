#pragma once
#include <stdbool.h>
#include <stdint.h>
#include "esp_err.h"
#ifdef __cplusplus
extern "C" {
#endif
#define TG_ZB_NETWORK 1
#define TG_ZB_JOIN 2
#define TG_ZB_ENDPOINT 3
#define TG_ZB_ATTRIBUTE 4
#define TG_ZB_TX_CONFIRM 5
#define TG_ZB_DEFAULT_RESPONSE 6
#define TG_ZB_REPORT_CONFIG 7
#define TG_ZB_LEAVE 8
#define TG_ZB_ERROR 9
#define TG_ZB_IDENTITY 10
#define TG_ZB_BIND 11
#define TG_ZB_TEMPERATURE_CLUSTER 0x0402
#define TG_ZB_ONOFF_CLUSTER 0x0006
#define TG_ZB_VALUE_CAPACITY 32
#define TG_ZB_VALUE_UNAVAILABLE 0
#define TG_ZB_VALUE_COMPLETE 1
#define TG_ZB_VALUE_TRUNCATED 2
#define TG_ZB_REPORT_CHANGE_CAPACITY 8

typedef struct {
    uint8_t kind;
    uint16_t short_address;
    uint8_t ieee_address[8]; /* Native little-endian EUI64; zero if not known */
    uint8_t endpoint;
    uint16_t cluster;
    uint16_t attribute;
    uint16_t profile;
    uint16_t manufacturer;
    int32_t status;          /* TX: SDK; attribute/default/report config: ZCL; bind/identity/error: SDK or ZDO */
    uint32_t request_id;     /* Supplied ID, on TX_CONFIRM and BIND */
    uint8_t tsn;            /* Native ZCL transaction sequence number */
    uint8_t command;        /* DEFAULT_RESPONSE: command being acknowledged */
    bool is_report;         /* ATTRIBUTE: unsolicited report vs read response */
    uint8_t value_type;     /* ATTRIBUTE: native ZCL type */
    uint8_t value_status;   /* TG_ZB_VALUE_*; independent of protocol status */
    uint8_t value_len;      /* Bytes copied, including any ZCL length prefix */
    uint8_t value[TG_ZB_VALUE_CAPACITY];
} tg_zigbee_event_t;

typedef struct {
    uint16_t attribute;
    uint8_t value_type;
    uint16_t min_interval_s;
    uint16_t max_interval_s;
    uint8_t change_len; /* Zero for discrete types; native little-endian bytes for analog types */
    uint8_t change[TG_ZB_REPORT_CHANGE_CAPACITY];
} tg_zigbee_reporting_t;
typedef struct {
    bool started;
    bool ready;
    bool join_open;
    uint8_t channel;
    uint16_t pan_id;
    uint16_t peer_address;  /* Most recently announced/reporting peer, 0xFFFF if unknown */
    uint32_t dropped_events;
    int32_t last_error;
} tg_zigbee_status_t;

/* Coordinator, channel 15, persistent dedicated zb_nvs. One application caller.
 * Startup is asynchronous; check ready. SDK accesses are lock-protected.
 * No shutdown/re-init API: this component runs for the lifetime of firmware. */
esp_err_t tg_zigbee_init(void);
esp_err_t tg_zigbee_get_status(tg_zigbee_status_t *status);
/* Nonblocking; ESP_ERR_NOT_FOUND means empty. On overflow, the newest event
 * is dropped and dropped_events increments. Consumers must resynchronize. */
esp_err_t tg_zigbee_poll_event(tg_zigbee_event_t *event);
/* The following operations can wait up to 20 ms for the SDK lock. ESP_OK
 * means submitted (or a cached identity event queued). Request IDs must be
 * nonzero and unique while in flight. Protocol responses carry TSN and target
 * context and may arrive before TX_CONFIRM. */
esp_err_t tg_zigbee_permit_join(uint8_t seconds); /* 0 closes; 1..254 seconds */
esp_err_t tg_zigbee_resolve(uint16_t address);
esp_err_t tg_zigbee_address_for(const uint8_t ieee_address[8], uint16_t *address);
esp_err_t tg_zigbee_discover(uint16_t address);
esp_err_t tg_zigbee_read_attribute(uint16_t address, uint8_t endpoint, uint16_t cluster,
                                  uint16_t attribute, uint32_t request_id);
/* The cached short address must still belong to the supplied IEEE address. */
esp_err_t tg_zigbee_send_onoff(uint16_t address, const uint8_t ieee_address[8],
                              uint8_t endpoint, uint8_t command,
                              uint32_t request_id); /* command 0/1/2 = off/on/toggle */
esp_err_t tg_zigbee_bind(uint16_t address, const uint8_t ieee_address[8], uint8_t endpoint,
                         uint16_t cluster, uint32_t request_id);
esp_err_t tg_zigbee_configure_reporting(uint16_t address, uint8_t endpoint,
                                       uint16_t cluster, const tg_zigbee_reporting_t *config,
                                       uint32_t request_id);
#ifdef __cplusplus
}
#endif
