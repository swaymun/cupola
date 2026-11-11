# Scoped LiveKit native encoder changes

`webrtc-sys/` is the complete crates.io `webrtc-sys 0.3.49` source, pinned by the
helper's Cargo patch. Upstream git revision: `49b3ebe9ab1c574114b164868ad226cc26a9b1be`.
Published crate SHA-256: `b803a52c561fbb9faaf09c1b6fc0260f337cbc1ae6f2a525f673c77eb4411022`.
No upstream checkout or global Cargo registry is edited by the build.

The Cupola delta is limited to:

- `build.rs`: require VAAPI development headers on Linux x64; compile the new
  Windows NVENC and hardware-only Media Foundation adapters (dynamically loaded
  system components; Direct3D11 for NVENC).
- `src/video_encoder_factory.cpp`: register the Windows factory, advertise H264
  level 5.1, and wrap hardware creation in WebRTC's software fallback wrapper.
  Both initialization failure and explicit encode failure retain H264/OpenH264.
- `src/video_decoder_factory.cpp`: matching level 5.1 advertisement for the
  FFmpeg decoder (including the isolated loopback answerer).
- `src/vaapi/{vaapi_encoder_factory.cpp,h264_encoder_impl.cpp,
  vaapi_h264_encoder_wrapper.{cpp,h}}`: level 5.1 advertisement, pass the
  negotiated level into the SPS, respect I420/VA image plane layouts and strides,
  upload allocated images, check every VA map/render result, and signal software fallback on encode failure.
  `test/vaapi_fault/run.sh` fault-injects these paths against a mock libva.
- `src/nvidia/windows_encoder.{h,cpp}`: bounded synchronous low-latency NVENC
  H264 adapter. No CUDA installation is required. Missing/incompatible drivers,
  unsupported profiles/sizes, and runtime failures use the software wrapper.
- `src/nvidia/windows_mf_encoder.{h,cpp}`: hardware-only H264 MFT enumeration,
  NV12 input, no B frames, bounded event waits, and deterministic COM/MF teardown.
  The selection chain is NVENC, Media Foundation, then OpenH264.
- `LICENSE-NVIDIA.txt`: verbatim notice from the already-vendored `nvEncodeAPI.h`,
  included in the helper's normal release notices, alongside `LICENSE-CUPOLA.txt`
  for the new adapters.

Upstream notices and licensing are preserved. Keep this delta small and review
it against the pinned published crate when upgrading. This does not claim that
Cisco's separately downloaded OpenH264 binary license covers our source build.
