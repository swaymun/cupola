#import "WonderSimulatorInput.h"
#import "WonderSimulatorDevices.h"
#import <CoreGraphics/CoreGraphics.h>
#import <dlfcn.h>
#import <mach/mach_time.h>
#import <stdatomic.h>
#import <xpc/xpc.h>

// Simulator HID input, written for Cupola from the wire formats documented in
// facebook/idb's FBSimulatorControl (MIT): PrivateHeaders/SimulatorApp/Indigo.h
// and FBSimulatorControl/HID (SimulatorIndigoHID, SimulatorDTUHIDConnection,
// DTUHIDModels). No idb code is vendored. Two transports:
//  - dtuhid: CoreSimulator 1155.4+ (Xcode 27) injects `dtuhidd` into the guest.
//    Events are plain XPC dictionaries sent to its digitizer service, looked up
//    in the device's bootstrap namespace. Legacy Indigo input is dropped there.
//  - indigo: SimulatorKit's SimDeviceLegacyHIDClient with the exported
//    IndigoHIDMessageFor* builders, for older toolchains.
// Every private class, selector and symbol is checked before use.

@interface NSObject (WonderSimInputLookup)
- (NSUUID *)UDID;
- (NSUInteger)state;
- (unsigned int)lookup:(NSString *)name error:(NSError **)error;
- (id)initWithDevice:(id)device error:(NSError **)error;
- (void)sendWithMessage:(void *)message freeWhenDone:(BOOL)freeWhenDone
        completionQueue:(dispatch_queue_t)queue completion:(void (^)(NSError *))completion;
@end

typedef void *(*IndigoMouseFn)(CGPoint *, CGPoint *, uint32_t, NSUInteger, CGSize, uint32_t);
typedef void *(*IndigoButtonFn)(int32_t, int32_t, int32_t);
typedef void *(*IndigoKeyboardFn)(int32_t, int32_t);
typedef void *(*IndigoHIDArbitraryFn)(int32_t, uint32_t, uint32_t, int32_t);
typedef xpc_object_t (*EndpointFromPortFn)(mach_port_t, uint64_t, uint64_t) __attribute__((ns_returns_retained));
typedef void (*EnableSim2HostFn)(xpc_connection_t);

static NSString *const DigitizerService = @"com.apple.coredevice.feature.remote.hid.digitizer";

// Indigo.h layout (pack 4): payload at 0x20, event at 0x30, IndigoTouch is
// 0x70 bytes with xRatio at 0x3c and yRatio at 0x44. A payload is 0x90 bytes.
enum {
    IndigoPayloadOffset = 0x20, IndigoEventOffset = 0x30, IndigoTouchSize = 0x70,
    IndigoPayloadSize = 0x90, IndigoMessageSize = 0xB0, IndigoXRatio = 0x3c, IndigoYRatio = 0x44,
    IndigoTargetDigitizer = 0x32, IndigoTargetHardware = 0x33,
    IndigoDown = 1, IndigoUp = 2,
};

static BOOL CoreSimulatorShipsDTUHID(void) {
    Class device = NSClassFromString(@"SimDevice");
    NSString *version = device ? [NSBundle bundleForClass:device].infoDictionary[@"CFBundleVersion"] : nil;
    return [version isKindOfClass:NSString.class] && [version compare:@"1155.4" options:NSNumericSearch] != NSOrderedAscending;
}

@implementation WonderSimulatorInput {
    id _device;
    // dtuhid
    xpc_connection_t _connection;
    atomic_bool _invalidated;
    // indigo
    id _client;
    IndigoMouseFn _mouse;
    IndigoButtonFn _buttonMessage;
    IndigoKeyboardFn _keyboard;
    IndigoHIDArbitraryFn _arbitrary;
    dispatch_queue_t _completionQueue;
    atomic_bool _deliveryFailed;
}

+ (instancetype)connect:(NSString *)udid developer:(NSString *)developer reason:(NSString **)reason {
    return [self connect:udid developer:developer transport:nil reason:reason];
}

