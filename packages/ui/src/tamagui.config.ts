// @okto/ui — shared Tamagui configuration (replaces the gluestack-ui v3 + NativeWind stack).
//
// The okto palette is scale-based (primary/typography/background/… each 0–950) and INVERTS
// between light and dark, so every step is a *theme* value (not a flat token). We keep the
// exact RGB triplets from the previous design system and expose them as Tamagui theme keys
// like `$primary600`, `$typography500`, `$background50`, `$backgroundError`, etc. — so
// component code reads `backgroundColor="$primary600"` in place of the old `bg-primary-600`.

import { createFont, createTamagui, createTokens } from 'tamagui'
import { defaultConfig } from '@tamagui/config/v4'

type Scale = Record<string, string>
type Palette = Record<string, Scale>

// Space-separated "R G B" triplets, copied verbatim from the prior token set.
const light: Palette = {
  primary: { 0: '240 253 250', 50: '204 251 241', 100: '153 246 228', 200: '94 234 212', 300: '45 212 191', 400: '20 184 166', 500: '13 148 136', 600: '15 118 110', 700: '17 94 89', 800: '19 78 74', 900: '6 56 54', 950: '3 36 35' },
  secondary: { 0: '253 253 253', 50: '251 251 251', 100: '246 246 246', 200: '242 242 242', 300: '237 237 237', 400: '230 230 231', 500: '217 217 219', 600: '198 199 199', 700: '189 189 189', 800: '177 177 177', 900: '165 164 164', 950: '157 157 157' },
  tertiary: { 0: '255 250 245', 50: '255 242 229', 100: '255 233 213', 200: '254 209 170', 300: '253 180 116', 400: '251 157 75', 500: '231 129 40', 600: '215 117 31', 700: '180 98 26', 800: '130 73 23', 900: '108 61 19', 950: '84 49 18' },
  error: { 0: '254 233 233', 50: '254 226 226', 100: '254 202 202', 200: '252 165 165', 300: '248 113 113', 400: '239 68 68', 500: '230 53 53', 600: '220 38 38', 700: '185 28 28', 800: '153 27 27', 900: '127 29 29', 950: '83 19 19' },
  success: { 0: '228 255 244', 50: '202 255 232', 100: '162 241 192', 200: '132 211 162', 300: '102 181 132', 400: '72 151 102', 500: '52 131 82', 600: '42 121 72', 700: '32 111 62', 800: '22 101 52', 900: '20 83 45', 950: '27 50 36' },
  warning: { 0: '255 249 245', 50: '255 244 236', 100: '255 231 213', 200: '254 205 170', 300: '253 173 116', 400: '251 149 75', 500: '231 120 40', 600: '215 108 31', 700: '180 90 26', 800: '130 68 23', 900: '108 56 19', 950: '84 45 18' },
  info: { 0: '236 248 254', 50: '199 235 252', 100: '162 221 250', 200: '124 207 248', 300: '87 194 246', 400: '50 180 244', 500: '13 166 242', 600: '11 141 205', 700: '9 115 168', 800: '7 90 131', 900: '5 64 93', 950: '3 38 56' },
  typography: { 0: '252 253 254', 50: '248 250 252', 100: '241 245 249', 200: '226 232 240', 300: '203 213 225', 400: '148 163 184', 500: '100 116 139', 600: '71 85 105', 700: '51 65 85', 800: '30 41 59', 900: '15 23 42', 950: '2 6 23' },
  outline: { 0: '253 254 254', 50: '243 243 243', 100: '230 230 230', 200: '221 220 219', 300: '211 211 211', 400: '165 163 163', 500: '140 141 141', 600: '115 116 116', 700: '83 82 82', 800: '65 65 65', 900: '39 38 36', 950: '26 23 23' },
  background: { 0: '255 255 255', 50: '246 246 246', 100: '242 241 241', 200: '220 219 219', 300: '213 212 212', 400: '162 163 163', 500: '142 142 142', 600: '116 116 116', 700: '83 82 82', 800: '65 64 64', 900: '39 38 37', 950: '18 18 18' },
}
const lightSpecial: Scale = { backgroundError: '254 241 241', backgroundWarning: '255 243 234', backgroundSuccess: '237 252 242', backgroundMuted: '247 248 247', backgroundInfo: '235 248 254', indicatorPrimary: '13 148 136', indicatorInfo: '83 153 236', indicatorError: '185 28 28' }

