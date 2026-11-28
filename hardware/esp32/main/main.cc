#include "xgimi_wake.h"
#include "curtain.h"
#include "light_ring.h"
#include "network.h"
#include "voice.h"
#include <cstdio>
#include <cstdarg>
#include <esp_log.h>
#include <string>
#include <stdexcept>
#include <driver/usb_serial_jtag.h>
#if CONFIG_IDF_TARGET_ESP32S3
#include <driver/uart.h>
#endif
#include <nvs_flash.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>

namespace {
constexpr size_t kMaxLine = 4096;
using WriteSerial = int (*)(const char*, size_t);

void Reply(const std::string& line, WriteSerial write) {
    auto request = cJSON_Parse(line.c_str());
    auto response = cJSON_CreateObject();
    auto id = cJSON_GetObjectItemCaseSensitive(request, "id");
    if (id) cJSON_AddItemToObject(response, "id", cJSON_Duplicate(id, true));
    else cJSON_AddNullToObject(response, "id");
    try {
        auto method = cJSON_GetObjectItemCaseSensitive(request, "method");
        auto params = cJSON_GetObjectItemCaseSensitive(request, "params");
        if (!cJSON_IsObject(request) || !cJSON_IsString(method))
            throw std::runtime_error("Expected a JSON object with method and id");
        if (params && !cJSON_IsObject(params)) throw std::runtime_error("params must be an object");
        std::string name = method->valuestring;
        cJSON* result;
#if CONFIG_IDF_TARGET_ESP32S3
        if (name.rfind("voice.", 0) == 0) result = HandleVoiceCommand(name, params, true);
        else if (name.rfind("light.", 0) == 0) result = HandleLightRingCommand(name, params);
        else
#endif
        result = name.rfind("network.", 0) == 0 ? HandleNetworkCommand(name, params) :
            name.rfind("curtain.", 0) == 0 ? HandleCurtainCommand(name, params) : HandleXgimiCommand(name, params);
        cJSON_AddItemToObject(response, "result", result);
    } catch (const std::exception& e) {
        cJSON_AddStringToObject(response, "error", e.what());
    }
    auto json = cJSON_PrintUnformatted(response);
    if (json) {
        auto output = std::string("MRLI_HW_RESULT ") + json + "\n";
        size_t offset = 0;
        while (offset < output.size()) {
            int written = write(output.data() + offset, output.size() - offset);
            if (written <= 0) break;
            offset += written;
        }
        cJSON_free(json);
    }
    cJSON_Delete(response);
    cJSON_Delete(request);
}

class SerialInput {
public:
    explicit SerialInput(WriteSerial write) : write_(write) {}

    void Read(const uint8_t* buffer, int count) {
        for (int i = 0; i < count; ++i) {
            const char c = static_cast<char>(buffer[i]);
            if (c == '\n') {
                if (overflow_) Reply("{}", write_);
                else if (!line_.empty()) Reply(line_, write_);
                line_.clear(); overflow_ = false;
            } else if (c != '\r' && !overflow_) {
                if (line_.size() == kMaxLine) { line_.clear(); overflow_ = true; }
                else line_ += c;
            }
        }
    }

private:
    WriteSerial write_;
    std::string line_;
    bool overflow_ = false;
};
}

extern "C" void app_main() {
    // Keep SDK logs from interleaving with USB JSON. Panic output still uses
    // the secondary USB console directly for diagnostics.
    esp_log_set_vprintf([](const char*, va_list) { return 0; });
    // Preserve NVS on errors: never silently erase learned data or old settings.
    ESP_ERROR_CHECK(nvs_flash_init());
    usb_serial_jtag_driver_config_t config = {};
    config.rx_buffer_size = 4096;
    config.tx_buffer_size = 4096;
    ESP_ERROR_CHECK(usb_serial_jtag_driver_install(&config));
#if CONFIG_IDF_TARGET_ESP32S3
    // The GOOUUU board's second USB connector is wired to UART0 through a bridge.
    uart_config_t uart_config = {};
    uart_config.baud_rate = 115200;
    uart_config.data_bits = UART_DATA_8_BITS;
    uart_config.parity = UART_PARITY_DISABLE;
    uart_config.stop_bits = UART_STOP_BITS_1;
    uart_config.flow_ctrl = UART_HW_FLOWCTRL_DISABLE;
    uart_config.source_clk = UART_SCLK_DEFAULT;
    ESP_ERROR_CHECK(uart_param_config(UART_NUM_0, &uart_config));
    ESP_ERROR_CHECK(uart_set_pin(UART_NUM_0, 43, 44, UART_PIN_NO_CHANGE, UART_PIN_NO_CHANGE));
    ESP_ERROR_CHECK(uart_driver_install(UART_NUM_0, 4096, 4096, 0, nullptr, 0));
    SerialInput uart([](const char* data, size_t size) { return uart_write_bytes(UART_NUM_0, data, size); });
    constexpr int kUsbReadMs = 20;
    StartHardwareVoice();
    StartLightRing();
#else
    constexpr int kUsbReadMs = 100;
#endif
    StartHardwareNetwork();
    SerialInput usb([](const char* data, size_t size) {
        return usb_serial_jtag_write_bytes(data, size, pdMS_TO_TICKS(1000));
    });
    uint8_t buffer[512];
    while (true) {
        usb.Read(buffer, usb_serial_jtag_read_bytes(buffer, sizeof(buffer), pdMS_TO_TICKS(kUsbReadMs)));
#if CONFIG_IDF_TARGET_ESP32S3
        uart.Read(buffer, uart_read_bytes(UART_NUM_0, buffer, sizeof(buffer), 0));
#endif
    }
}
