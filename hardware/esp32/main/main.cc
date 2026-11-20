#include "xgimi_wake.h"
#include "curtain.h"
#include "network.h"
#include <cstdio>
#include <string>
#include <stdexcept>
#include <driver/usb_serial_jtag.h>
#include <nvs_flash.h>
#include <freertos/FreeRTOS.h>
#include <freertos/task.h>

namespace {
constexpr size_t kMaxLine = 4096;
void Reply(const std::string& line) {
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
        auto result = name.rfind("network.", 0) == 0 ? HandleNetworkCommand(name, params) :
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
            int written = usb_serial_jtag_write_bytes(output.data() + offset,
                output.size() - offset, pdMS_TO_TICKS(1000));
            if (written <= 0) break;
            offset += written;
        }
        cJSON_free(json);
    }
    cJSON_Delete(response);
    cJSON_Delete(request);
}
}

extern "C" void app_main() {
    // Preserve NVS on errors: never silently erase learned data or old settings.
    ESP_ERROR_CHECK(nvs_flash_init());
    usb_serial_jtag_driver_config_t config = {};
    config.rx_buffer_size = 4096;
    config.tx_buffer_size = 4096;
    ESP_ERROR_CHECK(usb_serial_jtag_driver_install(&config));
    StartHardwareNetwork();
    std::string line;
    bool overflow = false;
    uint8_t buffer[128];
    while (true) {
        int count = usb_serial_jtag_read_bytes(buffer, sizeof(buffer), pdMS_TO_TICKS(100));
        for (int i = 0; i < count; ++i) {
            const char c = static_cast<char>(buffer[i]);
            if (c == '\n') {
                if (overflow) Reply("{}");
                else if (!line.empty()) Reply(line);
                line.clear(); overflow = false;
            } else if (c != '\r' && !overflow) {
                if (line.size() == kMaxLine) { line.clear(); overflow = true; }
                else line += c;
            }
        }
    }
}
