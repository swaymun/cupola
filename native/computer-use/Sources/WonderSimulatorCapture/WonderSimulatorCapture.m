#import "WonderSimulatorCapture.h"
#import "WonderSimulatorDevices.h"
#import <IOSurface/IOSurface.h>
#import <QuartzCore/QuartzCore.h>
#import <dlfcn.h>

// Optional Apple private selectors documented in facebook/idb's CoreSimulator
// headers (fcd8934dd93fe4b6095a5789beb1d84174a8a994). Check every entry point;
// a changed or absent Xcode runtime must not affect ordinary Mac sharing.
@protocol WonderSimDevice <NSObject>
- (NSUUID *)UDID;
- (NSString *)name;
- (NSUInteger)state;
- (id)io;
- (NSUInteger)registerNotificationHandlerOnQueue:(dispatch_queue_t)queue handler:(void (^)(NSDictionary *))handler;
- (BOOL)unregisterNotificationHandler:(NSUInteger)token error:(NSError **)error;
@end
@interface NSObject (WonderSimLookup)
+ (id)sharedServiceContextForDeveloperDir:(NSString *)path error:(NSError **)error;
- (id)defaultDeviceSetWithError:(NSError **)error;
- (NSArray *)availableDevices;
- (NSArray *)ioPorts;
- (id)descriptor;
- (id)state;
- (NSUInteger)displayClass;
- (id)framebufferSurface;
- (void)registerScreenCallbacksWithUUID:(NSUUID *)token callbackQueue:(dispatch_queue_t)queue
    frameCallback:(void (^)(void))frame surfacesChangedCallback:(void (^)(id,id))surface
    propertiesChangedCallback:(void (^)(id))properties;
- (void)unregisterScreenCallbacksWithUUID:(NSUUID *)token;
@end

NSArray *WonderSimulatorAvailableDevices(NSString *developer, NSError **error) {
    static void *framework;
    static dispatch_once_t once;
    dispatch_once(&once, ^{ framework = dlopen("/Library/Developer/PrivateFrameworks/CoreSimulator.framework/CoreSimulator", RTLD_NOW | RTLD_LOCAL); });
    Class cls = NSClassFromString(@"SimServiceContext");
    if (!framework || ![cls respondsToSelector:@selector(sharedServiceContextForDeveloperDir:error:)]) return nil;
    id context = [cls sharedServiceContextForDeveloperDir:developer error:error];
    if (![context respondsToSelector:@selector(defaultDeviceSetWithError:)]) return nil;
    id set = [context defaultDeviceSetWithError:error];
    return [set respondsToSelector:@selector(availableDevices)] ? [set availableDevices] : nil;
}

// Which display each attached Duo capture streams, shared with the input
// injector so touches follow the posture the picture follows.
static NSMutableDictionary<NSString *, NSNumber *> *WonderInnerActive(void) {
    static NSMutableDictionary *table; static dispatch_once_t once;
    dispatch_once(&once, ^{ table = [NSMutableDictionary dictionary]; });
    return table;
}
static void WonderSetInnerActive(NSString *udid, NSNumber *inner) {
    NSMutableDictionary *table = WonderInnerActive();
    @synchronized (table) { if (inner) table[udid] = inner; else [table removeObjectForKey:udid]; }
}

@implementation WonderSimulatorCapture {
    NSString *_udid;
    id<WonderSimDevice> _device;
    // Every main-class display. A device with one has one entry; the iPhone
    // Duo has two (inner and cover) and only the one for its posture is lit.
    NSMutableArray *_renderables;
    NSMutableArray *_surfaces;
    NSUInteger _active;
    NSUUID *_token;
    NSUInteger _notificationToken;
    BOOL _observing;
    void (^_frame)(CVPixelBufferRef);
    void (^_removed)(void);
    CFTimeInterval _lastFrame;
}

+ (NSArray *)bootedDevices:(NSString *)developer error:(NSError **)error {
    @try {
        NSArray *available = WonderSimulatorAvailableDevices(developer, error);
        if (!available) return nil;
        NSMutableArray *result = [NSMutableArray array];
        for (id<WonderSimDevice> device in available) {
            if (![device respondsToSelector:@selector(state)] || [device state] != 3) continue;
            if (![device respondsToSelector:@selector(UDID)] || ![device respondsToSelector:@selector(name)]) continue;
            NSString *name = [device name];
            if (name.length == 0) name = @"Simulator";
            [result addObject:@{@"id": [@"simulator:" stringByAppendingString:[[device UDID] UUIDString]],
                               @"name": [name substringToIndex:MIN(name.length, 120)], @"kind": @"simulator"}];
            if (result.count == 32) break;
        }
        return [result sortedArrayUsingComparator:^NSComparisonResult(NSDictionary *a, NSDictionary *b) {
            return [a[@"id"] compare:b[@"id"]];
        }];
    } @catch (NSException *exception) { return nil; }
}

