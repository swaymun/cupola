// Simulator-only Wonder input helper. NEVER add to any app target.
// Adapted and modified from Saffsanity/sill Tests/keyrig, Tests/duorig and
// Tests/touchrig at 5c323e6965b5011809548e5067f9e8e6fa6adf54 (2026-10-17).
// Copyright 2026 Noah Saffer. Licensed under Apache-2.0; see LICENSE.sill.
// Changes: display-targeted digitizer taps/drags, strict runtime checks, combined CLI.
#import <TargetConditionals.h>
#if !TARGET_OS_SIMULATOR
#error This helper must only be compiled for the iOS Simulator.
#endif
#ifndef WONDER_SIMRIG_STANDALONE
#error Build only with scripts/wonderctl as a standalone helper, never in an app target.
#endif
#import <Foundation/Foundation.h>
#import <math.h>
#import <objc/runtime.h>
#import <objc/message.h>
#import <dlfcn.h>
#import <mach/mach_time.h>

typedef struct __IOHIDEvent *IOHIDEventRef;
typedef IOHIDEventRef (*CreateKeyboard_t)(CFAllocatorRef, uint64_t, uint32_t, uint32_t, Boolean, uint32_t);

static void *iokit;

static void out(NSString *s) {
    fprintf(stdout, "%s\n", s.UTF8String);
    fflush(stdout);
}

static void *sym(const char *name) {
    void *p = dlsym(iokit, name);
    if (!p) { out([NSString stringWithFormat:@"wonder-simrig: %s is missing from this simulator's IOKit", name]); exit(2); }
    return p;
}

/// Inside a simulator launchd_sim sets SIMULATOR_UDID and SIMULATOR_ROOT; anywhere else this refuses
/// before touching any HID interface.
static void requireSimulator(void) {
    const char *udid = getenv("SIMULATOR_UDID");
    const char *root = getenv("SIMULATOR_ROOT");
    if (!udid || !root || !*udid || !*root) {
        out(@"wonder-simrig: refused: not inside an iOS Simulator. Run it with scripts/wonderctl (simctl spawn).");
        exit(3);
    }
    Dl_info foundation;
    if (!dladdr((const void *)NSClassFromString, &foundation) ||
        !strstr(foundation.dli_fname, root)) {
        out(@"wonder-simrig: refused: Foundation is outside SIMULATOR_ROOT"); exit(3);
    }
    iokit = dlopen("/System/Library/Frameworks/IOKit.framework/IOKit", RTLD_NOW);
    if (!iokit) { out(@"wonder-simrig: IOKit did not load"); exit(2); }
    if (!dlopen("/System/Library/PrivateFrameworks/HID.framework/HID", RTLD_NOW)) { out(@"wonder-simrig: HID.framework did not load"); exit(2); }
}

/// HIDVirtualEventService's delegate (the protocol is not registered in the runtime, so these are its
/// selectors by name): the service's properties, nothing else.
@interface RigDelegate : NSObject
@property (nonatomic, copy) NSDictionary *properties;
@end

@implementation RigDelegate
- (BOOL)setProperty:(id)value forKey:(NSString *)key forService:(id)service { return NO; }
- (id)propertyForKey:(NSString *)key forService:(id)service { return self.properties[key]; }
- (id)copyEventMatching:(NSDictionary *)matching forService:(id)service { return nil; }
- (BOOL)setOutputEvent:(id)event forService:(id)service { return NO; }
- (void)notification:(NSInteger)type withProperty:(NSDictionary *)property forService:(id)service {}
@end

static void spin(double seconds) {
    [[NSRunLoop mainRunLoop] runUntilDate:[NSDate dateWithTimeIntervalSinceNow:seconds]];
}

