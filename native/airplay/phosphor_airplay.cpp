// SPDX-License-Identifier: MIT
// C shim over airplay2-sender-cpp (Apache-2.0, vendored) for Phosphor.

#include "phosphor_airplay.h"

#include <atomic>
#include <chrono>
#include <cstdio>
#include <cstdlib>
#include <mutex>
#include <optional>
#include <span>
#include <string>
#include <thread>

#include "raop_loop.h"
#include "raop_sender.h"
#include "ring_buffer.h"

using namespace fxchain;

namespace {
struct NowPlaying {
    std::string title, artist, album;
};
}  // namespace

struct ap_session {
    // ~1.5 s of 48 kHz stereo; the Rust side only feeds it once streaming.
    RingBuffer<int16_t> ring{1u << 17};
    std::atomic<bool> stop{false};
    std::mutex mu;
    std::optional<double> volume;
    std::optional<NowPlaying> now_playing;
    ap_event_cb cb = nullptr;
    void* user = nullptr;
    std::atomic<bool> events_on{true};
    std::thread thread;

    void emit(int kind, const std::string& msg) {
        if (cb && events_on.load()) cb(user, kind, msg.c_str());
    }
};

extern "C" ap_session* ap_create(const char* host, uint16_t port, const char* sender_name,
                                 int auth, int airplay2, const char* receiver_device_id,
                                 uint32_t sample_rate, double initial_volume_pct,
                                 ap_event_cb cb, void* user) {
    auto* s = new (std::nothrow) ap_session();
    if (!s) return nullptr;
    s->cb = cb;
    s->user = user;
    std::string h = host ? host : "";
    std::string name = sender_name ? sender_name : "Phosphor";
    std::string dev = receiver_device_id ? receiver_device_id : "";

    s->thread = std::thread([s, h, port, name, auth, airplay2, dev, sample_rate,
                             initial_volume_pct] {
        RaopLoop loop;
        RaopEvents ev;
        ev.launched = [s](bool ok, const std::string& err) {
            s->emit(ok ? AP_EVENT_LAUNCHED : AP_EVENT_LAUNCH_FAILED, err);
        };
        ev.closed = [s] { s->emit(AP_EVENT_CLOSED, ""); };
        ev.pinRequired = [s](const std::string& n) { s->emit(AP_EVENT_PIN_REQUIRED, n); };
        RaopLogSink log;
        if (std::getenv("PHOSPHOR_AIRPLAY_LOG")) {
            log = [](RaopLogLevel, const std::string& line) {
                std::fprintf(stderr, "[airplay] %s\n", line.c_str());
            };
        }
        RaopSender sender(loop, ev, log);
        RaopIdentity id;
        id.name = name;
        sender.setIdentity(id);
        sender.setAuth(static_cast<RaopDeviceInfo::Auth>(auth), airplay2 != 0, dev, "", "");
        sender.setInputFormat(sample_rate);
        sender.attachRing(&s->ring);
        sender.start(h, port, name);
        // start() clears any pending volume, so set it after; it is sent
        // when streaming begins, before the first audio packet.
        if (initial_volume_pct >= 0.0) {
            sender.setVolume(initial_volume_pct);
            if (log) log(RaopLogLevel::Info, "phosphor: starting volume " +
                                                 std::to_string(int(initial_volume_pct + 0.5)) + "%");
        }

        while (!s->stop.load()) {
            loop.pump(sender, std::chrono::milliseconds(20));
            std::optional<double> vol;
            std::optional<NowPlaying> np;
            {
                std::lock_guard<std::mutex> g(s->mu);
                vol.swap(s->volume);
                np.swap(s->now_playing);
            }
            if (vol) sender.setVolume(*vol);
            if (np) sender.setNowPlaying(np->title, np->artist, np->album);
        }
        sender.stop();
        // Let the TEARDOWN reach the receiver before the sockets go.
        for (int i = 0; i < 5; ++i) loop.pump(sender, std::chrono::milliseconds(20));
    });
    return s;
}

extern "C" int ap_push_pcm(ap_session* s, const int16_t* samples, size_t n) {
    if (!s || !samples || n == 0) return 1;
    return s->ring.tryPush(std::span<const int16_t>(samples, n)) ? 1 : 0;
}

extern "C" void ap_set_volume(ap_session* s, double pct) {
    if (!s) return;
    std::lock_guard<std::mutex> g(s->mu);
    s->volume = pct;
}

extern "C" void ap_set_now_playing(ap_session* s, const char* title, const char* artist,
                                   const char* album) {
    if (!s) return;
    std::lock_guard<std::mutex> g(s->mu);
    s->now_playing = NowPlaying{title ? title : "", artist ? artist : "", album ? album : ""};
}

extern "C" void ap_destroy(ap_session* s) {
    if (!s) return;
    s->stop.store(true);
    if (s->thread.joinable()) s->thread.join();
    // The final `closed` from our own TEARDOWN has fired by now; nothing
    // after this point may call back into Rust.
    s->events_on.store(false);
    delete s;
}
