// @okto/ui — shared Tamagui design system for okto mobile + desktop.
// (Replaces the gluestack-ui v3 + NativeWind component set.)
//
// Apps import primitives (Stack/Text/Button/…) straight from here, plus the
// shared theme config and the OktoProvider that applies it.

export * from 'tamagui'
export { tamaguiConfig, default as config } from './tamagui.config'
export type { OktoTamaguiConfig } from './tamagui.config'
export { OktoProvider } from './provider'
export { Badge, BadgeText, ButtonText, Touchable } from './components'
