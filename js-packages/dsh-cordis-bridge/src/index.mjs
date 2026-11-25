/**
 * @dsh/cordis-bridge
 *
 * Implements the DeepSeek Harness (`@deepseek-ai/cordis`) plugin runtime surface inside
 * a Node/UtilityProcess or KRun Guest Node container, federated to Rust Shadow Fibers
 * via the `electron-cordis.wire.v1` ShadowFiberMessage protocol.
 *
 * Guarantees:
 * - Deterministic reverse-commit-order `ctx.effect()` teardown on `dispose`
 * - Automatic tool unregistration & effect unwinding on generation swap
 * - Service proxy stubs for DSH's Three-Role Seam (Definition / Provider / Consumer)
 */

export function defineTool(spec) {
  if (!spec || typeof spec.name !== 'string' || !spec.name.trim()) {
    throw new Error('defineTool requires a non-empty string `name`');
  }
  if (typeof spec.execute !== 'function') {
    throw new Error(`tool \`${spec.name}\` requires an \`execute\` function`);
  }
  return Object.freeze({
    name: spec.name,
    version: spec.version ?? '1',
    description: spec.description ?? '',
    input_schema: spec.input_schema ?? spec.parameters ?? { type: 'object' },
    requires_approval: Boolean(spec.requires_approval),
    execute: spec.execute,
  });
}

export class ShadowFiberRuntime {
  #emitWire;
  #fibers = new Map();
  #pendingRpc = new Map();
  #rpcSeq = 1;

  constructor(emitWire) {
    if (typeof emitWire !== 'function') {
      throw new Error('ShadowFiberRuntime requires an emitWire(msg) callback');
    }
    this.#emitWire = emitWire;
  }

  /**
   * Mounts a standard DSH plugin module (`{ name, inject, apply }`) when the Rust host
   * sends a `ShadowFiberMessage::Apply` after all `inject` services converge.
   */
  async applyPlugin({ fiber_id, generation, plugin_id, config }, pluginModule) {
    if (this.#fibers.has(fiber_id)) {
      await this.disposeFiber(fiber_id);
    }

    const effects = [];
    const tools = new Map();
    let disposed = false;

    const registerEffect = (fn) => {
      if (disposed) {
        throw new Error(`cannot register effect on disposed fiber ${fiber_id}`);
      }
      const cleanup = fn();
      if (typeof cleanup === 'function') {
        effects.push(cleanup);
      }
      return () => {
        const idx = effects.indexOf(cleanup);
        if (idx >= 0) {
          effects.splice(idx, 1);
          if (typeof cleanup === 'function') cleanup();
        }
      };
    };

    const invokeService = (service, method, params = {}) => {
      const request_id = `${fiber_id}:${generation}:${this.#rpcSeq++}`;
      return new Promise((resolve, reject) => {
        this.#pendingRpc.set(request_id, { resolve, reject });
        this.#emitWire({
          op: 'service_invoke',
          request_id,
          fiber_id,
          service,
          method,
          params,
        });
      });
    };

    const createServiceProxy = (serviceName) =>
      new Proxy(
        {},
        {
          get(_target, prop) {
            if (typeof prop !== 'string') return undefined;
            return (params) => invokeService(serviceName, prop, params);
          },
        }
      );

    const ctx = {
      fiberId: fiber_id,
      generation,
      config,
      effect: registerEffect,
      tools: {
        register: (toolSpec) => {
          const tool = defineTool(toolSpec);
          tools.set(tool.name, tool);
          this.#emitWire({
            op: 'register_tool',
            fiber_id,
            generation,
            tool: {
              name: tool.name,
              version: tool.version,
              origin_plugin: plugin_id,
              description: tool.description,
              input_schema: tool.input_schema,
              requires_approval: tool.requires_approval,
            },
          });
          registerEffect(() => () => {
            tools.delete(tool.name);
          });
        },
      },
      // Three-Role Seam: Consumer calls ctx.service('shell').exec(...) transparently.
      service: (name) => createServiceProxy(name),
      shell: createServiceProxy('shell'),
      fs: createServiceProxy('fs'),
      clipboard: createServiceProxy('clipboard'),
    };

    this.#fibers.set(fiber_id, {
      generation,
      plugin_id,
      effects,
      tools,
      markDisposed: () => {
        disposed = true;
      },
    });

    await pluginModule.apply(ctx, config);
  }

  /**
   * Executes registered `ctx.effect` cleanups in strict reverse commit order,
   * continuing after individual errors so teardown never leaks remaining resources.
   */
  async disposeFiber(fiberId) {
    const entry = this.#fibers.get(fiberId);
    if (!entry) return [];
    entry.markDisposed();
    this.#fibers.delete(fiberId);

    const errors = [];
    while (entry.effects.length > 0) {
      const cleanup = entry.effects.pop();
      try {
        await cleanup();
      } catch (err) {
        errors.push(err instanceof Error ? err.message : String(err));
      }
    }
    return errors;
  }

  /** Executes a registered DSH tool on behalf of any KRun harness (MCP / ACP / inkstone.tools.v1). */
  async invokeTool(toolName, args) {
    for (const entry of this.#fibers.values()) {
      const tool = entry.tools.get(toolName);
      if (tool) {
        return await tool.execute(args);
      }
    }
    throw new Error(`tool not found in active shadow fibers: ${toolName}`);
  }

  /** Delivers a `ShadowFiberMessage::RpcReply` from the Rust microkernel. */
  handleRpcReply({ request_id, ok, result, error }) {
    const pending = this.#pendingRpc.get(request_id);
    if (!pending) return;
    this.#pendingRpc.delete(request_id);
    if (ok) pending.resolve(result);
    else pending.reject(new Error(error ?? 'remote service error'));
  }
}

