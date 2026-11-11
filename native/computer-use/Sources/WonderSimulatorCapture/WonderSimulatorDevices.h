#import <Foundation/Foundation.h>

// Shared by capture and input: CoreSimulator's device list for the selected
// Xcode, or nil when the private runtime is missing or changed.
NSArray *_Nullable WonderSimulatorAvailableDevices(NSString *_Nonnull developer, NSError *_Nullable *_Nullable error);
