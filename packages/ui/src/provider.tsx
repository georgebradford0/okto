// OktoProvider — wraps the app in Tamagui with the shared okto config.
// Replaces the old GluestackUIProvider. `mode` selects the light/dark theme.
import React from 'react'
import { TamaguiProvider, Theme } from 'tamagui'
import { tamaguiConfig } from './tamagui.config'

export function OktoProvider({
  mode = 'light',
  children,
}: {
  mode?: 'light' | 'dark'
  children: React.ReactNode
}) {
  return (
    <TamaguiProvider config={tamaguiConfig} defaultTheme={mode}>
      <Theme name={mode}>{children}</Theme>
    </TamaguiProvider>
  )
}
