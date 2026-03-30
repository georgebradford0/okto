# claudulhu — project notes for Claude

## Docker image

The correct image name is **`ghcr.io/georgebradford0/claudulhu-server`**.

Pull:
```sh
docker pull ghcr.io/georgebradford0/claudulhu-server:latest
```

Build and push (replace `X.Y.Z` with the new version):
```sh
docker build -t ghcr.io/georgebradford0/claudulhu-server:X.Y.Z \
             -t ghcr.io/georgebradford0/claudulhu-server:latest .
docker push ghcr.io/georgebradford0/claudulhu-server:X.Y.Z
docker push ghcr.io/georgebradford0/claudulhu-server:latest
```

**Never** use `claudulhu:latest` or any name that omits `-server`.
