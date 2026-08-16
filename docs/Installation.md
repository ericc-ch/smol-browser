## Linux x86_64

```bash
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-x86_64-linux.tar.gz
tar xzf tinybrowser-x86_64-linux.tar.gz
./tinybrowser --version
```

## Linux ARM64

```bash
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-aarch64-linux.tar.gz
tar xzf tinybrowser-aarch64-linux.tar.gz
./tinybrowser --version
```

Linux builds target Ubuntu 22.04 and require glibc 2.35+.

## macOS Apple Silicon

```bash
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-aarch64-macos.tar.gz
tar xzf tinybrowser-aarch64-macos.tar.gz
./tinybrowser --version
```

## macOS Intel

```bash
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-x86_64-macos.tar.gz
tar xzf tinybrowser-x86_64-macos.tar.gz
./tinybrowser --version
```

## Windows

Download the `.zip` from [Releases](https://github.com/h4ckf0r0day/obscura/releases), extract, run `tinybrowser.exe --version`.

## Arch Linux (AUR)

```bash
yay -S obscura-browser
```

## Docker

```bash
docker run -d --name tinybrowser -p 127.0.0.1:9222:9222 h4ckf0r0day/obscura
```

Image: [h4ckf0r0day/obscura](https://hub.docker.com/r/h4ckf0r0day/obscura). Built on `distroless/cc`, with no shell or package manager in the runtime image.

Official archives and the Docker image include the rendering engine. Source
builders must pass `--features render`; see [Build from source](Build-from-source.md).

## From source

See [Build from source](Build-from-source.md).

## What's in the archive

- `tinybrowser`: CLI and CDP server.
- `tinybrowser-worker`: helper for the parallel `scrape` command. Keep both in the same directory.

Archive suffixes identify the feature set: no suffix includes rendering,
`-stealth` includes rendering and stealth, `-no-render` includes neither, and
`-no-render-stealth` includes stealth without rendering.

## Smoke test

```bash
./tinybrowser fetch https://example.com --eval "document.title"
./tinybrowser fetch https://example.com --screenshot smoke.png
```

Expected output: `"Example Domain"`, followed by a nonempty PNG at `smoke.png`.

## Troubleshooting

`cannot execute binary file`: wrong arch. Check `uname -m`.

`GLIBC_2.35 not found`: distro is older than Ubuntu 22.04. Use Docker or build from source.

macOS Gatekeeper warning: `xattr -d com.apple.quarantine ./tinybrowser`.
