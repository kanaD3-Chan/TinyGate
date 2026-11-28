#include "tg_zigbee.h"
#include <string.h>
#include "sdkconfig.h"
#include "freertos/FreeRTOS.h"
#include "freertos/queue.h"
#include "freertos/task.h"
#include "nvs_flash.h"
#include "esp_log.h"
#include "esp_coexist.h"
#include "esp_zigbee.h"
#include "ezbee/zha.h"

static portMUX_TYPE s_lock = portMUX_INITIALIZER_UNLOCKED;
static tg_zigbee_status_t s_status;
static QueueHandle_t s_events;
static const char *TAG = "TG-Zigbee";
enum {
    COORDINATOR_ENDPOINT = 1,
    SDK_LOCK_MS = 20,
    MAX_DISCOVERY_ENDPOINTS = 8,
    ROUTE_PROOFS = 16,
    ROUTE_PROOF_MS = 10000,
    BIND_REQUESTS = 8,
    BIND_CONTEXT_MS = 30000,
};

/* Accessed with the SDK lock held or from an SDK callback. */
static struct {
    uint16_t address;
    uint8_t ieee[8];
    TickType_t observed_at;
} s_routes[ROUTE_PROOFS];
static size_t s_next_route;

typedef struct {
    tg_zigbee_event_t event;
    TickType_t submitted_at;
} bind_context_t;

static bind_context_t s_bind_requests[BIND_REQUESTS];

static bool valid_address(uint16_t address)
{
    return address > 0 && address < 0xfff8;
}

static void remember_route(uint16_t address, const uint8_t ieee[8])
{
    const uint8_t unknown[8] = {0};
    const uint8_t broadcast[8] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    if (!valid_address(address) || !memcmp(ieee, unknown, 8) || !memcmp(ieee, broadcast, 8)) return;

    size_t slot = s_next_route;
    for (size_t i = 0; i < ROUTE_PROOFS; ++i) {
        if (s_routes[i].address == address || memcmp(s_routes[i].ieee, ieee, 8) == 0) {
            s_routes[i].address = 0;
            slot = i;
        }
    }
    s_routes[slot].address = address;
    memcpy(s_routes[slot].ieee, ieee, sizeof(s_routes[slot].ieee));
    s_routes[slot].observed_at = xTaskGetTickCount();
    s_next_route = (slot + 1) % ROUTE_PROOFS;
}

static esp_err_t identity_at(uint16_t address, uint8_t ieee[8])
{
    if (!valid_address(address)) return ESP_ERR_NOT_FOUND;
    ezb_extaddr_t candidate;
    bool found = ezb_address_extended_by_short(address, &candidate) == EZB_ERR_NONE;
    TickType_t now = xTaskGetTickCount();
    /* Check recent announces and ZDO responses even if the SDK has an entry,
     * since the table may lag behind an address change. */
    for (size_t i = 0; i < ROUTE_PROOFS; ++i) {
        if (!s_routes[i].address ||
            now - s_routes[i].observed_at >= pdMS_TO_TICKS(ROUTE_PROOF_MS)) continue;
        if (s_routes[i].address == address) {
            if (found && memcmp(candidate.u8, s_routes[i].ieee, sizeof(candidate.u8))) {
                return ESP_ERR_INVALID_RESPONSE;
            }
            memcpy(candidate.u8, s_routes[i].ieee, sizeof(candidate.u8));
            found = true;
        } else if (found && !memcmp(candidate.u8, s_routes[i].ieee, sizeof(candidate.u8))) {
            return ESP_ERR_INVALID_RESPONSE;
        }
    }
    if (!found) return ESP_ERR_NOT_FOUND;

    uint16_t current_address;
    if (ezb_address_short_by_extended(&candidate, &current_address) == EZB_ERR_NONE &&
        current_address != address) {
        return ESP_ERR_INVALID_RESPONSE;
    }
    memcpy(ieee, candidate.u8, sizeof(candidate.u8));
    return ESP_OK;
}

static void lookup_ieee(tg_zigbee_event_t *event)
{
    identity_at(event->short_address, event->ieee_address);
}

