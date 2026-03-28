package com.claudulhu;

import com.facebook.proguard.annotations.DoNotStrip;
import com.facebook.react.bridge.Promise;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.bridge.ReactContextBaseJavaModule;
import com.facebook.react.bridge.ReactMethod;
import com.facebook.react.turbomodule.core.interfaces.TurboModule;
import javax.annotation.Nonnull;

/**
 * Codegen spec for the NoiseConnection TurboModule.
 * Generated from NativeNoiseConnection.ts — do not edit manually.
 */
public abstract class NativeNoiseConnectionSpec extends ReactContextBaseJavaModule implements TurboModule {
    public static final String NAME = "NoiseConnection";

    public NativeNoiseConnectionSpec(ReactApplicationContext reactContext) {
        super(reactContext);
    }

    @Override
    public @Nonnull String getName() {
        return NAME;
    }

    @ReactMethod
    @DoNotStrip
    public abstract void connect(String host, double port, String serverPubKey, Promise promise);

    @ReactMethod
    @DoNotStrip
    public abstract void disconnect();
}
