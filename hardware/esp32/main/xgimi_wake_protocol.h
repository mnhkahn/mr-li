#pragma once

#include <array>
#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace xgimi {
// Payload excludes the little-endian manufacturer ID (46 00). Some remotes
// omit the leading counter; never increment a MAC byte in that 15-byte format.
inline bool ValidToken(const std::vector<uint8_t>& token) {
    static constexpr uint8_t tail[] = {0xff, 0xff, 0xff, 0x30, 0x43, 0x52, 0x4b, 0x54, 0x4d};
    return (token.size() == 15 || token.size() == 16) &&
           std::memcmp(token.data() + token.size() - sizeof(tail), tail, sizeof(tail)) == 0;
}

inline std::vector<uint8_t> ParseToken(const std::string& hex) {
    if (hex.size() != 30 && hex.size() != 32) return {};
    auto digit = [](char c) -> int {
        if (c >= '0' && c <= '9') return c - '0';
        if (c >= 'a' && c <= 'f') return c - 'a' + 10;
        if (c >= 'A' && c <= 'F') return c - 'A' + 10;
        return -1;
    };
    std::vector<uint8_t> out;
    for (size_t i = 0; i < hex.size(); i += 2) {
        int a = digit(hex[i]), b = digit(hex[i + 1]);
        if (a < 0 || b < 0) return {};
        out.push_back(static_cast<uint8_t>((a << 4) | b));
    }
    return ValidToken(out) ? out : std::vector<uint8_t>{};
}

inline std::string Hex(const std::vector<uint8_t>& data) {
    static constexpr char digits[] = "0123456789ABCDEF";
    std::string out;
    for (auto b : data) { out += digits[b >> 4]; out += digits[b & 15]; }
    return out;
}

inline std::vector<uint8_t> FindToken(const uint8_t* data, size_t size) {
    for (size_t i = 0; i < size;) {
        const size_t len = data[i];
        if (len == 0 || len > size - i - 1) break;
        if (len >= 3 && data[i + 1] == 0xff && data[i + 2] == 0x46 && data[i + 3] == 0x00) {
            std::vector<uint8_t> token(data + i + 4, data + i + 1 + len);
            if (ValidToken(token)) return token;
        }
        i += len + 1;
    }
    return {};
}

inline std::vector<uint8_t> Advertisement(const std::vector<uint8_t>& token) {
    if (!ValidToken(token)) return {};
    // General discoverable, BR/EDR unsupported; HID UUID; remote-control appearance.
    std::vector<uint8_t> out = {2, 1, 6, 3, 3, 0x12, 0x18, 3, 0x19, 0xc1, 3};
    out.insert(out.end(), {static_cast<uint8_t>(token.size() + 3), 0xff, 0x46, 0});
    out.insert(out.end(), token.begin(), token.end());
    return out;
}
}  // namespace xgimi