static void emit(tg_zigbee_event_t event)
{
    if (xQueueSend(s_events, &event, 0) != pdTRUE) {
        portENTER_CRITICAL(&s_lock);
        ++s_status.dropped_events;
        portEXIT_CRITICAL(&s_lock);
    }
}
static void error_event(int error)
{
    if (!error) return;
    portENTER_CRITICAL(&s_lock);
    s_status.last_error = error;
    portEXIT_CRITICAL(&s_lock);
    emit((tg_zigbee_event_t){.kind=TG_ZB_ERROR, .status=error});
}
static void peer_seen(uint16_t address)
{
    portENTER_CRITICAL(&s_lock);
    s_status.peer_address = address;
    portEXIT_CRITICAL(&s_lock);
}
static void network_ready(void)
{
    uint8_t channel=ezb_nwk_get_current_channel();
    uint16_t pan=ezb_nwk_get_panid();
    portENTER_CRITICAL(&s_lock);
    s_status.ready = true;
    s_status.channel = channel;
    s_status.pan_id = pan;
    s_status.last_error = 0;
    portEXIT_CRITICAL(&s_lock);
    emit((tg_zigbee_event_t){.kind=TG_ZB_NETWORK});
}

static void descriptor_result(const ezb_zdo_simple_desc_req_result_t *result, void *ctx)
{
    uint16_t address = (uint16_t)(uintptr_t)ctx;
    if (!result || result->error || !result->rsp || result->rsp->status) {
        emit((tg_zigbee_event_t){.kind=TG_ZB_ERROR, .short_address=address,
             .status=result ? (result->error ? result->error : ESP_FAIL) : ESP_FAIL});
        return;
    }
    const ezb_af_simple_desc_t *d = &result->rsp->desc;
    for (unsigned i=0; d->app_cluster_list && i<d->app_input_cluster_count; ++i) {
        tg_zigbee_event_t event = {.kind=TG_ZB_ENDPOINT, .short_address=address,
            .endpoint=d->ep_id, .profile=d->app_profile_id, .cluster=d->app_cluster_list[i]};
        lookup_ieee(&event);
        emit(event);
    }
}
static void endpoints_result(const ezb_zdo_active_ep_req_result_t *result, void *ctx)
{
    uint16_t address = (uint16_t)(uintptr_t)ctx;
    if (!result || result->error || !result->rsp || result->rsp->status) {
        emit((tg_zigbee_event_t){.kind=TG_ZB_ERROR, .short_address=address,
             .status=result ? (result->error ? result->error : ESP_FAIL) : ESP_FAIL});
        return;
    }
    if (result->rsp->active_ep_count > MAX_DISCOVERY_ENDPOINTS) {
        ESP_LOGW(TAG, "Limiting discovery of %04x to %d endpoints", address, MAX_DISCOVERY_ENDPOINTS);
    }
    for (unsigned i=0; result->rsp->active_ep_list && i<result->rsp->active_ep_count && i<MAX_DISCOVERY_ENDPOINTS; ++i) {
        ezb_zdo_simple_desc_req_t req = {
            .dst_nwk_addr=address, .field={.nwk_addr_of_interest=address,
                .endpoint=result->rsp->active_ep_list[i]},
            .cb=descriptor_result, .user_ctx=ctx,
        };
        error_event(ezb_zdo_simple_desc_req(&req));
    }
}
static ezb_err_t discover_locked(uint16_t address)
{
    ezb_zdo_active_ep_req_t req = {
        .dst_nwk_addr=address, .field={.nwk_addr_of_interest=address},
        .cb=endpoints_result, .user_ctx=(void *)(uintptr_t)address,
    };
    return ezb_zdo_active_ep_req(&req);
}

static void identity_result(const ezb_zdo_ieee_addr_req_result_t *result, void *ctx)
{
    tg_zigbee_event_t event = {.kind=TG_ZB_IDENTITY, .short_address=(uint16_t)(uintptr_t)ctx};
    if (!result || !result->rsp) {
        event.status = result && result->error ? result->error : ESP_FAIL;
    } else {
        event.status = result->error ? result->error : result->rsp->status;
        if (!event.status && result->rsp->nwk_addr_remote_dev != event.short_address) {
            event.status = ESP_ERR_INVALID_RESPONSE;
        }
        uint16_t current_address;
        if (!event.status && ezb_address_short_by_extended(&result->rsp->ieee_addr_remote_dev,
            &current_address) == EZB_ERR_NONE && current_address != event.short_address) {
            event.status = ESP_ERR_INVALID_RESPONSE;
        }
        uint8_t known[8];
        esp_err_t previous = identity_at(event.short_address, known);
        if (!event.status && (previous == ESP_ERR_INVALID_RESPONSE ||
            (previous == ESP_OK && memcmp(known, result->rsp->ieee_addr_remote_dev.u8, sizeof(known))))) {
            event.status = ESP_ERR_INVALID_RESPONSE;
        }
        if (!event.status) {
            memcpy(event.ieee_address, result->rsp->ieee_addr_remote_dev.u8, sizeof(event.ieee_address));
            remember_route(event.short_address, event.ieee_address);
        }
    }
    emit(event);
}

