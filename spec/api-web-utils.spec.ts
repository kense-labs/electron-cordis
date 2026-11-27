import { BrowserWindow } from 'electron/main';

import { describe, expect, it } from 'vitest';

import { once } from 'node:events';
import * as path from 'node:path';

import { defer } from './lib/spec-helpers.ts';

describe('webUtils module', { retry: 0 }, () => {
  const fixtures = path.resolve(import.meta.dirname, 'fixtures');

  describe('getPathForFile', () => {
    it('returns nothing for a Blob', async () => {
      const w = new BrowserWindow({
        show: false,
        webPreferences: {
          contextIsolation: false,
          nodeIntegration: true,
          sandbox: false
        }
      });
      defer(() => w.close());
      await w.loadFile(path.resolve(fixtures, 'pages', 'file-input.html'));
      const pathFromWebUtils = await w.webContents.executeJavaScript(
        'require("electron").webUtils.getPathForFile(new Blob([1, 2, 3]))'
      );
      expect(pathFromWebUtils).toBe('');
    });

    it('reports the correct path for a File object', async () => {
      const w = new BrowserWindow({
        show: false,
        webPreferences: {
          contextIsolation: false,
          nodeIntegration: true,
          sandbox: false
        }
      });
      defer(() => w.close());
      await w.loadFile(path.resolve(fixtures, 'pages', 'file-input.html'));
      const { debugger: debug } = w.webContents;
      debug.attach();
      try {
        const {
          root: { nodeId }
        } = await debug.sendCommand('DOM.getDocument');
        const { nodeId: inputNodeId } = await debug.sendCommand('DOM.querySelector', { nodeId, selector: 'input' });
        await debug.sendCommand('DOM.setFileInputFiles', {
          files: [import.meta.filename],
          nodeId: inputNodeId
        });
        const pathFromWebUtils = await w.webContents.executeJavaScript(
          'require("electron").webUtils.getPathForFile(document.querySelector("input").files[0])'
        );
        expect(pathFromWebUtils).toBe(import.meta.filename);
      } finally {
        debug.detach();
      }
    });

    it('reports the path of a dropped file in a sandboxed preload', async () => {
      const w = new BrowserWindow({
        show: false,
        webPreferences: {
          preload: path.join(fixtures, 'pages', 'file-drop-preload.cjs'),
          sandbox: true,
          contextIsolation: true,
          nodeIntegration: false
        }
      });
      defer(() => w.destroy());
      const painted = once(w, 'ready-to-show');
      await w.loadURL('data:text/html,<body style="width:100vw;height:100vh">Drop target</body>');
      await painted;

      const file = path.join(fixtures, 'pages', 'file-input.html');
      const debug = w.webContents.debugger;
      debug.attach();
      try {
        const data = { items: [], files: [file], dragOperationsMask: 1 };
        for (const type of ['dragEnter', 'dragOver', 'drop']) {
          await debug.sendCommand('Input.dispatchDragEvent', { type, x: 40, y: 40, data });
        }
        const result = await w.webContents.executeJavaScript('window.fileDropProbe.result()');
        expect(result).toEqual({
          name: path.basename(file),
          path: file,
          sandboxed: true,
          contextIsolated: true
        });
      } finally {
        debug.detach();
      }
    });
  });
});