/// A virtual service with these usage pairs, up for the run; dispatch with `dispatch`.
static id startService(RigDelegate *delegate, double settle) {
    Class cls = objc_getClass("HIDVirtualEventService");
    if (!cls) { out(@"wonder-simrig: no HIDVirtualEventService in this simulator"); exit(1); }
    id service = ((id (*)(id, SEL))objc_msgSend)([cls alloc], sel_registerName("init"));
    if (!service) { out(@"wonder-simrig: the virtual service was not made"); exit(1); }
    ((void (*)(id, SEL, id))objc_msgSend)(service, sel_registerName("setDelegate:"), delegate);
    ((void (*)(id, SEL, dispatch_queue_t))objc_msgSend)(service, sel_registerName("setDispatchQueue:"), dispatch_get_main_queue());
    ((void (*)(id, SEL))objc_msgSend)(service, sel_registerName("activate"));
    spin(settle);
    return service;
}

static BOOL dispatchEvent(id service, IOHIDEventRef event) {
    if (!event) return NO;
    BOOL ok = ((BOOL (*)(id, SEL, id))objc_msgSend)(service, sel_registerName("dispatchEvent:"), (__bridge id)event);
    CFRelease(event);
    return ok;
}

static void stopService(id service, double hold) {
    spin(hold);
    ((void (*)(id, SEL))objc_msgSend)(service, sel_registerName("cancel"));
    spin(0.1);
}

static int keys(NSString *spec, double settle, double hold) {
    CreateKeyboard_t create = (CreateKeyboard_t)sym("IOHIDEventCreateKeyboardEvent");
    __attribute__((objc_precise_lifetime)) RigDelegate *delegate = [RigDelegate new];
    delegate.properties = @{ @"PrimaryUsagePage": @1, @"PrimaryUsage": @6,
                             @"DeviceUsagePairs": @[ @{ @"DeviceUsagePage": @1, @"DeviceUsage": @6 } ],
                             @"Product": @"wonder-simrig keyboard (TEST ONLY)", @"Transport": @"Bluetooth", @"Built-In": @NO,
                             @"CountryCode": @0, @"StandardType": @0, @"HIDVirtualDevice": @YES };
    id service = startService(delegate, settle);
    int failures = 0;
    for (NSString *step in [spec componentsSeparatedByString:@","]) {
        NSArray *parts = [step componentsSeparatedByString:@":"];
        unsigned usage = 0;
        if (![[NSScanner scannerWithString:parts[0]] scanHexInt:&usage]) { out([@"wonder-simrig: cannot read " stringByAppendingString:step]); failures++; continue; }
        NSArray *phases = parts.count > 1 ? @[@([parts[1] isEqualToString:@"down"])] : @[@YES, @NO];
        for (NSNumber *down in phases) {
            BOOL ok = dispatchEvent(service, create(kCFAllocatorDefault, mach_absolute_time(), 7, usage, down.boolValue, 0));
            out([NSString stringWithFormat:@"wonder-simrig: usage 0x%x %@%@", usage, down.boolValue ? @"down" : @"up", ok ? @"" : @" NOT dispatched"]);
            if (!ok) failures++;
            spin(0.01);
        }
        spin(0.005);
    }
    stopService(service, hold);
    return failures ? 1 : 0;
}

typedef IOHIDEventRef (*FingerFn)(CFAllocatorRef, uint64_t, uint32_t, uint32_t, uint32_t,
                                  double, double, double, double, double, double, double, double, double, double,
                                  boolean_t, boolean_t, uint32_t);
typedef void (*SetIntFn)(IOHIDEventRef, uint32_t, CFIndex);
typedef void (*AppendFn)(IOHIDEventRef, IOHIDEventRef, uint32_t);

/// Match only the requested display's native digitizer, never an arbitrary
/// touchscreen from the simulator's other screen.
static uint64_t touchSender(void *client, NSString *wanted) {
    CFArrayRef services = ((CFArrayRef (*)(void *))sym("IOHIDEventSystemClientCopyServices"))(client);
    uint64_t sender = 0;
    for (id item in (__bridge NSArray *)services) {
        CFTypeRef product = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("Product"));
        BOOL match = product && [(__bridge id)product isEqual:wanted];
        if (product) CFRelease(product);
        if (!match) continue;
        CFTypeRef registry = ((CFTypeRef (*)(void *))sym("IOHIDServiceClientGetRegistryID"))((__bridge void *)item);
        if (registry && CFGetTypeID(registry) == CFNumberGetTypeID()) CFNumberGetValue(registry, kCFNumberSInt64Type, &sender);
    }
    if (services) CFRelease(services);
    return sender;
}