import { createConnection } from 'node:net';
import { createInterface } from 'node:readline';

/**
 * Bidirectional `ShadowFiberMessage` socket client connecting a JS `ShadowFiberRuntime`
 * to a live `electron-cordis-daemon --shadow-socket <path>` host microkernel.
 */
export class ShadowSocketBridge {
  #socket;
  #runtime;
  #onDisposed;
  #disposedWaiters = new Map();

  constructor(socket, { onDisposed } = {}) {
    this.#socket = socket;
    this.#onDisposed = onDisposed;
    this.#runtime = new ShadowFiberRuntime((msg) => {
      if (!this.#socket.destroyed) {
        this.#socket.write(JSON.stringify(msg) + '\n', 'utf8');
      }
    });

    const rl = createInterface({ input: socket });
    rl.on('line', async (line) => {
      const trimmed = line.trim();
      if (!trimmed) return;
      let msg;
      try {
        msg = JSON.parse(trimmed);
      } catch {
        return;
      }

      if (msg.op === 'rpc_reply') {
        this.#runtime.handleRpcReply(msg);
      } else if (msg.op === 'tool_invoke') {
        try {
          const result = await this.#runtime.invokeTool(msg.tool_name, msg.arguments ?? {});
          this.#send({
            op: 'rpc_reply',
            request_id: msg.request_id,
            ok: true,
            result,
            error: null,
          });
        } catch (err) {
          this.#send({
            op: 'rpc_reply',
            request_id: msg.request_id,
            ok: false,
            result: null,
            error: err instanceof Error ? err.message : String(err),
          });
        }
      } else if (msg.op === 'dispose') {
        const errors = await this.#runtime.disposeFiber(msg.fiber_id);
        if (this.#onDisposed) this.#onDisposed(msg.fiber_id, errors);
        const waiter = this.#disposedWaiters.get(msg.fiber_id);
        if (waiter) {
          this.#disposedWaiters.delete(msg.fiber_id);
          waiter(errors);
        }
      }
    });
  }

  static connect(socketPath, options = {}) {
    return new Promise((resolve, reject) => {
      const socket = createConnection(socketPath);
      socket.once('connect', () => resolve(new ShadowSocketBridge(socket, options)));
      socket.once('error', reject);
    });
  }

  #send(msg) {
    if (!this.#socket.destroyed) {
      this.#socket.write(JSON.stringify(msg) + '\n', 'utf8');
    }
  }

  async mountPlugin({ fiber_id, generation = 1, plugin_id, config = {} }, pluginModule) {
    this.#send({ op: 'apply', fiber_id, generation, plugin_id, config });
    await this.#runtime.applyPlugin({ fiber_id, generation, plugin_id, config }, pluginModule);
  }

  waitForDispose(fiberId, timeoutMs = 5000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#disposedWaiters.delete(fiberId);
        reject(new Error(`timed out waiting for dispose of ${fiberId}`));
      }, timeoutMs);
      this.#disposedWaiters.set(fiberId, (errors) => {
        clearTimeout(timer);
        resolve(errors);
      });
    });
  }

  close() {
    this.#socket.end();
  }
}
