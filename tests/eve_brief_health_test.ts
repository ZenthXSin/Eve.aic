import assert from "node:assert/strict";
import { KernelError } from "../kernel/protocol.ts";
import { createEvePlugin } from "../plugins/eve/mod.ts";
import { EveRuntime } from "../plugins/eve/runtime.ts";
import type { Handler, PluginContext } from "../sdk/plugin.ts";

function config(directory: string) {
  return {
    directory: `${directory}/journal`,
    promptFile: `${directory}/prompt`,
    sourceCommit: "a".repeat(40),
    modelTimeoutMs: 1000,
    maxOutputTokens: 12,
    pollMs: 60_000,
    defaultSleepMs: 300,
    dailyTokenBudget: 1,
  };
}

function withoutHistory(runtime: EveRuntime) {
  const original = Object.getOwnPropertyDescriptor(runtime.journal, "events")!;
  Object.defineProperty(runtime.journal, "events", {
    get() {
      throw new Error("unexpected history read");
    },
  });
  try {
    return runtime.briefStatus();
  } finally {
    Object.defineProperty(runtime.journal, "events", original);
  }
}

Deno.test("机械健康查询不读取历史正文；区分初始化、在途、通道错误、恢复与致命错误", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-brief-health-" });
  const opening = Promise.withResolvers<void>(), finishOpen = Promise.withResolvers<void>();
  const entered = Promise.withResolvers<void>(), release = Promise.withResolvers<void>();
  let dependency: KernelError | undefined = new KernelError("timeout", "fixture");
  const ctx = {
    async call(contract: string, method: string) {
      if (contract === "phone" && method === "notifications") {
        entered.resolve();
        await release.promise;
        if (dependency) throw dependency;
      }
      if (["poll", "completions", "notifications"].includes(method)) {
        return { items: [], throughSeq: 0 };
      }
      if (contract === "tools" && method === "list") return [];
      if (contract === "mind" && method === "update") return null;
      throw new Error(`unexpected ${contract}.${method}`);
    },
  } as unknown as PluginContext;
  const runtime = new EveRuntime(ctx, config(directory));
  const initialize = runtime.journal.initialize.bind(runtime.journal);
  runtime.journal.initialize = async () => {
    await initialize();
    opening.resolve();
    await finishOpen.promise;
  };
  try {
    await Deno.writeTextFile(runtime.config.promptFile, "fixture");
    const stopped = { running: false, busy: false, healthy: false, lastError: null };
    assert.deepEqual(withoutHistory(runtime), stopped);
    const ready = runtime.initialize();
    await opening.promise;
    assert.equal(runtime.journal.available, true);
    assert.deepEqual(withoutHistory(runtime), stopped);
    finishOpen.resolve();
    await ready;
    assert.deepEqual(withoutHistory(runtime), { ...stopped, healthy: true });
    runtime.start();
    const pulse = runtime.pulse();
    await entered.promise;
    const active = { running: true, busy: false, healthy: true, lastError: null };
    assert.deepEqual(withoutHistory(runtime), { ...active, busy: true });
    release.resolve();
    await pulse;
    assert.deepEqual(withoutHistory(runtime), { ...active, lastError: "timeout" });
    dependency = undefined;
    await runtime.pulse();
    assert.deepEqual(withoutHistory(runtime), active);
    dependency = new KernelError("storage", "fixture");
    await assert.rejects(runtime.pulse(), KernelError);
    assert.deepEqual(withoutHistory(runtime), { ...stopped, lastError: "storage" });
  } finally {
    finishOpen.resolve();
    release.resolve();
    await runtime.close();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("插件健康与显式brief读取不依赖历史；默认status保留完整诊断", async () => {
  const directory = await Deno.makeTempDir({ prefix: "eve-plugin-health-" });
  const eve = config(directory);
  const plugin = createEvePlugin(`${directory}/config.json`);
  const handlers: Record<string, Handler> = {};
  let close: (() => void | Promise<void>) | undefined;
  const ctx = {
    provide(_contract: string, methods: typeof handlers) {
      Object.assign(handlers, methods);
    },
    onDisable(disable: () => void | Promise<void>) {
      close = disable;
    },
  } as PluginContext;
  try {
    await Deno.writeTextFile(eve.promptFile, "fixture");
    await Deno.writeTextFile(`${directory}/config.json`, JSON.stringify({ eve }));
    await plugin.load!(ctx);
    assert.throws(() => plugin.health!(ctx), KernelError);
    await plugin.enable!(ctx);
    const detailed = await handlers.status(null);
    assert.ok(
      detailed && typeof detailed === "object" && "budget" in detailed && "records" in detailed,
    );
    assert.deepEqual(await handlers.status({ brief: "true" }), detailed);
    await Deno.rename(`${eve.directory}/events`, `${directory}/offline-events`);
    try {
      await plugin.health!(ctx);
      assert.deepEqual(await handlers.status({ brief: true }), {
        running: false,
        busy: false,
        healthy: true,
        lastError: null,
      });
    } finally {
      await Deno.rename(`${directory}/offline-events`, `${eve.directory}/events`);
    }
    await close!();
    close = undefined;
    assert.throws(() => plugin.health!(ctx), KernelError);
  } finally {
    await close?.();
    await Deno.remove(directory, { recursive: true });
  }
});

Deno.test("小时检查：仅请求brief，未观测到健康不报绿也不臆断实例已坏", async () => {
  const script = new URL("../deployment/eve-hourly-check.py", import.meta.url).pathname;
  const result = await new Deno.Command("python3", {
    args: [
      "-IB",
      "-c",
      `
import contextlib, io, json, runpy, sys
from unittest.mock import patch
main = runpy.run_path(sys.argv[1])['main']
configs = {
    '/etc/eve-rebuild/supervisor.json': {'supervisor': 'control'},
    '/etc/eve-rebuild/instances/test/config.json': {'instance': 'instance', 'phone': 'phone'},
    '/etc/eve-rebuild/qq.json': {'qq': {'modelControl': {'ownerPlatformUserId': 'test'}}},
}
def check(live, expected, issues, gateway='webhook_verified'):
    calls = []
    def rpc(config, method, args=None, channel='private'):
        if config == 'control' and method == 'status':
            return {'supervisor': {'current': 'test'}, 'runtime': {'available': True, 'throughSeq': 0},
                    'ownership': {'mode': 'active'}}
        if config == 'phone' and method == 'health':
            return {'available': True, 'gateway': {'status': gateway}}
        if config == 'instance' and method == 'status':
            calls.append(args)
            if isinstance(live, Exception): raise live
            return live
        if method == 'query' and args['resource'] == 'qqHistory': return {'messages': []}
        if method == 'query' and args['resource'] == 'qqFailures': return {'items': []}
        raise ValueError('unexpected call')
    stdout = io.StringIO()
    with patch.dict(main.__globals__, read=configs.__getitem__, rpc=rpc), contextlib.redirect_stdout(stdout):
        code = main()
    report = json.loads(stdout.getvalue())
    assert calls == [{'brief': True}], calls
    assert (report['instance_probe'], report['issues'], code) == (expected, issues, int(bool(issues))), report

active = {'state': 'active', 'eve': {'running': True, 'healthy': True, 'busy': True, 'lastError': None}}
for gateway in ('ready', 'resumed', 'webhook_verified'): check(active, 'healthy', [], gateway)
check(active, 'healthy', ['qq_unavailable'], 'webhook_listening')
for live in ({'state': 'standby'}, {'state': 'active', 'eve': {'running': False}},
             {'state': 'active', 'eve': {'running': True, 'healthy': False}}, dict(active, error='storage')):
    check(live, 'unhealthy', ['instance_unhealthy'])
for live in ({}, {'state': 'active', 'eve': None, 'error': 'timeout'},
             {'state': 'active', 'eve': {'healthy': 1, 'running': 'true'}}):
    check(live, 'unknown', ['instance_probe_unknown'])
check(None, 'unknown', ['instance:ValueError'])
check(TimeoutError(), 'unknown', ['instance:TimeoutError'])
`,
      script,
    ],
    stdout: "piped",
    stderr: "piped",
  }).output();
  assert.ok(result.success, new TextDecoder().decode(result.stderr));
});
