#include "network.h"
#include "xgimi_wake.h"
#include "curtain.h"
#include "light_ring.h"
#include "voice.h"
#include <algorithm>
#include <atomic>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <cstring>
#include <cstdio>
#include <vector>
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
constexpr size_t kMaxWifiProfiles = 8;
constexpr size_t kMaxSettingsBytes = 8192;
constexpr char kNetworkNamespace[] = "mrli_net";
constexpr char kNetworkSettingsKey[] = "profiles_v1";
struct WifiProfile { std::string ssid, password; };
struct NetworkSettings { std::vector<WifiProfile> profiles; std::string token; };
using JsonPtr = std::unique_ptr<cJSON, decltype(&cJSON_Delete)>;

WifiProfile LoadLegacyWifi(const char* space, std::string* token = nullptr) {
    WifiProfile profile;
    nvs_handle_t handle;
    if (nvs_open(space, NVS_READONLY, &handle) == ESP_OK) {
        profile.ssid = Read(handle, "ssid");
        profile.password = Read(handle, "password");
        if (token) *token = Read(handle, "api_token");
        nvs_close(handle);
    }
    return profile;
}

bool ValidProfile(const WifiProfile& profile) {
    return !profile.ssid.empty() && profile.ssid.size() <= 32 && profile.password.size() <= 64;
}

bool ValidToken(const std::string& token) {
    return token.size() == 64 && token.find_first_not_of("0123456789abcdef") == std::string::npos;
}

WifiProfile ParseProfile(const cJSON* object) {
    const auto* ssid = cJSON_GetObjectItemCaseSensitive(object, "ssid");
    const auto* password = cJSON_GetObjectItemCaseSensitive(object, "password");
    if (!cJSON_IsString(ssid) || !cJSON_IsString(password))
        throw std::runtime_error("ssid and password required");
    WifiProfile profile{ssid->valuestring, password->valuestring};
    if (!ValidProfile(profile)) throw std::runtime_error("Invalid Wi-Fi credential length");
    return profile;
}

NetworkSettings LoadSettings() {
    NetworkSettings settings;
    nvs_handle_t handle;
    auto rc = nvs_open(kNetworkNamespace, NVS_READONLY, &handle);
    if (rc == ESP_ERR_NVS_NOT_FOUND) return settings;
    Check(rc, "Open network settings");
    struct Close { nvs_handle_t handle; ~Close() { nvs_close(handle); } } close{handle};

    size_t size = 0;
    rc = nvs_get_blob(handle, kNetworkSettingsKey, nullptr, &size);
    if (rc == ESP_ERR_NVS_NOT_FOUND) {
        WifiProfile legacy{Read(handle, "ssid"), Read(handle, "password")};
        settings.token = Read(handle, "api_token");
        if (ValidProfile(legacy)) settings.profiles.push_back(std::move(legacy));
        return settings;
    }
    Check(rc, "Read Wi-Fi profile size");
    if (size < 2 || size > kMaxSettingsBytes) throw std::runtime_error("Invalid saved Wi-Fi profile size");
    std::string data(size, '\0');
    Check(nvs_get_blob(handle, kNetworkSettingsKey, data.data(), &size), "Read Wi-Fi profiles");
    JsonPtr object(cJSON_ParseWithLengthOpts(data.data(), size, nullptr, true), cJSON_Delete);
    const auto* token = cJSON_GetObjectItemCaseSensitive(object.get(), "api_token");
    const auto* profiles = cJSON_GetObjectItemCaseSensitive(object.get(), "profiles");
    if (!cJSON_IsString(token) || !cJSON_IsArray(profiles) ||
        cJSON_GetArraySize(profiles) > static_cast<int>(kMaxWifiProfiles))
        throw std::runtime_error("Invalid saved Wi-Fi settings");
    settings.token = token->valuestring;
    const cJSON* item;
    cJSON_ArrayForEach(item, profiles) settings.profiles.push_back(ParseProfile(item));
    return settings;
}

