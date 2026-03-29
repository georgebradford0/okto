# Changelog

## [0.0.2] - 2026-03-28

### Changed
- Renamed `GIT_TOKEN` environment variable to `GH_TOKEN` for consistency with GitHub's own naming convention. Update any `docker run` invocations or `.env` files accordingly.

## [0.0.1] - 2026-03-28

### Added
- Noise_XX_25519_ChaChaPoly_SHA256 transport replacing SSH tunnel — QR code format changed from v1 (SSH) to v2 (Noise)
- MixHash fixes per Noise spec §5.2 (empty payload) and §5.6 (empty prologue)
- `create_pull_request` tool for opening GitHub PRs and GitLab MRs from the agentic loop
- Auto-detection of public IP via `api.ipify.org` at container startup
- Base32+colon QR payload format for alphanumeric QR mode (smaller, more reliable scan)
- Repo name surfaced in app header from git remote URL
- Multi-platform Docker image (linux/amd64, linux/arm64)
