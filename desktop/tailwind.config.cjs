/** @type {import('tailwindcss').Config} */
module.exports = {
  // Theme (colors, fonts, shadows, safelist, nativewind preset) comes from @okto/ui.
  presets: [require('@okto/ui/tailwind-preset')],
  // Scan this app's source AND the shared component package so their classes are emitted.
  content: [
    './index.html',
    './src/**/*.{ts,tsx,html}',
    '../packages/ui/src/**/*.{ts,tsx}',
  ],
};
