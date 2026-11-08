# sitefile and Docker

sitefile fits next to Docker rather than replacing it: Docker runs things, and sitefile builds static content into the
same image format Docker uses. The two meet at OCI images and registries.

## 1. sitefile builds the site image, Docker runs it

```
# Sitefile
COPY ./public/ /
RENDER --escape=html ./index.html TO /index.html
EXPORT / AS oci BASE docker.io/library/nginx:alpine AT /usr/share/nginx/html
```

```bash
sitefile build -t ghcr.io/me/site:1.4 && sitefile push ghcr.io/me/site:1.4
docker run -p 8080:80 ghcr.io/me/site:1.4      # or Kubernetes, ECS, Fly, Cloud Run…
```

- No Docker daemon is needed to build: sitefile pulls the base image from the registry itself. CI runners need neither
  Docker-in-Docker nor root.
- The image is ordinary OCI. The engine tests (which run in CI on Linux) load a saved sitefile image into Docker and
  serve it from that nginx base.

## 2. Use it as a stage in a Dockerfile

Because the output is a normal image, Dockerfiles can consume it. For this, build a **files-only** image: without
`BASE`, the site's files sit under `/srv/www` (change it with `AT`) and the image has no entrypoint.

```
# Sitefile
COPY ./public/ /
RENDER --escape=html ./index.html TO /index.html
EXPORT / AS oci
```

```bash
sitefile build -t ghcr.io/me/site-files:1.4 && sitefile push ghcr.io/me/site-files:1.4
```

```dockerfile
FROM ghcr.io/me/site-files:1.4 AS site
FROM caddy:2
COPY --from=site /srv/www /usr/share/caddy
```

This replaces the usual "install Node, run npm build" stage. That way the final image never contains a toolchain,
`node_modules` or build scripts.

A files-only image is labelled `linux/amd64` by default (it's metadata only: the files are the same on every
platform). If you build the Dockerfile for arm64, label the image to match with `sitefile build --platform
linux/arm64`, or Docker warns about the mismatch.

## 3. What it adds that a Dockerfile can't

- **Reproducible image IDs.** The same inputs give the same digest on any machine. That comes from the shared
  deterministic compression, fixed timestamps and normalized paths. Docker builds are rarely byte-identical, so layer
  caching and "did anything change?" checks become exact.
- **Pinned inputs.** `INTEGRITY` hashes and `Sitefile.lock` pin every remote input. `--strict-pins` refuses any remote
  input or `BASE` image that isn't pinned, before anything is fetched, and `sitefile vendor` stores every remote input
  (and the base image's blobs) in the repository for offline builds
  ([LANGUAGE.md](LANGUAGE.md#vendoring-sitefile-vendor)).
- **No arbitrary commands.** There's no `RUN`, external programs need `--allow-exec`, and JavaScript plugins need
  `--allow-plugin`. A Sitefile is far safer to build from an untrusted pull request than a Dockerfile is.
- **A fast dev loop.** `sitefile serve --watch` rebuilds in memory and reloads open pages. You don't rebuild and
  restart a container on every edit.

## 4. Shared infrastructure

- **Same registries and credentials.** sitefile reads your Docker config (`$DOCKER_CONFIG/config.json` or
  `~/.docker/config.json`, including `credsStore` and `credHelpers`), so `docker login` covers both.
- **Same cache and supply-chain tools:**
  - `--cache-from` / `--cache-to` store cached `TRANSFORM` results in the registry, so CI runs and teammates reuse them
    ([CLI.md](CLI.md#registry-cache)).
  - `--provenance` and `--sbom` attach a SLSA v1 provenance statement and a CycloneDX 1.6 SBOM to the image. They're
    in-toto statements attached as OCI 1.1 referrers (artifacts whose `subject` is the image), which referrer-aware
    tools such as `oras discover` list. This is not BuildKit's layout, which stores its attestations inside the image
    index.
  - sitefile doesn't sign anything. Sign the image and its attestations with cosign or notation, as you would any other
    image.
- **Platforms.** An image is built for one platform (`--platform`, default `linux/amd64`). The site layer is the same
  bytes for every platform, so a registry stores it once; only the base image differs by platform. For a multi-platform
  tag, build once per platform.

## 5. Where Docker stays the right tool

- Anything that runs at request time: APIs, server-side rendering, databases.
- Build steps that need arbitrary tools. sitefile deliberately refuses `RUN`.

A common split: the app is a Docker image, and the static assets are a Sitefile image. You serve them through an nginx
sidecar, or export them as a folder (`EXPORT / AS directory`) and upload that to a CDN. Docker Compose can run both
side by side in development.
