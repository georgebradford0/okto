/* eslint-disable react/display-name */
// Lightweight stand-in for @okto/ui (Tamagui). The real package is a full
// design-system that's painful to boot under jest and irrelevant to behavioural
// assertions: these tests care about *what the app does*, not how Tamagui lays
// it out. We render plain RN primitives, forwarding only the props that affect
// behaviour or querying (children, onPress, style, testID, accessibility, …) and
// dropping the dozens of Tamagui style props (paddingHorizontal=…, color="$…")
// so React doesn't warn about unknown DOM/host props.

import React from 'react'
import {
  View as RNView,
  Text as RNText,
  Pressable,
  ActivityIndicator,
} from 'react-native'

// Host-component props worth forwarding; everything else (Tamagui style sugar)
// is intentionally dropped.
const KEEP = new Set([
  'children',
  'style',
  'testID',
  'nativeID',
  'onPress',
  'onLongPress',
  'onPressIn',
  'onPressOut',
  'onLayout',
  'onChangeText',
  'onSubmitEditing',
  'onBlur',
  'onFocus',
  'pointerEvents',
  'numberOfLines',
  'ellipsizeMode',
  'disabled',
  'hitSlop',
  'delayLongPress',
  'accessible',
  'accessibilityRole',
  'accessibilityLabel',
  'accessibilityState',
  'accessibilityHint',
  'selectable',
  'source',
  'value',
  'placeholder',
])

function pick(props: Record<string, any>) {
  const out: Record<string, any> = {}
  for (const k of Object.keys(props)) if (KEEP.has(k)) out[k] = props[k]
  return out
}

export const View = React.forwardRef<any, any>((props, ref) => (
  <RNView ref={ref} {...pick(props)} />
))

export const Text = React.forwardRef<any, any>((props, ref) => (
  <RNText ref={ref} {...pick(props)} />
))

// Touchable / Button are pressable surfaces. Pressable's onPress lets
// fireEvent.press find a handler whether the test targets the surface or a
// nested <Text> (fireEvent climbs the tree to the handler).
export const Touchable = React.forwardRef<any, any>((props, ref) => (
  <Pressable ref={ref} {...pick(props)} />
))

export const Button = React.forwardRef<any, any>((props, ref) => (
  <Pressable ref={ref} {...pick(props)} />
))

export const ButtonText = React.forwardRef<any, any>((props, ref) => (
  <RNText ref={ref} {...pick(props)} />
))

export const Spinner = (props: any) => <ActivityIndicator {...pick(props)} />

export const Badge = View
export const BadgeText = Text

export const OktoProvider = ({ children }: { children: React.ReactNode }) => (
  <>{children}</>
)
