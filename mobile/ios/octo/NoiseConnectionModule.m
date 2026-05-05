#import <React/RCTBridgeModule.h>

// Registers the Swift class `NoiseConnection` as a React Native bridge module.
// Compatible with both the classic bridge and the New Architecture interop layer.

@interface RCT_EXTERN_MODULE(NoiseConnection, NSObject)

RCT_EXTERN_METHOD(connect:(NSString *)host
                  port:(double)port
                  serverPubKey:(NSString *)serverPubKey
                  resolve:(RCTPromiseResolveBlock)resolve
                  reject:(RCTPromiseRejectBlock)reject)

RCT_EXTERN_METHOD(disconnect)

+ (BOOL)requiresMainQueueSetup { return NO; }

@end
