// Say worker's CPU-only ZipVoice engine. Text stays in a private request file,
// never shell code or command-line options. The same PID becomes afplay so that
// /speak stop also cancels synthesis and releases the playback lock.
#include <algorithm>
#include <chrono>
#include <cmath>
#include <cstdio>
#include <fcntl.h>
#include <fstream>
#include <iostream>
#include <iterator>
#include <stdexcept>
#include <string>
#include <sys/resource.h>
#include <sys/file.h>
#include <unistd.h>
#include <utility>
#include <vector>
#include "sherpa-onnx/c-api/cxx-api.h"

static std::string Read(const std::string &path) {
  std::ifstream in(path);
  if (!in) throw std::runtime_error("无法读取本地音色输入文件");
  return {std::istreambuf_iterator<char>(in), std::istreambuf_iterator<char>()};
}

// Shorten sustained near-silence between phrases without removing quiet onsets.
static void CompactPauses(sherpa_onnx::cxx::Wave &wave) {
  auto &samples = wave.samples;
  const size_t block = wave.sample_rate / 50;
  if (block == 0 || samples.size() < block * 26) return;
  struct Level {
    double rms;
    float peak;
  };
  std::vector<Level> levels;
  double max_rms = 0;
  float max_peak = 0;
  for (size_t start = 0; start < samples.size(); start += block) {
    const size_t end = std::min(start + block, samples.size());
    double squares = 0;
    float peak = 0;
    for (size_t i = start; i < end; ++i) {
      squares += double(samples[i]) * samples[i];
      peak = std::max(peak, std::abs(samples[i]));
    }
    const double rms = std::sqrt(squares / (end - start));
    levels.push_back({rms, peak});
    max_rms = std::max(max_rms, rms);
    max_peak = std::max(max_peak, peak);
  }
  if (max_peak == 0) return;
  const double quiet_rms = max_rms * 0.00178;  // -55 dB
  const float quiet_peak = max_peak * 0.00316f;  // -50 dB
  size_t start = 0;
  size_t read = 0;
  size_t write = 0;
  for (size_t i = 0; i < levels.size(); ++i) {
    if (levels[i].rms <= quiet_rms && levels[i].peak <= quiet_peak) continue;
    if (start > 0 && i - start >= 24) {  // At least 480 ms.
      // Keep 150 ms of room sound on each side of the pause.
      const size_t keep = wave.sample_rate * 15 / 100;
      const size_t cut = start * block + keep;
      if (write != read)
        std::copy(samples.begin() + read, samples.begin() + cut, samples.begin() + write);
      write += cut - read;
      read = i * block - keep;
    }
    start = i + 1;
  }
  if (read == 0) return;
  std::copy(samples.begin() + read, samples.end(), samples.begin() + write);
  samples.resize(write + samples.size() - read);
}

static void Publish(const std::string &output, const sherpa_onnx::cxx::Wave &wave) {
  const auto partial = output + "." + std::to_string(getpid()) + ".partial.wav";
  if (!sherpa_onnx::cxx::WriteWave(partial, wave))
    throw std::runtime_error("本地克隆音频写入失败");
  if (std::rename(partial.c_str(), output.c_str()) != 0)
    throw std::runtime_error("本地克隆音频保存失败");
}

static void Play(const std::string &root, const std::string &output, bool lock_on_play) {
  if (lock_on_play) {
    // Keep this descriptor across exec so audio stays serialized until afplay exits.
    const int playback = open((root + "/playback.lock").c_str(), O_CREAT | O_WRONLY, 0600);
    if (playback < 0 || flock(playback, LOCK_EX) != 0)
      throw std::runtime_error("无法取得音频播放锁");
  }
  execl("/usr/bin/afplay", "afplay", output.c_str(), static_cast<char *>(nullptr));
  throw std::runtime_error("无法启动本机音频播放");
}

