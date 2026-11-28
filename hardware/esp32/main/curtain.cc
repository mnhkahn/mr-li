#include "curtain.h"
#include "curtain_protocol.h"
#include "light_ring.h"
#include "rf_learning.h"
#include <cmath>
#include <mutex>
#include <stdexcept>
#include <driver/gpio.h>
#include <driver/pulse_cnt.h>
#include <esp_timer.h>
#include <atomic>
#include <driver/rmt_tx.h>
#include <driver/rmt_encoder.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>
#include <nvs.h>
#include <sdkconfig.h>

namespace {
const char* kActions[] = {"up", "down", "stop"};
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
int Action(const cJSON* params) {
    auto item = cJSON_GetObjectItemCaseSensitive(params, "action");
    if (cJSON_IsString(item)) for (int i = 0; i < 3; ++i) if (std::string(item->valuestring) == kActions[i]) return i;
    throw std::runtime_error("action must be up, down or stop");
}
void Pin(int pin, bool rx = false) {
#if CONFIG_IDF_TARGET_ESP32C3
    bool allowed = (pin >= 4 && pin <= 7) || pin == 10;
#elif CONFIG_IDF_TARGET_ESP32S3
    bool allowed = (pin >= 4 && pin <= 18) || pin == 21 || (rx && pin == 47);
#else
    bool allowed = false;
#endif
    if (!allowed || !GPIO_IS_VALID_GPIO(pin) || (!rx && !GPIO_IS_VALID_OUTPUT_GPIO(pin)))
        throw std::runtime_error("Unsupported RF GPIO; S3 wiring uses TX21/RX47; verify board pin use");
}
void CheckNewRfPin([[maybe_unused]] int pin) {
#if CONFIG_IDF_TARGET_ESP32S3
    if (pin == kLightRingGpio) {
        throw std::runtime_error("GPIO" + std::to_string(pin) + " is reserved for the light ring");
    }
#endif
}
struct Config {
    int pin = -1, rx = -1, repeat = 6;
    bool enabled = false;
    std::vector<int> frames[3];
};
std::vector<int> Pulses(const cJSON* array) {
    if (!cJSON_IsArray(array) || cJSON_GetArraySize(array) > 512) throw std::runtime_error("Expected at most 512 signed pulse durations");
    std::vector<int> pulses;
    const cJSON* item;
    cJSON_ArrayForEach(item, array) {
        if (!cJSON_IsNumber(item) || !std::isfinite(item->valuedouble) || item->valuedouble < -32767 ||
            item->valuedouble > 32767 || item->valuedouble != item->valueint) throw std::runtime_error("Pulse duration must be signed integer microseconds");
        pulses.push_back(item->valueint);
    }
    curtain::Validate(pulses);
    return pulses;
}
Config Parse(const cJSON* object) {
    auto protocol = cJSON_GetObjectItemCaseSensitive(object, "protocol");
    if (!cJSON_IsString(protocol) || std::string(protocol->valuestring) != "raw_ook_fixed") throw std::runtime_error("Only verified raw_ook_fixed waveforms are supported");
    Config config;
    config.pin = Integer(object, "tx_gpio", 0, 48); Pin(config.pin);
    config.repeat = Integer(object, "repeat", 1, 10);
    for (int i = 0; i < 3; ++i) config.frames[i] = Pulses(cJSON_GetObjectItemCaseSensitive(object, kActions[i]));
    return config;
}
bool Ready(const Config& config) {
    return config.pin >= 0 && !config.frames[0].empty() && !config.frames[1].empty() && !config.frames[2].empty();
}
class Curtain {
    std::mutex mutex_;
    bool busy_ = false, pending_stop_ = false, learning_ = false;
    std::vector<uint32_t> trace_;
    int action_ = 0, learn_seconds_ = 30, learn_gap_ = 6000;
    curtain::CaptureProgress progress_;
    std::string state_ = "idle", error_, last_action_;

