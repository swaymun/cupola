// Copyright 2026 Cupola contributors. SPDX-License-Identifier: MIT
// Uses the NVIDIA driver API, not a bundled CUDA runtime. One input texture and
// one synchronous output buffer are owned by the encoder and released together.
#include "windows_encoder.h"
#include "windows_mf_encoder.h"
#include "api/video_codecs/video_encoder_software_fallback_wrapper.h"
#include <d3d11.h>
#include <dxgi.h>
#include <wrl/client.h>
#include <algorithm>
#include <cstring>
#include <vector>
#include "NvCodec/include/nvEncodeAPI.h"
#include "api/video_codecs/h264_profile_level_id.h"
#include "api/video_codecs/video_encoder.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "third_party/libyuv/include/libyuv/convert.h"

namespace livekit_ffi {
namespace {
using Microsoft::WRL::ComPtr;
using namespace webrtc;

class WindowsNvenc final : public VideoEncoder {
 public:
  explicit WindowsNvenc(const SdpVideoFormat& format) : format_(format) {}
  ~WindowsNvenc() override { Release(); }

  int InitEncode(const VideoCodec* codec, const Settings&) override {
    Release();
    if (!codec || codec->codecType != kVideoCodecH264 || !codec->width ||
        !codec->height || !codec->maxFramerate || codec->width % 2 || codec->height % 2)
      return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    width_ = codec->width;
    height_ = codec->height;
    // System32 only: never resolve a driver DLL from the current directory.
    module_ = LoadLibraryExW(L"nvEncodeAPI64.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!module_) return Fail("driver unavailable");
    auto create = reinterpret_cast<NVENCSTATUS(NVENCAPI*)(NV_ENCODE_API_FUNCTION_LIST*)>(
        GetProcAddress(module_, "NvEncodeAPICreateInstance"));
    api_ = {};
    api_.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (!create || create(&api_) != NV_ENC_SUCCESS) return Fail("driver API unavailable");
    ComPtr<IDXGIFactory1> factory;
    if (FAILED(CreateDXGIFactory1(IID_PPV_ARGS(&factory)))) return Fail("DXGI unavailable");
    for (UINT i = 0;; ++i) {
      ComPtr<IDXGIAdapter1> adapter;
      if (FAILED(factory->EnumAdapters1(i, &adapter))) break;
      DXGI_ADAPTER_DESC1 desc{};
      if (FAILED(adapter->GetDesc1(&desc)) || desc.VendorId != 0x10de ||
          (desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE)) continue;
      if (SUCCEEDED(D3D11CreateDevice(adapter.Get(), D3D_DRIVER_TYPE_UNKNOWN, nullptr,
              0, nullptr, 0, D3D11_SDK_VERSION, &device_, nullptr, &context_))) break;
    }
    if (!device_) return Fail("NVIDIA adapter unavailable");
    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS open{NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER};
    open.device = device_.Get();
    open.deviceType = NV_ENC_DEVICE_TYPE_DIRECTX;
    open.apiVersion = NVENCAPI_VERSION;
    if (api_.nvEncOpenEncodeSessionEx(&open, &encoder_) != NV_ENC_SUCCESS)
      return Fail("hardware encode session unavailable");
    NV_ENC_PRESET_CONFIG preset{NV_ENC_PRESET_CONFIG_VER};
    preset.presetCfg.version = NV_ENC_CONFIG_VER;
    if (api_.nvEncGetEncodePresetConfigEx(encoder_, NV_ENC_CODEC_H264_GUID,
          NV_ENC_PRESET_P1_GUID, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &preset) != NV_ENC_SUCCESS)
      return Fail("H264 low latency preset unavailable");
    config_ = preset.presetCfg;
    config_.profileGUID = NV_ENC_H264_PROFILE_BASELINE_GUID;
    config_.gopLength = codec->maxFramerate * 5;
    config_.frameIntervalP = 1;
    config_.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    config_.rcParams.averageBitRate = std::max(1u, codec->startBitrate) * 1000;
    config_.rcParams.maxBitRate = std::max(codec->startBitrate, codec->maxBitrate) * 1000;
    config_.rcParams.enableLookahead = 0;
    config_.encodeCodecConfig.h264Config.repeatSPSPPS = 1;
    config_.encodeCodecConfig.h264Config.idrPeriod = config_.gopLength;
    auto profile = ParseSdpForH264ProfileLevelId(format_.parameters);
    if (!profile || profile->profile != H264Profile::kProfileConstrainedBaseline)
      return Fail("unsupported H264 profile");
    config_.encodeCodecConfig.h264Config.level = static_cast<uint32_t>(profile->level);
    init_ = {};
    init_.version = NV_ENC_INITIALIZE_PARAMS_VER;
    init_.encodeGUID = NV_ENC_CODEC_H264_GUID;
    init_.presetGUID = NV_ENC_PRESET_P1_GUID;
    init_.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    init_.encodeWidth = init_.darWidth = init_.maxEncodeWidth = width_;
    init_.encodeHeight = init_.darHeight = init_.maxEncodeHeight = height_;
    init_.frameRateNum = codec->maxFramerate;
    init_.frameRateDen = 1;
    init_.enablePTD = 1;
    init_.encodeConfig = &config_;
    if (api_.nvEncInitializeEncoder(encoder_, &init_) != NV_ENC_SUCCESS)
      return Fail("H264 initialization failed");
    D3D11_TEXTURE2D_DESC td{};
    td.Width = width_;
    td.Height = height_;
    td.MipLevels = td.ArraySize = td.SampleDesc.Count = 1;
    td.Format = DXGI_FORMAT_NV12;
    td.Usage = D3D11_USAGE_DEFAULT;
    td.BindFlags = D3D11_BIND_RENDER_TARGET;
    if (FAILED(device_->CreateTexture2D(&td, nullptr, &texture_))) return Fail("NV12 texture unavailable");
    NV_ENC_REGISTER_RESOURCE resource{NV_ENC_REGISTER_RESOURCE_VER};
    resource.resourceType = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
    resource.resourceToRegister = texture_.Get();
    resource.width = width_;
    resource.height = height_;
    resource.bufferFormat = NV_ENC_BUFFER_FORMAT_NV12;
    resource.bufferUsage = NV_ENC_INPUT_IMAGE;
    if (api_.nvEncRegisterResource(encoder_, &resource) != NV_ENC_SUCCESS)
      return Fail("texture registration failed");
    resource_ = resource.registeredResource;
    NV_ENC_CREATE_BITSTREAM_BUFFER buffer{NV_ENC_CREATE_BITSTREAM_BUFFER_VER};
    if (api_.nvEncCreateBitstreamBuffer(encoder_, &buffer) != NV_ENC_SUCCESS)
      return Fail("bitstream allocation failed");
    output_ = buffer.bitstreamBuffer;
    nv12_.resize(width_ * height_ * 3 / 2);
    failed_ = false;
    sending_ = true;
    RTC_LOG(LS_INFO) << "Cupola NVENC H264 initialized " << width_ << "x" << height_;
    return WEBRTC_VIDEO_CODEC_OK;
  }

  int32_t RegisterEncodeCompleteCallback(EncodedImageCallback* cb) override {
    callback_ = cb;
    return WEBRTC_VIDEO_CODEC_OK;
  }
  int32_t Release() override {
    if (encoder_) {
      if (output_) api_.nvEncDestroyBitstreamBuffer(encoder_, output_);
      if (resource_) api_.nvEncUnregisterResource(encoder_, resource_);
      api_.nvEncDestroyEncoder(encoder_);
    }
    output_ = nullptr;
    resource_ = nullptr;
    encoder_ = nullptr;
    texture_.Reset();
    context_.Reset();
    device_.Reset();
    if (module_) FreeLibrary(module_);
    module_ = nullptr;
    nv12_.clear();
    failed_ = true;
    return WEBRTC_VIDEO_CODEC_OK;
  }
  int32_t Encode(const VideoFrame& frame, const std::vector<VideoFrameType>* types) override {
    if (failed_ || !encoder_ || !callback_) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    if (!sending_) return WEBRTC_VIDEO_CODEC_OK;
    auto i420 = frame.video_frame_buffer()->ToI420();
    if (!i420 || i420->width() != static_cast<int>(width_) || i420->height() != static_cast<int>(height_))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    if (libyuv::I420ToNV12(i420->DataY(), i420->StrideY(), i420->DataU(), i420->StrideU(),
          i420->DataV(), i420->StrideV(), nv12_.data(), width_,
          nv12_.data() + width_ * height_, width_, width_, height_))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    context_->UpdateSubresource(texture_.Get(), 0, nullptr, nv12_.data(), width_, 0);
    NV_ENC_MAP_INPUT_RESOURCE map{NV_ENC_MAP_INPUT_RESOURCE_VER};
    map.registeredResource = resource_;
    if (api_.nvEncMapInputResource(encoder_, &map) != NV_ENC_SUCCESS)
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    NV_ENC_PIC_PARAMS picture{NV_ENC_PIC_PARAMS_VER};
    picture.inputBuffer = map.mappedResource;
    picture.bufferFmt = NV_ENC_BUFFER_FORMAT_NV12;
    picture.inputWidth = width_;
    picture.inputHeight = height_;
    picture.outputBitstream = output_;
    picture.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    picture.inputTimeStamp = frame.rtp_timestamp();
    if (types && std::find(types->begin(), types->end(), VideoFrameType::kVideoFrameKey) != types->end())
      picture.encodePicFlags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
    auto status = api_.nvEncEncodePicture(encoder_, &picture);
    NV_ENC_LOCK_BITSTREAM bits{NV_ENC_LOCK_BITSTREAM_VER};
    bits.outputBitstream = output_;
    if (status == NV_ENC_SUCCESS) status = api_.nvEncLockBitstream(encoder_, &bits);
    EncodedImage image;
    if (status == NV_ENC_SUCCESS) {
      image.SetEncodedData(EncodedImageBuffer::Create(
          static_cast<const uint8_t*>(bits.bitstreamBufferPtr), bits.bitstreamSizeInBytes));
      image._encodedWidth = width_;
      image._encodedHeight = height_;
      image.SetRtpTimestamp(frame.rtp_timestamp());
      image.capture_time_ms_ = frame.render_time_ms();
      image.SetColorSpace(frame.color_space());
      image._frameType = bits.pictureType == NV_ENC_PIC_TYPE_IDR ? VideoFrameType::kVideoFrameKey : VideoFrameType::kVideoFrameDelta;
      api_.nvEncUnlockBitstream(encoder_, output_);
    }
    api_.nvEncUnmapInputResource(encoder_, map.mappedResource);
    if (status != NV_ENC_SUCCESS) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    CodecSpecificInfo info{};
    info.codecType = kVideoCodecH264;
    info.codecSpecific.H264.packetization_mode = H264PacketizationMode::NonInterleaved;
    info.codecSpecific.H264.temporal_idx = kNoTemporalIdx;
    info.codecSpecific.H264.idr_frame = image._frameType == VideoFrameType::kVideoFrameKey;
    callback_->OnEncodedImage(image, &info);
    return WEBRTC_VIDEO_CODEC_OK;
  }
  void SetRates(const RateControlParameters& rates) override {
    sending_ = rates.bitrate.get_sum_bps() != 0;
    if (!encoder_ || !sending_ || failed_) return;
    config_.rcParams.averageBitRate = rates.bitrate.get_sum_bps();
    config_.rcParams.maxBitRate = rates.bitrate.get_sum_bps();
    init_.frameRateNum = static_cast<uint32_t>(std::max(1.0, rates.framerate_fps) * 1000);
    init_.frameRateDen = 1000;
    NV_ENC_RECONFIGURE_PARAMS update{NV_ENC_RECONFIGURE_PARAMS_VER};
    update.reInitEncodeParams = init_;
    if (api_.nvEncReconfigureEncoder(encoder_, &update) != NV_ENC_SUCCESS) failed_ = true;
  }
  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name = "Cupola NVENC H264";
    info.is_hardware_accelerated = true;
    info.supports_native_handle = false;
    info.supports_simulcast = false;
    info.scaling_settings = ScalingSettings::kOff;
    info.has_trusted_rate_controller = true;
    info.requested_resolution_alignment = 2;
    return info;
  }
 private:
  int Fail(const char* reason) {
    RTC_LOG(LS_WARNING) << "Cupola NVENC: " << reason << "; trying Media Foundation then OpenH264";
    Release();
    return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }
  SdpVideoFormat format_;
  HMODULE module_ = nullptr;
  NV_ENCODE_API_FUNCTION_LIST api_{};
  void* encoder_ = nullptr;
  NV_ENC_REGISTERED_PTR resource_ = nullptr;
  NV_ENC_OUTPUT_PTR output_ = nullptr;
  NV_ENC_INITIALIZE_PARAMS init_{};
  NV_ENC_CONFIG config_{};
  ComPtr<ID3D11Device> device_;
  ComPtr<ID3D11DeviceContext> context_;
  ComPtr<ID3D11Texture2D> texture_;
  std::vector<uint8_t> nv12_;
  uint32_t width_ = 0, height_ = 0;
  bool failed_ = true, sending_ = false;
  EncodedImageCallback* callback_ = nullptr; // Borrowed from WebRTC; never owns it.
};
class WindowsHardwareFactory final : public VideoEncoderFactory {
 public:
  std::vector<SdpVideoFormat> GetSupportedFormats() const override {
    return {SdpVideoFormat("H264", {{"profile-level-id", "42e033"},
        {"level-asymmetry-allowed", "1"}, {"packetization-mode", "1"}})};
  }
  std::unique_ptr<VideoEncoder> Create(const Environment& env, const SdpVideoFormat& format) override {
    for (const auto& supported : GetSupportedFormats())
      if (supported.IsSameCodec(format)) return CreateVideoEncoderSoftwareFallbackWrapper(
          env, CreateWindowsMfEncoder(format), std::make_unique<WindowsNvenc>(format), false);
    return nullptr;
  }
};
} // namespace
std::unique_ptr<webrtc::VideoEncoderFactory> CreateWindowsHardwareFactory() {
  return std::make_unique<WindowsHardwareFactory>();
}
} // namespace livekit_ffi