/// Xcode 27.2 reports no Product for any digitizer, so no service matches by name. The
/// simulator's own built-in digitizers (registered before this command's) are in display
/// order: ordinal 0 is the primary (cover) display, 1 the inner one.
static uint64_t builtInDigitizerSender(void *client, NSString *own, int ordinal) {
    CFArrayRef services = ((CFArrayRef (*)(void *))sym("IOHIDEventSystemClientCopyServices"))(client);
    NSMutableArray<NSNumber *> *found = [NSMutableArray array];
    for (id item in (__bridge NSArray *)services) {
        CFTypeRef page = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("PrimaryUsagePage"));
        CFTypeRef usage = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("PrimaryUsage"));
        CFTypeRef builtIn = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("Built-In"));
        CFTypeRef product = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("Product"));
        CFTypeRef transport = ((CFTypeRef (*)(void *, CFStringRef))sym("IOHIDServiceClientCopyProperty"))((__bridge void *)item, CFSTR("Transport"));
        CFTypeRef registry = ((CFTypeRef (*)(void *))sym("IOHIDServiceClientGetRegistryID"))((__bridge void *)item);
        uint64_t value = 0;
        if ([(__bridge id)page isEqual:@13] && [(__bridge id)usage isEqual:@4] && [(__bridge id)builtIn isEqual:@YES] &&
            ![(__bridge id)product isEqual:own] && ![(__bridge id)transport isEqual:@"CoreDevice"] && registry && CFGetTypeID(registry) == CFNumberGetTypeID() &&
            CFNumberGetValue(registry, kCFNumberSInt64Type, &value)) [found addObject:@(value)];
        if (page) CFRelease(page);
        if (usage) CFRelease(usage);
        if (builtIn) CFRelease(builtIn);
        if (product) CFRelease(product);
        if (transport) CFRelease(transport);
    }
    if (services) CFRelease(services);
    return (NSUInteger)ordinal < found.count ? [[found sortedArrayUsingSelector:@selector(compare:)][ordinal] unsignedLongLongValue] : 0;
}

