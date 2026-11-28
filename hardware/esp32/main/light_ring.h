#pragma once

#include <cstddef>
#include <cstdint>
#include <string>
#include <cJSON.h>

constexpr int kLightRingGpio = 14;

void StartLightRing();
void PushLightRingAudio(const int16_t* samples, size_t count);
cJSON* HandleLightRingCommand(const std::string& method, const cJSON* params);
