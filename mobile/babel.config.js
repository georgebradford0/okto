module.exports = {
  presets: ['module:@react-native/babel-preset'],
  // The reanimated/worklets plugin must stay LAST.
  plugins: [
    ['transform-inline-environment-variables', { include: ['TAMAGUI_TARGET'] }],
    // The reanimated/worklets plugin must stay LAST.
    'react-native-reanimated/plugin',
  ],
};