static bool app_signal(const ezb_app_signal_t *signal)
{
    ezb_app_signal_type_t type = ezb_app_signal_get_type(signal);
    const void *params = ezb_app_signal_get_params(signal);
    switch (type) {
    case EZB_ZDO_SIGNAL_SKIP_STARTUP:
        error_event(ezb_bdb_start_top_level_commissioning(EZB_BDB_MODE_INITIALIZATION));
        break;
    case EZB_BDB_SIGNAL_DEVICE_FIRST_START:
    case EZB_BDB_SIGNAL_DEVICE_REBOOT:
        if (params && *(ezb_bdb_comm_status_t *)params == EZB_BDB_STATUS_SUCCESS) {
            if (ezb_bdb_is_factory_new())
                error_event(ezb_bdb_start_top_level_commissioning(EZB_BDB_MODE_NETWORK_FORMATION));
            else network_ready();
        } else error_event(params ? *(ezb_bdb_comm_status_t *)params : ESP_FAIL);
        break;
    case EZB_BDB_SIGNAL_FORMATION:
        if (params && *(ezb_bdb_comm_status_t *)params == EZB_BDB_STATUS_SUCCESS) network_ready();
        else error_event(params ? *(ezb_bdb_comm_status_t *)params : ESP_FAIL);
        break;
    case EZB_NWK_SIGNAL_PERMIT_JOIN_STATUS:
        portENTER_CRITICAL(&s_lock);
        s_status.join_open = params && *(uint8_t *)params != 0;
        portEXIT_CRITICAL(&s_lock);
        break;
    case EZB_ZDO_SIGNAL_DEVICE_ANNCE: {
        const ezb_zdo_signal_device_annce_params_t *p = params;
        if (!p) break;
        tg_zigbee_event_t event = {.kind=TG_ZB_JOIN, .short_address=p->short_addr};
        memcpy(event.ieee_address, p->device_addr.u8, 8);
        remember_route(event.short_address, event.ieee_address);
        peer_seen(p->short_addr);
        emit(event);
        break;
    }
    case EZB_ZDO_SIGNAL_LEAVE_INDICATION: {
        const ezb_zdo_signal_leave_indication_params_t *p = params;
        if (!p) break;
        tg_zigbee_event_t event = {.kind=TG_ZB_LEAVE, .short_address=p->short_addr};
        memcpy(event.ieee_address, p->device_addr.u8, 8);
        for (size_t i = 0; i < ROUTE_PROOFS; ++i) {
            if (s_routes[i].address == event.short_address &&
                memcmp(s_routes[i].ieee, event.ieee_address, sizeof(event.ieee_address)) == 0) {
                s_routes[i].address = 0;
            }
        }
        portENTER_CRITICAL(&s_lock);
        if (s_status.peer_address == p->short_addr) s_status.peer_address = 0xffff;
        portEXIT_CRITICAL(&s_lock);
        emit(event);
        break;
    }
    default: break;
    }
    return true;
}

