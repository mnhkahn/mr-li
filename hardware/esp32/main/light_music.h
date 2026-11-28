#pragma once

#include <array>
#include <atomic>
#include <cstddef>
#include <cstdint>
#include <mutex>
#include <esp_err.h>

class LightMusic {
public:
    static constexpr size_t kBandCount = 7;

    esp_err_t Start();
    void Enable(bool enabled);
    void Push(const int16_t* samples, size_t count);
    esp_err_t Update(int64_t now_us);

    const std::array<float, kBandCount>& Levels() const { return levels_; }
    uint32_t WindowsProcessed() const { return windows_processed_; }

private:
    static constexpr size_t kWindowSize = 512;

    std::atomic<bool> enabled_{false};
    std::mutex samples_mutex_;
    std::array<int16_t, kWindowSize> pending_{};
    std::array<int16_t, kWindowSize> latest_{};
    std::array<int16_t, kWindowSize> analysis_{};
    size_t buffered_ = 0;
    bool fresh_ = false;

    alignas(16) std::array<float, kWindowSize> twiddle_{};
    alignas(16) std::array<float, kWindowSize> window_{};
    alignas(16) std::array<float, kWindowSize * 2> fft_{};
    std::array<float, kBandCount> levels_{};
    uint32_t windows_processed_ = 0;
    int64_t last_analysis_us_ = 0;
};
