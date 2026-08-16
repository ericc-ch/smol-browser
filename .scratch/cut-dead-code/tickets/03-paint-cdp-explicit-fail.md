# 03: Paint CDP methods fail explicitly

Type: grilling

Question: After deleting render cfg, should Page.captureScreenshot / printToPDF / screencast keep the existing "requires render feature" errors, become no-op successes, or drop through as unknown methods?

Answer: Explicit fail. Keep named arms so Playwright does not see "Unknown Page method" (#45, #53). Return the same JSON-RPC `-32601` error with a message in the `Page.captureSnapshot` style: the method exists, there is no layout or paint engine. Drop the "requires a build with the render feature" wording. Do not return a fake `{ data }` success.
