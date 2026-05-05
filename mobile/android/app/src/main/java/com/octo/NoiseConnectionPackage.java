package com.octo;

import com.facebook.react.TurboReactPackage;
import com.facebook.react.bridge.NativeModule;
import com.facebook.react.bridge.ReactApplicationContext;
import com.facebook.react.module.model.ReactModuleInfo;
import com.facebook.react.module.model.ReactModuleInfoProvider;

import java.util.HashMap;
import java.util.Map;

import androidx.annotation.NonNull;
import androidx.annotation.Nullable;

public class NoiseConnectionPackage extends TurboReactPackage {

    @Nullable
    @Override
    public NativeModule getModule(
            @NonNull String name,
            @NonNull ReactApplicationContext reactContext) {
        if (NoiseConnectionModule.NAME.equals(name)) {
            return new NoiseConnectionModule(reactContext);
        }
        return null;
    }

    @Override
    public ReactModuleInfoProvider getReactModuleInfoProvider() {
        return () -> {
            Map<String, ReactModuleInfo> map = new HashMap<>();
            map.put(
                NoiseConnectionModule.NAME,
                new ReactModuleInfo(
                    NoiseConnectionModule.NAME,
                    NoiseConnectionModule.NAME,
                    false,  // canOverrideExistingModule
                    false,  // needsEagerInit
                    false,  // isCxxModule
                    true    // isTurboModule
                )
            );
            return map;
        };
    }
}
