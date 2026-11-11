// Measurement tool (not built by SwiftPM). Attaches WonderSimulatorCapture to one
// booted simulator and logs each frame's size and brightness, so a Duo hinge
// change can be seen moving the stream between its inner and cover displays:
//   clang -fobjc-arc -I Sources/WonderSimulatorCapture/include -I Sources/WonderSimulatorCapture \
//     -framework Foundation -framework IOSurface -framework CoreVideo -framework CoreImage \
//     -framework CoreGraphics -framework QuartzCore Tests/Benchmarks/duo-posture-capture.m \
//     Sources/WonderSimulatorCapture/*.m -o /tmp/duo-capture
//   /tmp/duo-capture "$(xcode-select -p)" <UDID> <seconds> [<second> <out.png>]...
// then run `scripts/wonderctl --device duo hinge 0|180` while it runs.
#import "WonderSimulatorCapture.h"
#import <CoreVideo/CoreVideo.h>
#import <CoreImage/CoreImage.h>
static CVPixelBufferRef gLast;
int main(int c,char**v){@autoreleasepool{
 dispatch_queue_t q=dispatch_queue_create("q",0);
 WonderSimulatorCapture*cap=[WonderSimulatorCapture new];
 __block int n=0; __block size_t lw=0;
 NSError*e=nil;
 BOOL ok=[cap attach:[NSString stringWithUTF8String:v[2]] developer:[NSString stringWithUTF8String:v[1]] queue:q frame:^(CVPixelBufferRef p){
   CVPixelBufferLockBaseAddress(p,kCVPixelBufferLock_ReadOnly);size_t w=CVPixelBufferGetWidth(p),h=CVPixelBufferGetHeight(p);const uint8_t*b=CVPixelBufferGetBaseAddress(p);size_t bpr=CVPixelBufferGetBytesPerRow(p);unsigned long s=0,k=0;for(size_t y=0;y<h;y+=16)for(size_t x=0;x<w;x+=16){const uint8_t*q=b+y*bpr+x*4;s+=q[0]+q[1]+q[2];k+=3;}
   CVPixelBufferUnlockBaseAddress(p,kCVPixelBufferLock_ReadOnly);
   if(gLast)CVPixelBufferRelease(gLast); gLast=CVPixelBufferRetain(p);
   n++; {lw=w;NSLog(@"frame#%d size %zux%zu mean=%lu",n,w,h,s/k);} } removed:^{NSLog(@"REMOVED");} error:&e];
 NSLog(@"attach=%d",ok);
 for(int a=4;a<c;a++){}  cap.enabled=YES;
 int last=0; for(int i=0;i<atoi(v[3]);i++){[NSThread sleepForTimeInterval:1];NSLog(@"t=%d frames=%d (+%d)",i,n,n-last); for(int a=4;a+1<c;a+=2){ if(atoi(v[a])==i&&gLast){CIContext*cx=[CIContext new];[cx writePNGRepresentationOfImage:[CIImage imageWithCVPixelBuffer:gLast] toURL:[NSURL fileURLWithPath:[NSString stringWithUTF8String:v[a+1]]] format:kCIFormatBGRA8 colorSpace:CGColorSpaceCreateWithName(kCGColorSpaceSRGB) options:@{} error:nil];}}last=n;}
}}
