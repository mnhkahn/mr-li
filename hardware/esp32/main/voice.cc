#include "voice.h"

#include <algorithm>
#include <atomic>
#include <cmath>
#include <cstdint>
#include <cstring>
#include <memory>
#include <mutex>
#include <stdexcept>
#include <string_view>
#include <utility>
#include <vector>
#include <driver/i2s_std.h>
#include <esp_timer.h>
#include <esp_random.h>
#include <esp_vad.h>
#include <esp_mn_models.h>
#include <esp_mn_speech_commands.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>
#include <model_path.h>
#include <led_strip.h>

namespace {
constexpr int kMicRate = 16000;
constexpr int kFrameMs = 30;
constexpr int kFrameSamples = kMicRate * kFrameMs / 1000;
constexpr int64_t kRecordingLifetimeUs = 30'000'000;
constexpr char kWakeWord[] = "小李";
constexpr char kWakePinyin[] = "xiao li";
constexpr char kGreetingPinyin[] = "ni hao xiao li";
constexpr int kWakeCommand = 1;

struct Recording {
    std::vector<int16_t> samples;
    int64_t expires = 0;
    unsigned wake = 0;
};

std::atomic<const char*> phase{"starting"};
std::atomic<const char*> failure{""};
std::atomic<const char*> indicator{"unavailable"};
unsigned boot_id = 0;
std::atomic<unsigned> wakes{0}, pending_wake{0}, mic_peak{0}, read_errors{0};
std::atomic<unsigned> capture_finished{0};
std::mutex capture_mutex;
unsigned requested_capture = 0, next_capture = 0;
std::mutex recording_mutex;
std::shared_ptr<Recording> recording;

std::shared_ptr<Recording> RecentRecording() {
    std::lock_guard lock(recording_mutex);
    if (recording && esp_timer_get_time() >= recording->expires) recording.reset();
    return recording;
}

void Check(esp_err_t error, const char* operation) {
    if (error != ESP_OK) throw std::runtime_error(std::string(operation) + ": " + esp_err_to_name(error));
}

class WakeIndicator {
public:
    WakeIndicator() {
        led_strip_config_t config = {};
        config.strip_gpio_num = GPIO_NUM_48;
        config.max_leds = 1;
        config.color_component_format = LED_STRIP_COLOR_COMPONENT_FMT_GRB;
        config.led_model = LED_MODEL_WS2812;
        led_strip_rmt_config_t rmt = {};
        rmt.resolution_hz = 10'000'000;
        if (led_strip_new_rmt_device(&config, &rmt, &strip_) == ESP_OK) Set("off", 0, 0);
    }

    ~WakeIndicator() {
        if (!strip_) return;
        led_strip_clear(strip_);
        led_strip_del(strip_);
        indicator = "unavailable";
    }

    void Set(const char* state, unsigned green, unsigned blue) {
        if (!strip_) return;
        if (led_strip_set_pixel(strip_, 0, 0, green, blue) == ESP_OK &&
            led_strip_refresh(strip_) == ESP_OK) indicator = state;
        else indicator = "unavailable";
    }

private:
    led_strip_handle_t strip_ = nullptr;
};

struct AudioChannel {
    i2s_chan_handle_t handle = nullptr;
    bool enabled = false;
    int32_t previous_sample = 0;
    float high_pass = 0;

    ~AudioChannel() {
        if (enabled) i2s_channel_disable(handle);
        if (handle) i2s_del_channel(handle);
    }

