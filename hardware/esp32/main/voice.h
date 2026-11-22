#pragma once

#include <cJSON.h>
#include <esp_http_server.h>
#include <string>

void StartHardwareVoice();
cJSON* HandleVoiceCommand(const std::string& method, const cJSON* params, bool local_diagnostics = false);
esp_err_t SendWakeRecording(httpd_req_t* request);