+ (instancetype)connect:(NSString *)udid developer:(NSString *)developer transport:(NSString *)transport
                 reason:(NSString **)reason {
    NSString *why = @"simulator_input_unavailable";
    @try {
        id device = nil;
        for (id candidate in WonderSimulatorAvailableDevices(developer, NULL)) {
            if (![candidate respondsToSelector:@selector(UDID)] || ![candidate respondsToSelector:@selector(state)]) continue;
            if ([[[candidate UDID] UUIDString] caseInsensitiveCompare:udid] == NSOrderedSame) device = candidate;
        }
        if (!device || [device state] != 3) {
            why = @"simulator_not_booted";
        } else {
            WonderSimulatorInput *input = [[self alloc] initWithDevice:device udid:udid];
            BOOL dtuhid = transport ? [transport isEqualToString:@"dtuhid"] : CoreSimulatorShipsDTUHID();
            if (dtuhid) {
                if ([input connectDTUHID]) { return input; }
                why = @"simulator_input_service_unavailable";
            } else if ([input connectIndigo:developer]) {
                return input;
            } else {
                why = @"simulator_input_unavailable";
            }
            [input close];
        }
    } @catch (NSException *exception) {
        why = @"simulator_input_unavailable";
    }
    if (reason) *reason = why;
    return nil;
}

- (instancetype)initWithDevice:(id)device udid:(NSString *)udid {
    if ((self = [super init])) {
        _device = device;
        _udid = [udid uppercaseString];
        _transport = @"none";
        _completionQueue = dispatch_queue_create("com.wonder.simulator-input.completion", DISPATCH_QUEUE_SERIAL);
    }
    return self;
}

#pragma mark dtuhid

- (BOOL)connectDTUHID {
    EndpointFromPortFn endpointFromPort = (EndpointFromPortFn)dlsym(RTLD_DEFAULT, "xpc_endpoint_create_mach_port_4sim");
    EnableSim2HostFn enableSim2Host = (EnableSim2HostFn)dlsym(RTLD_DEFAULT, "xpc_connection_enable_sim2host_4sim");
    if (!endpointFromPort || !enableSim2Host || ![_device respondsToSelector:@selector(lookup:error:)]) return NO;
    // dtuhidd is demand-launched and can be throttled right after boot: two
    // attempts, each proven by a barrier reply (a send alone never fails).
    for (int attempt = 0; attempt < 2; attempt++) {
        NSError *error = nil;
        mach_port_t port = [_device lookup:DigitizerService error:&error];
        if (port == MACH_PORT_NULL) return NO;
        xpc_object_t endpoint = endpointFromPort(port, 0, 0);
        if (!endpoint) return NO;
        xpc_connection_t connection = xpc_connection_create_from_endpoint((xpc_endpoint_t)endpoint);
        if (!connection) return NO;
        enableSim2Host(connection);
        __weak WonderSimulatorInput *weakSelf = self;
        xpc_connection_set_target_queue(connection, _completionQueue);
        xpc_connection_set_event_handler(connection, ^(xpc_object_t event) {
            if (event == XPC_ERROR_CONNECTION_INVALID) { WonderSimulatorInput *s = weakSelf; if (s) s->_invalidated = YES; }
        });
        xpc_connection_activate(connection);
        if ([self barrier:connection]) {
            _connection = connection;
            _transport = @"dtuhid";
            // Device open in the guest follows the first reply (idb measured ~200 ms).
            [NSThread sleepForTimeInterval:0.2];
            return YES;
        }
        xpc_connection_cancel(connection);
        if (attempt == 0) [NSThread sleepForTimeInterval:1.0];
    }
    return NO;
}

- (BOOL)barrier:(xpc_connection_t)connection {
    // Usage 0 is "no event": the daemon replies without a guest keypress.
    xpc_object_t payload = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_uint64(payload, "usageCode", 0);
    xpc_dictionary_set_uint64(payload, "state", 2);
    xpc_object_t message = [self message:@"IndigoKeyboardButtonEvent" payload:payload barrier:YES];
    dispatch_semaphore_t done = dispatch_semaphore_create(0);
    __block BOOL answered = NO;
    xpc_connection_send_message_with_reply(connection, message, _completionQueue, ^(xpc_object_t reply) {
        answered = xpc_get_type(reply) != XPC_TYPE_ERROR;
        dispatch_semaphore_signal(done);
    });
    // Called only from the injector's worker queue, never the main thread.
    if (dispatch_semaphore_wait(done, dispatch_time(DISPATCH_TIME_NOW, 3 * NSEC_PER_SEC)) != 0) return NO;
    return answered;
}