const dark: Palette = {
  primary: { 0: '4 47 46', 50: '6 78 74', 100: '17 94 89', 200: '15 118 110', 300: '13 148 136', 400: '20 184 166', 500: '45 212 191', 600: '94 234 212', 700: '153 246 228', 800: '204 251 241', 900: '224 252 245', 950: '240 253 250' },
  secondary: { 0: '20 20 20', 50: '23 23 23', 100: '31 31 31', 200: '39 39 39', 300: '44 44 44', 400: '56 57 57', 500: '63 64 64', 600: '86 86 86', 700: '110 110 110', 800: '135 135 135', 900: '150 150 150', 950: '164 164 164' },
  tertiary: { 0: '84 49 18', 50: '108 61 19', 100: '130 73 23', 200: '180 98 26', 300: '215 117 31', 400: '231 129 40', 500: '251 157 75', 600: '253 180 116', 700: '254 209 170', 800: '255 233 213', 900: '255 242 229', 950: '255 250 245' },
  error: { 0: '83 19 19', 50: '127 29 29', 100: '153 27 27', 200: '185 28 28', 300: '220 38 38', 400: '230 53 53', 500: '239 68 68', 600: '249 97 96', 700: '229 91 90', 800: '254 202 202', 900: '254 226 226', 950: '254 233 233' },
  success: { 0: '27 50 36', 50: '20 83 45', 100: '22 101 52', 200: '32 111 62', 300: '42 121 72', 400: '52 131 82', 500: '72 151 102', 600: '102 181 132', 700: '132 211 162', 800: '162 241 192', 900: '202 255 232', 950: '228 255 244' },
  warning: { 0: '84 45 18', 50: '108 56 19', 100: '130 68 23', 200: '180 90 26', 300: '215 108 31', 400: '231 120 40', 500: '251 149 75', 600: '253 173 116', 700: '254 205 170', 800: '255 231 213', 900: '255 244 237', 950: '255 249 245' },
  info: { 0: '3 38 56', 50: '5 64 93', 100: '7 90 131', 200: '9 115 168', 300: '11 141 205', 400: '13 166 242', 500: '50 180 244', 600: '87 194 246', 700: '124 207 248', 800: '162 221 250', 900: '199 235 252', 950: '236 248 254' },
  typography: { 0: '2 6 23', 50: '15 23 42', 100: '30 41 59', 200: '51 65 85', 300: '71 85 105', 400: '100 116 139', 500: '148 163 184', 600: '203 213 225', 700: '226 232 240', 800: '241 245 249', 900: '248 250 252', 950: '252 253 254' },
  outline: { 0: '26 23 23', 50: '39 38 36', 100: '65 65 65', 200: '83 82 82', 300: '115 116 116', 400: '140 141 141', 500: '165 163 163', 600: '211 211 211', 700: '221 220 219', 800: '230 230 230', 900: '243 243 243', 950: '253 254 254' },
  background: { 0: '18 18 18', 50: '39 38 37', 100: '65 64 64', 200: '83 82 82', 300: '116 116 116', 400: '142 142 142', 500: '162 163 163', 600: '213 212 212', 700: '229 228 228', 800: '242 241 241', 900: '246 246 246', 950: '255 255 255' },
}
const darkSpecial: Scale = { backgroundError: '66 43 43', backgroundWarning: '65 47 35', backgroundSuccess: '28 43 33', backgroundMuted: '51 51 51', backgroundInfo: '26 40 46', indicatorPrimary: '45 212 191', indicatorInfo: '161 199 245', indicatorError: '232 70 69' }

const rgb = (triplet: string) => {
  const [r, g, b] = triplet.split(' ')
  return `rgb(${r}, ${g}, ${b})`
}

