#import <Foundation/Foundation.h>
#import <CoreVideo/CoreVideo.h>
#import "WonderSimulatorInput.h"
NS_ASSUME_NONNULL_BEGIN

/// Optional CoreSimulator access. No Simulator.app window, input or boot actions.
@interface WonderSimulatorCapture : NSObject
+ (nullable NSArray<NSDictionary<NSString *, NSString *> *> *)bootedDevices:(NSString *)developer error:(NSError **)error;
- (BOOL)attach:(NSString *)udid developer:(NSString *)developer
         queue:(dispatch_queue_t)queue
         frame:(void (^)(CVPixelBufferRef))frame
       removed:(void (^)(void))removed error:(NSError **)error;
@property(nonatomic) BOOL enabled;
/// Duo only: whether the attached capture for `udid` currently streams the inner
/// display (unfolded) rather than the cover (folded). NO when none is attached.
+ (BOOL)innerDisplayActiveForUDID:(NSString *)udid;
- (void)detach;
@end
NS_ASSUME_NONNULL_END
