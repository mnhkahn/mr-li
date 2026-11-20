#pragma once
#include <cstdint>
#include <stdexcept>
#include <vector>

namespace curtain {
// Signed microseconds: positive = carrier on, negative = carrier off.
// Carrier frequency comes from the external 433 MHz OOK transmitter, not RMT.
inline void Validate(const std::vector<int>& pulses) {
    if (pulses.size() < 2 || pulses.size() > 128 || pulses.size() % 2)
        throw std::runtime_error("Each frame needs 2..128 alternating high/low pulses");
    uint32_t duration = 0;
    for (size_t i = 0; i < pulses.size(); ++i) {
        const int pulse = pulses[i];
        if ((i % 2 == 0 && pulse <= 0) || (i % 2 == 1 && pulse >= 0) || pulse < -32767 || pulse > 32767)
            throw std::runtime_error("Pulses must alternate +high/-low, each 1..32767 microseconds");
        duration += static_cast<uint32_t>(pulse < 0 ? -pulse : pulse);
    }
    if (duration > 150000) throw std::runtime_error("One RF frame must not exceed 150 ms");
}
}  // namespace curtain
