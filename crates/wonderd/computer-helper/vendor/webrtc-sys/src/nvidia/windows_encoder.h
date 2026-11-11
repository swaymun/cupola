// Cupola Windows NVENC adapter. The driver DLL stays optional at runtime.
#pragma once
#include "api/video_codecs/video_encoder_factory.h"
namespace livekit_ffi {
std::unique_ptr<webrtc::VideoEncoderFactory> CreateWindowsHardwareFactory();
}