void SaveSettings(const NetworkSettings& settings) {
    JsonPtr object(cJSON_CreateObject(), cJSON_Delete);
    auto* profiles = cJSON_AddArrayToObject(object.get(), "profiles");
    if (!profiles || !cJSON_AddStringToObject(object.get(), "api_token", settings.token.c_str()))
        throw std::runtime_error("Cannot encode Wi-Fi settings");
    for (const auto& profile : settings.profiles) {
        auto* entry = cJSON_CreateObject();
        if (!entry || !cJSON_AddItemToArray(profiles, entry)) {
            cJSON_Delete(entry);
            throw std::runtime_error("Cannot encode Wi-Fi profile");
        }
        if (!cJSON_AddStringToObject(entry, "ssid", profile.ssid.c_str()) ||
            !cJSON_AddStringToObject(entry, "password", profile.password.c_str()))
            throw std::runtime_error("Cannot encode Wi-Fi profile");
    }
    std::unique_ptr<char, decltype(&cJSON_free)> data(cJSON_PrintUnformatted(object.get()), cJSON_free);
    if (!data) throw std::runtime_error("Cannot encode Wi-Fi settings");
    nvs_handle_t handle;
    Check(nvs_open(kNetworkNamespace, NVS_READWRITE, &handle), "Open network settings");
    struct Close { nvs_handle_t handle; ~Close() { nvs_close(handle); } } close{handle};
    Check(nvs_set_blob(handle, kNetworkSettingsKey, data.get(), strlen(data.get()) + 1), "Save Wi-Fi profiles");
    Check(nvs_commit(handle), "Commit Wi-Fi settings");
}
std::mutex state_mutex;
std::string hostname, ip, startup_error;
std::string active_token;
std::vector<WifiProfile> wifi_profiles;
size_t active_profile = 0;
bool connected = false, running = false;
int disconnect_reason = 0;
std::atomic<int> event_count{0}, last_connect_result{0}, last_wifi_event{-1}, last_ip_event{-1};
esp_timer_handle_t reconnect_timer;

cJSON* Status() {
    std::lock_guard<std::mutex> lock(state_mutex);
    auto stored = LoadSettings();
    auto result = cJSON_CreateObject();
    cJSON_AddNumberToObject(result, "uptime_seconds", esp_timer_get_time() / 1000000);
    cJSON_AddNumberToObject(result, "reset_reason", esp_reset_reason());
    cJSON_AddBoolToObject(result, "configured", !stored.profiles.empty() && ValidToken(stored.token));
    cJSON_AddBoolToObject(result, "saved_wifi_available", !LoadLegacyWifi("wifi").ssid.empty());
    cJSON_AddBoolToObject(result, "running", running);
    const auto* ssid = !wifi_profiles.empty() ? wifi_profiles[active_profile].ssid.c_str() :
        stored.profiles.empty() ? "" : stored.profiles.front().ssid.c_str();
    cJSON_AddStringToObject(result, "ssid", ssid);
    auto* ssids = cJSON_AddArrayToObject(result, "saved_ssids");
    for (const auto& profile : stored.profiles) cJSON_AddItemToArray(ssids, cJSON_CreateString(profile.ssid.c_str()));
    cJSON_AddNumberToObject(result, "event_count", event_count.load());
    cJSON_AddNumberToObject(result, "last_wifi_event", last_wifi_event.load());
    cJSON_AddNumberToObject(result, "last_ip_event", last_ip_event.load());
    cJSON_AddNumberToObject(result, "last_connect_result", last_connect_result.load());
    wifi_ap_record_t ap = {};
    cJSON_AddBoolToObject(result, "associated", running && esp_wifi_sta_get_ap_info(&ap) == ESP_OK);
    cJSON_AddBoolToObject(result, "connected", connected);
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
        std::lock_guard<std::mutex> lock(state_mutex);
        connected = false;
        ip.clear();
        disconnect_reason = static_cast<wifi_event_sta_disconnected_t*>(data)->reason;
    }
    if (base == IP_EVENT && id == IP_EVENT_STA_GOT_IP) {
        auto event = static_cast<ip_event_got_ip_t*>(data);
        char address[16];
        snprintf(address, sizeof(address), IPSTR, IP2STR(&event->ip_info.ip));
        std::lock_guard<std::mutex> lock(state_mutex);
        wifi_ap_record_t ap = {};
        if (esp_wifi_sta_get_ap_info(&ap) != ESP_OK ||
            wifi_profiles[active_profile].ssid != reinterpret_cast<const char*>(ap.ssid)) return;
        ip = address;
        disconnect_reason = 0;
        connected = true;
    }
    if (base == IP_EVENT && id == IP_EVENT_STA_LOST_IP) {
        std::lock_guard<std::mutex> lock(state_mutex);
        connected = false;
        ip.clear();
    }
}

esp_err_t SelectWifi(const WifiProfile& profile) {
    wifi_config_t config = {};
    memcpy(config.sta.ssid, profile.ssid.data(), profile.ssid.size());
    memcpy(config.sta.password, profile.password.data(), profile.password.size());
    config.sta.pmf_cfg.capable = true;
    return esp_wifi_set_config(WIFI_IF_STA, &config);
}

