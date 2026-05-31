// Small components that fill gaps in Tamagui core for the okto app surface.
import { Button, styled, Text, View, XStack } from 'tamagui'

// Pressable surface replacing RN TouchableOpacity, so it accepts Tamagui style
// props (the app's converted className props) while keeping onPress + press feedback.
export const Touchable = styled(View, {
  name: 'Touchable',
  pressStyle: { opacity: 0.6 },
  cursor: 'pointer',
})

// Pill badge (Tamagui core has no Badge). Themed via the okto outline/background tokens.
export const Badge = styled(XStack, {
  name: 'Badge',
  alignItems: 'center',
  alignSelf: 'flex-start',
  gap: 6,
  borderRadius: 999,
  borderWidth: 1,
  borderColor: '$outline200',
  backgroundColor: '$background0',
  paddingHorizontal: 10,
  paddingVertical: 4,
})

export const BadgeText = styled(Text, {
  name: 'BadgeText',
  fontFamily: '$body',
  fontSize: 12,
  color: '$typography600',
})

// gluestack used <Button><ButtonText>…; Tamagui exposes Button.Text.
export const ButtonText = Button.Text
