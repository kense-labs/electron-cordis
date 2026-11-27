import { ok, strictEqual } from 'node:assert/strict';
import { once } from 'node:events';
import { createServer } from 'node:http';
import { join } from 'node:path';
import { setTimeout as delay } from 'node:timers/promises';
import { pathToFileURL } from 'node:url';

const optionalBindings = [
  'electron_browser_clipboard',
  'electron_browser_clipboard_item',
  'electron_browser_content_tracing',
  'electron_browser_in_app_purchase',
  'electron_browser_push_notifications',
  'electron_browser_safe_storage'
];

function hasLinkedBinding(name) {
  try {
    process._linkedBinding(name);
    return true;
  } catch (error) {
    if (error.code !== 'ERR_INVALID_MODULE') throw error;
    return false;
  }
}

export async function probeLiteCapabilities(window, main) {
  const flags = process._linkedBinding('electron_common_features');
  const pdfViewer = flags.isPDFViewerEnabled();
  const linkedBindings = Object.fromEntries(optionalBindings.map(name => [name, hasLinkedBinding(name)]));
  let netFetch;
  if (Object.hasOwn(main, 'net')) {
    const server = createServer((_request, response) => response.end('cordis-lite-net'));
    server.listen(0, '127.0.0.1');
    try {
      await once(server, 'listening');
      const response = await main.net.fetch(`http://127.0.0.1:${server.address().port}/`, {
        signal: AbortSignal.timeout(5000)
      });
      netFetch = await response.text();
      strictEqual(netFetch, 'cordis-lite-net');
    } finally {
      server.close();
    }
  }

  let pdfFrames = 0;
  if (pdfViewer) {
    const pdf = pathToFileURL(join(import.meta.dirname, '../../spec/fixtures/cat.pdf')).href;
    // The upstream PDF test checks for a second frame hosting the document.
    await window.loadURL(pdf);
    const deadline = Date.now() + 10_000;
    do {
      pdfFrames = window.webContents.mainFrame.framesInSubtree.filter(frame => frame.url === pdf).length;
      if (pdfFrames >= 2) break;
      await delay(50);
    } while (Date.now() < deadline);
    ok(pdfFrames >= 2, 'The PDF viewer did not create its document frame.');
  }

  return {
    linkedBindings,
    pdfViewer,
    pdfFrames,
    netFetch: netFetch ?? null,
    printing: flags.isPrintingEnabled(),
    spellchecker: flags.isBuiltinSpellCheckerEnabled()
  };
}