    void Open() {
        i2s_chan_config_t channel = I2S_CHANNEL_DEFAULT_CONFIG(I2S_NUM_1, I2S_ROLE_MASTER);
        channel.dma_desc_num = 3;
        channel.dma_frame_num = kFrameSamples;
        Check(i2s_new_channel(&channel, nullptr, &handle), "Create I2S microphone");
        i2s_std_config_t config = {};
        config.clk_cfg = I2S_STD_CLK_DEFAULT_CONFIG(kMicRate);
        config.slot_cfg = I2S_STD_PHILIPS_SLOT_DEFAULT_CONFIG(I2S_DATA_BIT_WIDTH_32BIT, I2S_SLOT_MODE_MONO);
        config.slot_cfg.slot_mask = I2S_STD_SLOT_LEFT;
        config.gpio_cfg.mclk = I2S_GPIO_UNUSED;
        config.gpio_cfg.bclk = GPIO_NUM_2;
        config.gpio_cfg.ws = GPIO_NUM_1;
        config.gpio_cfg.dout = I2S_GPIO_UNUSED;
        config.gpio_cfg.din = GPIO_NUM_42;
        Check(i2s_channel_init_std_mode(handle, &config), "Initialize I2S");
    }

    void Enable(bool enable) {
        if (enable == enabled) return;
        Check(enable ? i2s_channel_enable(handle) : i2s_channel_disable(handle), "Change I2S state");
        enabled = enable;
    }
};

bool Read(AudioChannel& mic, int32_t* raw, int16_t* pcm, size_t samples) {
    size_t bytes = 0;
    if (i2s_channel_read(mic.handle, raw, samples * sizeof(*raw), &bytes, 200) != ESP_OK ||
        bytes != samples * sizeof(*raw)) {
        ++read_errors;
        return false;
    }
    unsigned peak = 0;
    for (size_t i = 0; i < samples; ++i) {
        // Retain the original board's I2S gain while removing low-frequency mic drift.
        const int32_t sample = raw[i] >> 12;
        mic.high_pass = 0.98f * mic.high_pass + (sample - mic.previous_sample);
        mic.previous_sample = sample;
        pcm[i] = std::clamp<int32_t>(static_cast<int32_t>(mic.high_pass), -32768, 32767);
        peak = std::max(peak, static_cast<unsigned>(std::abs(static_cast<int>(pcm[i]))));
    }
    mic_peak = peak;
    return true;
}

std::shared_ptr<Recording> Capture(AudioChannel& mic, vad_handle_t vad, unsigned wake) {
    phase = "recording";
    vad_reset_trigger(vad);
    auto clip = std::make_shared<Recording>();
    clip->samples.reserve(kFrameSamples * (8000 / kFrameMs));
    int32_t raw[kFrameSamples];
    int16_t pcm[kFrameSamples];
    bool heard_speech = false;
    int quiet_ms = 0;
    for (int ms = 0; ms + kFrameMs <= 8000; ms += kFrameMs) {
        if (!Read(mic, raw, pcm, kFrameSamples)) return {};
        const bool speech = vad_process_with_trigger(vad, pcm) == VAD_SPEECH;
        heard_speech |= speech;
        quiet_ms = speech ? 0 : quiet_ms + kFrameMs;
        clip->samples.insert(clip->samples.end(), pcm, pcm + kFrameSamples);
        if (!heard_speech && ms >= 4500) return {};
        if (heard_speech && ms >= 3200 && quiet_ms >= 600) break;
    }
    if (!heard_speech) return {};
    clip->expires = esp_timer_get_time() + kRecordingLifetimeUs;
    clip->wake = wake;
    return clip;
}

void Record(AudioChannel& mic, vad_handle_t vad, WakeIndicator& led, unsigned id) {
    led.Set("green", 18, 0);
    auto clip = Capture(mic, vad, id);
    {
        std::lock_guard lock(recording_mutex);
        recording = std::move(clip);
    }
    led.Set("off", 0, 0);
}

void Listen() {
    AudioChannel mic;
    mic.Open();

    auto models = std::unique_ptr<srmodel_list_t, decltype(&esp_srmodel_deinit)>(
        esp_srmodel_init("model"), esp_srmodel_deinit);
    if (!models || models->num <= 0) throw std::runtime_error("Missing speech model partition");
    auto* name = esp_srmodel_filter(models.get(), ESP_MN_PREFIX, "cn");
    if (!name) throw std::runtime_error("Missing Chinese MultiNet model");
    const auto* iface = esp_mn_handle_from_name(name);
    if (!iface) throw std::runtime_error("Missing MultiNet implementation");
    auto detector = std::unique_ptr<model_iface_data_t, decltype(iface->destroy)>(
        iface->create(name, 6000), iface->destroy);
    if (!detector || iface->get_samp_rate(detector.get()) != kMicRate) throw std::runtime_error("Create MultiNet failed");
    struct Commands {
        ~Commands() { esp_mn_commands_free(); }
    } commands;
    Check(esp_mn_commands_alloc(iface, detector.get()), "Initialize wake command");
    Check(esp_mn_commands_add(kWakeCommand, kWakePinyin), "Add wake command");
    Check(esp_mn_commands_add(kWakeCommand, kGreetingPinyin), "Add greeting wake command");
    if (esp_mn_commands_update()) throw std::runtime_error("Install wake command failed");
    iface->set_det_threshold(detector.get(), 0.30f);
    const int samples = iface->get_samp_chunksize(detector.get());
    if (samples <= 0 || samples > 2048) throw std::runtime_error("Invalid MultiNet chunk size");
    auto vad = std::unique_ptr<vad_handle_with_trigger_t, decltype(&vad_destroy)>(
        vad_create_with_param(VAD_MODE_2, kMicRate, kFrameMs, 150, 180), vad_destroy);
    if (!vad) throw std::runtime_error("Create speech detector failed");
    std::vector<int32_t> raw(samples);
    std::vector<int16_t> pcm(samples);
    WakeIndicator led;
    mic.Enable(true);
    for (;;) {
        unsigned capture;
        {
            std::lock_guard lock(capture_mutex);
            capture = std::exchange(requested_capture, 0);
            phase = capture ? "recording" : "listening";
        }
        RecentRecording(); // Drop expired utterances even if nobody connects.
        if (capture) {
            iface->clean(detector.get());
            Record(mic, vad.get(), led, capture);
            capture_finished = capture;
            continue;
        }
        if (!Read(mic, raw.data(), pcm.data(), samples)) continue;
        const auto detection = iface->detect(detector.get(), pcm.data());
        if (detection == ESP_MN_STATE_DETECTING) continue;
        const auto* result = detection == ESP_MN_STATE_DETECTED ? iface->get_results(detector.get()) : nullptr;
        const bool matched = result && result->num > 0 && result->command_id[0] == kWakeCommand;
        iface->clean(detector.get());
        if (!matched) continue;
        unsigned wake;
        {
            std::lock_guard lock(capture_mutex);
            if (requested_capture) continue;
            ++wakes;
            wake = ++next_capture;
            phase = "awaiting_host";
            pending_wake = wake;
        }
        {
            std::lock_guard lock(recording_mutex);
            recording.reset();
        }
        led.Set("blue", 0, 18);
        const int64_t deadline = esp_timer_get_time() + 15'000'000;
        while (pending_wake.load() == wake && esp_timer_get_time() < deadline) {
            // Keep I2S clocks running so the mic does not have to settle again.
            // These frames are discarded while the host plays the prompt.
            Read(mic, raw.data(), pcm.data(), samples);
        }
        unsigned expected = wake;
        const bool accepted = !pending_wake.compare_exchange_strong(expected, 0);
        if (accepted) {
            Record(mic, vad.get(), led, wake);
        } else {
            led.Set("off", 0, 0);
        }
    }
}

void PutLE(char* output, unsigned value, unsigned bytes) {
    for (unsigned i = 0; i < bytes; ++i) output[i] = static_cast<char>(value >> (i * 8));
}
}