static tg_zigbee_event_t from_header(uint8_t kind, const ezb_zcl_cmd_hdr_t *h)
{
    tg_zigbee_event_t event = {.kind=kind, .short_address=0xffff};
    if (h) {
        if (h->src_addr.addr_mode == EZB_ADDR_MODE_SHORT) {
            event.short_address=h->src_addr.u.short_addr;
            lookup_ieee(&event);
        } else if (h->src_addr.addr_mode == EZB_ADDR_MODE_EXT) {
            const ezb_extaddr_t *ieee = &h->src_addr.u.extended_addr;
            memcpy(event.ieee_address, ieee->u8, sizeof(event.ieee_address));
            ezb_address_short_by_extended(ieee, &event.short_address);
        }
        event.endpoint=h->src_ep;
        event.cluster=h->cluster_id;
        event.profile=h->profile_id;
        event.manufacturer=h->manuf_code;
        event.tsn=h->tsn;
    }
    return event;
}
static void attribute_event(const ezb_zcl_cmd_hdr_t *h, uint16_t attribute,
                            uint8_t type, const void *value, uint8_t status, bool report)
{
    if (!h) return;
    tg_zigbee_event_t event=from_header(TG_ZB_ATTRIBUTE, h);
    event.attribute=attribute;
    event.status=status;
    event.is_report=report;
    event.value_type=type;
    if (!status && value) {
        uint16_t size = ezb_zcl_get_attr_value_size(type, value);
        if (size > 0) {
            bool truncated = size > sizeof(event.value);
            event.value_len = truncated ? sizeof(event.value) : size;
            event.value_status = truncated ? TG_ZB_VALUE_TRUNCATED : TG_ZB_VALUE_COMPLETE;
            memcpy(event.value, value, event.value_len);
        }
    }
    if (valid_address(event.short_address)) peer_seen(event.short_address);
    emit(event);
}
static void zcl_action(ezb_zcl_core_action_callback_id_t id, void *message)
{
    if (!message) return;
    if (id == EZB_ZCL_CORE_REPORT_ATTR_CB_ID) {
        ezb_zcl_cmd_report_attr_message_t *m=message;
        for (ezb_zcl_report_attr_variable_t *v=m->in.variables; v; v=v->next)
            attribute_event(m->in.header,v->attr_id,v->attr_type,v->attr_value,0,true);
    } else if (id == EZB_ZCL_CORE_READ_ATTR_RSP_CB_ID) {
        ezb_zcl_cmd_read_attr_rsp_message_t *m=message;
        for (ezb_zcl_read_attr_rsp_variable_t *v=m->in.variables; v; v=v->next)
            attribute_event(m->in.header,v->attr_id,v->attr_type,v->attr_value,v->status,false);
    } else if (id == EZB_ZCL_CORE_DEFAULT_RSP_CB_ID) {
        ezb_zcl_cmd_default_rsp_message_t *m=message;
        tg_zigbee_event_t event=from_header(TG_ZB_DEFAULT_RESPONSE,m->in.header);
        event.status=m->in.status_code;
        event.command=m->in.rsp_to_cmd;
        emit(event);
    } else if (id == EZB_ZCL_CORE_CONFIG_REPORT_RSP_CB_ID) {
        ezb_zcl_cmd_config_report_rsp_message_t *m=message;
        for (ezb_zcl_config_report_rsp_variable_t *v=m->in.variables; v; v=v->next) {
            tg_zigbee_event_t event=from_header(TG_ZB_REPORT_CONFIG,m->in.header);
            event.attribute=v->attr_id; event.status=v->status;
            emit(event);
        }
    }
}
static void tx_confirm(ezb_af_user_cnf_t *cnf, void *ctx)
{
    if (!cnf) return;
    emit((tg_zigbee_event_t){.kind=TG_ZB_TX_CONFIRM, .request_id=(uint32_t)(uintptr_t)ctx,
        .short_address=cnf->dst_addr.u.short_addr, .endpoint=cnf->dst_ep,
        .cluster=cnf->cluster_id, .tsn=cnf->tsn, .status=cnf->status});
}

