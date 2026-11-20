#include "xgimi_wake.h"
#include "xgimi_wake_protocol.h"
#include <cJSON.h>
#include <mutex>
#include <stdexcept>
#include <esp_log.h>
#include <nvs.h>
#include <freertos/FreeRTOS.h>
#include <freertos/event_groups.h>
#include <freertos/task.h>
#include <nimble/nimble_port.h>
#include <nimble/nimble_port_freertos.h>
#include <host/ble_hs.h>
#include <host/ble_gap.h>
#include <host/util/util.h>

namespace {
constexpr EventBits_t kSynced = 1, kDone = 2;
class XgimiWake {
    std::mutex mutex_;
    EventGroupHandle_t events_ = xEventGroupCreate();
    bool initialized_ = false;
    bool busy_ = false;
    bool learning_ = false;
    bool reset_ = false;
    int completion_reason_ = 0;
    std::string state_ = "idle", error_;
    std::vector<uint8_t> candidate_;
    bool configured_ = false;

    static void Check(int rc, const char* operation) {
        if (rc != 0) throw std::runtime_error(std::string(operation) + " failed: " + std::to_string(rc));
    }
    std::vector<uint8_t> Load() {
        nvs_handle_t nvs;
        int rc = nvs_open("xgimi_wake", NVS_READONLY, &nvs);
        if (rc == ESP_ERR_NVS_NOT_FOUND) return {};
        Check(rc, "Open XGIMI settings");
        char hex[33] = {};
        size_t size = sizeof(hex);
        rc = nvs_get_str(nvs, "token", hex, &size);
        nvs_close(nvs);
        if (rc == ESP_ERR_NVS_NOT_FOUND) return {};
        Check(rc, "Read XGIMI token");
        return xgimi::ParseToken(hex);
    }
    void Save(const std::vector<uint8_t>& token) {
        nvs_handle_t nvs;
        Check(nvs_open("xgimi_wake", NVS_READWRITE, &nvs), "Open XGIMI settings");
        int rc = nvs_set_str(nvs, "token", xgimi::Hex(token).c_str());
        if (rc == ESP_OK) rc = nvs_commit(nvs);
        nvs_close(nvs);
        Check(rc, "Save XGIMI token");
    }
    static void HostTask(void*) { nimble_port_run(); nimble_port_freertos_deinit(); }
    void EnsureReady() {
        if (!events_) throw std::runtime_error("No memory for Bluetooth events");
        if (!initialized_) {
            Check(nimble_port_init(), "Initialize NimBLE");
            ble_hs_cfg.sync_cb = [] { xEventGroupSetBits(Instance().events_, kSynced); };
            ble_hs_cfg.reset_cb = [](int) {
                auto& self = Instance();
                { std::lock_guard<std::mutex> lock(self.mutex_); self.reset_ = true; }
                xEventGroupClearBits(self.events_, kSynced);
                xEventGroupSetBits(self.events_, kDone);
            };
            initialized_ = true;
            nimble_port_freertos_init(HostTask);
        }
        if (!(xEventGroupWaitBits(events_, kSynced, pdFALSE, pdTRUE, pdMS_TO_TICKS(5000)) & kSynced))
            throw std::runtime_error("Bluetooth synchronization timed out");
    }
    static int GapEvent(ble_gap_event* event, void*) {
        auto& self = Instance();
        if (event->type == BLE_GAP_EVENT_DISC) {
            auto token = xgimi::FindToken(event->disc.data, event->disc.length_data);
            if (!token.empty()) {
                std::lock_guard<std::mutex> lock(self.mutex_);
                if (self.learning_ && self.candidate_.empty()) {
                    self.candidate_ = token;
                    xEventGroupSetBits(self.events_, kDone);
                }
            }
        } else if (event->type == BLE_GAP_EVENT_ADV_COMPLETE || event->type == BLE_GAP_EVENT_DISC_COMPLETE) {
            std::lock_guard<std::mutex> lock(self.mutex_);
            if (self.busy_ && ((self.learning_ && event->type == BLE_GAP_EVENT_DISC_COMPLETE) ||
                              (!self.learning_ && event->type == BLE_GAP_EVENT_ADV_COMPLETE))) {
                self.completion_reason_ = self.learning_ ? event->disc_complete.reason : event->adv_complete.reason;
                xEventGroupSetBits(self.events_, kDone);
            }
        }
        return 0;
    }
    void Run(bool learn) {
        try {
            EnsureReady();
            uint8_t own_type;
            Check(ble_hs_util_ensure_addr(0), "Ensure Bluetooth address");
            Check(ble_hs_id_infer_auto(0, &own_type), "Get Bluetooth address");
            xEventGroupClearBits(events_, kDone);
            {
                std::lock_guard<std::mutex> lock(mutex_);
                state_ = learn ? "learning" : "advertising";
            }
            if (learn) {
                ble_gap_disc_params params = {};
                params.passive = 1;
                params.itvl = 0x50;
                params.window = 0x30;
                Check(ble_gap_disc(own_type, 20000, &params, GapEvent, nullptr), "Start remote learning");
                xEventGroupWaitBits(events_, kDone, pdTRUE, pdFALSE, pdMS_TO_TICKS(21000));
                ble_gap_disc_cancel();
                std::vector<uint8_t> token;
                {
                    std::lock_guard<std::mutex> lock(mutex_);
                    learning_ = false;
                    token = candidate_;
                }
                if (token.empty()) throw std::runtime_error("No XGIMI wake packet found; press the original remote power button near this device while projector is off");
                Save(token);
                std::lock_guard<std::mutex> lock(mutex_);
                configured_ = true;
            } else {
                auto token = Load();
                if (token.empty()) throw std::runtime_error("Learn or configure this projector wake token first");
                // Use a fresh counter, persisted before transmission so repeated wake
                // requests (including after reboot) do not replay a frozen counter.
                if (token.size() == 16) { ++token[0]; Save(token); }
                auto data = xgimi::Advertisement(token);
                Check(ble_gap_adv_set_data(data.data(), data.size()), "Set XGIMI advertisement");
                // The legacy packet is full; expose the original remote name
                // separately to scanners, without changing this board's identity.
                const uint8_t scan_response[] = {
                    0x11, 0x09, 'B', 'l', 'u', 'e', 't', 'o', 'o', 't', 'h',
                    ' ', '4', '.', '0', ' ', 'R', 'C'
                };
                Check(ble_gap_adv_rsp_set_data(scan_response, sizeof(scan_response)), "Set XGIMI scan response");
                ble_gap_adv_params params = {};
                params.conn_mode = BLE_GAP_CONN_MODE_NON;
                params.disc_mode = BLE_GAP_DISC_MODE_GEN;
                params.itvl_min = 0x30;
                params.itvl_max = 0x40;
                Check(ble_gap_adv_start(own_type, nullptr, 3000, &params, GapEvent, nullptr), "Start XGIMI wake advertisement");
                auto result = xEventGroupWaitBits(events_, kDone, pdTRUE, pdFALSE, pdMS_TO_TICKS(4000));
                ble_gap_adv_stop();
                if (!(result & kDone)) throw std::runtime_error("Bluetooth advertisement completion timed out");
            }
            std::lock_guard<std::mutex> lock(mutex_);
            if (reset_) throw std::runtime_error("Bluetooth host reset during operation");
            if (!learn && completion_reason_ != BLE_HS_ETIMEOUT)
                throw std::runtime_error("Bluetooth advertisement ended unexpectedly: " + std::to_string(completion_reason_));
            state_ = learn ? "learned" : "transmitted";
            busy_ = false;
        } catch (const std::exception& e) {
            std::lock_guard<std::mutex> lock(mutex_);
            state_ = "error"; error_ = e.what(); busy_ = false; learning_ = false;
            ESP_LOGW("XgimiWake", "%s", e.what());
        }
    }
public:
    static XgimiWake& Instance() { static XgimiWake instance; return instance; }
    void Initialize() { EnsureReady(); }
    cJSON* Status() {
        std::lock_guard<std::mutex> lock(mutex_);
        configured_ = !Load().empty();
        auto result = cJSON_CreateObject();
        cJSON_AddBoolToObject(result, "configured", configured_);
        cJSON_AddBoolToObject(result, "busy", busy_);
        cJSON_AddStringToObject(result, "state", state_.c_str());
        cJSON_AddStringToObject(result, "error", error_.c_str());
        cJSON_AddBoolToObject(result, "physical_effect_confirmed", false);
        return result;
    }
    cJSON* Start(bool learn) {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (busy_) throw std::runtime_error("XGIMI Bluetooth operation already in progress");
            if (!learn && Load().empty()) throw std::runtime_error("Learn or configure the wake token first");
            if (!events_) throw std::runtime_error("No memory for Bluetooth events");
            busy_ = true; learning_ = learn; reset_ = false; completion_reason_ = 0; candidate_.clear(); error_.clear();
            state_ = "initializing";
            auto task = [](void* arg) {
                Instance().Run(arg != nullptr);
                vTaskDelete(nullptr);
            };
            if (xTaskCreate(task, "xgimi_wake", 6144, learn ? this : nullptr, 2, nullptr) != pdPASS) {
                busy_ = false; learning_ = false; state_ = "error"; error_ = "No memory for Bluetooth worker";
                throw std::runtime_error(error_);
            }
        }
        auto result = Status();
        cJSON_AddBoolToObject(result, "accepted", true);
        return result;
    }
    cJSON* Configure(const std::string& hex) {
        auto token = xgimi::ParseToken(hex);
        if (token.empty()) throw std::runtime_error("Expected 30 or 32 hexadecimal token characters, excluding manufacturer ID 4600, with XGIMI wake suffix");
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (busy_) throw std::runtime_error("Wait for current Bluetooth operation to finish");
            Save(token); configured_ = true; state_ = "configured"; error_.clear();
        }
        return Status();
    }
};
}  // namespace

cJSON* HandleXgimiCommand(const std::string& method, const cJSON* params) {
    auto& device = XgimiWake::Instance();
    if (method == "xgimi.status") return device.Status();
    if (method == "xgimi.learn") return device.Start(true);
    if (method == "xgimi.power_on") return device.Start(false);
    if (method == "xgimi.configure") {
        const auto token = cJSON_GetObjectItemCaseSensitive(params, "token");
        if (!cJSON_IsString(token)) throw std::runtime_error("token must be a hexadecimal string");
        return device.Configure(token->valuestring);
    }
    throw std::runtime_error("Unknown hardware method");
}

void InitializeXgimiBluetooth() { XgimiWake::Instance().Initialize(); }
