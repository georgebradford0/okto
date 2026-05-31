/* eslint-disable react/display-name */
// Lightweight stand-in for @okto/ui (Tamagui / react-native-web). The real
// package is a full design-system that's painful to boot under jsdom and
// irrelevant to behavioural assertions: these tests care about *what the app
// does*, not how Tamagui lays it out. We render plain DOM elements, forwarding
// only the props that affect behaviour or querying (children, style, testid,
// onPress→onClick, value/placeholder, disabled, accessibility) and dropping the
// dozens of Tamagui style sugar props (paddingHorizontal=…, color="$…",
// fontSize=…) so React doesn't warn about unknown DOM attributes.
//
// Web counterpart of mobile/__tests__/helpers/oktoUiMock.tsx.

import React from 'react'

// Plain host attributes worth forwarding; everything else (Tamagui style sugar)
// is intentionally dropped. `data-*` and `aria-*` are passed through wholesale.
const PASS = new Set([
  'children',
  'style',
  'id',
  'title',
  'value',
  'placeholder',
  'role',
  'tabIndex',
  'htmlFor',
  'numberOfLines',
])

function domProps(props: Record<string, any>, pressable = false) {
  const out: Record<string, any> = {}
  for (const k of Object.keys(props)) {
    if (PASS.has(k)) out[k] = props[k]
    else if (k.startsWith('data-') || k.startsWith('aria-')) out[k] = props[k]
  }
  // `numberOfLines` is an RN prop, not a DOM attribute — keep it off the host.
  delete out.numberOfLines
  if (pressable) {
    out.role = props.role ?? 'button'
    out.tabIndex = props.tabIndex ?? 0
    if (props.disabled) {
      out['aria-disabled'] = true
    } else if (props.onPress) {
      out.onClick = (e: any) => props.onPress(e)
    }
  }
  return out
}

export const View = React.forwardRef<any, any>((props, ref) => (
  <div ref={ref} {...domProps(props)} />
))

export const Text = React.forwardRef<any, any>((props, ref) => (
  <span ref={ref} {...domProps(props)} />
))

// Touchable / Button are pressable surfaces. Rendered as a role="button" div
// (not a real <button>) so the app's nested pressables — a row Touchable that
// contains add/delete Touchables — produce valid, click-bubbling DOM where an
// inner `onPress` calling `e.stopPropagation()` correctly shields the outer one.
export const Touchable = React.forwardRef<any, any>((props, ref) => (
  <div ref={ref} {...domProps(props, true)} />
))

export const Button = Touchable
export const ButtonText = Text

export const Spinner = (props: any) => <div role="progressbar" {...domProps(props)} />

export const Badge = View
export const BadgeText = Text

export const OktoProvider = ({ children }: { children: React.ReactNode }) => (
  <>{children}</>
)