static int touch(double x, double y, double endX, double endY, double width, double height,
                 double duration, NSString *display, int ordinal) {
    void *client = ((void *(*)(CFAllocatorRef, int, CFDictionaryRef))sym("IOHIDEventSystemClientCreateWithType"))(kCFAllocatorDefault, 0, NULL);
    if (!client) return 1;
    NSString *wanted = [NSString stringWithFormat:@"CoreDevice touchscreen(%@)", display];
    uint64_t sender = touchSender(client, wanted);
    __attribute__((objc_precise_lifetime)) RigDelegate *touchDelegate = nil;
    id touchService = nil;
    if (!sender) {
        // Fresh pooled clones have no Device Hub connection. Register the
        // same display-addressed digitizer for this command's lifetime.
        // Geometry and the display UUID were validated by wonderctl.
        touchDelegate = [RigDelegate new];
        touchDelegate.properties = @{ @"PrimaryUsagePage": @13, @"PrimaryUsage": @4,
            @"DeviceUsagePairs": @[ @{ @"DeviceUsagePage": @13, @"DeviceUsage": @4 } ],
            @"Product": wanted, @"Transport": @"CoreDevice", @"Built-In": @YES };
        touchService = startService(touchDelegate, 0.5);
        sender = touchSender(client, wanted);
        if (!sender) sender = builtInDigitizerSender(client, wanted, ordinal);
        out([NSString stringWithFormat:@"wonder-simrig: registered command-scoped touchscreen on %@", display]);
    }
    if (!sender) {
        if (touchService) stopService(touchService, 0);
        out(@"wonder-simrig: no touchscreen for requested simulator display after registration");
        CFRelease(client); return 1;
    }
    FingerFn finger = (FingerFn)sym("IOHIDEventCreateDigitizerFingerEventWithQuality");
    SetIntFn setInt = (SetIntFn)sym("IOHIDEventSetIntegerValue");
    AppendFn append = (AppendFn)sym("IOHIDEventAppendEvent");
    int moves = duration > 0 ? (int)ceil(duration * 60) : 0;
    for (int step = 0; step <= moves + 1; step++) {
        BOOL down = step <= moves;
        double progress = moves ? fmin((double)step / moves, 1) : 0;
        double px = x + (endX - x) * progress, py = y + (endY - y) * progress;
        uint32_t mask = step > 0 && step <= moves ? 4 : 3; // position vs range/touch change
        uint64_t now = mach_absolute_time();
        // Match the external CoreDevice touchscreen stream, not in-process UIKit
        // touchrig: a finger collection at the same normalized point as its child.
        IOHIDEventRef event = finger(kCFAllocatorDefault, now, 0x400002, 1, mask, px/width, py/height,
                                    0, 0, 0, 5, 5, 1, 1, 1, down, down, 0x10);
        IOHIDEventRef child = finger(kCFAllocatorDefault, now, 1, 2, mask, px/width, py/height,
                                    0, 0, 0, 4.6, 3.8, 1.5, 1.5, 0, down, down, 0x10);
        if (!event || !child) { out(@"wonder-simrig: no digitizer event"); exit(1); }
        setInt(event, (11 << 16) + 25, 1); // IsDisplayIntegrated, as Sill touchrig.
        setInt(child, (11 << 16) + 25, 1);
        append(event, child, 0);
        CFRelease(child);
        ((void (*)(IOHIDEventRef, uint64_t))sym("IOHIDEventSetSenderID"))(event, sender);
        ((void (*)(void *, IOHIDEventRef))sym("IOHIDEventSystemClientDispatchEvent"))(client, event);
        CFRelease(event);
        spin(down && moves ? duration / moves : 0.08);
    }
    spin(0.5);
    if (touchService) stopService(touchService, 0);
    CFRelease(client);
    out([NSString stringWithFormat:@"wonder-simrig: %@ %.1f,%.1f to %.1f,%.1f in %.1fx%.1f on %@: submitted (assert resulting UI)",
         moves ? @"swipe" : @"tap", x, y, endX, endY, width, height, display]);
    return 0;
}

typedef CFDataRef (*SerializeFn)(CFTypeRef, uint32_t);
typedef IOHIDEventRef (*VendorFn)(CFAllocatorRef, uint64_t, uint32_t, uint32_t, uint32_t, const uint8_t *, CFIndex, uint32_t);
static int vendor(NSString *source, NSString *type, id value) {
    NSDictionary *dict = @{@"provider": @"com.apple.Virtualization.VirtualMachines",
                            @"source": source, @"type": type, @"value": value};
    CFDataRef data = ((SerializeFn)sym("IOCFSerialize"))((__bridge CFTypeRef)dict, 1);
    if (!data) { out(@"wonder-simrig: serialize failed"); return 1; }
    IOHIDEventRef event = ((VendorFn)sym("IOHIDEventCreateVendorDefinedEvent"))(
        kCFAllocatorDefault, mach_absolute_time(), 0xFF61, 0x5B, 0, CFDataGetBytePtr(data), CFDataGetLength(data), 0);
    CFRelease(data);
    if (!event) { out(@"wonder-simrig: no vendor event"); return 1; }
    __attribute__((objc_precise_lifetime)) RigDelegate *delegate = [RigDelegate new];
    delegate.properties = @{@"PrimaryUsagePage": @0xFF61, @"PrimaryUsage": @0x5B,
        @"DeviceUsagePairs": @[@{@"DeviceUsagePage": @0xFF61, @"DeviceUsage": @0x5B}],
        @"Product": @"Wonder simulator posture", @"Transport": @"Virtual",
        @"Built-In": @YES, @"HIDVirtualDevice": @YES};
    id service = startService(delegate, 0.4);
    BOOL ok = dispatchEvent(service, event);
    stopService(service, 0.5);
    out([NSString stringWithFormat:@"wonder-simrig: %@ %@: %@", source, value, ok ? @"dispatched" : @"NOT dispatched"]);
    return ok ? 0 : 1;
}

