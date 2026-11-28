#include "light_ring.h"

#include "curtain.h"
#include "light_music.h"
#include <algorithm>
#include <array>
#include <cmath>
#include <cstdint>
#include <mutex>
#include <optional>
#include <stdexcept>
#include <string_view>
#include <esp_timer.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>
#include <led_strip.h>
#include <nvs.h>

namespace {
constexpr unsigned kLedCount = 35;
constexpr int kFrameMs = 20;
constexpr int kRainbowPeriodMs = 2400;
constexpr int kFlashRainbowPeriodMs = 1200;
constexpr float kFullTurn = 6.283185307f;
constexpr float kMusicBrightness = 72.f / 255.f;
// Fit 35 RGB pixels, reset and end markers into one half of the DMA buffer.
constexpr unsigned kRmtDmaSymbols = 1792;
static_assert(kRmtDmaSymbols / 2 >= kLedCount * 24 + 2);

enum class Mode { Off, White, Warm, Rainbow, Flash, Music };

struct ColorOrder {
    const char* name;
    std::array<size_t, 3> indices;
};
constexpr ColorOrder kColorOrders[] = {
    {"GRB", {1, 0, 2}}, {"RGB", {0, 1, 2}}, {"BRG", {2, 0, 1}},
    {"BGR", {2, 1, 0}}, {"RBG", {0, 2, 1}}, {"GBR", {1, 2, 0}},
};
constexpr std::array<std::array<unsigned, 3>, LightMusic::kBandCount> kBandColors = {{
    {255, 0, 0}, {255, 75, 0}, {255, 215, 0}, {0, 255, 0},
    {0, 230, 230}, {0, 65, 255}, {175, 0, 255},
}};

uint8_t LoadColorOrder() {
    nvs_handle_t handle;
    if (nvs_open("light_ring", NVS_READONLY, &handle) != ESP_OK) return 0;
    uint8_t order = 0;
    const auto result = nvs_get_u8(handle, "color_order", &order);
    nvs_close(handle);
    return result == ESP_OK && order < std::size(kColorOrders) ? order : 0;
}

void SaveColorOrder(uint8_t order) {
    nvs_handle_t handle;
    auto result = nvs_open("light_ring", NVS_READWRITE, &handle);
    if (result == ESP_OK) {
        result = nvs_set_u8(handle, "color_order", order);
        if (result == ESP_OK) result = nvs_commit(handle);
        nvs_close(handle);
    }
    if (result != ESP_OK) throw std::runtime_error(esp_err_to_name(result));
}

std::optional<uint8_t> ParseColorOrder(const cJSON* params) {
    const auto* item = cJSON_GetObjectItemCaseSensitive(params, "color_order");
    if (!item) return std::nullopt;
    if (cJSON_IsString(item)) {
        const std::string_view order = item->valuestring;
        for (uint8_t i = 0; i < std::size(kColorOrders); ++i) {
            if (order == kColorOrders[i].name) return i;
        }
    }
    throw std::invalid_argument("color_order must be an RGB permutation");
}

bool Animated(Mode mode) {
    return mode == Mode::Rainbow || mode == Mode::Flash || mode == Mode::Music;
}

Mode ParseMode(const cJSON* params) {
    const auto* value = cJSON_GetObjectItemCaseSensitive(params, "mode");
    if (!cJSON_IsNumber(value) || !(value->valuedouble >= 0 && value->valuedouble <= static_cast<int>(Mode::Music)) ||
        value->valuedouble != value->valueint) {
        throw std::invalid_argument("mode must be an integer from 0 to 5");
    }
    return static_cast<Mode>(value->valueint);
}

class LightRing {
public:
    void Start() {
        try {
            if (CurtainUsesGpio(kLightRingGpio)) {
                startup_error_ = "GPIO" + std::to_string(kLightRingGpio) +
                    " is in use by the saved curtain wiring";
                return;
            }
        } catch (const std::exception& error) {
            startup_error_ = error.what();
            return;
        }

        led_strip_config_t config = {};
        config.strip_gpio_num = kLightRingGpio;
        config.max_leds = kLedCount;
        config.led_model = LED_MODEL_WS2812;
        // Give the driver wire-order bytes so color calibration needs no channel restart.
        config.color_component_format = LED_STRIP_COLOR_COMPONENT_FMT_RGB;
        color_order_ = LoadColorOrder();
        // ESP32-S3 assigns DMA to the last TX channel. The wake indicator uses
        // one of the remaining blocks, leaving two adjacent blocks for curtain TX.
        led_strip_rmt_config_t rmt = {};
        rmt.resolution_hz = 10'000'000;
        rmt.mem_block_symbols = kRmtDmaSymbols;
        rmt.flags.with_dma = true;
        error_ = led_strip_new_rmt_device(&config, &rmt, &strip_);
        if (error_ != ESP_OK) return;
        music_error_ = music_.Start();
        error_ = Render(Mode::Off, 0);
        if (error_ == ESP_OK &&
            xTaskCreate([](void* context) { static_cast<LightRing*>(context)->Animate(); },
                        "light_ring", 3072, this, 2, &task_) != pdPASS) {
            error_ = ESP_ERR_NO_MEM;
        }
        if (error_ != ESP_OK) {
            led_strip_del(strip_);
            strip_ = nullptr;
        }
    }

