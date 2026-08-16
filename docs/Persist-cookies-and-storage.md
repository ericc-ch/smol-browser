`--storage-dir` persists cookies and localStorage to disk so they survive across runs.

## CLI

```bash
tinybrowser fetch https://example.com --storage-dir ./tinybrowser-data
tinybrowser fetch https://example.com --storage-dir ./tinybrowser-data
```

The second invocation starts with the cookies and localStorage left by the first.

## Server

```bash
tinybrowser serve --storage-dir ./tinybrowser-data
```

All CDP sessions read and write to the same directory. Run separate `tinybrowser serve` processes with different `--storage-dir` paths for isolated profiles.

## Layout

Inside `./tinybrowser-data`:

- `cookies.json`: cookie jar in a stable format with `same_site`, `expires`, `http_only`, `secure`.
- `localStorage/<origin>.json`: one file per origin.

The format is stable. Inspect with `jq`:

```bash
jq '.[] | select(.domain == "example.com")' ./tinybrowser-data/cookies.json
```

## When state is written

- On clean process exit (Ctrl-C, SIGTERM).
- After every navigation completes (CDP `Page.navigate`).
- Manually via CDP `Network.setCookie` and `Network.deleteCookies`.

## Login once, scrape many

```bash
tinybrowser serve --storage-dir ./session-1
```

Drive a login flow once via Puppeteer or Playwright. Stop the server. Subsequent runs against the same `--storage-dir` start logged in.

## Multiple identities

```bash
tinybrowser serve --port 9222 --storage-dir ./identity-a
tinybrowser serve --port 9223 --storage-dir ./identity-b
```

## Clear state

```bash
rm -rf ./tinybrowser-data
```
