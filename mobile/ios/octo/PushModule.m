#import <React/RCTBridgeModule.h>

// Registers the Swift class `Push` as a React Native bridge module.

@interface RCT_EXTERN_MODULE(Push, NSObject)

RCT_EXTERN_METHOD(requestPermissionAndRegister:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject)

RCT_EXTERN_METHOD(getAuthorizationStatus:(RCTPromiseResolveBlock)resolve
                  rejecter:(RCTPromiseRejectBlock)reject)

+ (BOOL)requiresMainQueueSetup { return NO; }

@end
