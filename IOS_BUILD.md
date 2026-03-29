# iOS Build

The GitHub Actions workflow at `.github/workflows/ios.yml` builds the React Native iOS app on a macOS runner. It runs on demand only.

## Trigger manually (UI)

GitHub → Actions → iOS Build → Run workflow

## Trigger programmatically

**REST API:**
```bash
curl -X POST \
  -H "Authorization: Bearer YOUR_GITHUB_TOKEN" \
  -H "Accept: application/vnd.github+json" \
  https://api.github.com/repos/OWNER/REPO/actions/workflows/ios.yml/dispatches \
  -d '{"ref": "main"}'
```

**gh CLI:**
```bash
gh workflow run ios.yml --repo OWNER/REPO
```

The token needs `repo` scope (classic) or `Actions: write` (fine-grained).

## TestFlight distribution

The workflow includes a commented-out `distribute` job. To enable it:

1. Add these GitHub Secrets (`Settings → Secrets → Actions`):
   - `DISTRIBUTION_CERTIFICATE_P12` — base64-encoded .p12: `base64 -i cert.p12 | pbcopy`
   - `DISTRIBUTION_CERTIFICATE_PASSWORD`
   - `PROVISIONING_PROFILE` — base64-encoded .mobileprovision: `base64 -i profile.mobileprovision | pbcopy`
   - `APP_STORE_CONNECT_API_KEY_ID`
   - `APP_STORE_CONNECT_API_KEY_ISSUER_ID`
   - `APP_STORE_CONNECT_API_KEY_CONTENT` — base64-encoded .p8 key

2. Create `mobile/ios/ExportOptions.plist`:
   ```xml
   <?xml version="1.0" encoding="UTF-8"?>
   <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
   <plist version="1.0">
   <dict>
     <key>method</key>
     <string>app-store</string>
     <key>teamID</key>
     <string>YOUR_TEAM_ID</string>
   </dict>
   </plist>
   ```

3. Uncomment the `distribute` job in `.github/workflows/ios.yml`.
