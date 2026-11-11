// Copyright 2026 Cupola contributors. SPDX-License-Identifier: MIT
#pragma once
#include "api/video_codecs/video_encoder.h"
#include "api/video_codecs/sdp_video_format.h"
namespace livekit_ffi {
std::unique_ptr<webrtc::VideoEncoder> CreateWindowsMfEncoder(const webrtc::SdpVideoFormat& format);
}
