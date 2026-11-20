#pragma once
#include <string>
#include <cJSON.h>

// Synchronous dispatch; learn/wake return an asynchronous job acknowledgement.
cJSON* HandleXgimiCommand(const std::string& method, const cJSON* params);

// Initialize the shared radio at boot without transmitting a wake packet.
void InitializeXgimiBluetooth();
