// Copyright 2026 Cupola contributors. SPDX-License-Identifier: MIT
// Optional hardware-only Media Foundation MFT. No worker thread or unbounded
// queue: each Encode consumes at most one sample, with bounded event waits.
#include "windows_mf_encoder.h"
#include <windows.h>
#include <mfapi.h>
#include <mfidl.h>
#include <mftransform.h>
#include <mferror.h>
#include <codecapi.h>
#include <strmif.h>
#include <wrl/client.h>
#include <algorithm>
#include <chrono>
#include <vector>
#include "api/video_codecs/h264_profile_level_id.h"
#include "modules/video_coding/include/video_codec_interface.h"
#include "modules/video_coding/include/video_error_codes.h"
#include "rtc_base/logging.h"
#include "third_party/libyuv/include/libyuv/convert.h"

namespace livekit_ffi {
namespace {
using Microsoft::WRL::ComPtr;
using namespace webrtc;
class MediaFoundationEncoder final : public VideoEncoder {
 public:
  explicit MediaFoundationEncoder(const SdpVideoFormat& format) : format_(format) {}
  ~MediaFoundationEncoder() override { Release(); }
  int InitEncode(const VideoCodec* codec, const Settings&) override {
    Release();
    if (!codec || !codec->width || !codec->height || !codec->maxFramerate ||
        codec->codecType != kVideoCodecH264 || codec->width % 2 || codec->height % 2) return WEBRTC_VIDEO_CODEC_ERR_PARAMETER;
    width_ = codec->width; height_ = codec->height; fps_ = codec->maxFramerate;
    HRESULT hr = CoInitializeEx(nullptr, COINIT_MULTITHREADED);
    com_ = SUCCEEDED(hr);
    if (FAILED(hr) && hr != RPC_E_CHANGED_MODE) return Fail("COM unavailable");
    // Media Foundation is optional on Windows Server/N editions too. Static
    // imports would prevent the helper (and its VP8 fallback) from starting.
    module_ = LoadLibraryExW(L"mfplat.dll", nullptr, LOAD_LIBRARY_SEARCH_SYSTEM32);
    if (!module_) return Fail("Media Foundation unavailable");
#define LOAD_MF(name) name##_ = reinterpret_cast<decltype(&::name)>(GetProcAddress(module_, #name)); if (!name##_) return Fail(#name " unavailable")
    LOAD_MF(MFStartup); LOAD_MF(MFShutdown); LOAD_MF(MFTEnumEx);
    LOAD_MF(MFCreateMediaType); LOAD_MF(MFCreateSample); LOAD_MF(MFCreateMemoryBuffer);
#undef LOAD_MF
    if (FAILED(MFStartup_(MF_VERSION, MFSTARTUP_NOSOCKET))) return Fail("startup failed");
    started_ = true;
    MFT_REGISTER_TYPE_INFO input{MFMediaType_Video, MFVideoFormat_NV12};
    MFT_REGISTER_TYPE_INFO output{MFMediaType_Video, MFVideoFormat_H264};
    IMFActivate** candidates = nullptr;
    UINT32 count = 0;
    hr = MFTEnumEx_(MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
                   &input, &output, &candidates, &count);
    if (FAILED(hr)) return Fail("hardware enumeration failed");
    for (UINT32 i = 0; i < count; ++i) {
      if (!transform_) {
        ComPtr<IMFTransform> candidate;
        if (SUCCEEDED(candidates[i]->ActivateObject(IID_PPV_ARGS(&candidate)))) {
          transform_ = candidate;
          activation_ = candidates[i];
          if (!Configure(codec)) {
            events_.Reset(); codec_api_.Reset(); transform_.Reset();
            activation_->ShutdownObject(); activation_.Reset();
          }
        }
      }
      candidates[i]->Release();
    }
    CoTaskMemFree(candidates);
    if (!transform_) return Fail("no compatible hardware H264 MFT");
    sending_ = true;
    RTC_LOG(LS_INFO) << "Cupola Media Foundation hardware H264 initialized " << width_ << "x" << height_;
    return WEBRTC_VIDEO_CODEC_OK;
  }
  int32_t RegisterEncodeCompleteCallback(EncodedImageCallback* cb) override {
    callback_ = cb; return WEBRTC_VIDEO_CODEC_OK;
  }
  int32_t Release() override {
    if (transform_) transform_->ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
    events_.Reset(); codec_api_.Reset(); transform_.Reset();
    if (activation_) activation_->ShutdownObject();
    activation_.Reset();
    if (started_) MFShutdown_();
    started_ = false;
    if (module_) FreeLibrary(module_);
    module_ = nullptr;
    if (com_) CoUninitialize();
    com_ = false; need_input_ = have_output_ = 0; failed_ = false;
    header_.clear();
    return WEBRTC_VIDEO_CODEC_OK;
  }
  int32_t Encode(const VideoFrame& frame, const std::vector<VideoFrameType>* types) override {
    if (!transform_ || failed_ || !callback_) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    if (!sending_) return WEBRTC_VIDEO_CODEC_OK;
    auto buffer = frame.video_frame_buffer()->ToI420();
    if (!buffer || buffer->width() != static_cast<int>(width_) || buffer->height() != static_cast<int>(height_))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    bool key = types && std::find(types->begin(), types->end(), VideoFrameType::kVideoFrameKey) != types->end();
    if (key && !SetU32(CODECAPI_AVEncVideoForceKeyFrame, 1)) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    if (!WaitFor(false)) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    ComPtr<IMFMediaBuffer> media;
    ComPtr<IMFSample> sample;
    DWORD size = width_ * height_ * 3 / 2;
    if (FAILED(MFCreateMemoryBuffer_(size, &media)) || FAILED(MFCreateSample_(&sample)))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    BYTE* bytes = nullptr;
    if (FAILED(media->Lock(&bytes, nullptr, nullptr))) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    int converted = libyuv::I420ToNV12(buffer->DataY(), buffer->StrideY(), buffer->DataU(), buffer->StrideU(),
        buffer->DataV(), buffer->StrideV(), bytes, width_, bytes + width_ * height_, width_, width_, height_);
    media->Unlock();
    if (converted || FAILED(media->SetCurrentLength(size)) || FAILED(sample->AddBuffer(media.Get())) ||
        FAILED(sample->SetSampleTime(frame.timestamp_us() * 10)) ||
        FAILED(sample->SetSampleDuration(static_cast<LONGLONG>(10000000.0 / fps_))))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    --need_input_;
    if (FAILED(transform_->ProcessInput(input_id_, sample.Get(), 0)) || !WaitFor(true))
      return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    --have_output_;
    MFT_OUTPUT_STREAM_INFO stream{};
    if (FAILED(transform_->GetOutputStreamInfo(output_id_, &stream))) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    ComPtr<IMFSample> output_sample;
    if (!(stream.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES)) {
      ComPtr<IMFMediaBuffer> output_buffer;
      if (FAILED(MFCreateSample_(&output_sample)) ||
          FAILED(MFCreateMemoryBuffer_(std::max(stream.cbSize, size * 2), &output_buffer)) ||
          FAILED(output_sample->AddBuffer(output_buffer.Get()))) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    }
    MFT_OUTPUT_DATA_BUFFER output{};
    output.dwStreamID = output_id_;
    output.pSample = output_sample.Get();
    DWORD status = 0;
    HRESULT hr = transform_->ProcessOutput(0, 1, &output, &status);
    if (output.pEvents) output.pEvents->Release();
    if (!output_sample && output.pSample) output_sample.Attach(output.pSample);
    if (FAILED(hr) || !output_sample) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    ComPtr<IMFMediaBuffer> encoded;
    DWORD length = 0;
    if (FAILED(output_sample->ConvertToContiguousBuffer(&encoded)) ||
        FAILED(encoded->Lock(&bytes, nullptr, &length))) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    // MFT H264 output is Annex B. Fail cleanly if a driver returns another form.
    bool annex_b = length >= 4 && bytes[0] == 0 && bytes[1] == 0 && (bytes[2] == 1 || (bytes[2] == 0 && bytes[3] == 1));
    std::vector<uint8_t> packet;
    UINT32 clean = 0;
    output_sample->GetUINT32(MFSampleExtension_CleanPoint, &clean);
    if (annex_b) {
      if (clean && !header_.empty()) packet = header_;
      packet.insert(packet.end(), bytes, bytes + length);
    }
    encoded->Unlock();
    if (!annex_b) return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
    EncodedImage image;
    image.SetEncodedData(EncodedImageBuffer::Create(packet.data(), packet.size()));
    image._encodedWidth = width_; image._encodedHeight = height_;
    image.SetRtpTimestamp(frame.rtp_timestamp());
    image.capture_time_ms_ = frame.render_time_ms();
    image.SetColorSpace(frame.color_space());
    image._frameType = clean ? VideoFrameType::kVideoFrameKey : VideoFrameType::kVideoFrameDelta;
    CodecSpecificInfo info{};
    info.codecType = kVideoCodecH264;
    info.codecSpecific.H264.packetization_mode = H264PacketizationMode::NonInterleaved;
    info.codecSpecific.H264.temporal_idx = kNoTemporalIdx;
    info.codecSpecific.H264.idr_frame = clean != 0;
    callback_->OnEncodedImage(image, &info);
    return WEBRTC_VIDEO_CODEC_OK;
  }
  void SetRates(const RateControlParameters& rates) override {
    sending_ = rates.bitrate.get_sum_bps() != 0;
    fps_ = std::max(1.0, rates.framerate_fps);
    if (codec_api_ && sending_ && !SetU32(CODECAPI_AVEncCommonMeanBitRate, rates.bitrate.get_sum_bps())) failed_ = true;
  }
  EncoderInfo GetEncoderInfo() const override {
    EncoderInfo info;
    info.implementation_name = "Cupola Media Foundation H264";
    info.is_hardware_accelerated = true;
    info.supports_native_handle = false; info.supports_simulcast = false;
    info.scaling_settings = ScalingSettings::kOff;
    info.requested_resolution_alignment = 2;
    return info;
  }
 private:
  bool Configure(const VideoCodec* codec) {
    header_.clear();
    ComPtr<IMFAttributes> attributes;
    if (FAILED(transform_->GetAttributes(&attributes)) ||
        FAILED(attributes->SetUINT32(MF_TRANSFORM_ASYNC_UNLOCK, TRUE)) ||
        FAILED(transform_.As(&events_)) || FAILED(transform_.As(&codec_api_))) return false;
    input_id_ = output_id_ = 0;
    HRESULT ids = transform_->GetStreamIDs(1, &input_id_, 1, &output_id_);
    if (FAILED(ids) && ids != E_NOTIMPL) return false;
    // Require low latency and no B frames; no encoder-side frame queue.
    if (!SetBool(CODECAPI_AVLowLatencyMode, true) ||
        !SetU32(CODECAPI_AVEncMPVDefaultBPictureCount, 0)) return false;
    SetU32(CODECAPI_AVEncCommonRateControlMode, eAVEncCommonRateControlMode_CBR);
    ComPtr<IMFMediaType> output, input;
    if (FAILED(MFCreateMediaType_(&output)) || FAILED(MFCreateMediaType_(&input))) return false;
    auto type = [&](IMFMediaType* t, const GUID& subtype) {
      return SUCCEEDED(t->SetGUID(MF_MT_MAJOR_TYPE, MFMediaType_Video)) &&
        SUCCEEDED(t->SetGUID(MF_MT_SUBTYPE, subtype)) &&
        SUCCEEDED(MFSetAttributeSize(t, MF_MT_FRAME_SIZE, width_, height_)) &&
        SUCCEEDED(MFSetAttributeRatio(t, MF_MT_FRAME_RATE, codec->maxFramerate, 1)) &&
        SUCCEEDED(MFSetAttributeRatio(t, MF_MT_PIXEL_ASPECT_RATIO, 1, 1)) &&
        SUCCEEDED(t->SetUINT32(MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive));
    };
    auto profile = ParseSdpForH264ProfileLevelId(format_.parameters);
    if (!profile || !type(output.Get(), MFVideoFormat_H264) || !type(input.Get(), MFVideoFormat_NV12) ||
        FAILED(output->SetUINT32(MF_MT_AVG_BITRATE, codec->startBitrate * 1000)) ||
        FAILED(output->SetUINT32(MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Base)) ||
        FAILED(output->SetUINT32(MF_MT_MPEG2_LEVEL, static_cast<UINT32>(profile->level))) ||
        FAILED(transform_->SetOutputType(output_id_, output.Get(), 0)) ||
        FAILED(transform_->SetInputType(input_id_, input.Get(), 0))) return false;
    ComPtr<IMFMediaType> current;
    if (SUCCEEDED(transform_->GetOutputCurrentType(output_id_, &current))) {
      UINT32 length = 0;
      if (SUCCEEDED(current->GetBlobSize(MF_MT_MPEG_SEQUENCE_HEADER, &length)) && length <= 65536) {
        header_.resize(length);
        if (FAILED(current->GetBlob(MF_MT_MPEG_SEQUENCE_HEADER, header_.data(), length, nullptr)) ||
            length < 4 || header_[0] || header_[1] || (header_[2] != 1 && (header_[2] || header_[3] != 1))) header_.clear();
      }
    }
    need_input_ = have_output_ = 0;
    return SUCCEEDED(transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)) &&
           SUCCEEDED(transform_->ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0));
  }
  bool WaitFor(bool output) {
    auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(100);
    while ((output ? have_output_ : need_input_) == 0) {
      ComPtr<IMFMediaEvent> event;
      HRESULT hr = events_->GetEvent(MF_EVENT_FLAG_NO_WAIT, &event);
      if (hr == MF_E_NO_EVENTS_AVAILABLE) {
        if (std::chrono::steady_clock::now() >= deadline) return false;
        Sleep(1); continue;
      }
      HRESULT status = S_OK;
      MediaEventType type;
      if (FAILED(hr) || FAILED(event->GetStatus(&status)) || FAILED(status) || FAILED(event->GetType(&type))) return false;
      if (type == METransformNeedInput) ++need_input_;
      if (type == METransformHaveOutput) ++have_output_;
      // A broken transform must not produce an unbounded stream of events.
      if (need_input_ > 16 || have_output_ > 2 || std::chrono::steady_clock::now() >= deadline) return false;
    }
    return true;
  }
  bool SetBool(const GUID& key, bool value) {
    VARIANT v{}; v.vt = VT_BOOL; v.boolVal = value ? VARIANT_TRUE : VARIANT_FALSE;
    return codec_api_ && SUCCEEDED(codec_api_->SetValue(&key, &v));
  }
  bool SetU32(const GUID& key, UINT32 value) {
    VARIANT v{}; v.vt = VT_UI4; v.ulVal = value;
    return codec_api_ && SUCCEEDED(codec_api_->SetValue(&key, &v));
  }
  int Fail(const char* reason) {
    RTC_LOG(LS_WARNING) << "Cupola Media Foundation: " << reason << "; falling back to OpenH264";
    Release(); return WEBRTC_VIDEO_CODEC_FALLBACK_SOFTWARE;
  }
  SdpVideoFormat format_;
  HMODULE module_ = nullptr;
#define MF_FN(name) decltype(&::name) name##_ = nullptr
  MF_FN(MFStartup); MF_FN(MFShutdown); MF_FN(MFTEnumEx);
  MF_FN(MFCreateMediaType); MF_FN(MFCreateSample); MF_FN(MFCreateMemoryBuffer);
#undef MF_FN
  ComPtr<IMFActivate> activation_;
  ComPtr<IMFTransform> transform_;
  ComPtr<IMFMediaEventGenerator> events_;
  ComPtr<ICodecAPI> codec_api_;
  bool com_ = false, started_ = false, sending_ = false, failed_ = false;
  UINT32 width_ = 0, height_ = 0;
  DWORD input_id_ = 0, output_id_ = 0;
  UINT32 need_input_ = 0, have_output_ = 0;
  double fps_ = 30;
  std::vector<uint8_t> header_;
  EncodedImageCallback* callback_ = nullptr;
};
}
std::unique_ptr<webrtc::VideoEncoder> CreateWindowsMfEncoder(const webrtc::SdpVideoFormat& format) {
  return std::make_unique<MediaFoundationEncoder>(format);
}
}
