#include "curtain_protocol.h"
#include <cassert>
#include <iostream>

int main() {
    curtain::Validate({350, -1050, 1050, -350, 350, -10850});
    for (auto frame : std::vector<std::vector<int>>{{}, {350}, {0, -100}, {100, 100},
            {-100, 100}, {32768, -100}, {100, -32768}, {30000, -30000, 30000, -30000, 30000, -30000},
            std::vector<int>(130, 100)}) {
        bool failed = false;
        try { curtain::Validate(frame); } catch (const std::runtime_error&) { failed = true; }
        assert(failed);
    }
    curtain::Validate({32767, -32767});
    std::cout << "Curtain RF waveform boundary checks passed\n";
}
