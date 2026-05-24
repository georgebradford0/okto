# Android Build

Deploys the Android app to Google Play via GitHub Actions + Fastlane.

## Trigger

Manually via `workflow_dispatch`. Push your branch first, then run:

```sh
# Internal testing (closed track)
gh workflow run android.yml -f track=closed

# Production
gh workflow run android.yml -f track=production
```

> **Important:** always `git push` before running `gh workflow run`, otherwise the workflow runs stale code.

## Required GitHub Secrets

| Secret | How to set |
|---|---|
| `ANDROID_KEYSTORE` | `base64 -i okto-upload-key.keystore \| pbcopy` |
| `ANDROID_KEYSTORE_PASSWORD` | keystore password |
| `ANDROID_KEY_ALIAS` | key alias |
| `ANDROID_KEY_PASSWORD` | key password |
| `GOOGLE_PLAY_SERVICE_ACCOUNT_JSON` | `base64 -i service-account.json \| pbcopy` |

## What it does

1. Checks out code and installs JS deps (`npm ci`) with Node 22
2. Sets up Java 17 (Temurin) and Ruby 3.3
3. Decodes the keystore to `/tmp/okto-upload-key.keystore`
4. Writes `android/gradle.properties` with signing config
5. Decodes the Play service account JSON to `/tmp/play-service-account.json`
6. Runs `bundle exec fastlane android <track>`