- (xpc_object_t)message:(NSString *)type payload:(xpc_object_t)payload barrier:(BOOL)barrier {
    xpc_object_t message = xpc_dictionary_create(NULL, NULL, 0);
    xpc_dictionary_set_string(message, "messageType", type.UTF8String);
    xpc_dictionary_set_bool(message, "isBarrier", barrier);
    xpc_dictionary_set_string(message, "featureIdentifier", DigitizerService.UTF8String);
    xpc_dictionary_set_value(message, "payload", payload);
    return message;
}

- (BOOL)sendDTUHID:(NSString *)type payload:(xpc_object_t)payload {
    if (!_connection || _invalidated) return NO;
    xpc_connection_send_message(_connection, [self message:type payload:payload barrier:NO]);
    return YES;
}

#pragma mark indigo

- (BOOL)connectIndigo:(NSString *)developer {
    NSString *path = [[developer stringByDeletingLastPathComponent]
        stringByAppendingPathComponent:@"SharedFrameworks/SimulatorKit.framework/SimulatorKit"];
    void *kit = dlopen(path.fileSystemRepresentation, RTLD_NOW | RTLD_LOCAL);
    if (!kit) return NO;
    _mouse = (IndigoMouseFn)dlsym(kit, "IndigoHIDMessageForMouseNSEvent");
    _buttonMessage = (IndigoButtonFn)dlsym(kit, "IndigoHIDMessageForButton");
    _keyboard = (IndigoKeyboardFn)dlsym(kit, "IndigoHIDMessageForKeyboardArbitrary");
    _arbitrary = (IndigoHIDArbitraryFn)dlsym(kit, "IndigoHIDMessageForHIDArbitrary");
    Class cls = NSClassFromString(@"_TtC12SimulatorKit24SimDeviceLegacyHIDClient") ?: NSClassFromString(@"SimulatorKit.SimDeviceLegacyHIDClient");
    if (!_mouse || !_buttonMessage || !_keyboard || !_arbitrary || !cls ||
        ![cls instancesRespondToSelector:@selector(initWithDevice:error:)] ||
        ![cls instancesRespondToSelector:@selector(sendWithMessage:freeWhenDone:completionQueue:completion:)]) return NO;
    NSError *error = nil;
    id client = [[cls alloc] initWithDevice:_device error:&error];
    if (!client) return NO;
    _client = client;
    _transport = @"indigo";
    return YES;
}

- (BOOL)sendIndigo:(void *)message {
    if (!message) return NO;
    if (!_client || _deliveryFailed) { free(message); return NO; }
    __weak WonderSimulatorInput *weakSelf = self;
    // Ownership passes to the client (freeWhenDone). If it raises, the buffer
    // is deliberately leaked rather than risk a double free.
    [_client sendWithMessage:message freeWhenDone:YES completionQueue:_completionQueue completion:^(NSError *error) {
        WonderSimulatorInput *s = weakSelf;
        if (s && error) s->_deliveryFailed = YES;
    }];
    return YES;
}

// SimulatorKit has no single-touch builder: take the contact from the mouse
// builder and wrap it in a single-touch (eventType 2) two-payload message.
- (void *)indigoTouchX:(double)x y:(double)y down:(BOOL)down {
    CGPoint point = CGPointMake(x, y);
    uint8_t *source = _mouse(&point, NULL, IndigoTargetDigitizer, down ? IndigoDown : IndigoUp, CGSizeMake(1, 1), 0);
    if (!source) return NULL;
    uint8_t *message = calloc(1, IndigoMessageSize + IndigoPayloadSize);
    if (!message) { free(source); return NULL; }
    uint32_t innerSize = IndigoPayloadSize, eventKind = 0xB, one = 1, two = 2;
    uint64_t timestamp = mach_absolute_time();
    memcpy(message + 0x18, &innerSize, 4);
    message[0x1c] = 2;
    memcpy(message + IndigoPayloadOffset, &eventKind, 4);
    memcpy(message + IndigoPayloadOffset + 4, &timestamp, 8);
    memcpy(message + IndigoEventOffset, source + IndigoEventOffset, IndigoTouchSize);
    free(source);
    memcpy(message + IndigoXRatio, &x, 8);
    memcpy(message + IndigoYRatio, &y, 8);
    uint8_t *second = message + IndigoPayloadOffset + IndigoPayloadSize;
    memcpy(second, message + IndigoPayloadOffset, IndigoPayloadSize);
    memcpy(second + 0x10, &one, 4);
    memcpy(second + 0x14, &two, 4);
    return message;
}

