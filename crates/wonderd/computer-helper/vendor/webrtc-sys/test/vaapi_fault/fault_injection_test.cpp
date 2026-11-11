// Fault-injection harness: compiles the production wrapper TU unchanged against a mock libva.
#include <va/va.h>
#include <va/va_enc_h264.h>
#include <map>
#include <vector>
#include <cstdio>
#include <cstdlib>
#include <cstring>

struct Buf { int type; std::vector<uint8_t> mem; VACodedBufferSegment seg; std::vector<uint8_t> coded; };
static std::map<VABufferID, Buf> g_bufs;
static VABufferID g_next = 1;
static int g_map_calls = 0, g_fail_map_at = -1;     // 1-based index of vaMapBuffer call to fail
static int g_render_calls = 0, g_fail_render_at = -1;
static int g_end_calls = 0;

extern "C" {
VAStatus vaBeginPicture(VADisplay, VAContextID, VASurfaceID) { return VA_STATUS_SUCCESS; }
VAStatus vaEndPicture(VADisplay, VAContextID) { g_end_calls++; return VA_STATUS_SUCCESS; }
VAStatus vaCreateBuffer(VADisplay, VAContextID, VABufferType t, unsigned int size, unsigned int n, void* data, VABufferID* id) {
  Buf b; b.type = t; b.mem.assign(size * n, 0); if (data) memcpy(b.mem.data(), data, size * n);
  *id = g_next++; g_bufs[*id] = std::move(b); return VA_STATUS_SUCCESS; }
VAStatus vaDestroyBuffer(VADisplay, VABufferID id) { return g_bufs.erase(id) ? VA_STATUS_SUCCESS : VA_STATUS_ERROR_INVALID_BUFFER; }
VAStatus vaMapBuffer(VADisplay, VABufferID id, void** p) {
  g_map_calls++;
  if (g_map_calls == g_fail_map_at) { return VA_STATUS_ERROR_OPERATION_FAILED; }  // *p untouched, as real drivers may leave it
  Buf& b = g_bufs.at(id);
  if (b.type == VAEncCodedBufferType) { b.coded.assign(100, 0xAB); memset(&b.seg, 0, sizeof b.seg); b.seg.size = 100; b.seg.buf = b.coded.data(); *p = &b.seg; }
  else *p = b.mem.data();
  return VA_STATUS_SUCCESS; }
VAStatus vaUnmapBuffer(VADisplay, VABufferID) { return VA_STATUS_SUCCESS; }
VAStatus vaRenderPicture(VADisplay, VAContextID, VABufferID* ids, int n) {
  g_render_calls++;
  for (int i = 0; i < n; i++) g_bufs.erase(ids[i]);  // driver consumes rendered buffers
  return g_render_calls == g_fail_render_at ? VA_STATUS_ERROR_OPERATION_FAILED : VA_STATUS_SUCCESS; }
VAStatus vaSyncSurface(VADisplay, VASurfaceID) { return VA_STATUS_SUCCESS; }
VAStatus vaDeriveImage(VADisplay, VASurfaceID, VAImage*) { return VA_STATUS_ERROR_OPERATION_FAILED; }
VAStatus vaCreateImage(VADisplay, VAImageFormat* f, int w, int h, VAImage* im) {
  memset(im, 0, sizeof *im); im->format = *f; im->width = w; im->height = h; im->image_id = 1;
  im->pitches[0] = im->pitches[1] = im->pitches[2] = w; im->offsets[1] = w * h; im->offsets[2] = w * h * 2;
  VABufferID id = g_next++; g_bufs[id].type = 0; g_bufs[id].mem.assign(w * h * 3, 0); im->buf = id; return VA_STATUS_SUCCESS; }
VAStatus vaDestroyImage(VADisplay, VAImageID) { return VA_STATUS_SUCCESS; }
VAStatus vaPutImage(VADisplay, VASurfaceID, VAImageID, int,int,unsigned,unsigned,int,int,unsigned,unsigned) { return VA_STATUS_SUCCESS; }
VAStatus vaCreateConfig(VADisplay, VAProfile, VAEntrypoint, VAConfigAttrib*, int, VAConfigID*) { return 0; }
VAStatus vaCreateContext(VADisplay, VAConfigID, int, int, int, VASurfaceID*, int, VAContextID*) { return 0; }
VAStatus vaCreateSurfaces(VADisplay, unsigned, unsigned, unsigned, VASurfaceID*, unsigned, VASurfaceAttrib*, unsigned) { return 0; }
VAStatus vaDestroyConfig(VADisplay, VAConfigID) { return 0; }
VAStatus vaDestroyContext(VADisplay, VAContextID) { return 0; }
VAStatus vaDestroySurfaces(VADisplay, VASurfaceID*, int) { return 0; }
VAStatus vaGetConfigAttributes(VADisplay, VAProfile, VAEntrypoint, VAConfigAttrib*, int) { return 0; }
int vaMaxNumEntrypoints(VADisplay) { return 0; }
VAStatus vaQueryConfigEntrypoints(VADisplay, VAProfile, VAEntrypoint*, int*) { return 0; }
}

