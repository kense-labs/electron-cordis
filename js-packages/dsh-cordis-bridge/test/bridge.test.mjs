import test from 'node:test';
import assert from 'node:assert/strict';
import { ShadowFiberRuntime, defineTool } from '../src/index.mjs';

test('DSH plugin registers tool, invokes bridged service, and drains ctx.effect in reverse order', async () => {
  const wireMessages = [];
  const order = [];
  const rt = new ShadowFiberRuntime((msg) => {
    wireMessages.push(msg);
    if (msg.op === 'service_invoke') {
      queueMicrotask(() => {
        rt.handleRpcReply({
          request_id: msg.request_id,
          ok: true,
          result: { stdout: `ran:${msg.params.cmd}` },
        });
      });
    }
  });

  const samplePlugin = {
    name: 'sample-dsh-plugin',
    inject: ['tools', 'shell'],
    apply(ctx) {
      ctx.effect(() => {
        order.push('setup-1');
        return () => order.push('cleanup-1');
      });
      ctx.tools.register(
        defineTool({
          name: 'sample.check',
          description: 'Run check via bridged shell service',
          execute: async ({ cmd }) => ctx.shell.exec({ cmd }),
        })
      );
      ctx.effect(() => {
        order.push('setup-2');
        return () => order.push('cleanup-2');
      });
    },
  };

  await rt.applyPlugin(
    { fiber_id: 'f-1', generation: 1, plugin_id: 'sample-dsh-plugin', config: {} },
    samplePlugin
  );

  assert.equal(wireMessages[0].op, 'register_tool');
  assert.equal(wireMessages[0].tool.name, 'sample.check');

  const out = await rt.invokeTool('sample.check', { cmd: 'uname -a' });
  assert.deepEqual(out, { stdout: 'ran:uname -a' });

  const errors = await rt.disposeFiber('f-1');
  assert.deepEqual(errors, []);
  assert.deepEqual(order, ['setup-1', 'setup-2', 'cleanup-2', 'cleanup-1']);
  await assert.rejects(() => rt.invokeTool('sample.check', { cmd: 'ls' }));
});
