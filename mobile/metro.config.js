const path = require('path');
const { getDefaultConfig, mergeConfig } = require('@react-native/metro-config');

const projectRoot = __dirname;
// npm workspace root (one level up): hoists @okto/ui + shared deps into <root>/node_modules.
const workspaceRoot = path.resolve(projectRoot, '..');

/**
 * Metro configuration
 * https://reactnative.dev/docs/metro
 *
 * @type {import('@react-native/metro-config').MetroConfig}
 */
const config = {
  // Watch the whole workspace so the linked @okto/ui source is bundled + HMR'd.
  watchFolders: [workspaceRoot],
  resolver: {
    // Resolve from the app first, then the hoisted workspace root node_modules.
    nodeModulesPaths: [
      path.resolve(projectRoot, 'node_modules'),
      path.resolve(workspaceRoot, 'node_modules'),
    ],
  },
};

module.exports = mergeConfig(getDefaultConfig(projectRoot), config);
