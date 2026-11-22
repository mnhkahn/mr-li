#pragma once
#include <algorithm>
#include <cstdint>
#include <cstdlib>
#include <functional>
#include <string>
#include <vector>
#include "curtain_protocol.h"

namespace curtain {
inline bool SameFrame(const std::vector<int>& a, const std::vector<int>& b) {
    if (a.size() != b.size()) return false;
    for (size_t i = 0; i < a.size(); ++i) {
        if ((a[i] > 0) != (b[i] > 0)) return false;
        const int x = std::abs(a[i]), y = std::abs(b[i]);
        if (std::abs(x - y) > std::max(80, x / 5)) return false;
    }
    return true;
}
// Accept only full frames ending in a measured low gap, never a clipped RX idle
// timeout or unfinished buffer tail. Actual duration limits remain TX-validated.
inline std::vector<std::vector<int>> SplitFrames(const std::vector<int>& pulses, int gap_us, std::vector<int64_t>* ends = nullptr) {
    std::vector<std::vector<int>> frames;
    std::vector<int> frame;
    int64_t elapsed = 0;
    for (int pulse : pulses) {
        elapsed += std::abs(pulse);
        if (frame.empty() && pulse < 0) continue;
        if (pulse == 0 || std::abs(pulse) < 80 || std::abs(pulse) >= 29000) { frame.clear(); continue; }
        if (!frame.empty() && ((pulse > 0) == (frame.back() > 0))) { frame.clear(); }
        if (frame.empty() && pulse < 0) continue;
        frame.push_back(pulse);
        if (pulse < 0 && -pulse >= gap_us) {
            if (frame.size() >= 16 && frame.size() <= 512) {
                try { Validate(frame); frames.push_back(frame); if (ends) ends->push_back(elapsed); } catch (...) {}
            }
            frame.clear();
        } else if (frame.size() > 512) frame.clear();
    }
    return frames;
}
struct CaptureProgress { int frames = 0, matches = 0, bursts = 0; };
struct CaptureResult { std::vector<int> pulses; CaptureProgress progress; std::string error; };
class FrameLearner {
    struct Candidate { std::vector<int> frame; int matches = 0, bursts = 0; int64_t last = 0; };
    std::vector<Candidate> candidates_;
    CaptureProgress progress_;
public:
    bool Feed(const std::vector<int>& frame, int64_t time_us) {
        ++progress_.frames;
        for (auto& candidate : candidates_) {
            if (!SameFrame(candidate.frame, frame)) continue;
            if (time_us - candidate.last >= 700000) ++candidate.bursts;
            candidate.last = time_us;
            ++candidate.matches;
            if (candidate.bursts > progress_.bursts || (candidate.bursts == progress_.bursts && candidate.matches > progress_.matches)) {
                progress_.bursts = candidate.bursts; progress_.matches = candidate.matches;
            }
            return candidate.bursts >= 3 && candidate.matches >= 3;
        }
        if (candidates_.size() == 12) {
            auto weakest = std::min_element(candidates_.begin(), candidates_.end(), [](const Candidate& a, const Candidate& b) {
                if (a.bursts != b.bursts) return a.bursts < b.bursts;
                if (a.matches != b.matches) return a.matches < b.matches;
                return a.last < b.last;
            });
            candidates_.erase(weakest);
        }
        candidates_.push_back({frame, 1, 1, time_us});
        progress_.matches = std::max(progress_.matches, 1);
        progress_.bursts = std::max(progress_.bursts, 1);
        return false;
    }
    CaptureProgress Progress() const { return progress_; }
};
CaptureResult LearnRf(int gpio, int seconds, int gap_us, const std::function<void(CaptureProgress)>& progress,
                      bool raw_capture = false);
}
