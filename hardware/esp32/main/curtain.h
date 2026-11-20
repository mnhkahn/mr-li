#pragma once
#include <string>
#include <cJSON.h>

cJSON* HandleCurtainCommand(const std::string& method, const cJSON* params);
