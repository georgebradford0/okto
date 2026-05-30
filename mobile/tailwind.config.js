/** @type {import('tailwindcss').Config} */
module.exports = {
  // Theme (colors, shadows, nativewind preset) comes from @okto/ui.
  presets: [require('@okto/ui/tailwind-preset')],
  // Scan this app's source AND the shared component package so their classes are emitted.
  content: [
    './App.tsx',
    './src/**/*.{ts,tsx}',
    '../packages/ui/src/**/*.{ts,tsx}',
  ],
  theme: {
    extend: {
      // The app's bundled brand fonts (assets/fonts). `font-brand` → headings,
      // `font-sans` → body. Monospace stays an inline style (it's platform-
      // dependent: Menlo on iOS, monospace on Android).
      fontFamily: {
        brand: ['Nunito'],
        sans: ['Arimo'],
      },
    },
  },
};
