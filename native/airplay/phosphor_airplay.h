/* C API over airplay2-sender-cpp for Phosphor's Rust side.
 *
 * One ap_session = one AirPlay connection driven by its own thread (the
 * library's RaopLoop). PCM goes in through a lock-free ring: call
 * ap_push_pcm from ONE thread only. Volume / now-playing may be called
 * from any thread. Events are delivered on the session thread. */
#ifndef PHOSPHOR_AIRPLAY_H
#define PHOSPHOR_AIRPLAY_H

#include <stddef.h>
#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

/* Event kinds passed to ap_event_cb. */
enum {
    AP_EVENT_LAUNCHED = 1,      /* streaming started */
    AP_EVENT_LAUNCH_FAILED = 2, /* msg = reason */
    AP_EVENT_CLOSED = 3,        /* session ended (receiver or us) */
    AP_EVENT_PIN_REQUIRED = 4   /* receiver shows a PIN (not supported yet) */
};

/* Auth modes (mirror fxchain::RaopDeviceInfo::Auth). */
enum {
    AP_AUTH_NONE = 0,
    AP_AUTH_PASSWORD = 1,
    AP_AUTH_AUTH_SETUP = 2,
    AP_AUTH_LEGACY_PIN = 3,
    AP_AUTH_HAP_TRANSIENT = 4,
    AP_AUTH_HAP_PIN = 5
};

typedef struct ap_session ap_session;
typedef void (*ap_event_cb)(void* user, int kind, const char* msg);

/* Starts connecting immediately. `sample_rate` is the rate of the PCM
 * you will push (resampled to 44.1 kHz internally). `initial_volume_pct`
 * (0..100) is applied before any audio flows; without it AirPlay 2
 * receivers get 0 dB, which many TVs map to their maximum volume. Pass a
 * negative value to leave the receiver's volume alone. Never returns
 * NULL unless out of memory. */
ap_session* ap_create(const char* host, uint16_t port, const char* sender_name,
                      int auth, int airplay2, const char* receiver_device_id,
                      uint32_t sample_rate, double initial_volume_pct,
                      ap_event_cb cb, void* user);

/* Push interleaved stereo i16 samples (`n` = sample count, not frames).
 * Returns 1 if accepted, 0 if the ring is full (drop or retry). */
int ap_push_pcm(ap_session* s, const int16_t* samples, size_t n);

/* 0..100 %. */
void ap_set_volume(ap_session* s, double pct);
void ap_set_now_playing(ap_session* s, const char* title, const char* artist,
                        const char* album);

/* TEARDOWN, join the session thread, free. No events fire after return. */
void ap_destroy(ap_session* s);

#ifdef __cplusplus
}
#endif
#endif
