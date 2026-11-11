// Measurement tool (not built by SwiftPM): attaches capture (so the input follows
// the lit Duo display) then taps one normalized native point:
//   clang -fobjc-arc -I Sources/WonderSimulatorCapture/include -I Sources/WonderSimulatorCapture \
//     -framework Foundation -framework IOSurface -framework CoreVideo -framework CoreImage \
//     -framework CoreGraphics -framework QuartzCore Tests/Benchmarks/duo-touch-target.m \
//     Sources/WonderSimulatorCapture/*.m -o /tmp/duo-touch
//   /tmp/duo-touch "$(xcode-select -p)" <UDID> X Y
#import "WonderSimulatorCapture.h"
int main(int c,char**v){@autoreleasepool{
 dispatch_queue_t q=dispatch_queue_create("q",0);
 WonderSimulatorCapture*cap=[WonderSimulatorCapture new]; NSError*e=nil;
 BOOL ok=[cap attach:@(v[2]) developer:@(v[1]) queue:q frame:^(CVPixelBufferRef p){} removed:^{} error:&e]; cap.enabled=YES;
 [NSThread sleepForTimeInterval:2];
 NSLog(@"attach=%d inner=%d",ok,[WonderSimulatorCapture innerDisplayActiveForUDID:@(v[2])]);
 NSString*r=nil; WonderSimulatorInput*i=[WonderSimulatorInput connect:@(v[2]) developer:@(v[1]) reason:&r]; if(!i){NSLog(@"no input %@",r);return 1;}
 double x=atof(v[3]),y=atof(v[4]);
 [i touchX:x y:y phase:WonderSimulatorTouchPhaseBegan]; [NSThread sleepForTimeInterval:0.08];
 [i touchX:x y:y phase:WonderSimulatorTouchPhaseEnded]; [NSThread sleepForTimeInterval:0.5];
 [cap detach];
}}
