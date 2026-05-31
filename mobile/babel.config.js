module.exports = function (api) {
  // Re-evaluate (and re-cache) when NODE_ENV changes so the test build can drop
  // the reanimated worklet transform — under jest the worklet runtime is mocked,
  // so transforming closures into worklets would reference helpers that don't exist.
  api.cache.using(() => process.env.NODE_ENV);
  const isTest = api.env('test');

  return {
    presets: ['module:@react-native/babel-preset'],
    // The reanimated/worklets plugin must stay LAST.
    plugins: [
      ['transform-inline-environment-variables', { include: ['TAMAGUI_TARGET'] }],
      // The reanimated/worklets plugin must stay LAST — and is omitted under test,
      // where react-native-reanimated is mocked with plain components.
      ...(isTest ? [] : ['react-native-reanimated/plugin']),
    ],
  };
};
