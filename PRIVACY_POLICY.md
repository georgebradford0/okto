---
layout: default
title: Privacy Policy
permalink: /privacy/
---

# Privacy Policy for Okto

**Last updated: May 31, 2026**

Okto ("the app", "we", "us") is a mobile client for connecting to your own
self-hosted Okto "lair" server. This policy explains what data the app handles.

The guiding principle is simple: **Okto is a thin client that talks only to a
server you control. We do not operate any servers that receive your data, and the
app contains no analytics, advertising, or tracking of any kind.**

## Information We Do Not Collect

- We do **not** collect your name, email address, phone number, or any account
  credentials. The app has no sign-up or login to any service operated by us.
- We do **not** use any third-party analytics, advertising, tracking, or
  crash-reporting SDKs (no Google Analytics, Firebase, Facebook SDK, Sentry,
  Crashlytics, or similar).
- We do **not** sell, rent, or share your data with anyone. There is no central
  Okto service that your data flows through.

## Information Stored On Your Device

The app stores the following data **locally on your device only**. None of it is
transmitted to us:

- **Server connection details** — the host, port, and public key of the
  self-hosted server you connect to (obtained by scanning a QR code or entering
  the details manually), plus an optional label.
- **Message drafts** — text you have typed but not yet sent, so it is preserved
  if you close the app.
- **Chat history** — a local cache of your recent conversations with your server
  (message text, roles, and tool output), retained for offline viewing.

You can delete all of this data at any time by logging out within the app, which
clears the locally stored connection details, drafts, and chat history.

## Information You Send To Your Own Server

When you use the app to chat, the messages you type are sent to **your own
self-hosted server**, over an end-to-end encrypted connection
(Noise_XX_25519_ChaChaPoly_SHA256). This data goes only to the server you
configured — not to us or to any third party. How that server stores or processes
your messages is governed by you as its operator.

## Camera

The app requests access to your device camera **solely to scan QR codes** used to
configure a server connection. Camera images are processed on-device in real time
and are **never recorded, stored, or transmitted**. Granting camera access is
optional — you can instead enter connection details manually.

## Push Notifications (iOS)

On iOS, the app can register for push notifications so you can be alerted about
activity on your server. To do this, the app sends your device's Apple Push
Notification service (APNs) token to a **relay server specified by your own lair
server** (a server you or your operator control). The token is used only to
deliver notifications to your device and is not shared with us or with unrelated
third parties. Apple processes push delivery in accordance with Apple's own
privacy policy. Push notifications are optional and can be disabled in your
device settings.

## Permissions Summary

| Permission | Why it's used |
|------------|---------------|
| Camera | Scan QR codes to set up a server connection (optional) |
| Internet / Network | Connect to your self-hosted server over an encrypted tunnel |
| Notifications (iOS) | Deliver alerts about activity on your server (optional) |

## Children's Privacy

Okto is not directed to children under 13, and we do not knowingly collect any
personal information from children.

## Data Security

All communication between the app and your server is end-to-end encrypted using
the Noise protocol. Data cached on your device is protected by your device's
standard operating-system protections.

## Changes To This Policy

We may update this policy from time to time. Changes will be posted on this page
with an updated "Last updated" date.

## Contact

If you have questions about this privacy policy, contact:

**georgebradford0@proton.me**
