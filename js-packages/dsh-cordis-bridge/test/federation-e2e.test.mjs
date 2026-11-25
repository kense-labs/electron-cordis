import test from 'node:test';
import assert from 'node:assert/strict';
import { spawn } from 'node:child_process';
import { existsSync, mkdtempSync, rmSync } from 'node:fs';
import { createConnection } from 'node:net';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { createInterface } from 'node:readline';
import { fileURLToPath } from 'node:url';

import { ShadowSocketBridge, defineTool } from '../src/index.mjs';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ELECTRON_CORDIS_ROOT = path.resolve(__dirname, '../../..');
const DAEMON_BIN = path.join(
  ELECTRON_CORDIS_ROOT,
  'cordis',
  'target',
  'debug',
  process.platform === 'win32' ? 'electron-cordis-daemon.exe' : 'electron-cordis-daemon'
);

class JsonRpcLineClient {
  #input;
  #output;
  #nextId = 1;
  #pending = new Map();

  constructor(input, output) {
    this.#input = input;
    this.#output = output;
    const rl = createInterface({ input });
    rl.on('line', (line) => {
      const trimmed = line.trim();
      if (!trimmed) return;
      const msg = JSON.parse(trimmed);
      const p = this.#pending.get(msg.id);
      if (!p) return;
      this.#pending.delete(msg.id);
      if (msg.error) p.reject(new Error(`RPC ${msg.error.code}: ${msg.error.message}`));
      else p.resolve(msg.result);
    });
  }

  request(method, params = {}) {
    const id = this.#nextId++;
    return new Promise((resolve, reject) => {
      this.#pending.set(id, { resolve, reject });
      this.#output.write(JSON.stringify({ jsonrpc: '2.0', id, method, params }) + '\n', 'utf8');
    });
  }
}

async function connectUnixJsonRpc(socketPath) {
  for (let attempt = 0; attempt < 50; attempt++) {
    try {
      const sock = await new Promise((resolve, reject) => {
        const s = createConnection(socketPath);
        s.once('connect', () => resolve(s));
        s.once('error', reject);
      });
      return { socket: sock, rpc: new JsonRpcLineClient(sock, sock) };
    } catch {
      await new Promise((r) => setTimeout(r, 40));
    }
  }
  throw new Error(`failed to connect to unix socket: ${socketPath}`);
}

