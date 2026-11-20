#include "curtain.h"
#include "curtain_protocol.h"
#include <cmath>
#include <mutex>
#include <stdexcept>
#include <driver/gpio.h>
#include <driver/rmt_tx.h>
#include <driver/rmt_encoder.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>
#include <nvs.h>
#include <sdkconfig.h>

namespace {
void Check(esp_err_t error, const char* operation) {
    if (error != ESP_OK) throw std::runtime_error(std::string(operation) + ": " + esp_err_to_name(error));
}
int Integer(const cJSON* object, const char* name, int min, int max) {
    auto value = cJSON_GetObjectItemCaseSensitive(object, name);
    if (!cJSON_IsNumber(value) || !std::isfinite(value->valuedouble) ||
        value->valuedouble < min || value->valuedouble > max || value->valuedouble != value->valueint)
        throw std::runtime_error(std::string(name) + " is out of range or not an integer");
    return value->valueint;
}
struct Config {
    int pin = -1;
    int repeat = 0;
    std::vector<int> frames[3];
};
Config Parse(const cJSON* object) {
    Config config;
    auto protocol = cJSON_GetObjectItemCaseSensitive(object, "protocol");
    if (!cJSON_IsString(protocol) || std::string(protocol->valuestring) != "raw_ook_fixed")
        throw std::runtime_error("Only raw_ook_fixed is supported; rolling-code remotes need a model-specific implementation");
    config.pin = Integer(object, "tx_gpio", 0, 48);
#if CONFIG_IDF_TARGET_ESP32C3
    const bool allowed = (config.pin >= 4 && config.pin <= 7) || config.pin == 10;
#elif CONFIG_IDF_TARGET_ESP32S3
    const bool allowed = (config.pin >= 4 && config.pin <= 18) || config.pin == 21;
#else
    const bool allowed = false;
#endif
    if (!allowed || !GPIO_IS_VALID_OUTPUT_GPIO(config.pin))
        throw std::runtime_error("Unsupported TX pin: C3 allows 4..7/10, S3 allows 4..18/21; check board wiring first");
    config.repeat = Integer(object, "repeat", 1, 10);
    const char* names[] = {"up", "down", "stop"};
    for (int i = 0; i < 3; ++i) {
        auto array = cJSON_GetObjectItemCaseSensitive(object, names[i]);
        if (!cJSON_IsArray(array) || cJSON_GetArraySize(array) > 128)
            throw std::runtime_error("up/down/stop must each be a pulse array of at most 128 entries");
        const cJSON* item;
        cJSON_ArrayForEach(item, array) {
            if (!cJSON_IsNumber(item) || !std::isfinite(item->valuedouble) || item->valuedouble < -32767 ||
                item->valuedouble > 32767 || item->valuedouble != item->valueint)
                throw std::runtime_error("Pulse durations must be signed integer microseconds");
            config.frames[i].push_back(item->valueint);
        }
        curtain::Validate(config.frames[i]);
    }
    return config;
}

class Curtain {
    std::mutex mutex_;
    bool busy_ = false, pending_stop_ = false;
    int action_ = 0;
    std::string state_ = "idle", error_, last_action_;