static BOOL number(const char *s, double *value) {
    char *end = NULL;
    *value = strtod(s, &end);
    return end != s && !*end && isfinite(*value);
}
static int usage(void) {
    out(@"usage: wonder-simrig tap X Y WIDTH HEIGHT DISPLAY_UUID [DISPLAY_ORDINAL] | swipe X Y END_X END_Y WIDTH HEIGHT SECONDS DISPLAY_UUID [DISPLAY_ORDINAL] | keys HEX[:down|:up],... | hinge 0..180 | orient portrait|pud|landscape-left|landscape-right|faceup|facedown");
    return 2;
}
int main(int argc, const char *argv[]) {
    @autoreleasepool {
        requireSimulator();
        if (argc < 3) return usage();
        NSString *cmd = @(argv[1]);
        if ([cmd isEqualToString:@"tap"] && (argc == 7 || argc == 8)) {
            double x,y,w,h;
            if (!number(argv[2], &x) || !number(argv[3], &y) || !number(argv[4], &w) || !number(argv[5], &h)
                || w <= 0 || h <= 0 || x < 0 || y < 0 || x >= w || y >= h) return usage();
            return touch(x,y,x,y,w,h,0,@(argv[6]), argc == 8 ? atoi(argv[7]) : 0);
        }
        if ([cmd isEqualToString:@"swipe"] && (argc == 10 || argc == 11)) {
            double x,y,endX,endY,w,h,duration;
            if (!number(argv[2], &x) || !number(argv[3], &y) || !number(argv[4], &endX) || !number(argv[5], &endY)
                || !number(argv[6], &w) || !number(argv[7], &h) || !number(argv[8], &duration)
                || w <= 0 || h <= 0 || duration <= 0 || duration > 10
                || x < 0 || y < 0 || x >= w || y >= h || endX < 0 || endY < 0 || endX >= w || endY >= h) return usage();
            return touch(x,y,endX,endY,w,h,duration,@(argv[9]), argc == 11 ? atoi(argv[10]) : 0);
        }
        if ([cmd isEqualToString:@"hinge"] && argc == 3) {
            double angle;
            if (!number(argv[2], &angle) || angle < 0 || angle > 180) return usage();
            return vendor(@"hinge-slider-control", @"range", @(angle));
        }
        if ([cmd isEqualToString:@"orient"] && argc == 3) {
            if (![@[@"portrait", @"pud", @"landscape-left", @"landscape-right", @"faceup", @"facedown"] containsObject:@(argv[2])]) return usage();
            return vendor(@"orientation-picker-control", @"enum", @(argv[2]));
        }
        if ([cmd isEqualToString:@"keys"] && argc == 3) {
            // Validate the whole sequence before emitting anything, and require paired modifiers.
            NSMutableSet *held = [NSMutableSet set];
            for (NSString *step in [@(argv[2]) componentsSeparatedByString:@","]) {
                NSArray *parts = [step componentsSeparatedByString:@":"];
                NSScanner *scan = [NSScanner scannerWithString:parts[0]];
                unsigned code;
                if (![scan scanHexInt:&code] || !scan.isAtEnd || !code || code > 0xE7 || parts.count > 2) return usage();
                NSNumber *key = @(code);
                if (parts.count == 2) {
                    if ([parts[1] isEqualToString:@"down"] && ![held containsObject:key]) [held addObject:key];
                    else if ([parts[1] isEqualToString:@"up"] && [held containsObject:key]) [held removeObject:key];
                    else return usage();
                } else if ([held containsObject:key]) return usage();
            }
            if (held.count) return usage();
            return keys(@(argv[2]), 0.6, 0.3);
        }
        return usage();
    }
}