    cJSON* Status() {
        std::lock_guard lock(mutex_);
        return StatusLocked();
    }

    cJSON* SetMode(Mode mode, std::optional<uint8_t> order) {
        std::lock_guard lock(mutex_);
        if (!task_) throw std::runtime_error(Error());
        if (mode == Mode::Music && music_error_ != ESP_OK) {
            throw std::runtime_error(std::string("Music spectrum unavailable: ") + esp_err_to_name(music_error_));
        }
        if (order && *order != color_order_) {
            SaveColorOrder(*order);
            color_order_ = *order;
        }
        const auto now = esp_timer_get_time();
        error_ = Render(mode, 0);
        if (error_ != ESP_OK) throw std::runtime_error(Error());
        mode_ = mode;
        started_at_ = now;
        music_.Enable(mode == Mode::Music);
        xTaskNotifyGive(task_);
        return StatusLocked();
    }

    void PushAudio(const int16_t* samples, size_t count) { music_.Push(samples, count); }

private:
    const char* Error() const {
        if (!startup_error_.empty()) return startup_error_.c_str();
        return error_ == ESP_OK ? "" : esp_err_to_name(error_);
    }

    cJSON* StatusLocked() const {
        auto* result = cJSON_CreateObject();
        cJSON_AddBoolToObject(result, "ready", task_ && error_ == ESP_OK);
        cJSON_AddNumberToObject(result, "mode", static_cast<int>(mode_));
        cJSON_AddNumberToObject(result, "gpio", kLightRingGpio);
        cJSON_AddNumberToObject(result, "led_count", kLedCount);
        cJSON_AddNumberToObject(result, "frames_sent", frames_sent_);
        cJSON_AddStringToObject(result, "color_order", kColorOrders[color_order_].name);
        cJSON_AddBoolToObject(result, "music_ready", task_ && music_error_ == ESP_OK);
        cJSON_AddNumberToObject(result, "music_windows", music_.WindowsProcessed());
        cJSON_AddStringToObject(result, "error", Error());
        return result;
    }

    esp_err_t Render(Mode mode, int64_t elapsed_ms) {
        if (mode == Mode::Music) {
            const auto result = music_.Update(esp_timer_get_time());
            if (result != ESP_OK) return result;
        }
        const bool dark = mode == Mode::Off || (mode == Mode::Flash && elapsed_ms % 500 >= 250);
        const auto result = dark ? led_strip_clear(strip_) : RenderColor(mode, elapsed_ms);
        if (result == ESP_OK) ++frames_sent_;
        return result;
    }