void RetryWifi(void*) {
    std::lock_guard<std::mutex> lock(state_mutex);
    if (connected) return;
    // Cancel a scan/association or an AP that failed to provide a DHCP lease.
    esp_wifi_disconnect();
    if (wifi_profiles.size() > 1) {
        const auto next = (active_profile + 1) % wifi_profiles.size();
        const auto rc = SelectWifi(wifi_profiles[next]);
        if (rc != ESP_OK) {
            last_connect_result = rc;
            return;
        }
        active_profile = next;
    }
    last_connect_result = esp_wifi_connect();
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
    struct Params { cJSON* value = nullptr; ~Params() { cJSON_Delete(value); } } params;
    try {
        const std::string uri = request->uri;
#if CONFIG_IDF_TARGET_ESP32S3
        if (uri == "/api/voice/recording.wav") return SendWakeRecording(request);
        if (uri == "/api/voice/status") return Json(request, "200 OK", HandleVoiceCommand("voice.status", nullptr));
        if (uri == "/api/light/status") return Json(request, "200 OK", HandleLightRingCommand("light.status", nullptr));
#endif
        if (request->method == HTTP_GET) {
            if (uri == "/api/curtain/status") return Json(request, "200 OK", HandleCurtainCommand("curtain.status", nullptr));
            auto result = cJSON_CreateObject();
            cJSON_AddStringToObject(result, "device", hostname.c_str());
            cJSON_AddItemToObject(result, "network", Status());
            cJSON_AddItemToObject(result, "xgimi", HandleXgimiCommand("xgimi.status", nullptr));
            cJSON_AddItemToObject(result, "curtain", HandleCurtainCommand("curtain.status", nullptr));
#if CONFIG_IDF_TARGET_ESP32S3
            cJSON_AddItemToObject(result, "light", HandleLightRingCommand("light.status", nullptr));
#endif
            return Json(request, "200 OK", result);
        }
        if (uri == "/api/xgimi/power_on") {
            if (request->content_len) return Error(request, "400 Bad Request", "Send an empty POST body");
            return Json(request, "202 Accepted", HandleXgimiCommand("xgimi.power_on", nullptr));
        }
        if (request->content_len > 512) return Error(request, "413 Content Too Large", "JSON body exceeds 512 bytes");
        std::string body(request->content_len, '\0');
        size_t received = 0;
        while (received < body.size()) {
            int n = httpd_req_recv(request, body.data() + received, body.size() - received);
            if (n <= 0) return Error(request, "400 Bad Request", "Incomplete request body");
            received += n;
        }
        params.value = body.empty() ? cJSON_CreateObject() : cJSON_Parse(body.c_str());
        if (!cJSON_IsObject(params.value)) return Error(request, "400 Bad Request", "Expected JSON object");
#if CONFIG_IDF_TARGET_ESP32S3
        if (uri == "/api/voice/listen") return Json(request, "202 Accepted", HandleVoiceCommand("voice.listen", params.value));
        if (uri == "/api/light/mode") return Json(request, "200 OK", HandleLightRingCommand("light.set_mode", params.value));
#endif
        if (uri == "/api/curtain/enable")
            return Json(request, "200 OK", HandleCurtainCommand("curtain.enable", params.value));
        const std::string learning = "/api/curtain/learn/";
        if (uri.rfind(learning, 0) == 0) {
            auto action = uri.substr(learning.size());
            cJSON_DeleteItemFromObjectCaseSensitive(params.value, "action");
            cJSON_AddStringToObject(params.value, "action", action.c_str());
            return Json(request, "202 Accepted", HandleCurtainCommand("curtain.learn", params.value));
        }
        if (request->content_len) return Error(request, "400 Bad Request", "Control commands require empty body");
        auto action = uri.substr(std::string("/api/curtain/").size());
        return Json(request, "202 Accepted", HandleCurtainCommand("curtain." + action, nullptr));
    } catch (const std::invalid_argument& e) {
        return Error(request, "400 Bad Request", e.what());
    } catch (const std::exception& e) {
        return Error(request, "409 Conflict", e.what());
    }
}
void Start() {
    auto settings = LoadSettings();
    uint8_t mac[6];
    Check(esp_read_mac(mac, ESP_MAC_WIFI_STA), "Read MAC");
    char name[32];
    snprintf(name, sizeof(name), "mr-li-esp32-%02x%02x%02x", mac[3], mac[4], mac[5]);
    hostname = name;
    if (settings.profiles.empty() || settings.token.empty()) return;
    if (!ValidToken(settings.token))
        throw std::runtime_error("Invalid saved network settings; configure via USB again");
    wifi_profiles = std::move(settings.profiles);
    active_token = std::move(settings.token);
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
    Check(esp_wifi_set_mode(WIFI_MODE_STA), "Set station mode");
    Check(SelectWifi(wifi_profiles.front()), "Set Wi-Fi configuration");
    Check(esp_wifi_start(), "Start Wi-Fi");
    last_connect_result = esp_wifi_connect();
    // Wi-Fi/BLE coexistence is enabled; retain the controller's power-save default.
    Check(mdns_init(), "Initialize mDNS");
    Check(mdns_hostname_set(hostname.c_str()), "Set mDNS name");
    Check(mdns_instance_name_set("mr-li ESP32 hardware"), "Set mDNS instance");
    Check(mdns_service_add(nullptr, "_http", "_tcp", 80, nullptr, 0), "Advertise HTTP");
    httpd_config_t http = HTTPD_DEFAULT_CONFIG();
    http.stack_size = 8192;
    http.max_open_sockets = 3;
    http.max_uri_handlers = 16;
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
    const char* rf_paths[] = {"/api/curtain/status", "/api/curtain/up", "/api/curtain/down", "/api/curtain/stop", "/api/curtain/learn/up", "/api/curtain/learn/down", "/api/curtain/learn/stop", "/api/curtain/enable"};
    for (int i = 0; i < 8; ++i) {
        httpd_uri_t route = {};
        route.uri = rf_paths[i]; route.method = i == 0 ? HTTP_GET : HTTP_POST; route.handler = Request;
        Check(httpd_register_uri_handler(server, &route), "Register curtain API");
    }
#if CONFIG_IDF_TARGET_ESP32S3
    const httpd_uri_t s3_paths[] = {
        {.uri = "/api/voice/status", .method = HTTP_GET, .handler = Request, .user_ctx = nullptr},
        {.uri = "/api/voice/recording.wav", .method = HTTP_GET, .handler = Request, .user_ctx = nullptr},
        {.uri = "/api/voice/listen", .method = HTTP_POST, .handler = Request, .user_ctx = nullptr},
        {.uri = "/api/light/status", .method = HTTP_GET, .handler = Request, .user_ctx = nullptr},
        {.uri = "/api/light/mode", .method = HTTP_POST, .handler = Request, .user_ctx = nullptr},
    };
    for (const auto& route : s3_paths) Check(httpd_register_uri_handler(server, &route), "Register S3 API");
#endif
    esp_timer_create_args_t timer = {};
    timer.callback = RetryWifi;
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
    if (method == "network.forget") {
        const auto* ssid = cJSON_GetObjectItemCaseSensitive(params, "ssid");
        if (!cJSON_IsString(ssid) || !*ssid->valuestring) throw std::runtime_error("ssid required");
        std::lock_guard<std::mutex> lock(state_mutex);
        auto settings = LoadSettings();
        const auto found = std::find_if(settings.profiles.begin(), settings.profiles.end(),
            [&](const WifiProfile& profile) { return profile.ssid == ssid->valuestring; });
        const bool removed = found != settings.profiles.end();
        if (removed) {
            settings.profiles.erase(found);
            SaveSettings(settings);
        }
        auto result = cJSON_CreateObject();
        cJSON_AddBoolToObject(result, "removed", removed);
        cJSON_AddBoolToObject(result, "restart_required", removed);
        cJSON_AddNumberToObject(result, "saved_wifi_count", settings.profiles.size());
        return result;
    }
    if (method != "network.configure") throw std::runtime_error("Unknown network method");
    const auto* saved = cJSON_GetObjectItemCaseSensitive(params, "use_saved_wifi");
    WifiProfile profile;
    if (cJSON_IsTrue(saved)) {
        if (cJSON_HasObjectItem(params, "ssid") || cJSON_HasObjectItem(params, "password"))
            throw std::runtime_error("Do not combine use_saved_wifi with credentials");
        profile = LoadLegacyWifi("wifi");
        if (!ValidProfile(profile)) throw std::runtime_error("No valid original Wi-Fi found; provide ssid and password via USB");
    } else {
        profile = ParseProfile(params);
    }

    std::lock_guard<std::mutex> lock(state_mutex);
    auto settings = LoadSettings();
    if (const auto* token = cJSON_GetObjectItemCaseSensitive(params, "api_token")) {
        if (!cJSON_IsString(token)) throw std::runtime_error("api_token must be a string");
        settings.token = token->valuestring;
    }
    if (!ValidToken(settings.token))
        throw std::runtime_error("api_token must contain 64 lowercase hex characters");
    const auto existing = std::find_if(settings.profiles.begin(), settings.profiles.end(),
        [&](const WifiProfile& saved_profile) { return saved_profile.ssid == profile.ssid; });
    if (existing != settings.profiles.end()) {
        existing->password = std::move(profile.password);
    } else {
        if (settings.profiles.size() >= kMaxWifiProfiles) throw std::runtime_error("Wi-Fi list is full (8 networks)");
        settings.profiles.push_back(std::move(profile));
    }
    SaveSettings(settings);
    auto result = cJSON_CreateObject();
    cJSON_AddBoolToObject(result, "saved", true);
    cJSON_AddBoolToObject(result, "restart_required", true);
    cJSON_AddNumberToObject(result, "saved_wifi_count", settings.profiles.size());
    return result;
}
