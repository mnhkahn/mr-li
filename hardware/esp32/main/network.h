#pragma once
#include <string>
#include <cJSON.h>
void StartHardwareNetwork();
cJSON* HandleNetworkCommand(const std::string& method, const cJSON* params);
