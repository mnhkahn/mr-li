#include "light_music.h"

#include <algorithm>
#include <cmath>
#include <dsps_fft2r.h>
#include <dsps_wind_hann.h>

namespace {
// At 16 kHz with 512 samples, each bin is 31.25 Hz. Skip the first two bins.
constexpr std::array<unsigned, LightMusic::kBandCount + 1> kBandEdges = {2, 4, 8, 16, 32, 64, 128, 256};
constexpr int64_t kAudioTimeoutUs = 150'000;
}

esp_err_t LightMusic::Start() {
    // ESP-DSP's FFT Window example uses a Hann window before its FFT and bit reversal.
    const auto result = dsps_fft2r_init_fc32(twiddle_.data(), kWindowSize);
    if (result != ESP_OK) return result;
    if (dsps_fft_w_table_size < static_cast<int>(kWindowSize)) return ESP_ERR_INVALID_SIZE;
    dsps_wind_hann_f32(window_.data(), kWindowSize);
    return ESP_OK;
}

void LightMusic::Enable(bool enabled) {
    if (enabled_.load(std::memory_order_relaxed) == enabled) return;
    std::lock_guard lock(samples_mutex_);
    buffered_ = 0;
    fresh_ = false;
    last_analysis_us_ = 0;
    levels_.fill(0);
    enabled_.store(enabled, std::memory_order_relaxed);
}

void LightMusic::Push(const int16_t* samples, size_t count) {
    if (!enabled_.load(std::memory_order_relaxed)) return;
    // The microphone task never waits for the light renderer.
    std::unique_lock lock(samples_mutex_, std::try_to_lock);
    if (!lock || !enabled_.load(std::memory_order_relaxed)) return;
    while (count > 0) {
        const auto size = std::min(count, pending_.size() - buffered_);
        std::copy_n(samples, size, pending_.begin() + buffered_);
        buffered_ += size;
        samples += size;
        count -= size;
        if (buffered_ == pending_.size()) {
            latest_ = pending_;
            fresh_ = true;
            buffered_ = 0;
        }
    }
}

esp_err_t LightMusic::Update(int64_t now_us) {
    bool got_samples;
    {
        std::lock_guard lock(samples_mutex_);
        got_samples = fresh_;
        if (got_samples) {
            analysis_ = latest_;
            fresh_ = false;
        }
    }
    if (!got_samples) {
        if (now_us - last_analysis_us_ >= kAudioTimeoutUs) levels_.fill(0);
        return ESP_OK;
    }

    for (size_t i = 0; i < analysis_.size(); ++i) {
        fft_[2 * i] = analysis_[i] * window_[i] / 32768.f;
        fft_[2 * i + 1] = 0;
    }
    auto result = dsps_fft2r_fc32(fft_.data(), kWindowSize);
    if (result == ESP_OK) result = dsps_bit_rev_fc32(fft_.data(), kWindowSize);
    if (result != ESP_OK) return result;

    for (size_t band = 0; band < levels_.size(); ++band) {
        float peak_power = 0;
        for (auto bin = kBandEdges[band]; bin < kBandEdges[band + 1]; ++bin) {
            const float real = fft_[2 * bin];
            const float imaginary = fft_[2 * bin + 1];
            peak_power = std::max(peak_power, real * real + imaginary * imaginary);
        }
        const float peak = std::sqrt(peak_power) * 4.f / kWindowSize;
        const float level = std::clamp((peak - 0.004f) * 18.f, 0.f, 1.f);
        levels_[band] = std::max(level, levels_[band] * 0.8f);
    }
    last_analysis_us_ = now_us;
    ++windows_processed_;
    return ESP_OK;
}
