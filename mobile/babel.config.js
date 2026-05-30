module.exports = {
  presets: [
    'module:@react-native/babel-preset',
    // NativeWind v4 — adds the className jsx transform + jsxImportSource so gluestack-ui
    // components (in @okto/ui) get their Tailwind styles applied on native.
    'nativewind/babel',
  ],
  // The reanimated/worklets plugin must stay LAST.
  plugins: ['react-native-reanimated/plugin'],
};