#pragma mark input

- (BOOL)isUsable {
    @try {
        if (_invalidated || _deliveryFailed || (!_connection && !_client)) return NO;
        return [_device respondsToSelector:@selector(state)] && [_device state] == 3;
    } @catch (NSException *exception) { return NO; }
}

- (BOOL)touchX:(double)x y:(double)y phase:(WonderSimulatorTouchPhase)phase {
    if (!isfinite(x) || !isfinite(y) || x < 0 || x > 1 || y < 0 || y > 1) return NO;
    @try {
        if (_connection) {
            xpc_object_t point = xpc_dictionary_create(NULL, NULL, 0);
            xpc_dictionary_set_double(point, "x", x);
            xpc_dictionary_set_double(point, "y", y);
            xpc_object_t payload = xpc_dictionary_create(NULL, NULL, 0);
            xpc_dictionary_set_value(payload, "pointOne", point);
            xpc_dictionary_set_uint64(payload, "eventType", (uint64_t)phase);
            xpc_dictionary_set_uint64(payload, "edge", 0);
            xpc_dictionary_set_uint64(payload, "target", 0);
            return [self sendDTUHID:@"IndigoDigitizerEvent" payload:payload];
        }
        // The legacy contact has only down/up: a moved contact is another down.
        return [self sendIndigo:[self indigoTouchX:x y:y down:phase != WonderSimulatorTouchPhaseEnded]];
    } @catch (NSException *exception) { return NO; }
}

- (BOOL)button:(WonderSimulatorButton)button down:(BOOL)down {
    // HID Consumer page (0x0C): Menu, Power, Volume Increment, Volume Decrement.
    uint32_t usage;
    switch (button) {
    case WonderSimulatorButtonHome: usage = 0x40; break;
    case WonderSimulatorButtonLock: usage = 0x30; break;
    case WonderSimulatorButtonVolumeUp: usage = 0xE9; break;
    case WonderSimulatorButtonVolumeDown: usage = 0xEA; break;
    default: return NO;
    }
    @try {
        if (_connection) {
            xpc_object_t payload = xpc_dictionary_create(NULL, NULL, 0);
            xpc_dictionary_set_uint64(payload, "usagePage", 0x0C);
            xpc_dictionary_set_uint64(payload, "usageCode", usage);
            xpc_dictionary_set_uint64(payload, "state", down ? 1 : 2);
            return [self sendDTUHID:@"IndigoButtonEvent" payload:payload];
        }
        if (button == WonderSimulatorButtonHome || button == WonderSimulatorButtonLock) {
            // ButtonEventSourceHomeButton 0, ButtonEventSourceLock 1.
            return [self sendIndigo:_buttonMessage(button == WonderSimulatorButtonHome ? 0 : 1, down ? IndigoDown : IndigoUp, IndigoTargetHardware)];
        }
        return [self sendIndigo:_arbitrary(IndigoTargetDigitizer, 0x0C, usage, down ? IndigoDown : IndigoUp)];
    } @catch (NSException *exception) { return NO; }
}

- (BOOL)keyboardUsage:(uint32_t)usage down:(BOOL)down {
    if (usage == 0 || usage > 0xE7) return NO;
    @try {
        if (_connection) {
            xpc_object_t payload = xpc_dictionary_create(NULL, NULL, 0);
            xpc_dictionary_set_uint64(payload, "usageCode", usage);
            xpc_dictionary_set_uint64(payload, "state", down ? 1 : 2);
            return [self sendDTUHID:@"IndigoKeyboardButtonEvent" payload:payload];
        }
        return [self sendIndigo:_keyboard((int32_t)usage, down ? IndigoDown : IndigoUp)];
    } @catch (NSException *exception) { return NO; }
}

- (void)close {
    @try { if (_connection) xpc_connection_cancel(_connection); } @catch (NSException *exception) {}
    _connection = nil;
    _client = nil;
    _device = nil;
}

- (void)dealloc { [self close]; }
@end
