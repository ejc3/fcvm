/**
 * ComputeSDK provider for `fcvm serve`, in the shape of the published provider
 * packages (`defineProvider` from @computesdk/provider). It is copied into a
 * checkout of computesdk/benchmarks by run.sh and registered there as the
 * `fcvm` participant.
 *
 * create  -> POST   {url}/v1/sandboxes   (returns once the sandbox is healthy)
 * run     -> POST   {url}/s/{id}/run/command
 * destroy -> DELETE {url}/v1/sandboxes/{id}
 */
import { defineProvider } from '@computesdk/provider';

export interface FcvmConfig {
  /** Base URL of `fcvm serve`, e.g. http://127.0.0.1:8090 */
  url: string;
  /** Runtime passed to serve; `node` maps to node:22-slim. */
  runtime?: string;
  cpu?: number;
  memoryMib?: number;
}

interface FcvmSandbox {
  id: string;
  base: string;
}

async function call(method: string, url: string, body?: unknown): Promise<any> {
  const response = await fetch(url, {
    method,
    headers: body === undefined ? {} : { 'Content-Type': 'application/json' },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  const text = await response.text();
  if (!response.ok) {
    throw new Error(`fcvm ${method} ${url}: ${response.status} ${text.slice(0, 300)}`);
  }
  return text ? JSON.parse(text) : {};
}

export const fcvm = defineProvider<FcvmSandbox, FcvmConfig>({
  name: 'fcvm',
  methods: {
    instances: {
      create: async (config) => {
        const resp = await call('POST', `${config.url}/v1/sandboxes`, {
          runtime: config.runtime ?? 'node',
          cpu: config.cpu,
          memory_mib: config.memoryMib,
        });
        if (!resp.success) throw new Error(`fcvm create failed: ${JSON.stringify(resp)}`);
        const id: string = resp.data.sandboxId;
        return { sandbox: { id, base: config.url }, sandboxId: id };
      },
      getById: async (config, id) => {
        try {
          const resp = await call('GET', `${config.url}/v1/sandboxes/${encodeURIComponent(id)}`);
          return resp.success ? { sandbox: { id, base: config.url }, sandboxId: id } : null;
        } catch {
          return null;
        }
      },
      list: async (config) => {
        const resp = await call('GET', `${config.url}/v1/sandboxes`);
        return (resp.data ?? []).map((s: any) => ({
          sandbox: { id: s.sandboxId, base: config.url },
          sandboxId: s.sandboxId,
        }));
      },
      destroy: async (config, id) => {
        await call('DELETE', `${config.url}/v1/sandboxes/${encodeURIComponent(id)}`);
      },
      runCommand: async (sandbox, command, options) => {
        const start = Date.now();
        const resp = await call('POST', `${sandbox.base}/s/${encodeURIComponent(sandbox.id)}/run/command`, {
          command,
          cwd: options?.cwd,
          env: options?.env,
        });
        return {
          stdout: resp.data.stdout,
          stderr: resp.data.stderr,
          exitCode: resp.data.exit_code,
          durationMs: Date.now() - start,
        };
      },
      getInfo: async (sandbox) => ({
        id: sandbox.id,
        provider: 'fcvm',
        status: 'running',
        createdAt: new Date(),
        timeout: 0,
        metadata: {},
      }),
      getUrl: async () => {
        throw new Error('fcvm serve does not expose sandbox ports');
      },
    },
  },
});