static void stack_task(void *arg)
{
    (void)arg;
    esp_zigbee_config_t config = {
        .device_config={.device_type=EZB_NWK_DEVICE_TYPE_COORDINATOR, .zczr_config={.max_children=10}},
        .platform_config={.storage_partition_name="zb_nvs", .radio_config={.radio_mode=ESP_ZIGBEE_RADIO_MODE_NATIVE}},
    };
    esp_err_t err=esp_zigbee_init(&config);
    if (err) goto fail;
#if CONFIG_ESP_COEX_SW_COEXIST_ENABLE && CONFIG_SOC_WIFI_SUPPORTED && CONFIG_SOC_IEEE802154_SUPPORTED
    /* The config switch alone does not enable Wi-Fi/802.15.4 arbitration.
     * Match the IDF Zigbee gateway's runtime opt-in before Wi-Fi can start. */
    err=esp_coex_wifi_i154_enable();
    if (err) goto fail;
    ESP_LOGI(TAG,"Wi-Fi/802.15.4 coexistence enabled");
#endif
    ezb_aps_secur_enable_distributed_security(false);
    if ((err=esp_zigbee_err_to_esp(ezb_bdb_set_primary_channel_set(1UL << CONFIG_TG_ZIGBEE_CHANNEL)))) goto fail;
    if ((err=esp_zigbee_err_to_esp(ezb_bdb_set_secondary_channel_set(0)))) goto fail;
    if ((err=esp_zigbee_err_to_esp(ezb_app_signal_add_handler(app_signal)))) goto fail;
    ezb_af_device_desc_t device=ezb_af_create_device_desc();
    ezb_zha_on_off_switch_config_t switch_config=EZB_ZHA_ON_OFF_SWITCH_CONFIG();
    ezb_af_ep_desc_t endpoint=ezb_zha_create_on_off_switch(COORDINATOR_ENDPOINT,&switch_config);
    if (!device || !endpoint) { err=ESP_ERR_NO_MEM; goto fail; }
    ezb_zcl_cluster_desc_t temperature=ezb_zcl_temperature_measurement_create_cluster_desc(NULL,EZB_ZCL_CLUSTER_CLIENT);
    if (!temperature) { err=ESP_ERR_NO_MEM; goto fail; }
    if ((err=esp_zigbee_err_to_esp(ezb_af_endpoint_add_cluster_desc(endpoint,temperature)))) goto fail;
    if ((err=esp_zigbee_err_to_esp(ezb_af_device_add_endpoint_desc(device,endpoint)))) goto fail;
    if ((err=esp_zigbee_err_to_esp(ezb_af_device_desc_register(device)))) goto fail;
    ezb_zcl_core_action_handler_register(zcl_action);
    if ((err=esp_zigbee_start(false))) goto fail;
    err=esp_zigbee_launch_mainloop();
fail:
    portENTER_CRITICAL(&s_lock);
    s_status.ready=false;
    portEXIT_CRITICAL(&s_lock);
    error_event(err ? err : ESP_FAIL);
    ESP_LOGE(TAG,"Stack stopped/error: %d; reboot required",err);
    vTaskDelete(NULL);
}

esp_err_t tg_zigbee_init(void)
{
    if (s_events) return ESP_ERR_INVALID_STATE;
    esp_err_t err=nvs_flash_init_partition("zb_nvs");
    if (err) return err; /* Never erase settings implicitly. */
    s_events=xQueueCreate(32,sizeof(tg_zigbee_event_t));
    if (!s_events) return ESP_ERR_NO_MEM;
    s_status=(tg_zigbee_status_t){.started=true,.peer_address=0xffff};
    if (xTaskCreate(stack_task,"tg_zigbee",6144,NULL,5,NULL) != pdPASS) {
        vQueueDelete(s_events); s_events=NULL; s_status.started=false;
        return ESP_ERR_NO_MEM;
    }
    return ESP_OK;
}
esp_err_t tg_zigbee_get_status(tg_zigbee_status_t *status)
{
    if (!status) return ESP_ERR_INVALID_ARG;
    portENTER_CRITICAL(&s_lock); *status=s_status; portEXIT_CRITICAL(&s_lock);
    return ESP_OK;
}
esp_err_t tg_zigbee_poll_event(tg_zigbee_event_t *event)
{
    if (!event) return ESP_ERR_INVALID_ARG;
    if (!s_events) return ESP_ERR_INVALID_STATE;
    return xQueueReceive(s_events,event,0) == pdTRUE ? ESP_OK : ESP_ERR_NOT_FOUND;
}
static esp_err_t lock_ready(void)
{
    tg_zigbee_status_t status; tg_zigbee_get_status(&status);
    if (!status.ready) return ESP_ERR_INVALID_STATE;
    return esp_zigbee_lock_acquire(pdMS_TO_TICKS(SDK_LOCK_MS)) ? ESP_OK : ESP_ERR_TIMEOUT;
}
esp_err_t tg_zigbee_permit_join(uint8_t seconds)
{
    if (seconds == 255) return ESP_ERR_INVALID_ARG;
    esp_err_t err=lock_ready(); if (err) return err;
    err=esp_zigbee_err_to_esp(ezb_bdb_open_network(seconds));
    esp_zigbee_lock_release(); return err;
}
esp_err_t tg_zigbee_discover(uint16_t address)
{
    if (!valid_address(address)) return ESP_ERR_INVALID_ARG;
    esp_err_t err=lock_ready(); if (err) return err;
    err=esp_zigbee_err_to_esp(discover_locked(address));
    esp_zigbee_lock_release(); return err;
}

