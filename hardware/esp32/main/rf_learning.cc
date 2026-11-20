#include "rf_learning.h"
#include <stdexcept>
#include <string>
#include <driver/rmt_rx.h>
#include <esp_heap_caps.h>
#include <esp_timer.h>
#include <freertos/FreeRTOS.h>
#include <freertos/queue.h>
#include <soc/soc_caps.h>

namespace curtain {
CaptureResult LearnRf(int gpio, int seconds, int gap_us, const std::function<void(CaptureProgress)>& progress) {
#if !SOC_RMT_SUPPORT_DMA
    return {{}, {}, "RF learning requires ESP32-S3 DMA; import verified waveforms on C3"};
#else
    struct Resources {
        rmt_channel_handle_t channel = nullptr;
        QueueHandle_t queue = nullptr;
        rmt_symbol_word_t* buffer = nullptr;
        bool enabled = false;
        ~Resources() {
            if (enabled) rmt_disable(channel);
            if (channel) rmt_del_channel(channel);
            if (queue) vQueueDelete(queue);
            heap_caps_free(buffer);
        }
    } resources;
    // Timeouts and peripheral errors are ordinary outcomes, not exceptions
    // crossing a FreeRTOS task boundary. Resources unwind on normal return.
#define RF_CHECK(call, operation) do { const auto rc = (call); if (rc != ESP_OK) return {{}, {}, std::string(operation) + ": " + esp_err_to_name(rc)}; } while (false)
    constexpr size_t kSymbols = 512;
    resources.buffer = static_cast<rmt_symbol_word_t*>(heap_caps_calloc(kSymbols, sizeof(rmt_symbol_word_t), MALLOC_CAP_DMA | MALLOC_CAP_INTERNAL));
    resources.queue = xQueueCreate(1, sizeof(rmt_rx_done_event_data_t));
    if (!resources.buffer || !resources.queue) return {{}, {}, "No memory for RF capture"};
    rmt_rx_channel_config_t rx = {};
    rx.clk_src = RMT_CLK_SRC_DEFAULT;
    rx.resolution_hz = 1000000;
    rx.mem_block_symbols = kSymbols;
    rx.gpio_num = static_cast<gpio_num_t>(gpio);
    rx.flags.with_dma = true;
    RF_CHECK(rmt_new_rx_channel(&rx, &resources.channel), "Create RF receiver");
    rmt_rx_event_callbacks_t callbacks = {};
    callbacks.on_recv_done = [](rmt_channel_handle_t, const rmt_rx_done_event_data_t* data, void* context) {
        BaseType_t wake = pdFALSE;
        xQueueSendFromISR(static_cast<QueueHandle_t>(context), data, &wake);
        return wake == pdTRUE;
    };
    RF_CHECK(rmt_rx_register_event_callbacks(resources.channel, &callbacks, resources.queue), "Register RF capture");
    RF_CHECK(rmt_enable(resources.channel), "Enable RF receiver");
    resources.enabled = true;
    rmt_receive_config_t receive = {};
    receive.signal_range_min_ns = 1250;
    receive.signal_range_max_ns = 30000000;
    const int64_t deadline = esp_timer_get_time() + static_cast<int64_t>(seconds) * 1000000;
    FrameLearner learner;
    while (esp_timer_get_time() < deadline) {
        RF_CHECK(rmt_receive(resources.channel, resources.buffer, kSymbols * sizeof(rmt_symbol_word_t), &receive), "Start RF capture");
        rmt_rx_done_event_data_t event;
        while (xQueueReceive(resources.queue, &event, pdMS_TO_TICKS(100)) != pdTRUE) {
            if (esp_timer_get_time() >= deadline) return {{}, learner.Progress(), "Learning timed out; press the SAME button three separate times, pausing 1 second between presses"};
        }
        std::vector<int> pulses;
        pulses.reserve(event.num_symbols * 2);
        for (size_t i = 0; i < event.num_symbols; ++i) {
            const auto& symbol = event.received_symbols[i];
            if (symbol.duration0) pulses.push_back(symbol.level0 ? static_cast<int>(symbol.duration0) : -static_cast<int>(symbol.duration0));
            if (symbol.duration1) pulses.push_back(symbol.level1 ? static_cast<int>(symbol.duration1) : -static_cast<int>(symbol.duration1));
        }
        const int64_t now = esp_timer_get_time();
        std::vector<int64_t> ends;
        auto frames = SplitFrames(pulses, gap_us, &ends);
        int64_t total = 0;
        for (int pulse : pulses) total += std::abs(pulse);
        for (size_t i = 0; i < frames.size(); ++i) {
            if (learner.Feed(frames[i], now - total + ends[i])) return {frames[i], learner.Progress(), {}};
        }
        progress(learner.Progress());
    }
    return {{}, learner.Progress(), "No stable repeated RF frame found; check wiring, modulation and rolling-code behavior"};
#undef RF_CHECK
#endif
}
}