#define private public
#include "vaapi_h264_encoder_wrapper.cpp"
#undef private
#include "vaapi_display_drm.h"
namespace livekit_ffi {
VaapiDisplayDrm::~VaapiDisplayDrm() {}
bool VaapiDisplayDrm::Open() { return true; }
void VaapiDisplayDrm::Close() {}
}

using namespace livekit_ffi;
static int failures = 0;
#define CHECK(c) do { if (!(c)) { printf("FAIL line %d: %s\n", __LINE__, #c); failures++; } } while (0)

static void reset_mock(int fail_map, int fail_render) {
  g_bufs.clear(); g_map_calls = 0; g_render_calls = 0; g_end_calls = 0; g_fail_map_at = fail_map; g_fail_render_at = fail_render;
}
static bool run_encode(VaapiH264EncoderWrapper& w, std::vector<uint8_t>& out) {
  std::vector<uint8_t> plane(64 * 64, 1);
  return w.Encode(plane.data(), 64, plane.data(), 32, plane.data(), 32, true, out);
}
static void setup(VaapiH264EncoderWrapper& w) {
  VA264Context* c = w.context_.get();
  c->va_dpy = (VADisplay)0x1; c->context_id = 1;
  c->config.frame_width = c->config.frame_height = 64; c->frame_width_mbaligned = c->frame_height_mbaligned = 64;
  c->config.intra_period = c->config.intra_idr_period = 30; c->config.ip_period = 1; c->config.bitrate = 1000000;
  c->config.level_idc = 40; c->encoded_buffer = (uint8_t*)malloc(64 * 64 * 3);
  for (int i = 0; i < SURFACE_NUM; i++) { c->src_surface[i] = i; c->ref_surface[i] = i; c->coded_buf[i] = VA_INVALID_ID; }
  for (int i = 0; i < SURFACE_NUM; i++) { VABufferID id; vaCreateBuffer(c->va_dpy, 1, VAEncCodedBufferType, 4096, 1, NULL, &id); c->coded_buf[i] = id; }
}

int main() {
  // 1. baseline success
  { reset_mock(-1, -1); VaapiH264EncoderWrapper w; setup(w); std::vector<uint8_t> out;
    CHECK(run_encode(w, out)); CHECK(out.size() == 100); }
  // 2. render_sequence direct: vaMapBuffer fails -> -1, no crash, no leaked buffers
  { reset_mock(-1, -1); VaapiH264EncoderWrapper w; setup(w); size_t base = g_bufs.size();
    g_fail_map_at = 1; int r = render_sequence(w.context_.get()); CHECK(r == -1); CHECK(g_bufs.size() == base); }
  // 3. Encode IDR: first map (rate-control buffer) fails -> false, picture closed
  { reset_mock(-1, -1); VaapiH264EncoderWrapper w; setup(w); std::vector<uint8_t> out;
    // upload_surface_yuv maps image buffer first (map #1); rate control map is #2
    g_fail_map_at = 2; CHECK(!run_encode(w, out)); CHECK(g_end_calls == 1); }
  // 4. vaRenderPicture fails on each of the first renders -> false
  for (int k = 1; k <= 3; k++) { reset_mock(-1, k); VaapiH264EncoderWrapper w; setup(w); std::vector<uint8_t> out;
    CHECK(!run_encode(w, out)); }
  // 5. coded buffer map fails (last map) -> false
  { reset_mock(-1, -1); VaapiH264EncoderWrapper w; setup(w); std::vector<uint8_t> out; run_encode(w, out);
    int total = g_map_calls; reset_mock(total, -1); VaapiH264EncoderWrapper w2; setup(w2); CHECK(!run_encode(w2, out)); }
  printf(failures ? "HARNESS FAILED (%d)\n" : "HARNESS PASSED\n", failures);
  return failures != 0;
}
