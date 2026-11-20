#include "xgimi_wake_protocol.h"
#include <cassert>
#include <iostream>

int main() {
    // Example shape from XGIMI protocol research, never a device default.
    const auto token = xgimi::ParseToken("12D7C7899B9F80FFFFFF3043524B544D");
    assert(token.size() == 16);
    assert(xgimi::Hex(token) == "12D7C7899B9F80FFFFFF3043524B544D");
    auto advertisement = xgimi::Advertisement(token);
    assert(advertisement.size() == 31);
    assert(advertisement[11] == 19 && advertisement[12] == 0xff);
    assert(advertisement[13] == 0x46 && advertisement[14] == 0);
    assert(xgimi::FindToken(advertisement.data(), advertisement.size()) == token);
    // Every truncated manufacturer section must fail, without reading beyond it.
    for (size_t n = 0; n < advertisement.size(); ++n)
        assert(xgimi::FindToken(advertisement.data(), n).empty());
    advertisement[13] = 0x4c; // Apple advertisements are not XGIMI wake packets.
    assert(xgimi::FindToken(advertisement.data(), advertisement.size()).empty());
    assert(xgimi::ParseToken("460012D7C7899B9F80FFFFFF3043524B544D").empty());
    assert(xgimi::ParseToken("12D7C7899B9F80FFFFFF3043524B544Z").empty());
    assert(xgimi::ParseToken("12D7C7899B9F800000003043524B544D").empty());
    assert(xgimi::Advertisement({}).empty());
    auto legacy = xgimi::ParseToken("D7C7899B9F80FFFFFF3043524B544D");
    assert(legacy.size() == 15);
    auto legacy_ad = xgimi::Advertisement(legacy);
    assert(legacy_ad.size() == 30);
    assert(xgimi::FindToken(legacy_ad.data(), legacy_ad.size()) == legacy);
    // An unrelated preceding AD section and trailing scan-response data are safe.
    legacy_ad.insert(legacy_ad.end(), {2, 9, 'x'});
    assert(xgimi::FindToken(legacy_ad.data(), legacy_ad.size()) == legacy);
    const uint8_t malformed[] = {255, 0xff, 0x46, 0};
    assert(xgimi::FindToken(malformed, sizeof(malformed)).empty());
    std::cout << "XGIMI wake protocol checks passed\n";
}