void StartHardwareVoice() {
    boot_id = esp_random();
    if (xTaskCreatePinnedToCore([](void*) {
        try { Listen(); }
        catch (const std::exception& error) {
            static char message[128];
            std::strncpy(message, error.what(), sizeof(message) - 1);
            failure = message;
            phase = "error";
        }
        vTaskDelete(nullptr);
    }, "voice", 16 * 1024, nullptr, 5, nullptr, 1) != pdPASS) {
        failure = "Create voice task failed";
        phase = "error";
    }
}

cJSON* HandleVoiceCommand(const std::string& method, const cJSON* params, bool local_diagnostics) {
    if (method == "voice.listen") {
        const auto boot = cJSON_GetObjectItemCaseSensitive(params, "boot_id");
        const auto wake = cJSON_GetObjectItemCaseSensitive(params, "wake");
        if (cJSON_IsNumber(boot) && boot->valuedouble == boot_id &&
            cJSON_IsNumber(wake) && wake->valuedouble == 0) {
            std::lock_guard lock(capture_mutex);
            if (requested_capture || std::string_view(phase.load()) != "listening")
                throw std::runtime_error("Microphone busy");
            requested_capture = ++next_capture;
            auto result = cJSON_CreateObject();
            cJSON_AddNumberToObject(result, "capture", requested_capture);
            return result;
        }
        unsigned pending = pending_wake.load();
        if (!cJSON_IsNumber(boot) || boot->valuedouble != boot_id ||
            !cJSON_IsNumber(wake) || !pending || wake->valuedouble != pending ||
            !pending_wake.compare_exchange_strong(pending, 0))
            throw std::runtime_error("No matching pending wake word");
        auto result = cJSON_CreateObject();
        cJSON_AddBoolToObject(result, "accepted", true);
        return result;
    }
    if (method != "voice.status") throw std::runtime_error("Unknown voice method");
    auto result = cJSON_CreateObject();
    auto clip = RecentRecording();
    cJSON_AddStringToObject(result, "state", phase.load());
    cJSON_AddStringToObject(result, "error", failure.load());
    cJSON_AddStringToObject(result, "wake_word", kWakeWord);
    cJSON_AddStringToObject(result, "indicator", indicator.load());
    cJSON_AddNumberToObject(result, "wake_count", wakes.load());
    cJSON_AddNumberToObject(result, "boot_id", boot_id);
    cJSON_AddNumberToObject(result, "pending_wake", pending_wake.load());
    cJSON_AddBoolToObject(result, "on_demand_capture", true);
    cJSON_AddNumberToObject(result, "capture_finished", capture_finished.load());
    if (local_diagnostics) {
        cJSON_AddNumberToObject(result, "mic_peak", mic_peak.load());
        cJSON_AddNumberToObject(result, "read_errors", read_errors.load());
    }
    cJSON_AddNumberToObject(result, "recording_wake", clip ? clip->wake : 0);
    cJSON_AddNumberToObject(result, "recording_samples", clip ? clip->samples.size() : 0);
    return result;
}