test('M4 e2e: standard DSH JS plugin + Rust cap-clipboard federated across MCP & Host Native Contract with unified Around approval & reverse ctx.effect cleanup', async () => {
  assert.ok(existsSync(DAEMON_BIN), `daemon binary missing at ${DAEMON_BIN}`);

  const tmp = mkdtempSync(path.join(tmpdir(), 'cordis-m4-'));
  const mcpSock = path.join(tmp, 'dsh-mcp.sock');
  const nativeSock = path.join(tmp, 'host-tools.sock');
  const acpSock = path.join(tmp, 'dsh-rpc.sock');
  const llmSock = path.join(tmp, 'llm-gateway.sock');
  const shadowSock = path.join(tmp, 'shadow.sock');

  const daemon = spawn(
    DAEMON_BIN,
    [
      '--mcp-socket',
      mcpSock,
      '--native-tools-socket',
      nativeSock,
      '--acp-socket',
      acpSock,
      '--llm-gateway-socket',
      llmSock,
      '--shadow-socket',
      shadowSock,
    ],
    { stdio: ['pipe', 'pipe', 'pipe'], shell: false }
  );
  const control = new JsonRpcLineClient(daemon.stdout, daemon.stdin);

  let mcpConn;
  let nativeConn;
  let acpConn;
  let llmConn;
  let shadowBridge;

  try {
    // Wait for daemon ready
    const health = await control.request('cordis.health');
    assert.equal(health.profile, 'unconfigured');

    // Connect ALL 4 KRun Harness projections + ShadowFiber JS plugin host
    mcpConn = await connectUnixJsonRpc(mcpSock);
    nativeConn = await connectUnixJsonRpc(nativeSock);
    acpConn = await connectUnixJsonRpc(acpSock);
    llmConn = await connectUnixJsonRpc(llmSock);
    shadowBridge = await ShadowSocketBridge.connect(shadowSock);

    // 1. Mount a standard DSH JS plugin (`defineTool` + `ctx.effect` + bridged `ctx.clipboard`)
    const effectCleanupOrder = [];
    const jsDshPlugin = {
      name: '@deepseek-ai/dsh-clipboard-digest',
      apply(ctx) {
        ctx.effect(() => () => effectCleanupOrder.push('effect-1-outer'));
        ctx.tools.register(
          defineTool({
            name: 'dsh.clipboard_digest',
            description: 'Reads host clipboard via bridged Rust service and returns length digest',
            requires_approval: true,
            input_schema: { type: 'object', properties: { prefix: { type: 'string' } } },
            async execute({ prefix = 'digest' }) {
              // Calls Rust cap-clipboard across the ShadowFiber bridge!
              const text = await ctx.clipboard.readText({});
              // Security check: untrusted ShadowFiber MUST be denied safeStorage!
              let safeStorageDenied = false;
              try {
                await ctx.service('safeStorage').isEncryptionAvailable({});
              } catch (err) {
                safeStorageDenied = String(err.message).includes('SAFE_STORAGE_REALM_DENIED');
              }
              return {
                label: `${prefix}:${text.length}`,
                safe_storage_denied: safeStorageDenied,
              };
            },
          })
        );
        ctx.effect(() => () => effectCleanupOrder.push('effect-2-inner'));
      },
    };

    await shadowBridge.mountPlugin(
      {
        fiber_id: 'sf-dsh-1',
        generation: 1,
        plugin_id: jsDshPlugin.name,
        config: {},
      },
      jsDshPlugin
    );

    // Wait briefly for async register_tool frame processing on daemon socket
    let mcpTools = [];
    for (let i = 0; i < 30; i++) {
      const res = await mcpConn.rpc.request('tools/list');
      mcpTools = res.tools.map((t) => t.name);
      if (mcpTools.includes('dsh.clipboard_digest')) break;
      await new Promise((r) => setTimeout(r, 25));
    }

    // 2. Both external MCP client & Host Native Contract client discover Rust + JS tools identically
    const mcpInit = await mcpConn.rpc.request('initialize', {
      protocolVersion: '2025-03-26',
      clientInfo: { name: 'external-mcp-harness', version: '1.0' },
    });
    assert.equal(mcpInit.serverInfo.name, 'electron-cordis-bridge-mcp');

    assert.ok(mcpTools.includes('clipboard.available_formats'));
    assert.ok(mcpTools.includes('clipboard.write_text'));
    assert.ok(mcpTools.includes('dsh.clipboard_digest'));

    const nativeList = await nativeConn.rpc.request('inkstone.tools.list');
    assert.equal(nativeList.schema, 'inkstone.tools.v1');
    const nativeTools = nativeList.tools.map((t) => t.name);
    assert.deepEqual(nativeTools, mcpTools);

    // 3. MCP client writes clipboard (Rust cap-clipboard), Host Native Contract invokes JS DSH tool
    const writeRes = await mcpConn.rpc.request('tools/call', {
      name: 'clipboard.write_text',
      arguments: { text: 'cross-harness-payload' },
    });
    assert.equal(writeRes.isError, false);

    const nativeJsRes = await nativeConn.rpc.request('inkstone.tools.call', {
      session_id: 'sess-1',
      harness_id: 'inkstone-harness',
      tool: 'dsh.clipboard_digest',
      input: { prefix: 'inkstone' },
    });
    assert.equal(nativeJsRes.ok, true);
    assert.equal(nativeJsRes.output.label, 'inkstone:21');
    assert.equal(nativeJsRes.output.safe_storage_denied, true);

    // MCP client can also invoke the JS DSH tool transparently
    const mcpJsRes = await mcpConn.rpc.request('tools/call', {
      name: 'dsh.clipboard_digest',
      arguments: { prefix: 'mcp' },
    });
    assert.equal(mcpJsRes.isError, false);
    assert.equal(mcpJsRes.structuredContent.label, 'mcp:21');

    // 3b. ACP client (`--acp-socket`) invokes the SAME JS DSH tool seamlessly!
    const acpSess = await acpConn.rpc.request('session/new', { harness_id: 'acp-harness' });
    const acpPrompt = await acpConn.rpc.request('session/prompt', {
      session_id: acpSess.sessionId,
      prompt: 'digest clipboard',
      tool_call: { name: 'dsh.clipboard_digest', arguments: { prefix: 'acp' } },
    });
    assert.equal(acpPrompt.stopReason, 'end_turn');
    assert.equal(acpPrompt.toolResult.ok, true);
    assert.equal(acpPrompt.toolResult.output.label, 'acp:21');

    // 3c. LLM Gateway (`--llm-gateway-socket`): Mapper redacts secrets + Around enforces model policy
    const llmRes = await llmConn.rpc.request('llm.chat.completions', {
      session_id: acpSess.sessionId,
      harness_id: 'acp-harness',
      model: 'intern-s2.1-flash',
      messages: [{ role: 'user', content: 'use Bearer tok_live_999 to query' }],
    });
    assert.equal(llmRes.ok, true);
    assert.ok(llmRes.content.includes('[REDACTED]'));
    assert.ok(!llmRes.content.includes('tok_live_999'));
    assert.deepEqual(llmRes.redacted_fields, ['messages[0].content']);

    // 4. Unified Around approval middleware intercepts BOTH MCP and Host Native Contract
    await control.request('cordis.policy.set', {
      enforce_requires_approval: true,
      approved_tools: [],
    });

    const mcpDenied = await mcpConn.rpc.request('tools/call', {
      name: 'dsh.clipboard_digest',
      arguments: { prefix: 'mcp' },
    });
    assert.equal(mcpDenied.isError, true);
    assert.match(mcpDenied.content[0].text, /APPROVAL_REQUIRED/);

    const nativeDenied = await nativeConn.rpc.request('inkstone.tools.call', {
      session_id: 'sess-1',
      harness_id: 'inkstone-harness',
      tool: 'clipboard.read_text',
      input: {},
    });
    assert.equal(nativeDenied.ok, false);
    assert.match(nativeDenied.error, /APPROVAL_REQUIRED/);

    // Verify unified trajectory recorded both harnesses' denials
    const traj = await control.request('cordis.trajectory.list');
    assert.ok(
      traj.records.some((r) => r.harness_id === 'mcp-client' && r.kind === 'tool.denied')
    );
    assert.ok(
      traj.records.some((r) => r.harness_id === 'inkstone-harness' && r.kind === 'tool.denied')
    );

    // 5. Reverse-order cleanup: disposing ShadowFiber triggers LIFO `ctx.effect` unwinding on JS side
    // and removes the JS tool from both MCP and Host Native Contract immediately.
    const disposeDone = shadowBridge.waitForDispose('sf-dsh-1');
    await control.request('cordis.shadow.message', {
      op: 'dispose',
      fiber_id: 'sf-dsh-1',
      generation: 1,
    });
    const disposeErrors = await disposeDone;
    assert.deepEqual(disposeErrors, []);
    assert.deepEqual(effectCleanupOrder, ['effect-2-inner', 'effect-1-outer']);

    const afterMcp = await mcpConn.rpc.request('tools/list');
    assert.ok(!afterMcp.tools.some((t) => t.name === 'dsh.clipboard_digest'));
    const afterNative = await nativeConn.rpc.request('inkstone.tools.list');
    assert.ok(!afterNative.tools.some((t) => t.name === 'dsh.clipboard_digest'));

    // 6. Clean daemon shutdown drains remaining Rust fibers in reverse order
    const shut = await control.request('cordis.shutdown');
    assert.equal(shut.drained, true);
    assert.deepEqual(shut.errors, []);
  } finally {
    shadowBridge?.close();
    mcpConn?.socket?.end();
    nativeConn?.socket?.end();
    acpConn?.socket?.end();
    llmConn?.socket?.end();
    daemon.stdin.end();
    await new Promise((r) => daemon.once('exit', r));
    rmSync(tmp, { recursive: true, force: true });
  }
});