int main(int argc, char **argv) {
  try {
    if (argc != 6) throw std::runtime_error("本地音色参数不完整");
    const std::string root = argv[1], request = argv[2], output = argv[3];
    const std::string mode = argv[5];
    if (mode == "compact-cache" || mode == "compact-play") {
      auto wave = sherpa_onnx::cxx::ReadWave(request);
      if (wave.samples.empty() || wave.sample_rate <= 0)
        throw std::runtime_error("缓存音频无效");
      CompactPauses(wave);
      Publish(output, wave);
      if (mode == "compact-play") Play(root, output, true);
      std::cout << output << std::endl;
      return 0;
    }
    if (mode != "cache" && mode != "play" && mode != "play-locked")
      throw std::runtime_error("本地音色模式无效");
    const int steps = std::stoi(argv[4]);
    if (steps < 1 || steps > 16) throw std::runtime_error("无效的生成步数");
    const auto text = Read(request);
    std::remove(request.c_str());
    if (text.empty() || text.size() > 8192 || text.find('\0') != std::string::npos)
      throw std::runtime_error("播报文字无效");
    using namespace sherpa_onnx::cxx;
    OfflineTtsConfig model;
    const auto dir = root + "/models/";
    model.model.zipvoice.encoder = dir + "encoder.int8.onnx";
    model.model.zipvoice.decoder = dir + "decoder.int8.onnx";
    model.model.zipvoice.vocoder = root + "/vocos_24khz.onnx";
    model.model.zipvoice.data_dir = dir + "espeak-ng-data";
    model.model.zipvoice.lexicon = dir + "lexicon.txt";
    model.model.zipvoice.tokens = dir + "tokens.txt";
    model.model.num_threads = 2;
    model.model.provider = "cpu";
    auto start = std::chrono::steady_clock::now();
    auto tts = OfflineTts::Create(model);
    if (!tts.Get()) throw std::runtime_error("本地克隆模型加载失败");
    const auto loaded = std::chrono::steady_clock::now();
    auto reference = ReadWave(root + "/reference.wav");
    if (reference.samples.empty() || reference.sample_rate <= 0)
      throw std::runtime_error("克隆参考音频无效");
    GenerationConfig options;
    options.reference_text = Read(root + "/reference.txt");
    if (options.reference_text.empty()) throw std::runtime_error("克隆参考文字为空");
    options.reference_audio = std::move(reference.samples);
    options.reference_sample_rate = reference.sample_rate;
    options.num_steps = steps;
    options.speed = 1.0;
    // Quiet cloned syllables must not be mistaken for silence and shortened.
    options.silence_scale = 1.0;
    options.extra["min_char_in_sentence"] = "10";
    options.extra["max_char_in_sentence"] = "20";
    auto audio = tts.Generate(text, options);
    const auto finished = std::chrono::steady_clock::now();
    if (audio.samples.empty() || audio.sample_rate <= 0)
      throw std::runtime_error("本地克隆没有生成有效音频");
    Wave wave{std::move(audio.samples), audio.sample_rate};
    CompactPauses(wave);
    Publish(output, wave);
    struct rusage usage {};
    getrusage(RUSAGE_SELF, &usage);
    std::fprintf(stderr, "LOCAL_TTS load_seconds=%.3f synth_seconds=%.3f audio_seconds=%.3f peak_rss_mb=%.1f steps=%d\n",
        std::chrono::duration<double>(loaded - start).count(),
        std::chrono::duration<double>(finished - loaded).count(),
        static_cast<double>(wave.samples.size()) / wave.sample_rate,
        static_cast<double>(usage.ru_maxrss) / 1024 / 1024, steps);
    if (mode == "play" || mode == "play-locked") Play(root, output, mode == "play-locked");
    std::cout << output << std::endl;
    return 0;
  } catch (const std::exception &error) {
    std::cerr << "本地克隆播报失败：" << error.what() << std::endl;
    return 1;
  }
}