    esp_err_t SetPixel(unsigned index, const std::array<unsigned, 3>& rgb) const {
        const auto& order = kColorOrders[color_order_].indices;
        return led_strip_set_pixel(strip_, index, rgb[order[0]], rgb[order[1]], rgb[order[2]]);
    }

    esp_err_t SetMusicPixel(unsigned index) const {
        constexpr auto bands = LightMusic::kBandCount;
        const auto band = index * bands / kLedCount;
        const auto first = (band * kLedCount + bands - 1) / bands;
        const auto end = ((band + 1) * kLedCount + bands - 1) / bands;
        const float height = music_.Levels()[band] * (end - first);
        const float brightness = std::clamp(height - (index - first), 0.f, 1.f) * kMusicBrightness;
        const auto& color = kBandColors[band];
        return SetPixel(index, {static_cast<unsigned>(color[0] * brightness),
            static_cast<unsigned>(color[1] * brightness), static_cast<unsigned>(color[2] * brightness)});
    }

    esp_err_t RenderColor(Mode mode, int64_t elapsed_ms) const {
        const auto period = mode == Mode::Flash ? kFlashRainbowPeriodMs : kRainbowPeriodMs;
        const float frame_angle = kFullTurn * (elapsed_ms % period) / period;
        const float brightness = mode == Mode::Flash ? 200 : 100;
        for (unsigned i = 0; i < kLedCount; ++i) {
            esp_err_t result;
            if (mode == Mode::Music) {
                result = SetMusicPixel(i);
            } else if (Animated(mode)) {
                // Three phase-shifted sine waves from ESP-IDF's CC0/Unlicense
                // peripherals/rmt/led_strip_simple_encoder example.
                const float angle = frame_angle + i * kFullTurn / kLedCount;
                const float color_offset = kFullTurn / 3;
                const auto channel = [brightness](float phase) {
                    return static_cast<uint32_t>((std::sin(phase) + 1) * brightness / 2);
                };
                result = SetPixel(i, {channel(angle), channel(angle + color_offset),
                    channel(angle + 2 * color_offset)});
            } else if (mode == Mode::Warm) {
                result = SetPixel(i, {30, 180 * 30 / 255, 50 * 30 / 255});
            } else {
                result = SetPixel(i, {255, 255, 255});
            }
            if (result != ESP_OK) return result;
        }
        return led_strip_refresh(strip_);
    }

    void Animate() {
        TickType_t wait = portMAX_DELAY;
        while (true) {
            const auto changed = ulTaskNotifyTake(pdTRUE, wait);
            std::lock_guard lock(mutex_);
            if (!changed && Animated(mode_) && error_ == ESP_OK) {
                error_ = Render(mode_, (esp_timer_get_time() - started_at_) / 1000);
            }
            wait = Animated(mode_) && error_ == ESP_OK ? pdMS_TO_TICKS(kFrameMs) : portMAX_DELAY;
        }
    }

    std::mutex mutex_;
    led_strip_handle_t strip_ = nullptr;
    TaskHandle_t task_ = nullptr;
    LightMusic music_;
    Mode mode_ = Mode::Off;
    int64_t started_at_ = 0;
    uint32_t frames_sent_ = 0;
    uint8_t color_order_ = 0;
    esp_err_t error_ = ESP_OK;
    esp_err_t music_error_ = ESP_OK;
    std::string startup_error_;
};

LightRing ring;
}

void StartLightRing() {
    ring.Start();
}

void PushLightRingAudio(const int16_t* samples, size_t count) {
    ring.PushAudio(samples, count);
}

cJSON* HandleLightRingCommand(const std::string& method, const cJSON* params) {
    if (method == "light.status") return ring.Status();
    if (method == "light.set_mode") return ring.SetMode(ParseMode(params), ParseColorOrder(params));
    throw std::runtime_error("Unknown light ring command");
}