esp_err_t SendWakeRecording(httpd_req_t* request) {
    auto clip = RecentRecording();
    httpd_resp_set_hdr(request, "Cache-Control", "no-store");
    if (!clip) {
        httpd_resp_set_status(request, "204 No Content");
        return httpd_resp_send(request, nullptr, 0);
    }
    const unsigned bytes = clip->samples.size() * sizeof(int16_t);
    char header[44] = {};
    std::memcpy(header, "RIFF", 4);
    PutLE(header + 4, bytes + 36, 4);
    std::memcpy(header + 8, "WAVEfmt ", 8);
    PutLE(header + 16, 16, 4);
    PutLE(header + 20, 1, 2);
    PutLE(header + 22, 1, 2);
    PutLE(header + 24, kMicRate, 4);
    PutLE(header + 28, kMicRate * sizeof(int16_t), 4);
    PutLE(header + 32, sizeof(int16_t), 2);
    PutLE(header + 34, 16, 2);
    std::memcpy(header + 36, "data", 4);
    PutLE(header + 40, bytes, 4);
    httpd_resp_set_type(request, "audio/wav");
    const std::string wake = std::to_string(boot_id) + "-" + std::to_string(clip->wake);
    httpd_resp_set_hdr(request, "X-Mr-Li-Wake", wake.c_str());
    esp_err_t rc = httpd_resp_send_chunk(request, header, sizeof(header));
    if (rc != ESP_OK) return rc;
    rc = httpd_resp_send_chunk(request, reinterpret_cast<const char*>(clip->samples.data()), bytes);
    return rc == ESP_OK ? httpd_resp_send_chunk(request, nullptr, 0) : rc;
}
