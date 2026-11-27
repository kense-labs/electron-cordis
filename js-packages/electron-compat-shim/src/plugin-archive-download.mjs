import { createHash } from 'node:crypto';
import { createWriteStream } from 'node:fs';
import { mkdtemp, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { Transform } from 'node:stream';
import { pipeline } from 'node:stream/promises';

const manifestLimit = 16 * 1024;

async function fetchAsset(address, signal) {
  const url = new URL(address);
  if (url.protocol !== 'https:' || url.username || url.password || url.hash) {
    throw new Error('Plugin assets require HTTPS URLs without credentials or fragments');
  }
  const response = await fetch(url, { signal, redirect: 'error', headers: { 'accept-encoding': 'identity' } });
  const encoding = response.headers.get('content-encoding')?.toLowerCase();
  if (response.status !== 200 || !response.body || (encoding && encoding !== 'identity')) {
    throw new Error('Expected a complete, untransformed plugin response');
  }
  return response;
}

async function readManifest(response) {
  const buffers = [];
  let bytes = 0;
  for await (const chunk of response.body) {
    bytes += chunk.length;
    if (bytes > manifestLimit) throw new Error('Plugin manifest exceeds the metadata size limit');
    buffers.push(chunk);
  }
  return JSON.parse(Buffer.concat(buffers, bytes).toString('utf8'));
}

async function saveArchive(response, path, manifest, signal) {
  const expectedBytes = manifest.archive_bytes;
  const contentLength = response.headers.get('content-length');
  if (contentLength !== null && Number(contentLength) !== expectedBytes) {
    throw new Error('Plugin archive Content-Length differs from its signed size');
  }
  const digest = createHash('sha256');
  let bytes = 0;
  const verify = new Transform({
    transform(chunk, _encoding, finish) {
      bytes += chunk.length;
      if (bytes > expectedBytes) {
        finish(new Error('Plugin download exceeds its signed size'));
        return;
      }
      digest.update(chunk);
      finish(null, chunk);
    },
    flush(finish) {
      if (bytes !== expectedBytes || digest.digest('hex') !== manifest.archive_sha256) {
        finish(new Error('Plugin download differs from its signed size or checksum'));
        return;
      }
      finish();
    }
  });
  await pipeline(response.body, verify, createWriteStream(path, { flags: 'wx', mode: 0o600 }), { signal });
}

export async function acquirePluginArchive({ installer, pluginId, acceptsVersion, manifestUrl, archiveUrl, signal, timeoutMs }) {
  const request = new AbortController();
  const signals = [request.signal, AbortSignal.timeout(timeoutMs)];
  if (signal) signals.push(signal);
  const downloadSignal = AbortSignal.any(signals);
  let directory;
  try {
    const manifest = await readManifest(await fetchAsset(manifestUrl, downloadSignal));
    if (manifest?.plugin_id !== pluginId || !acceptsVersion(manifest.version)) {
      throw new Error(`Unexpected archive identity for ${pluginId}`);
    }
    if (!Number.isSafeInteger(manifest.archive_bytes) || manifest.archive_bytes <= 0) {
      throw new Error('Plugin manifest has an invalid archive size');
    }
    const { verified } = await installer.verifyArchiveManifest(manifest);
    if (verified !== true) throw new Error('Cordis did not verify the plugin manifest');
    const response = await fetchAsset(archiveUrl, downloadSignal);
    directory = await mkdtemp(join(tmpdir(), 'cordis-plugin-download-'));
    const archivePath = join(directory, 'archive.tar.gz');
    await saveArchive(response, archivePath, manifest, downloadSignal);
    downloadSignal.throwIfAborted();
    return { manifest, archivePath, release: () => rm(directory, { recursive: true, force: true }) };
  } catch (error) {
    if (directory) await rm(directory, { recursive: true, force: true });
    throw error;
  } finally {
    request.abort();
  }
}
