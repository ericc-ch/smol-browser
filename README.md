<h2 align="center">tinybrowser</h2>
<p align="center">
  <strong>The open-source headless browser for AI agents and web scraping.</strong><br>
  Lightweight, stealthy, and built in Rust.
</p>
<h3 align="center">Native rendering is here. No Chromium required. 🎉 </h3>
<p align="center">
  Capture screenshots, screencast live pages, and export PDFs directly with tinybrowser.
</p>

---

tinybrowser is a headless browser engine written in Rust, built for web scraping and AI agent automation. It runs real JavaScript via V8, supports the Chrome DevTools Protocol, and acts as a drop-in replacement for headless Chrome with Puppeteer and Playwright.

### Why tinybrowser over headless Chrome?

Designed for automation at scale, not desktop browsing.

| Metric       | tinybrowser      | Headless Chrome |
|--------------|--------------|------------------|
| Memory       | **30 MB**    | 200+ MB          |
| Binary size  | **70 MB**    | 300+ MB          |
| Anti-detect  | **Built-in** | None          |
| Page load    | **85 ms**    | ~500 ms          |
| Startup      | **Instant**  | ~2s              |
| Puppeteer    | **Yes**      | Yes              |
| Playwright   | **Yes**      | Yes              |

## Install

### Download