esp_err_t tg_zigbee_resolve(uint16_t address)
{
    if (!valid_address(address)) return ESP_ERR_INVALID_ARG;
    esp_err_t err = lock_ready();
    if (err) return err;
    tg_zigbee_event_t event = {.kind=TG_ZB_IDENTITY, .short_address=address};
    lookup_ieee(&event);
    const uint8_t unknown[8] = {0};
    if (memcmp(event.ieee_address, unknown, sizeof(unknown))) {
        emit(event);
    } else {
        ezb_zdo_ieee_addr_req_t req = {
            .dst_nwk_addr=address,
            .field={.nwk_addr_of_interest=address, .request_type=ZDO_ADDR_REQUEST_TYPE_SINGLE_DEVICE},
            .cb=identity_result, .user_ctx=(void *)(uintptr_t)address,
        };
        err = esp_zigbee_err_to_esp(ezb_zdo_ieee_addr_req(&req));
    }
    esp_zigbee_lock_release();
    return err;
}

static bool valid_target(uint16_t address,uint8_t ep,uint32_t id)
{
    return valid_address(address) && ep && ep <= 240 && id;
}

esp_err_t tg_zigbee_address_for(const uint8_t ieee_address[8], uint16_t *address)
{
    if (!ieee_address || !address) return ESP_ERR_INVALID_ARG;
    esp_err_t err = lock_ready();
    if (err) return err;
    ezb_extaddr_t ieee;
    memcpy(ieee.u8, ieee_address, sizeof(ieee.u8));
    err = esp_zigbee_err_to_esp(ezb_address_short_by_extended(&ieee, address));
    esp_zigbee_lock_release();
    return err;
}
static ezb_zcl_cmd_ctrl_t control(uint16_t address,uint8_t ep,uint16_t cluster,uint32_t id)
{
    return (ezb_zcl_cmd_ctrl_t){.dst_addr={.addr_mode=EZB_ADDR_MODE_SHORT,.u.short_addr=address},
        .dst_ep=ep,.src_ep=COORDINATOR_ENDPOINT,.cluster_id=cluster,.manuf_code=EZB_ZCL_STD_MANUF_CODE,
        .cnf_ctx={.cb=tx_confirm,.user_ctx=(void *)(uintptr_t)id}};
}
esp_err_t tg_zigbee_read_attribute(uint16_t address,uint8_t ep,uint16_t cluster,uint16_t attribute,uint32_t id)
{
    if (!valid_target(address,ep,id)) return ESP_ERR_INVALID_ARG;
    esp_err_t err=lock_ready(); if (err) return err;
    ezb_zcl_read_attr_cmd_t req={.cmd_ctrl=control(address,ep,cluster,id),
        .payload={.attr_number=1,.attr_field=&attribute}};
    err=esp_zigbee_err_to_esp(ezb_zcl_read_attr_cmd_req(&req));
    esp_zigbee_lock_release(); return err;
}
esp_err_t tg_zigbee_send_onoff(uint16_t address,const uint8_t ieee_address[8],
                              uint8_t ep,uint8_t command,uint32_t id)
{
    if (!valid_target(address,ep,id) || !ieee_address || command > 2) return ESP_ERR_INVALID_ARG;
    esp_err_t err=lock_ready(); if (err) return err;
    uint8_t resolved[8];
    err = identity_at(address, resolved);
    if (err == ESP_OK && memcmp(resolved, ieee_address, sizeof(resolved))) {
        err = ESP_ERR_INVALID_RESPONSE;
    }
    if (err != ESP_OK) {
        esp_zigbee_lock_release();
        return err;
    }
    ezb_zcl_on_off_cmd_t req={.cmd_ctrl={
        .dst_addr={.addr_mode=EZB_ADDR_MODE_SHORT,.u.short_addr=address},
        .src_ep=COORDINATOR_ENDPOINT,.dst_ep=ep,
        .cnf_ctx={.cb=tx_confirm,.user_ctx=(void *)(uintptr_t)id}}};
    ezb_err_t result=command == 0 ? ezb_zcl_on_off_off_cmd_req(&req) :
        command == 1 ? ezb_zcl_on_off_on_cmd_req(&req) : ezb_zcl_on_off_toggle_cmd_req(&req);
    esp_zigbee_lock_release(); return esp_zigbee_err_to_esp(result);
}