    Config Load() {
        nvs_handle_t nvs;
        auto rc = nvs_open("curtain_rf", NVS_READONLY, &nvs);
        if (rc == ESP_ERR_NVS_NOT_FOUND) return {};
        Check(rc, "Open curtain settings");
        size_t size = 0;
        rc = nvs_get_str(nvs, "config", nullptr, &size);
        if (rc != ESP_OK) {
            nvs_close(nvs);
            if (rc == ESP_ERR_NVS_NOT_FOUND) return {};
            Check(rc, "Read RF settings size");
        }
        if (size == 0 || size > 4096) { nvs_close(nvs); throw std::runtime_error("Invalid RF settings size"); }
        std::string data(size, '\0');
        rc = nvs_get_str(nvs, "config", data.data(), &size);
        nvs_close(nvs);
        if (rc == ESP_ERR_NVS_NOT_FOUND) return {};
        Check(rc, "Read curtain settings");
        auto json = cJSON_Parse(data.c_str());
        try { auto config = Parse(json); cJSON_Delete(json); return config; }
        catch (...) { cJSON_Delete(json); throw; }
    }
    void Run() {
        rmt_channel_handle_t channel = nullptr;
        rmt_encoder_handle_t encoder = nullptr;
        bool enabled = false;
        int pin = -1;
        std::string failure;
        auto cleanup = [&] {
            if (enabled) { rmt_disable(channel); enabled = false; }
            if (encoder) { rmt_del_encoder(encoder); encoder = nullptr; }
            if (channel) { rmt_del_channel(channel); channel = nullptr; }
            if (pin >= 0) gpio_set_level(static_cast<gpio_num_t>(pin), 0);
        };
        try {
            auto config = Load();
            pin = config.pin;
            if (pin < 0) throw std::runtime_error("Configure a connected 433 MHz transmitter and real curtain waveforms first");
            rmt_tx_channel_config_t tx = {};
            tx.clk_src = RMT_CLK_SRC_DEFAULT;
            tx.gpio_num = static_cast<gpio_num_t>(pin);
            tx.resolution_hz = 1000000;
            tx.mem_block_symbols = 64;
            tx.trans_queue_depth = 1;
            Check(rmt_new_tx_channel(&tx, &channel), "Create RF RMT channel");
            rmt_copy_encoder_config_t copy = {};
            Check(rmt_new_copy_encoder(&copy, &encoder), "Create RF waveform encoder");
            Check(rmt_enable(channel), "Enable RF output");
            enabled = true;
            int action;
            { std::lock_guard<std::mutex> lock(mutex_); action = action_; state_ = "transmitting"; }
            for (int repetition = 0; repetition < config.repeat; ++repetition) {
                {
                    std::lock_guard<std::mutex> lock(mutex_);
                    // Prioritize a stop at the next frame boundary (<=150 ms).
                    if (pending_stop_) { action = 2; pending_stop_ = false; repetition = 0; }
                    last_action_ = action == 0 ? "up" : action == 1 ? "down" : "stop";
                }
                const auto& pulses = config.frames[action];
                std::vector<rmt_symbol_word_t> symbols(pulses.size() / 2);
                for (size_t i = 0; i < symbols.size(); ++i) {
                    symbols[i].level0 = 1;
                    symbols[i].duration0 = pulses[i * 2];
                    symbols[i].level1 = 0;
                    symbols[i].duration1 = -pulses[i * 2 + 1];
                }
                rmt_transmit_config_t transmission = {};
                transmission.flags.eot_level = 0;
                Check(rmt_transmit(channel, encoder, symbols.data(), symbols.size() * sizeof(symbols[0]), &transmission), "Transmit RF waveform");
                Check(rmt_tx_wait_all_done(channel, 1000), "Wait for RF waveform");
                {
                    std::lock_guard<std::mutex> lock(mutex_);
                    if (pending_stop_) { action = 2; pending_stop_ = false; repetition = -1; }
                    else if (repetition + 1 == config.repeat) {
                        // Finish atomically relative to new commands. A stop
                        // accepted during the last frame must not be discarded.
                        cleanup(); state_ = "transmitted"; busy_ = false;
                        return;
                    }
                }
            }
        } catch (const std::exception& error) { failure = error.what(); }
        {
            std::lock_guard<std::mutex> lock(mutex_);
            cleanup();
            state_ = failure.empty() ? "transmitted" : "error";
            error_ = failure;
            busy_ = false;
            pending_stop_ = false;
        }
    }
public:
    static Curtain& Instance() { static Curtain value; return value; }
    cJSON* Status() {
        std::lock_guard<std::mutex> lock(mutex_);
        auto config = Load();
        auto result = cJSON_CreateObject();
        cJSON_AddStringToObject(result, "protocol", "raw_ook_fixed");
        cJSON_AddBoolToObject(result, "configured", config.pin >= 0);
        cJSON_AddNumberToObject(result, "tx_gpio", config.pin);
        cJSON_AddBoolToObject(result, "busy", busy_);
        cJSON_AddBoolToObject(result, "stop_pending", pending_stop_);
        cJSON_AddStringToObject(result, "state", state_.c_str());
        cJSON_AddStringToObject(result, "last_action", last_action_.c_str());
        cJSON_AddStringToObject(result, "error", error_.c_str());
        cJSON_AddBoolToObject(result, "physical_effect_confirmed", false);
        return result;
    }
    cJSON* Configure(const cJSON* params) {
        Parse(params); // Validate everything before writing or driving any pin.
        const auto allowed = {"protocol", "tx_gpio", "repeat", "up", "down", "stop"};
        auto persisted = cJSON_CreateObject();
        for (auto key : allowed) cJSON_AddItemToObject(persisted, key, cJSON_Duplicate(cJSON_GetObjectItemCaseSensitive(params, key), true));
        auto data = cJSON_PrintUnformatted(persisted);
        cJSON_Delete(persisted);
        if (!data) throw std::runtime_error("No memory for RF settings");
        std::string serialized(data);
        cJSON_free(data);
        if (serialized.size() >= 4000) throw std::runtime_error("RF settings too large");
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (busy_) throw std::runtime_error("Wait for RF transmission to finish before reconfiguring");
            nvs_handle_t nvs;
            Check(nvs_open("curtain_rf", NVS_READWRITE, &nvs), "Open RF settings");
            auto rc = nvs_set_str(nvs, "config", serialized.c_str());
            if (rc == ESP_OK) rc = nvs_commit(nvs);
            nvs_close(nvs);
            Check(rc, "Save RF settings");
            state_ = "configured"; error_.clear();
        }
        return Status();
    }
    cJSON* Send(int action) {
        {
            std::lock_guard<std::mutex> lock(mutex_);
            if (Load().pin < 0) throw std::runtime_error("RF transmitter and curtain waveforms are not configured");
            if (busy_) {
                if (action != 2) throw std::runtime_error("RF busy; stop is the only accepted interrupt");
                pending_stop_ = true;
            } else {
                busy_ = true; action_ = action; error_.clear(); state_ = "starting";
                auto task = [](void*) { Instance().Run(); vTaskDelete(nullptr); };
                if (xTaskCreate(task, "curtain_rf", 8192, nullptr, 2, nullptr) != pdPASS) {
                    busy_ = false; state_ = "error"; error_ = "No memory for RF worker";
                    throw std::runtime_error(error_);
                }
            }
        }
        auto result = Status();
        cJSON_AddBoolToObject(result, "accepted", true);
        return result;
    }
};
}

cJSON* HandleCurtainCommand(const std::string& method, const cJSON* params) {
    auto& curtain = Curtain::Instance();
    if (method == "curtain.status") return curtain.Status();
    if (method == "curtain.configure") return curtain.Configure(params);
    if (method == "curtain.up") return curtain.Send(0);
    if (method == "curtain.down") return curtain.Send(1);
    if (method == "curtain.stop") return curtain.Send(2);
    throw std::runtime_error("Unknown curtain command");
}