// The caller confines attach, detach and callbacks to one serial queue. Frames
// are copied while locked: Apple's framebuffer is mutable and must never be
// retained directly by the asynchronous encoder.
- (BOOL)attach:(NSString *)udid developer:(NSString *)developer queue:(dispatch_queue_t)queue
         frame:(void (^)(CVPixelBufferRef))frame removed:(void (^)(void))removed error:(NSError **)error {
    [self detach];
    _udid = udid.uppercaseString;
    @try {
        for (id<WonderSimDevice> device in WonderSimulatorAvailableDevices(developer, error)) {
            if (![device respondsToSelector:@selector(UDID)] || ![device respondsToSelector:@selector(state)]) continue;
            if ([[[device UDID] UUIDString] caseInsensitiveCompare:udid] == NSOrderedSame && [device state] == 3) _device = device;
        }
        if (!_device || ![_device respondsToSelector:@selector(io)] ||
            ![_device respondsToSelector:@selector(registerNotificationHandlerOnQueue:handler:)] ||
            ![_device respondsToSelector:@selector(unregisterNotificationHandler:error:)]) return NO;
        id io = [_device io];
        if (![io respondsToSelector:@selector(ioPorts)]) return NO;
        Protocol *renderable = NSProtocolFromString(@"SimDisplayIOSurfaceRenderable");
        for (id port in [io ioPorts]) {
            if (![port respondsToSelector:@selector(descriptor)]) continue;
            id candidate = [port descriptor];
            if (!renderable || ![candidate conformsToProtocol:renderable] || ![candidate respondsToSelector:@selector(state)]) continue;
            id state = [(NSObject *)candidate state];
            if (![state respondsToSelector:@selector(displayClass)] || [state displayClass] != 0) continue;
            // A device with several main-class displays (the iPhone Duo) keeps
            // them all: on Xcode 27.1 the Duo lists its 2007x2853 inner display
            // first and its 1398x2034 cover second, and only the one for the
            // current hinge posture draws (the other stays black), so a fixed
            // pick streamed a black picture whenever the posture was the other.
            if (![candidate respondsToSelector:@selector(framebufferSurface)] ||
                ![candidate respondsToSelector:@selector(registerScreenCallbacksWithUUID:callbackQueue:frameCallback:surfacesChangedCallback:propertiesChangedCallback:)] ||
                ![candidate respondsToSelector:@selector(unregisterScreenCallbacksWithUUID:)]) continue;
            if (!_renderables) { _renderables = [NSMutableArray array]; _surfaces = [NSMutableArray array]; }
            [_renderables addObject:candidate];
            [_surfaces addObject:[NSNull null]];
        }
        if (_renderables.count == 0) { [self detach]; return NO; }
        _active = 0;
        _enabled = YES;
        _token = NSUUID.UUID; _frame = [frame copy]; _removed = [removed copy]; _lastFrame = 0;
        NSUUID *identity = _token;
        __weak WonderSimulatorCapture *weakSelf = self;
        _notificationToken = [_device registerNotificationHandlerOnQueue:queue handler:^(NSDictionary *change) {
            WonderSimulatorCapture *s = weakSelf;
            if (!s || s->_token != identity || [s->_device state] == 3) return;
            void (^removed)(void) = s->_removed;
            [s detach];
            if (removed) removed();
        }];
        _observing = YES;
        for (NSUInteger index = 0; index < _renderables.count; index++) {
            [self storeSurface:[_renderables[index] framebufferSurface] at:index];
        }
        for (NSUInteger index = 0; index < _renderables.count; index++) {
            [_renderables[index] registerScreenCallbacksWithUUID:identity callbackQueue:queue
                frameCallback:^{ WonderSimulatorCapture *s = weakSelf; if (s && s->_token == identity) [s copyFrame]; }
                surfacesChangedCallback:^(id value, id masked) {
                    WonderSimulatorCapture *s = weakSelf;
                    if (s && s->_token == identity) [s changeSurface:value at:index];
                }
                propertiesChangedCallback:^(id properties) {}];
        }
        if ([_device state] != 3) { [self detach]; return NO; }
        _lastFrame = 0;
        [self copyFrame];
        return YES;
    } @catch (NSException *exception) { [self detach]; return NO; }
}

- (void)storeSurface:(id)value at:(NSUInteger)index {
    if (index >= _surfaces.count) return;
    _surfaces[index] = value && CFGetTypeID((__bridge CFTypeRef)value) == IOSurfaceGetTypeID() ? value : [NSNull null];
}

- (void)changeSurface:(id)value at:(NSUInteger)index {
    [self storeSurface:value at:index];
    _lastFrame = 0;
    [self copyFrame];
}

