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

@implementation WonderSimulatorCapture {
    id<WonderSimDevice> _device;
    id _renderable;
    id _surface;
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
            // A device with several main-class displays (the iPhone Duo) shows
            // the first CoreSimulator lists: on the Duo (Xcode 27.1) that is its
            // 2007x2853 inner display, the one `simctl io screenshot` captures.
            if (!_renderable) _renderable = candidate;
        }
        if (!_renderable || ![_renderable respondsToSelector:@selector(framebufferSurface)] ||
            ![_renderable respondsToSelector:@selector(registerScreenCallbacksWithUUID:callbackQueue:frameCallback:surfacesChangedCallback:propertiesChangedCallback:)] ||
            ![_renderable respondsToSelector:@selector(unregisterScreenCallbacksWithUUID:)]) { [self detach]; return NO; }
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
        [_renderable registerScreenCallbacksWithUUID:identity callbackQueue:queue
            frameCallback:^{ WonderSimulatorCapture *s = weakSelf; if (s && s->_token == identity) [s copyFrame]; }
            surfacesChangedCallback:^(id value, id masked) {
                WonderSimulatorCapture *s = weakSelf;
                if (s && s->_token == identity) [s changeSurface:value];
            }
            propertiesChangedCallback:^(id properties) {}];
        if ([_device state] != 3) { [self detach]; return NO; }
        [self changeSurface:[_renderable framebufferSurface]];
        return YES;
    } @catch (NSException *exception) { [self detach]; return NO; }
}

- (void)changeSurface:(id)value {
    _surface = value && CFGetTypeID((__bridge CFTypeRef)value) == IOSurfaceGetTypeID() ? value : nil;
    _lastFrame = 0;
    [self copyFrame];
}

- (void)copyFrame {
    CFTimeInterval now = CACurrentMediaTime();
    if (!_enabled || !_token || !_surface || !_frame || now - _lastFrame < 1.0 / 15.0) return;
    _lastFrame = now;
    IOSurfaceRef surface = (__bridge IOSurfaceRef)_surface;
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

- (void)detach {
    NSUUID *token = _token; _token = nil;
    @try { if (token) [_renderable unregisterScreenCallbacksWithUUID:token]; } @catch (NSException *exception) {}
    @try { if (_observing) [_device unregisterNotificationHandler:_notificationToken error:nil]; } @catch (NSException *exception) {}
    _observing = NO; _renderable = nil; _surface = nil; _device = nil; _frame = nil; _removed = nil;
}
- (void)dealloc { [self detach]; }
@end
