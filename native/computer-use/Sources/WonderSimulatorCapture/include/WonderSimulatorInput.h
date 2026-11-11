#import <Foundation/Foundation.h>
NS_ASSUME_NONNULL_BEGIN

typedef NS_ENUM(NSInteger, WonderSimulatorTouchPhase) {
    WonderSimulatorTouchPhaseBegan = 0,
    WonderSimulatorTouchPhaseMoved = 1,
    WonderSimulatorTouchPhaseEnded = 2,
};

typedef NS_ENUM(NSInteger, WonderSimulatorButton) {
    WonderSimulatorButtonHome = 0,
    WonderSimulatorButtonLock = 1,
    WonderSimulatorButtonVolumeUp = 2,
    WonderSimulatorButtonVolumeDown = 3,
};

/// Optional HID input for one booted simulator, through Apple's private
/// SimulatorKit/CoreSimulator runtime. Every entry point is checked before it
/// is used; any missing piece makes the injector unavailable, never a crash.
/// Not thread safe: the caller confines one instance to one serial queue.
@interface WonderSimulatorInput : NSObject
/// Nil with a short code in `reason` when this device cannot take input.
/// May wait up to a few seconds for the simulator's input service; never call
/// it on the main thread.
+ (nullable instancetype)connect:(NSString *)udid developer:(NSString *)developer
                          reason:(NSString *_Nullable *_Nullable)reason;
/// Measurement only: force `dtuhid` or `indigo`; nil picks by CoreSimulator version.
+ (nullable instancetype)connect:(NSString *)udid developer:(NSString *)developer
                       transport:(nullable NSString *)transport
                          reason:(NSString *_Nullable *_Nullable)reason;
/// `dtuhid` (Xcode 27 and later) or `indigo` (the older SimulatorKit client).
@property(nonatomic, readonly) NSString *transport;
@property(nonatomic, readonly) NSString *udid;
/// The exact device still exists and is booted, and the connection is intact.
- (BOOL)isUsable;
/// Normalized 0...1 point in the device's native (unrotated) display.
- (BOOL)touchX:(double)x y:(double)y phase:(WonderSimulatorTouchPhase)phase;
- (BOOL)button:(WonderSimulatorButton)button down:(BOOL)down;
/// A USB HID keyboard usage (page 0x07).
- (BOOL)keyboardUsage:(uint32_t)usage down:(BOOL)down;
- (void)close;
@end
NS_ASSUME_NONNULL_END