// An off display's surface is all zero bytes (measured on the Duo: 0 against
// 58-132 for the lit one). A sparse grid is enough to tell them apart.
static BOOL WonderSurfaceIsLit(IOSurfaceRef surface) {
    size_t width = IOSurfaceGetWidth(surface), height = IOSurfaceGetHeight(surface);
    if (!width || !height || IOSurfaceGetPixelFormat(surface) != kCVPixelFormatType_32BGRA) return NO;
    if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess) return NO;
    const uint8_t *base = IOSurfaceGetBaseAddress(surface);
    size_t stride = IOSurfaceGetBytesPerRow(surface);
    BOOL lit = NO;
    for (size_t y = 0; base && !lit && y < height; y += MAX(height / 48, 1)) {
        for (size_t x = 0; x < width; x += MAX(width / 48, 1)) {
            const uint8_t *pixel = base + y * stride + x * 4;
            if (pixel[0] | pixel[1] | pixel[2]) { lit = YES; break; }
        }
    }
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    return lit;
}

/// The display to copy: the first one that draws, in CoreSimulator's order.
/// On the Duo that is the inner display, which goes black when the device is
/// folded (measured), while the cover keeps its last picture when unfolded, so
/// "first lit" follows the posture. A hinge or orientation change moves the
/// stream without a new attach. Nothing lit keeps the last choice.
- (id)activeSurface {
    for (NSUInteger index = 0; index < _surfaces.count; index++) {
        id candidate = _surfaces[index];
        if (candidate == [NSNull null]) continue;
        if (_surfaces.count == 1 || WonderSurfaceIsLit((__bridge IOSurfaceRef)candidate)) { _active = index; break; }
    }
    id surface = _surfaces[_active];
    if (_surfaces.count > 1 && _udid && surface != [NSNull null]) {
        // The inner display is the largest main-class one.
        size_t widest = 0;
        for (id other in _surfaces) if (other != [NSNull null]) widest = MAX(widest, IOSurfaceGetWidth((__bridge IOSurfaceRef)other));
        WonderSetInnerActive(_udid, @(IOSurfaceGetWidth((__bridge IOSurfaceRef)surface) == widest));
    }
    return surface == [NSNull null] ? nil : surface;
}

- (void)copyFrame {
    CFTimeInterval now = CACurrentMediaTime();
    if (!_enabled || !_token || !_frame || now - _lastFrame < 1.0 / 15.0) return;
    id active = [self activeSurface];
    if (!active) return;
    _lastFrame = now;
    IOSurfaceRef surface = (__bridge IOSurfaceRef)active;
    size_t width = IOSurfaceGetWidth(surface), height = IOSurfaceGetHeight(surface);
    if (!width || !height || width > 4096 || height > 4096 || IOSurfaceGetPixelFormat(surface) != kCVPixelFormatType_32BGRA) return;
    if (IOSurfaceLock(surface, kIOSurfaceLockReadOnly, NULL) != kIOReturnSuccess) return;
    CVPixelBufferRef copy = NULL;
    BOOL copied = NO;
    if (CVPixelBufferCreate(NULL, width, height, kCVPixelFormatType_32BGRA,
                           (__bridge CFDictionaryRef)@{(id)kCVPixelBufferIOSurfacePropertiesKey: @{}}, &copy) == kCVReturnSuccess) {
        CVPixelBufferLockBaseAddress(copy, 0);
        const uint8_t *input = IOSurfaceGetBaseAddress(surface);
        uint8_t *output = CVPixelBufferGetBaseAddress(copy);
        size_t inputStride = IOSurfaceGetBytesPerRow(surface), outputStride = CVPixelBufferGetBytesPerRow(copy);
        if (input && output && inputStride >= width * 4 && outputStride >= width * 4) {
            for (size_t row = 0; row < height; row++) memcpy(output + row * outputStride, input + row * inputStride, width * 4);
            copied = YES;
        }
        CVPixelBufferUnlockBaseAddress(copy, 0);
    }
    IOSurfaceUnlock(surface, kIOSurfaceLockReadOnly, NULL);
    if (copy) { if (copied) _frame(copy); CVPixelBufferRelease(copy); }
}

+ (BOOL)innerDisplayActiveForUDID:(NSString *)udid {
    NSMutableDictionary *table = WonderInnerActive();
    @synchronized (table) { return [table[udid.uppercaseString] boolValue]; }
}

- (void)detach {
    if (_udid) WonderSetInnerActive(_udid, nil);
    NSUUID *token = _token; _token = nil;
    for (id renderable in _renderables) {
        @try { if (token) [renderable unregisterScreenCallbacksWithUUID:token]; } @catch (NSException *exception) {}
    }
    @try { if (_observing) [_device unregisterNotificationHandler:_notificationToken error:nil]; } @catch (NSException *exception) {}
    _observing = NO; _renderables = nil; _surfaces = nil; _active = 0; _device = nil; _frame = nil; _removed = nil; _udid = nil;
}
- (void)dealloc { [self detach]; }
@end