    Config Load() {
        Config config;
        nvs_handle_t handle;
        auto rc = nvs_open("curtain_rf", NVS_READONLY, &handle);
        if (rc == ESP_ERR_NVS_NOT_FOUND) return config;
        Check(rc, "Open RF settings");
        struct Close { nvs_handle_t h; ~Close() { nvs_close(h); } } close{handle};
        int32_t value;
        rc = nvs_get_i32(handle, "tx", &value);
        if (rc == ESP_ERR_NVS_NOT_FOUND) {
            size_t size = 0;
            if (nvs_get_str(handle, "config", nullptr, &size) == ESP_OK && size && size <= 4096) {
                std::string data(size, '\0');
                Check(nvs_get_str(handle, "config", data.data(), &size), "Read legacy RF settings");
                auto json = cJSON_Parse(data.c_str());
                try { config = Parse(json); cJSON_Delete(json); } catch (...) { cJSON_Delete(json); throw; }
            }
            return config; // Legacy data is retained, but transmission needs explicit enable.
        }
        Check(rc, "Read RF TX pin"); config.pin = value; Pin(config.pin);
        rc = nvs_get_i32(handle, "rx", &value);
        if (rc == ESP_OK) { config.rx = value; if (config.rx >= 0) Pin(config.rx, true); }
        else if (rc != ESP_ERR_NVS_NOT_FOUND) Check(rc, "Read RF RX pin");
        rc = nvs_get_i32(handle, "repeat", &value);
        if (rc == ESP_OK) config.repeat = value;
        else if (rc != ESP_ERR_NVS_NOT_FOUND) Check(rc, "Read RF repeats");
        if (config.repeat < 1 || config.repeat > 10) throw std::runtime_error("Invalid saved RF repeat count");
        uint8_t enabled = 0;
        rc = nvs_get_u8(handle, "enabled", &enabled);
        if (rc != ESP_OK && rc != ESP_ERR_NVS_NOT_FOUND) Check(rc, "Read RF enable");
        config.enabled = enabled != 0;
        for (int i = 0; i < 3; ++i) {
            size_t size = 0;
            rc = nvs_get_blob(handle, kActions[i], nullptr, &size);
            if (rc == ESP_ERR_NVS_NOT_FOUND) continue;
            Check(rc, "Read RF frame size");
            if (!size || size > 512 * sizeof(int) || size % sizeof(int)) throw std::runtime_error("Invalid saved RF waveform");
            config.frames[i].resize(size / sizeof(int));
            Check(nvs_get_blob(handle, kActions[i], config.frames[i].data(), &size), "Read RF frame");
            curtain::Validate(config.frames[i]);
        }
        return config;
    }
    void Save(const Config& config) {
        nvs_handle_t handle;
        Check(nvs_open("curtain_rf", NVS_READWRITE, &handle), "Open RF settings");
        struct Close { nvs_handle_t h; ~Close() { nvs_close(h); } } close{handle};
        Check(nvs_set_u8(handle, "enabled", 0), "Disable RF while updating settings");
        Check(nvs_set_i32(handle, "tx", config.pin), "Save RF TX");
        Check(nvs_set_i32(handle, "rx", config.rx), "Save RF RX");
        Check(nvs_set_i32(handle, "repeat", config.repeat), "Save RF repeats");
        for (int i = 0; i < 3; ++i) {
            if (!config.frames[i].empty()) Check(nvs_set_blob(handle, kActions[i], config.frames[i].data(), config.frames[i].size() * sizeof(int)), "Save RF frame");
        }
        Check(nvs_set_u8(handle, "enabled", config.enabled), "Save RF enable");
        Check(nvs_commit(handle), "Commit RF settings");
    }
    void RunLearn() {
        try {
            Config config;
            int action, seconds, gap;
            { std::lock_guard<std::mutex> lock(mutex_); config = Load(); action = action_; seconds = learn_seconds_; gap = learn_gap_; state_ = "learning"; }
            gpio_set_level(static_cast<gpio_num_t>(config.pin), 0);
            gpio_set_direction(static_cast<gpio_num_t>(config.pin), GPIO_MODE_OUTPUT);
            gpio_set_pull_mode(static_cast<gpio_num_t>(config.rx), GPIO_PULLDOWN_ONLY);
            auto captured = curtain::LearnRf(config.rx, seconds, gap, [this](curtain::CaptureProgress progress) {
                std::lock_guard<std::mutex> lock(mutex_); progress_ = progress;
            });
            std::lock_guard<std::mutex> lock(mutex_);
            if (!captured.error.empty()) {
                progress_ = captured.progress; state_ = "error"; error_ = captured.error;
                busy_ = false; learning_ = false; return;
            }
            config.frames[action] = captured.pulses;
            config.enabled = false;
            Save(config);
            progress_ = captured.progress;
            state_ = "learned"; error_.clear(); busy_ = false; learning_ = false;
        } catch (const std::exception& e) {
            std::lock_guard<std::mutex> lock(mutex_);
            state_ = "error"; error_ = e.what(); busy_ = false; learning_ = false;
        }
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
#if CONFIG_IDF_TARGET_ESP32S3
    bool UsesGpio(int pin) {
        std::lock_guard lock(mutex_);
        auto config = Load();
        return config.pin == pin || config.rx == pin;
    }
#endif
    cJSON* Status() {
        std::lock_guard<std::mutex> lock(mutex_);
        auto config = Load();
        auto result = cJSON_CreateObject();
        cJSON_AddStringToObject(result, "protocol", "raw_ook_fixed");
        cJSON_AddBoolToObject(result, "configured", Ready(config));
        cJSON_AddBoolToObject(result, "pins_configured", config.pin >= 0 && config.rx >= 0);
        cJSON_AddBoolToObject(result, "enabled", config.enabled);
        cJSON_AddNumberToObject(result, "tx_gpio", config.pin);
        cJSON_AddNumberToObject(result, "rx_gpio", config.rx);
        cJSON_AddNumberToObject(result, "repeat", config.repeat);
        auto learned = cJSON_AddObjectToObject(result, "learned");
        for (int i = 0; i < 3; ++i) cJSON_AddNumberToObject(learned, kActions[i], config.frames[i].size());
        cJSON_AddBoolToObject(result, "busy", busy_);
        cJSON_AddBoolToObject(result, "stop_pending", pending_stop_);
        cJSON_AddStringToObject(result, "state", state_.c_str());
        cJSON_AddStringToObject(result, "last_action", last_action_.c_str());
        cJSON_AddStringToObject(result, "learning_action", learning_ ? kActions[action_] : "");
        cJSON_AddNumberToObject(result, "frames_seen", progress_.frames);
        cJSON_AddNumberToObject(result, "matches", progress_.matches);
        cJSON_AddNumberToObject(result, "bursts", progress_.bursts);
        cJSON_AddStringToObject(result, "error", error_.c_str());
        cJSON_AddBoolToObject(result, "physical_effect_confirmed", false);
        cJSON_AddBoolToObject(result, "position_supported", false);
        return result;
    }
    cJSON* TraceInput(const cJSON* params) {
        std::unique_lock<std::mutex> lock(mutex_);
        const int seconds = Integer(params, "seconds", 1, 45);
        if (busy_) throw std::runtime_error("RF busy");
        auto config = Load();
        if (config.rx < 0) throw std::runtime_error("Configure RF RX first");
        if (config.enabled) throw std::runtime_error("Disable RF transmission before tracing");
        const auto pin = static_cast<gpio_num_t>(config.rx);
        trace_.resize(16000);
        struct Trace {
            uint32_t* edges;
            unsigned capacity;
            gpio_num_t pin;
            int64_t start;
            std::atomic<unsigned> count{0};
        } trace{trace_.data(), static_cast<unsigned>(trace_.size()), pin, 0};
        Check(gpio_set_direction(pin, GPIO_MODE_INPUT), "Set RF input mode");
        Check(gpio_set_pull_mode(pin, GPIO_PULLDOWN_ONLY), "Set RF input pull-down");
        Check(gpio_set_intr_type(pin, GPIO_INTR_ANYEDGE), "Set RF edge interrupt");
        Check(gpio_install_isr_service(0), "Install GPIO diagnostic interrupt");
        struct Cleanup {
            gpio_num_t pin;
            ~Cleanup() { gpio_isr_handler_remove(pin); gpio_uninstall_isr_service(); }
        } cleanup{pin};
        const auto initial = gpio_get_level(pin);
        trace.start = esp_timer_get_time();
        Check(gpio_isr_handler_add(pin, [](void* context) {
            auto& t = *static_cast<Trace*>(context);
            const auto i = t.count.load(std::memory_order_relaxed);
            if (i >= t.capacity) return;
            const auto elapsed = static_cast<uint32_t>(esp_timer_get_time() - t.start);
            t.edges[i] = (elapsed << 1) | static_cast<uint32_t>(gpio_get_level(t.pin));
            t.count.store(i + 1, std::memory_order_release);
        }, &trace), "Record RF edge timestamps");
        busy_ = true;
        lock.unlock();
        vTaskDelay(pdMS_TO_TICKS(seconds * 1000));
        const auto stop_result = gpio_intr_disable(pin);
        lock.lock();
        busy_ = false;
        Check(stop_result, "Stop recording RF edges");
        const auto count = trace.count.load(std::memory_order_acquire);
        trace_.resize(count);
        auto result = cJSON_CreateObject();
        cJSON_AddNumberToObject(result, "initial_level", initial);
        cJSON_AddNumberToObject(result, "edges", count);
        cJSON_AddBoolToObject(result, "buffer_full", count == trace.capacity);
        return result;
    }

    cJSON* ReadTrace(const cJSON* params) {
        std::lock_guard<std::mutex> lock(mutex_);
        const auto offset = static_cast<size_t>(Integer(params, "offset", 0, 16000));
        auto result = cJSON_CreateObject();
        auto rows = cJSON_AddArrayToObject(result, "packed_edges");
        for (size_t i = offset; i < trace_.size() && i < offset + 250; ++i)
            cJSON_AddItemToArray(rows, cJSON_CreateNumber(trace_[i]));
        return result;
    }

    cJSON* CaptureRaw() {
        std::lock_guard<std::mutex> lock(mutex_);
        if (busy_) throw std::runtime_error("RF busy");
        const auto config = Load();
        if (config.rx < 0) throw std::runtime_error("Configure RF RX first");
        auto capture = curtain::LearnRf(config.rx, 1, 0, {}, true);
        if (!capture.error.empty()) throw std::runtime_error(capture.error);
        auto result = cJSON_CreateObject();
        cJSON_AddNumberToObject(result, "rx_gpio", config.rx);
        auto pulses = cJSON_AddArrayToObject(result, "pulses_us");
        for (int pulse : capture.pulses) cJSON_AddItemToArray(pulses, cJSON_CreateNumber(pulse));
        return result;
    }
    cJSON* ProbeInput() {
#if CONFIG_SOC_PCNT_SUPPORTED
        std::lock_guard<std::mutex> lock(mutex_);
        if (busy_) throw std::runtime_error("RF busy");
        auto config = Load();
        if (config.rx < 0) throw std::runtime_error("Configure RF RX first");
        struct Counter {
            pcnt_unit_handle_t unit = nullptr;
            pcnt_channel_handle_t channel = nullptr;
            bool enabled = false, started = false;
            ~Counter() {
                if (started) pcnt_unit_stop(unit);
                if (enabled) pcnt_unit_disable(unit);
                if (channel) pcnt_del_channel(channel);
                if (unit) pcnt_del_unit(unit);
            }
        } counter;
        pcnt_unit_config_t unit = {};
        unit.low_limit = -1;
        unit.high_limit = 32767;
        Check(pcnt_new_unit(&unit, &counter.unit), "Create RX pulse counter");
        pcnt_chan_config_t channel = {};
        channel.edge_gpio_num = config.rx;
        channel.level_gpio_num = -1;
        Check(pcnt_new_channel(counter.unit, &channel, &counter.channel), "Attach RX pulse counter");
        Check(pcnt_channel_set_edge_action(counter.channel, PCNT_CHANNEL_EDGE_ACTION_INCREASE,
                                          PCNT_CHANNEL_EDGE_ACTION_HOLD), "Count rising edges");
        pcnt_glitch_filter_config_t filter = {};
        filter.max_glitch_ns = 2000;
        Check(pcnt_unit_set_glitch_filter(counter.unit, &filter), "Filter short RX glitches");
        Check(pcnt_unit_enable(counter.unit), "Enable RX pulse counter");
        counter.enabled = true;

        int counts[10] = {};
        const int level_before = gpio_get_level(static_cast<gpio_num_t>(config.rx));
        for (int& count : counts) {
            Check(pcnt_unit_clear_count(counter.unit), "Clear RX pulse counter");
            Check(pcnt_unit_start(counter.unit), "Start RX pulse counter");
            counter.started = true;
            vTaskDelay(pdMS_TO_TICKS(100));
            Check(pcnt_unit_stop(counter.unit), "Stop RX pulse counter");
            counter.started = false;
            Check(pcnt_unit_get_count(counter.unit, &count), "Read RX pulse counter");
        }
        auto result = cJSON_CreateObject();
        cJSON_AddNumberToObject(result, "rx_gpio", config.rx);
        cJSON_AddNumberToObject(result, "level_before", level_before);
        cJSON_AddNumberToObject(result, "level_after", gpio_get_level(static_cast<gpio_num_t>(config.rx)));
        cJSON_AddNumberToObject(result, "sample_ms", 100);
        cJSON_AddItemToObject(result, "rising_edges", cJSON_CreateIntArray(counts, 10));
        return result;
#else
        throw std::runtime_error("RF pulse counter is unavailable on this ESP32 target");
#endif
    }
    cJSON* Setup(const cJSON* params) {
        int tx = Integer(params, "tx_gpio", 0, 48), rx = Integer(params, "rx_gpio", 0, 48);
        Pin(tx); Pin(rx, true);
        CheckNewRfPin(tx); CheckNewRfPin(rx);
        if (tx == rx) throw std::runtime_error("RX and TX must use separate GPIOs");
        int repeat = cJSON_HasObjectItem(params, "repeat") ? Integer(params, "repeat", 1, 10) : 6;
        { std::lock_guard<std::mutex> lock(mutex_);
          if (busy_) throw std::runtime_error("RF busy");
          auto config = Load(); config.pin = tx; config.rx = rx; config.repeat = repeat; config.enabled = false;
          Save(config); state_ = "pins_configured"; error_.clear(); }
        return Status();
    }
    cJSON* Configure(const cJSON* params) {
        auto config = Parse(params);
        CheckNewRfPin(config.pin);
        { std::lock_guard<std::mutex> lock(mutex_);
          if (busy_) throw std::runtime_error("RF busy");
          config.rx = Load().rx; config.enabled = false;
          Save(config); state_ = "configured"; error_.clear(); }
        return Status();
    }
    cJSON* Enable(const cJSON* params) {
        auto item = cJSON_GetObjectItemCaseSensitive(params, "enabled");
        if (!cJSON_IsBool(item)) throw std::runtime_error("enabled must be boolean");
        { std::lock_guard<std::mutex> lock(mutex_);
          if (busy_) throw std::runtime_error("RF busy");
          auto config = Load();
          if (cJSON_IsTrue(item)) {
              if (!cJSON_IsTrue(cJSON_GetObjectItemCaseSensitive(params, "fixed_code_confirmed")))
                  throw std::runtime_error("Explicit fixed_code_confirmed is required; rolling codes cannot be replayed");
              if (!Ready(config)) throw std::runtime_error("Learn all three buttons first");
              for (int i = 0; i < 3; ++i) for (int j = i + 1; j < 3; ++j)
                  if (curtain::SameFrame(config.frames[i], config.frames[j])) throw std::runtime_error("Two actions have identical frames; relearn the correct buttons");
          }
          config.enabled = cJSON_IsTrue(item); Save(config); state_ = config.enabled ? "ready" : "disabled"; error_.clear(); }
        return Status();
    }
    cJSON* Export(const cJSON* params) {
        int action = Action(params);
        std::lock_guard<std::mutex> lock(mutex_);
        auto config = Load();
        auto result = cJSON_CreateObject();
        cJSON_AddStringToObject(result, "action", kActions[action]);
        auto pulses = cJSON_AddArrayToObject(result, "pulses");
        for (int value : config.frames[action]) cJSON_AddItemToArray(pulses, cJSON_CreateNumber(value));
        return result;
    }
    cJSON* Learn(const cJSON* params) {
        int action = Action(params);
        int seconds = cJSON_HasObjectItem(params, "seconds") ? Integer(params, "seconds", 5, 60) : 30;
        int gap = cJSON_HasObjectItem(params, "frame_gap_us") ? Integer(params, "frame_gap_us", 3000, 20000) : 6000;
        { std::lock_guard<std::mutex> lock(mutex_);
          if (busy_) throw std::runtime_error("RF busy; finish current operation first");
          auto config = Load();
          if (config.pin < 0 || config.rx < 0) throw std::runtime_error("Configure TX21/RX47 via curtain.setup first");
          config.enabled = false; Save(config);
          action_ = action; learn_seconds_ = seconds; learn_gap_ = gap;
          busy_ = true; learning_ = true; progress_ = {}; state_ = "starting_learning"; error_.clear();
          if (xTaskCreate([](void*) { Instance().RunLearn(); vTaskDelete(nullptr); }, "rf_learn", 8192, nullptr, 2, nullptr) != pdPASS) {
              busy_ = false; learning_ = false; state_ = "error"; throw std::runtime_error("No memory for RF learner");
          } }
        auto result = Status(); cJSON_AddBoolToObject(result, "accepted", true); return result;
    }
    cJSON* Send(int action) {
        { std::lock_guard<std::mutex> lock(mutex_);
          auto config = Load();
          if (!Ready(config) || !config.enabled) throw std::runtime_error("RF disabled; learn all buttons and explicitly enable verified fixed codes first");
          if (busy_) {
              if (learning_) throw std::runtime_error("Learning active; no transmission allowed");
              if (action != 2) throw std::runtime_error("RF busy; stop is the only accepted interrupt");
              pending_stop_ = true;
          } else {
              busy_ = true; action_ = action; error_.clear(); state_ = "starting";
              if (xTaskCreate([](void*) { Instance().Run(); vTaskDelete(nullptr); }, "curtain_rf", 8192, nullptr, 2, nullptr) != pdPASS) {
                  busy_ = false; state_ = "error"; throw std::runtime_error("No memory for RF worker");
              }
          } }
        auto result = Status(); cJSON_AddBoolToObject(result, "accepted", true); return result;
    }
};
}
#if CONFIG_IDF_TARGET_ESP32S3
bool CurtainUsesGpio(int pin) {
    return Curtain::Instance().UsesGpio(pin);
}
#endif
cJSON* HandleCurtainCommand(const std::string& method, const cJSON* params) {
    auto& curtain = Curtain::Instance();
    if (method == "curtain.status") return curtain.Status();
    if (method == "curtain.probe_input") return curtain.ProbeInput();
    if (method == "curtain.capture_raw") return curtain.CaptureRaw();
    if (method == "curtain.trace_input") return curtain.TraceInput(params);
    if (method == "curtain.read_trace") return curtain.ReadTrace(params);
    if (method == "curtain.setup") return curtain.Setup(params);
    if (method == "curtain.configure") return curtain.Configure(params);
    if (method == "curtain.enable") return curtain.Enable(params);
    if (method == "curtain.export") return curtain.Export(params);
    if (method == "curtain.learn") return curtain.Learn(params);
    if (method == "curtain.up") return curtain.Send(0);
    if (method == "curtain.down") return curtain.Send(1);
    if (method == "curtain.stop") return curtain.Send(2);
    throw std::runtime_error("Unknown curtain command");
}