static void bind_result(const ezb_zdp_bind_req_result_t *result, void *ctx)
{
    uint32_t id = (uint32_t)(uintptr_t)ctx;
    for (size_t i = 0; i < BIND_REQUESTS; ++i) {
        bind_context_t *slot = &s_bind_requests[i];
        if (slot->event.request_id != id) continue;

        tg_zigbee_event_t event = slot->event;
        slot->event.request_id = 0;
        event.status = ESP_FAIL;
        if (result && result->error) {
            event.status = result->error;
        } else if (result && result->rsp) {
            event.status = result->rsp->status;
        }
        emit(event);
        return;
    }
}

esp_err_t tg_zigbee_bind(uint16_t address,const uint8_t ieee_address[8],uint8_t ep,uint16_t cluster,uint32_t id)
{
    if (!valid_target(address,ep,id) || !ieee_address) return ESP_ERR_INVALID_ARG;
    esp_err_t err = lock_ready();
    if (err) return err;

    TickType_t now = xTaskGetTickCount();
    bind_context_t *slot = NULL;
    for (size_t i = 0; i < BIND_REQUESTS; ++i) {
        bind_context_t *candidate = &s_bind_requests[i];
        if (now - candidate->submitted_at >= pdMS_TO_TICKS(BIND_CONTEXT_MS)) {
            candidate->event.request_id = 0;
        }
        if (candidate->event.request_id == id) {
            esp_zigbee_lock_release();
            return ESP_ERR_INVALID_STATE;
        }
        if (!candidate->event.request_id && !slot) slot = candidate;
    }
    if (!slot) {
        esp_zigbee_lock_release();
        return ESP_ERR_NO_MEM;
    }

    slot->event = (tg_zigbee_event_t){.kind=TG_ZB_BIND, .request_id=id,
        .short_address=address, .endpoint=ep, .cluster=cluster};
    memcpy(slot->event.ieee_address, ieee_address, sizeof(slot->event.ieee_address));
    slot->submitted_at = now;
    ezb_zdo_bind_req_t req = {
        .dst_nwk_addr=address,
        .field={.src_ep=ep, .cluster_id=cluster,
            .dst_addr_mode=EZB_ADDR_MODE_EXT, .dst_ep=COORDINATOR_ENDPOINT},
        .cb=bind_result, .user_ctx=(void *)(uintptr_t)id,
    };
    memcpy(req.field.src_addr.u8, ieee_address, sizeof(req.field.src_addr.u8));
    ezb_nwk_get_extended_address(&req.field.dst_addr.extended_addr);
    err = esp_zigbee_err_to_esp(ezb_zdo_bind_req(&req));
    if (err != ESP_OK) slot->event.request_id = 0;
    esp_zigbee_lock_release();
    return err;
}

esp_err_t tg_zigbee_configure_reporting(uint16_t address,uint8_t ep,uint16_t cluster,
                                       const tg_zigbee_reporting_t *config,uint32_t id)
{
    if (!valid_target(address,ep,id) || !config) return ESP_ERR_INVALID_ARG;
    ezb_zcl_config_report_record_t record = {
        .direction=EZB_ZCL_REPORTING_SEND, .attr_id=config->attribute,
        .client={.attr_type=config->value_type, .min_interval=config->min_interval_s,
                 .max_interval=config->max_interval_s},
    };
    if (ezb_zcl_attr_type_is_analog(config->value_type)) {
        uint16_t size = ezb_zcl_get_attr_value_size(config->value_type, config->change);
        if (!size || size != config->change_len || size > sizeof(config->change) ||
            size > sizeof(record.client.reportable_change)) return ESP_ERR_INVALID_ARG;
        memcpy(&record.client.reportable_change, config->change, size);
    } else if (config->change_len) {
        return ESP_ERR_INVALID_ARG;
    }
    esp_err_t err=lock_ready(); if (err) return err;
    ezb_zcl_config_report_cmd_t req={.cmd_ctrl=control(address,ep,cluster,id),
        .payload={.record_number=1,.record_field=&record}};
    err=esp_zigbee_err_to_esp(ezb_zcl_config_report_cmd_req(&req));
    esp_zigbee_lock_release(); return err;
}