// Flatten a palette + special map into `{ primary0: 'rgb(...)', backgroundError: 'rgb(...)', … }`.
function buildTheme(palette: Palette, special: Scale): Record<string, string> {
  const out: Record<string, string> = {}
  for (const [name, scale] of Object.entries(palette)) {
    for (const [step, triplet] of Object.entries(scale)) out[`${name}${step}`] = rgb(triplet)
  }
  for (const [key, triplet] of Object.entries(special)) out[key] = rgb(triplet)
  // Tamagui core expects a handful of base theme keys; map them onto the okto ramp so
  // unstyled primitives still read correctly.
  out.background = rgb(palette.background[0])
  out.backgroundHover = rgb(palette.background[50])
  out.backgroundPress = rgb(palette.background[100])
  out.backgroundFocus = rgb(palette.background[100])
  out.color = rgb(palette.typography[900])
  out.colorHover = rgb(palette.typography[800])
  out.colorPress = rgb(palette.typography[950])
  out.colorFocus = rgb(palette.typography[900])
  out.borderColor = rgb(palette.outline[200])
  out.borderColorHover = rgb(palette.outline[300])
  out.borderColorFocus = rgb(palette.primary[600])
  out.placeholderColor = rgb(palette.typography[400])
  return out
}

const lightTheme = buildTheme(light, lightSpecial)
const darkTheme = buildTheme(dark, darkSpecial)

// Fonts: brand=Nunito (headings), body=Arimo, mono=platform monospace.
// On web the brand fonts aren't loaded (no @font-face), so without a fallback the browser
// drops to Times serif. Append a system sans stack on web; mobile keeps its bundled families.
declare const process: { env: Record<string, string | undefined> }
const IS_WEB = process.env.TAMAGUI_TARGET === 'web'
const sans = (f: string) =>
  IS_WEB ? `${f}, -apple-system, system-ui, "Segoe UI", Roboto, sans-serif` : f

const headingFont = createFont({
  family: sans('Nunito'),
  size: { 1: 11, 2: 12, 3: 13, 4: 14, 5: 16, 6: 18, 7: 20, 8: 24, 9: 30, 10: 36, true: 16 },
  lineHeight: { 1: 16, 2: 18, 3: 19, 4: 20, 5: 23, 6: 26, 7: 28, 8: 32, 9: 38, 10: 44, true: 23 },
  weight: { 4: '400', 6: '600', 7: '700', 8: '800', true: '700' },
})
const bodyFont = createFont({
  family: sans('Arimo'),
  size: { 1: 11, 2: 12, 3: 13, 4: 14, 5: 15.5, 6: 18, 7: 20, 8: 24, 9: 30, 10: 36, true: 15.5 },
  lineHeight: { 1: 16, 2: 18, 3: 19, 4: 20, 5: 23, 6: 26, 7: 28, 8: 32, 9: 38, 10: 44, true: 23 },
  weight: { 4: '400', 5: '500', 6: '600', 7: '700', true: '400' },
})
const monoFont = createFont({
  family: 'Menlo, ui-monospace, monospace',
  size: { 1: 10, 2: 11, 3: 12, 4: 12.5, 5: 13, 6: 14, true: 12.5 },
  lineHeight: { 1: 15, 2: 16, 3: 18, 4: 19, 5: 20, 6: 22, true: 19 },
  weight: { 4: '400', 6: '600', 7: '700', true: '400' },
})

const tokens = createTokens({
  ...defaultConfig.tokens,
  color: {
    // Light values as theme-independent token fallbacks (themes override at runtime).
    ...Object.fromEntries(Object.entries(lightTheme).map(([k, v]) => [k, v])),
  },
})

export const tamaguiConfig = createTamagui({
  ...defaultConfig,
  tokens,
  fonts: {
    ...defaultConfig.fonts,
    heading: headingFont,
    body: bodyFont,
    mono: monoFont,
  },
  themes: {
    ...defaultConfig.themes,
    light: { ...defaultConfig.themes.light, ...lightTheme },
    dark: { ...defaultConfig.themes.dark, ...darkTheme },
  },
  defaultTheme: 'light',
})

export type OktoTamaguiConfig = typeof tamaguiConfig
export default tamaguiConfig