Grab the latest binary from [Releases](https://github.com/h4ckf0r0day/obscura/releases):

```bash
# Linux x86_64
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-x86_64-linux.tar.gz
tar xzf tinybrowser-x86_64-linux.tar.gz
./tinybrowser fetch https://example.com --eval "document.title"

# Linux ARM64 (aarch64)
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-aarch64-linux.tar.gz
tar xzf tinybrowser-aarch64-linux.tar.gz

# Arch Linux (AUR)
yay -S obscura-browser

# NixOS
nix-env -iA nixpkgs.obscura

# macOS Apple Silicon
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-aarch64-macos.tar.gz
tar xzf tinybrowser-aarch64-macos.tar.gz

# macOS Intel
curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/tinybrowser-x86_64-macos.tar.gz
tar xzf tinybrowser-x86_64-macos.tar.gz

# Windows
Download the `.zip` from the releases page and extract it manually.
```

No Chrome, no Node.js, no dependencies. Release archives include both
`tinybrowser` and `tinybrowser-worker`; keep them in the same directory for the
parallel `scrape` command.

| Archive suffix | Rendering | Stealth transport |
|----------------|-----------|-------------------|
| none | Yes | No |
| `-stealth` | Yes | Yes |
| `-no-render` | No | No |
| `-no-render-stealth` | No | Yes |

Linux release builds target Ubuntu 22.04 so the downloaded binary remains
usable on common LTS servers with glibc 2.35+.

### Docker

```bash
docker run -d --name tinybrowser -p 127.0.0.1:9222:9222 h4ckf0r0day/obscura
```

Image on [Docker Hub](https://hub.docker.com/r/h4ckf0r0day/obscura). Multi-stage build on `distroless/cc`, no shell, no package manager, ~57 MB compressed.

### Build from source

```bash
git clone https://github.com/h4ckf0r0day/obscura.git
cd tinybrowser

# Rendering
cargo build --release -p tinybrowser-cli --bins --features render

# Rendering and stealth
cargo build --release -p tinybrowser-cli --bins --features render,stealth

# No rendering
cargo build --release -p tinybrowser-cli --bins --no-default-features

# No rendering, with stealth
cargo build --release -p tinybrowser-cli --bins --no-default-features --features stealth
```

Requires Rust 1.75+ ([rustup.rs](https://rustup.rs)). First build takes ~5 min (V8 compiles from source, cached after).
The stealth build also compiles BoringSSL and generates bindings, so it needs
CMake, Clang, and the libclang/LLVM development libraries. On Ubuntu/Debian:

```bash
sudo apt-get install build-essential cmake clang libclang-dev llvm-dev
```

The rendering build uses rustls. The rendering-and-stealth build uses
wreq/BoringSSL and therefore needs the additional build tools above.

## Quick Start

### Fetch a page

```bash
# Get the page title
tinybrowser fetch https://example.com --eval "document.title"

# Extract all links
tinybrowser fetch https://example.com --dump links

# Render JavaScript and dump HTML
tinybrowser fetch https://news.ycombinator.com --dump html

# Write dump or eval output to a file
tinybrowser fetch https://example.com --dump text --output page.txt

# Stream the raw response body verbatim (binary-safe; bypasses the JS/DOM layer).
# Use this for images, JSON, JS, CSS, or any non-HTML resource.
tinybrowser fetch https://picsum.photos/200/300 --dump original > photo.jpg

# List every sub-resource URL the page would fetch (NDJSON; one record per asset)
tinybrowser fetch https://example.com --dump assets

# Fetch through an HTTP or SOCKS proxy
tinybrowser --proxy socks5://127.0.0.1:1080 fetch https://example.com --dump text

# Wait for dynamic content
tinybrowser fetch https://example.com --wait-until networkidle0

# Bound navigation time for slow or broken pages
tinybrowser fetch https://example.com --timeout 10

# Capture the settled page as PNG
tinybrowser fetch https://example.com --screenshot page.png

# The screenshot flag also has a short form
tinybrowser fetch https://example.com -s page.png
```

## Rendering

Official release archives and the Docker image include the rendering engine.
It provides CSS layout and paint, viewport and full-page screenshots,
scroll-aware fixed and sticky geometry, activity-driven CDP screencasting, and
raster PDF export without starting Chromium.

```javascript
await page.setViewport({ width: 1440, height: 1000 });
await page.goto('https://example.com', { waitUntil: 'load' });
await page.screenshot({ path: 'page.png', fullPage: true });
await page.pdf({ path: 'page.pdf', format: 'A4', printBackground: true });
```

The current implementation covers block, inline, flex, grid, table, float,
positioning, overflow, transform, text, image, SVG, canvas, background, border,
and animation paths. It remains an evolving independent
engine: long-tail CSS, some Web APIs, media playback, compositor effects, and
platform font rasterization may differ from Chromium. The existing
[Puppeteer](docs/Use-with-Puppeteer.md),
[Playwright](docs/Use-with-Playwright.md), and
[MCP](docs/Use-the-MCP-server.md) guides cover their capture APIs and limits.

### Start the CDP server

```bash
tinybrowser serve --port 9222

# With stealth mode (anti-detection + tracker blocking)
tinybrowser serve --port 9222 --stealth
```

### Scrape in parallel

```bash
tinybrowser scrape url1 url2 url3 ... \
  --concurrency 25 \
  --eval "document.querySelector('h1').textContent" \
  --format json

# Suppress scrape progress on stderr for script-friendly output
tinybrowser scrape https://example.com --quiet --format json

# Scrape workers inherit the global proxy
tinybrowser --proxy http://127.0.0.1:8080 scrape https://example.com https://news.ycombinator.com
```

## Puppeteer / Playwright

### Puppeteer

```bash
npm install puppeteer-core
```

```javascript
import puppeteer from 'puppeteer-core';

const browser = await puppeteer.connect({
  browserWSEndpoint: 'ws://127.0.0.1:9222/devtools/browser',
});

const page = await browser.newPage();
await page.goto('https://news.ycombinator.com');

const stories = await page.evaluate(() =>
  Array.from(document.querySelectorAll('.titleline > a'))
    .map(a => ({ title: a.textContent, url: a.href }))
);
console.log(stories);

await browser.disconnect();
```

### Playwright

```bash
npm install playwright-core
```

```javascript
import { chromium } from 'playwright-core';

const browser = await chromium.connectOverCDP({
  endpointURL: 'ws://127.0.0.1:9222',
});

const page = await browser.newContext().then(ctx => ctx.newPage());
await page.goto('https://en.wikipedia.org/wiki/Web_scraping');
console.log(await page.title());

await browser.close();
```

### Form submission & login

```javascript
await page.goto('https://quotes.toscrape.com/login');
await page.evaluate(() => {
  document.querySelector('#username').value = 'admin';
  document.querySelector('#password').value = 'admin';
  document.querySelector('form').submit();
});
// tinybrowser handles the POST, follows the 302 redirect, maintains cookies
```

## Benchmarks

Page load:

| Page | tinybrowser | Chrome |
|------|---------|--------|
| Static HTML | **51 ms** | ~500 ms |
| JS + XHR + fetch | **84 ms** | ~800 ms |
| Dynamic scripts | **78 ms** | ~700 ms |

The full benchmark suite (WPT conformance, obstacle course, real-world corpus, and vs-Chrome speed) lives in a separate repo: https://github.com/h4ckf0r0day/obscura-benchmark

## Stealth Mode

Build with `--features render,stealth`, then enable stealth at runtime with the
global `--stealth` flag. The stealth build includes the complete rendering
engine; enabling stealth does not remove screenshot, screencast, PDF, CDP, or
MCP functionality.

### Anti-fingerprinting
- Per-session fingerprint randomization (GPU, screen, canvas, audio, battery)
- Realistic `navigator.userAgentData` (Chrome 145, high-entropy values)
- `event.isTrusted = true` for dispatched events
- Hidden internal properties (`Object.keys(window)` safe)
- Native function masking (`Function.prototype.toString()` → `[native code]`)
- `navigator.webdriver = undefined` (matches real Chrome)

### Tracker Blocking
- 3,520 domains blocked
- Blocks analytics, ads, telemetry, and fingerprinting scripts
- Prevents trackers from loading entirely
- Enabled automatically with `--stealth`

## CDP API

tinybrowser implements the Chrome DevTools Protocol for Puppeteer/Playwright compatibility.

| Domain | Methods |
|--------|---------|
| **Target** | createTarget, closeTarget, attachToTarget, createBrowserContext, disposeBrowserContext |
| **Page** | navigate, getFrameTree, lifecycleEvents, captureScreenshot, start/stopScreencast, printToPDF |
| **Runtime** | evaluate, callFunctionOn, getProperties, addBinding |
| **DOM** | getDocument, querySelector, querySelectorAll, getOuterHTML, resolveNode |
| **Network** | enable, setCookies, getCookies, setExtraHTTPHeaders, setUserAgentOverride |
| **Fetch** | enable, continueRequest, fulfillRequest, failRequest (live interception), takeResponseBodyAsStream |
| **IO** | read, close (stream a large response body in chunks) |
| **Storage** | getCookies, setCookies, deleteCookies |
| **Input** | dispatchMouseEvent, dispatchKeyEvent |
| **LP** | getMarkdown (DOM-to-Markdown conversion) |

To download a large resource without one giant `Network.getResponseBody` blob, call `Fetch.takeResponseBodyAsStream` then read it in chunks with `IO.read` / `IO.close`. Response bodies over the cache limit (`TINYBROWSER_NETWORK_BODY_BUFFER_BYTES`, default 2 MiB) are not retained, so raise that limit when you intend to stream large downloads.
## CLI Reference

### Tuning V8

tinybrowser embeds V8 directly. Use `--v8-flags` to pass raw flags through to V8, same syntax as Chromium's `--js-flags` and Node's command-line flags. Most common use is raising the heap cap to fix `JavaScript heap out of memory` on JS-heavy pages:

```bash
tinybrowser --v8-flags "--max-old-space-size=4096" fetch <url>
```

### Heavy SPAs (script execution budget)

tinybrowser caps the page's script-execution phase so one slow or hung page cannot stall a worker. The default budget is 30s; pages that finish sooner return immediately, so the cap only affects pages that keep running. A very heavy React/Vue/Angular SPA on a slow network can need more time to boot before it fires its data requests. Raise the budget with `TINYBROWSER_SCRIPT_DEADLINE_MS` (milliseconds), and pair it with a matching navigation timeout in your CDP client:

```bash
TINYBROWSER_SCRIPT_DEADLINE_MS=60000 tinybrowser serve --port 9222
```

### `tinybrowser serve`

Start a CDP WebSocket server.

| Flag | Default | Description |
|------|---------|-------------|
| `--port` | `9222` | WebSocket port |
| `--proxy` | — | HTTP/SOCKS5 proxy URL |
| `--stealth` | off | Enable anti-detection + tracker blocking |
| `--workers` | `1` | Number of parallel worker processes |
| `--obey-robots` | off | Respect robots.txt |

### `tinybrowser fetch <URL>`

Fetch and render a single page.

| Flag | Default | Description |
|------|---------|-------------|
| `--dump` | `html` | Output: `html`, `text`, `links`, `markdown`, `assets` (NDJSON of every sub-resource URL the page references), or `original` (raw response body) |
| `--eval` | — | JavaScript expression to evaluate |
| `--wait-until` | `load` | Wait: `load`, `domcontentloaded`, `networkidle0` |
| `--timeout` | `30` | Maximum navigation time in seconds |
| `--wait` | adaptive, up to `5` | Post-load settling; an explicit value is a fixed delay in seconds |
| `--selector` | — | Wait for CSS selector |
| `-s`, `--screenshot` | — | Write a PNG screenshot (single URL; render-enabled build) |
| `--stealth` | off | Anti-detection mode |
| `--output` | — | Write dump or eval output to a file |
| `--quiet` | off | Suppress banner |
| `--proxy` | — | Inherited global HTTP/SOCKS5 proxy URL |

### `tinybrowser scrape <URL...>`

Scrape multiple URLs in parallel with worker processes.

| Flag | Default | Description |
|------|---------|-------------|
| `--concurrency` | `10` | Parallel workers |
| `--eval` | — | JS expression per page |
| `--format` | `json` | Output: `json` or `text` |
| `--quiet` | off | Suppress scrape progress on stderr |
| `--proxy` | — | Inherited global HTTP/SOCKS5 proxy URL for all workers |

## MCP (Model Context Protocol)

tinybrowser ships an MCP server that exposes browser automation tools to AI agents (Claude Desktop, Cursor, etc.).

### Start

**stdio** (default) — for Claude Desktop and MCP clients that launch a subprocess:

```bash
tinybrowser mcp
```

**HTTP** — for clients that connect over the network:

```bash
tinybrowser mcp --http --port 8080
# endpoint: http://127.0.0.1:8080/mcp
```

Optional flags (both transports):

| Flag | Description |
|------|-------------|
| `--proxy <URL>` | HTTP/SOCKS5 proxy |
| `--user-agent <UA>` | Custom User-Agent string |
| `--stealth` | Enable anti-detection mode |

### Claude Desktop config

```json
{
  "mcpServers": {
    "tinybrowser": {
      "command": "tinybrowser",
      "args": ["mcp"]
    }
  }
}
```

### Tools

| Tool | Description |
|------|-------------|
| `browser_navigate` | Navigate to a URL (`url`, optional `waitUntil`: `load` / `domcontentloaded` / `networkidle0`) |
| `browser_snapshot` | Return the current page URL, title, readable body text, and element references |
| `browser_screenshot` | Return the current page as an MCP PNG image (render-enabled build) |
| `browser_pdf` | Return the current page as an embedded PDF resource (render-enabled build) |
| `browser_click` | Click by current snapshot reference or CSS selector |
| `browser_fill` | Set an input value by reference or selector (triggers `input` + `change`) |
| `browser_type` | Append text to an input |
| `browser_press_key` | Dispatch a keyboard event (`key`, optional `selector`) |
| `browser_select_option` | Select an `<option>` by value or text |
| `browser_evaluate` | Evaluate a JavaScript expression and return the result |
| `browser_wait_for` | Wait for a CSS selector to appear (`selector`, optional `timeout` in seconds) |
| `browser_network_requests` | List network requests made by the current page |
| `browser_console_messages` | Return console messages logged by the page |
| `browser_close` | Close the page and reset browser state |

The MCP server exposes still-image and PDF output. Use CDP when you need the
streaming `Page.startScreencast` protocol.

## Integrations

- **[Hermes agent plugin](https://github.com/SGavrl/hermes-plugin-obscura)**: run [Hermes](https://github.com/NousResearch/hermes-agent) agent browser tasks on tinybrowser. The plugin spawns `tinybrowser serve` per session (or connects to an already running server) and drives it over CDP, with optional `--stealth`.

## License

Apache 2.0

smol-browser is a fork of [Obscura](https://github.com/h4ckf0r0day/obscura) (Apache-2.0) with substantial modifications. See [NOTICE](NOTICE) for attribution.

---
