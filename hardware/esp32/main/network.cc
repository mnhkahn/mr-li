#include "network.h"
#include "xgimi_wake.h"
#include <atomic>
#include <mutex>
#include <stdexcept>
#include <cstring>
#include <cstdio>
#include <esp_event.h>
#include <esp_http_server.h>
#include <esp_mac.h>
#include <esp_netif.h>
#include <esp_system.h>
#include <esp_timer.h>
#include <esp_wifi.h>
#include <nvs.h>
#include <mdns.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>

namespace {
void Check(esp_err_t rc, const char* operation) {
    if (rc != ESP_OK) throw std::runtime_error(std::string(operation) + ": " + esp_err_to_name(rc));
}
std::string Read(nvs_handle_t handle, const char* key) {
    size_t size = 0;
    if (nvs_get_str(handle, key, nullptr, &size) != ESP_OK || size > 256 || !size) return {};
    std::string value(size, '\0');
    if (nvs_get_str(handle, key, value.data(), &size) != ESP_OK) return {};
    value.resize(size - 1);
    return value;
}
struct Credentials { std::string ssid, password, token; };
Credentials Load(const char* space) {
    Credentials c;
    nvs_handle_t handle;
    if (nvs_open(space, NVS_READONLY, &handle) == ESP_OK) {
        c.ssid = Read(handle, "ssid");
        c.password = Read(handle, "password");
        c.token = Read(handle, "api_token");
        nvs_close(handle);
    }
    return c;
}
std::mutex state_mutex;
std::string hostname, ip, startup_error;
std::string active_token;
std::atomic<bool> connected{false};
bool running = false;
int disconnect_reason = 0;
std::atomic<int> event_count{0}, last_connect_result{0}, last_wifi_event{-1}, last_ip_event{-1};
esp_timer_handle_t reconnect_timer;

cJSON* Status() {
    std::lock_guard<std::mutex> lock(state_mutex);
    auto stored = Load("mrli_net");
    auto result = cJSON_CreateObject();
    cJSON_AddNumberToObject(result, "uptime_seconds", esp_timer_get_time() / 1000000);
    cJSON_AddNumberToObject(result, "reset_reason", esp_reset_reason());
    cJSON_AddBoolToObject(result, "configured", !stored.ssid.empty() && !stored.token.empty());
    cJSON_AddBoolToObject(result, "saved_wifi_available", !Load("wifi").ssid.empty());
    cJSON_AddBoolToObject(result, "running", running);
    cJSON_AddStringToObject(result, "ssid", stored.ssid.c_str());
    cJSON_AddNumberToObject(result, "event_count", event_count.load());
    cJSON_AddNumberToObject(result, "last_wifi_event", last_wifi_event.load());
    cJSON_AddNumberToObject(result, "last_ip_event", last_ip_event.load());
    cJSON_AddNumberToObject(result, "last_connect_result", last_connect_result.load());
    wifi_ap_record_t ap = {};
    cJSON_AddBoolToObject(result, "associated", running && esp_wifi_sta_get_ap_info(&ap) == ESP_OK);
    cJSON_AddBoolToObject(result, "connected", connected.load());
    cJSON_AddStringToObject(result, "hostname", hostname.c_str());
    cJSON_AddStringToObject(result, "ip", ip.c_str());
    cJSON_AddNumberToObject(result, "disconnect_reason", disconnect_reason);
    cJSON_AddStringToObject(result, "error", startup_error.c_str());
    return result;
}
void Event(void*, esp_event_base_t base, int32_t id, void* data) {
    ++event_count;
    if (base == WIFI_EVENT) last_wifi_event = id;
    if (base == IP_EVENT) last_ip_event = id;
    if (base == WIFI_EVENT && id == WIFI_EVENT_STA_DISCONNECTED) {
        connected = false;
        std::lock_guard<std::mutex> lock(state_mutex);
        ip.clear();
        disconnect_reason = static_cast<wifi_event_sta_disconnected_t*>(data)->reason;
    }
    if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        auto event = static_cast<ip_event_got_ip_t*>(data);
        char address[16];
        snprintf(address, sizeof(address), IPSTR, IP2STR(&event->ip_info.ip));
        std::lock_guard<std::mutex> lock(state_mutex);
        ip = address;
        disconnect_reason = 0;
        connected = true;
    }
    if (base == IP_EVENT && id == IP_EVENT_STA_LOST_IP) {
        connected = false;
        std::lock_guard<std::mutex> lock(state_mutex);
        ip.clear();
    }
}
esp_err_t Json(httpd_req_t* request, const char* status, cJSON* value) {
    char* output = cJSON_PrintUnformatted(value);
    cJSON_Delete(value);
    httpd_resp_set_status(request, status);
    httpd_resp_set_type(request, "application/json");
    httpd_resp_set_hdr(request, "Cache-Control", "no-store");
    // Low-rate control traffic: do not retain sockets across Wi-Fi reconnects/reboots.
    httpd_resp_set_hdr(request, "Connection", "close");
    esp_err_t rc = output ? httpd_resp_sendstr(request, output) : ESP_ERR_NO_MEM;
    cJSON_free(output);
    return rc;
}
esp_err_t Error(httpd_req_t* request, const char* status, const char* message) {
    auto result = cJSON_CreateObject();
    cJSON_AddStringToObject(result, "error", message);
    return Json(request, status, result);
}
bool Authorized(httpd_req_t* request) {
    const size_t size = httpd_req_get_hdr_value_len(request, "Authorization");
    if (!size || size > 128) return false;
    char header[129] = {};
    if (httpd_req_get_hdr_value_str(request, "Authorization", header, sizeof(header)) != ESP_OK) return false;
    const std::string expected = "Bearer " + active_token;
    if (active_token.empty() || size != expected.size()) return false;
    unsigned diff = 0;
    for (size_t i = 0; i < size; ++i) diff |= static_cast<unsigned char>(header[i]) ^ static_cast<unsigned char>(expected[i]);
    return diff == 0;
}
esp_err_t Request(httpd_req_t* request) {
    if (!Authorized(request)) return Error(request, "401 Unauthorized", "Bearer token required");
    try {
        if (request->method == HTTP_GET) {
            auto result = cJSON_CreateObject();
            cJSON_AddStringToObject(result, "device", hostname.c_str());
            cJSON_AddItemToObject(result, "network", Status());
            cJSON_AddItemToObject(result, "xgimi", HandleXgimiCommand("xgimi.status", nullptr));
            return Json(request, "200 OK", result);
        }
        if (request->content_len) return Error(request, "400 Bad Request", "Send an empty POST body");
        return Json(request, "202 Accepted", HandleXgimiCommand("xgimi.power_on", nullptr));
    } catch (const std::exception& e) {
        return Error(request, "409 Conflict", e.what());
    }
}
void Start() {
    auto c = Load("mrli_net");
    uint8_t mac[6];
    Check(esp_read_mac(mac, ESP_MAC_WIFI_STA), "Read MAC");
    char name[32];
    snprintf(name, sizeof(name), "mr-li-esp32-%02x%02x%02x", mac[3], mac[4], mac[5]);
    hostname = name;
    if (c.ssid.empty() || c.token.empty()) return;
    if (c.ssid.size() > 32 || c.password.size() > 64 || c.token.size() != 64)
        throw std::runtime_error("Invalid saved network settings; configure via USB again");
    active_token = c.token;
    // Initialize both radios before accepting network wake commands.
    // Do not advertise or change the learned counter during boot.
    InitializeXgimiBluetooth();
    Check(esp_netif_init(), "Initialize network");
    Check(esp_event_loop_create_default(), "Create event loop");
    auto interface = esp_netif_create_default_wifi_sta();
    if (!interface) throw std::runtime_error("Create Wi-Fi interface failed");
    Check(esp_netif_set_hostname(interface, hostname.c_str()), "Set hostname");
    wifi_init_config_t init = WIFI_INIT_CONFIG_DEFAULT();
    Check(esp_wifi_init(&init), "Initialize Wi-Fi");
    Check(esp_wifi_set_storage(WIFI_STORAGE_RAM), "Set Wi-Fi storage");
    Check(esp_event_handler_register(WIFI_EVENT, ESP_EVENT_ANY_ID, Event, nullptr), "Register Wi-Fi events");
    Check(esp_event_handler_register(IP_EVENT, ESP_EVENT_ANY_ID, Event, nullptr), "Register IP events");
    wifi_config_t config = {};
    memcpy(config.sta.ssid, c.ssid.data(), c.ssid.size());
    memcpy(config.sta.password, c.password.data(), c.password.size());
    config.sta.pmf_cfg.capable = true;
    Check(esp_wifi_set_mode(WIFI_MODE_STA), "Set station mode");
    Check(esp_wifi_set_config(WIFI_IF_STA, &config), "Set Wi-Fi configuration");
    Check(esp_wifi_start(), "Start Wi-Fi");
    last_connect_result = esp_wifi_connect();
    Check(last_connect_result.load(), "Connect Wi-Fi");
    // Wi-Fi/BLE coexistence is enabled; retain the controller's power-save default.
    Check(mdns_init(), "Initialize mDNS");
    Check(mdns_hostname_set(hostname.c_str()), "Set mDNS name");
    Check(mdns_instance_name_set("mr-li ESP32 hardware"), "Set mDNS instance");
    Check(mdns_service_add(nullptr, "_http", "_tcp", 80, nullptr, 0), "Advertise HTTP");
    httpd_config_t http = HTTPD_DEFAULT_CONFIG();
    http.stack_size = 8192;
    http.max_open_sockets = 3;
    http.lru_purge_enable = true;
    http.recv_wait_timeout = 3;
    http.send_wait_timeout = 3;
    httpd_handle_t server;
    Check(httpd_start(&server, &http), "Start HTTP server");
    httpd_uri_t status = {};
    status.uri = "/api/status"; status.method = HTTP_GET; status.handler = Request;
    Check(httpd_register_uri_handler(server, &status), "Register status API");
    httpd_uri_t wake = {};
    wake.uri = "/api/xgimi/power_on"; wake.method = HTTP_POST; wake.handler = Request;
    Check(httpd_register_uri_handler(server, &wake), "Register wake API");
    esp_timer_create_args_t timer = {};
    timer.callback = [](void*) { if (!connected.load()) last_connect_result = esp_wifi_connect(); };
    timer.name = "wifi_reconnect";
    Check(esp_timer_create(&timer, &reconnect_timer), "Create reconnect timer");
    Check(esp_timer_start_periodic(reconnect_timer, 10000000), "Start reconnect timer");
    std::lock_guard<std::mutex> lock(state_mutex);
    running = true;
}
}
void StartHardwareNetwork() {
    try { Start(); }
    catch (const std::exception& e) {
        std::lock_guard<std::mutex> lock(state_mutex);
        startup_error = e.what();
    }
}
cJSON* HandleNetworkCommand(const std::string& method, const cJSON* params) {
    if (method == "network.status") return Status();
    if (method == "network.restart") {
        if (xTaskCreate([](void*) { vTaskDelay(pdMS_TO_TICKS(1500)); esp_restart(); },
                        "restart", 2048, nullptr, 3, nullptr) != pdPASS)
            throw std::runtime_error("Cannot schedule restart");
        auto result = cJSON_CreateObject();
        cJSON_AddBoolToObject(result, "restarting", true);
        return result;
    }
    if (method != "network.configure") throw std::runtime_error("Unknown network method");
    auto token = cJSON_GetObjectItemCaseSensitive(params, "api_token");
    auto ssid = cJSON_GetObjectItemCaseSensitive(params, "ssid");
    auto password = cJSON_GetObjectItemCaseSensitive(params, "password");
    auto saved = cJSON_GetObjectItemCaseSensitive(params, "use_saved_wifi");
    Credentials c;
    if (cJSON_IsTrue(saved)) {
        if (ssid || password) throw std::runtime_error("Do not combine use_saved_wifi with credentials");
        c = Load("wifi");
        if (c.ssid.empty()) throw std::runtime_error("No saved Wi-Fi found; provide ssid and password via USB");
    } else {
        if (!cJSON_IsString(ssid) || !cJSON_IsString(password)) throw std::runtime_error("ssid and password required");
        c.ssid = ssid->valuestring; c.password = password->valuestring;
    }
    if (c.ssid.empty() || c.ssid.size() > 32 || c.password.size() > 64)
        throw std::runtime_error("Invalid Wi-Fi credential length");
    if (!cJSON_IsString(token)) throw std::runtime_error("api_token required");
    c.token = token->valuestring;
    if (c.token.size() != 64 || c.token.find_first_not_of("0123456789abcdef") != std::string::npos)
        throw std::runtime_error("api_token must contain 64 lowercase hex characters");
    nvs_handle_t handle;
    Check(nvs_open("mrli_net", NVS_READWRITE, &handle), "Open network settings");
    auto rc = nvs_set_str(handle, "ssid", c.ssid.c_str());
    if (rc == ESP_OK) rc = nvs_set_str(handle, "password", c.password.c_str());
    if (rc == ESP_OK) rc = nvs_set_str(handle, "api_token", c.token.c_str());
    if (rc == ESP_OK) rc = nvs_commit(handle);
    nvs_close(handle);
    Check(rc, "Save network settings");
    auto result = cJSON_CreateObject();
    cJSON_AddBoolToObject(result, "saved", true);
    cJSON_AddBoolToObject(result, "restart_required", true);
    return result;
}
