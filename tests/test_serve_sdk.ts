/**
 * Comprehensive E2E test for fcvm serve using the public ComputeSDK API.
 *
 * This test is fully self-contained: it starts `fcvm serve` on a random port,
 * runs all tests using the standard `computesdk` package, then cleans up.
 *
 * Usage:
 *   npx tsx tests/test_serve_sdk.ts
 */

import { compute, Sandbox } from 'computesdk';
import { spawn, ChildProcess } from 'child_process';
import * as net from 'net';
import * as path from 'path';
import * as fs from 'fs';
import { fileURLToPath } from 'url';

const __filename = fileURLToPath(import.meta.url);
const __dirname = path.dirname(__filename);

// ============================================================================
// Helpers
// ============================================================================

/** Find a free port by binding to port 0 */
function findFreePort(): Promise<number> {
  return new Promise((resolve, reject) => {
    const server = net.createServer();
    server.listen(0, () => {
      const addr = server.address();
      if (!addr || typeof addr === 'string') {
        reject(new Error('Could not get port'));
        return;
      }
      const port = addr.port;
      server.close(() => resolve(port));
    });
    server.on('error', reject);
  });
}

/** Find the fcvm binary */
function findFcvmBinary(): string {
  // Check relative to project root
  const candidates = [
    path.join(process.cwd(), 'target/release/fcvm'),
    path.join(__dirname, '..', 'target/release/fcvm'),
  ];
  for (const p of candidates) {
    if (fs.existsSync(p)) return p;
  }
  // Fall back to PATH
  return 'fcvm';
}

/** Wait for server to accept connections */
async function waitForServer(url: string, timeoutMs: number = 30000): Promise<void> {
  const start = Date.now();
  while (Date.now() - start < timeoutMs) {
    try {
      const resp = await fetch(`${url}/v1/sandboxes`);
      if (resp.ok) return;
    } catch {
      // Server not ready yet
    }
    await new Promise(r => setTimeout(r, 500));
  }
  throw new Error(`Server at ${url} did not start within ${timeoutMs}ms`);
}

let passed = 0;
let failed = 0;

function pass(name: string) {
  passed++;
  console.log(`  ✓ ${name}`);
}

function fail(name: string, err: any) {
  failed++;
  console.error(`  ✗ ${name}: ${err}`);
}

function assert(condition: boolean, message: string) {
  if (!condition) throw new Error(`Assertion failed: ${message}`);
}

function assertEqual(actual: any, expected: any, message: string) {
  if (actual !== expected) {
    throw new Error(`${message}: expected ${JSON.stringify(expected)}, got ${JSON.stringify(actual)}`);
  }
}

// ============================================================================
// Main Test Suite
// ============================================================================

async function main() {
  console.log('=== fcvm serve — ComputeSDK E2E Integration Test ===\n');

  // 1. Find fcvm binary and start server
  const fcvmPath = findFcvmBinary();
  console.log(`Binary: ${fcvmPath}`);

  const port = await findFreePort();
  const gatewayUrl = `http://localhost:${port}`;
  console.log(`Port: ${port}`);
  console.log(`Gateway: ${gatewayUrl}\n`);

  const serveProcess: ChildProcess = spawn(fcvmPath, ['serve', '--port', port.toString()], {
    stdio: ['ignore', 'inherit', 'inherit'],
    env: { ...process.env, RUST_LOG: 'info' },
  });

  // Ensure cleanup on exit
  const cleanup = () => {
    if (serveProcess.pid) {
      try { process.kill(serveProcess.pid, 'SIGTERM'); } catch {}
    }
  };
  process.on('exit', cleanup);
  process.on('SIGINT', cleanup);
  process.on('SIGTERM', cleanup);

  try {
    // Wait for server
    console.log('Waiting for server...');
    await waitForServer(gatewayUrl);
    console.log('Server ready.\n');

    // 2. Configure ComputeSDK
    compute.setConfig({
      provider: 'fcvm',
      computesdkApiKey: 'local',
      gatewayUrl,
      fcvm: { apiKey: 'local' },
    });

    // =====================================================================
    // Test: Create Sandbox
    // =====================================================================
    console.log('--- Sandbox Lifecycle ---');
    let sandbox: Sandbox;
    try {
      sandbox = await compute.sandbox.create();
      assert(!!sandbox.sandboxId, 'sandboxId should be set');
      pass(`create sandbox (id=${sandbox.sandboxId})`);
    } catch (err) {
      fail('create sandbox', err);
      throw err; // Can't continue without a sandbox
    }

    // =====================================================================
    // Test: Health Check
    // =====================================================================
    try {
      const health = await sandbox.health();
      assert(health.status === 'ok', `health status should be "ok", got "${health.status}"`);
      pass('health check');
    } catch (err) {
      fail('health check', err);
    }

    // =====================================================================
    // Test: Ready Check
    // =====================================================================
    try {
      const ready = await sandbox.ready();
      assert(ready.ready === true, 'ready should be true');
      pass('ready check');
    } catch (err) {
      fail('ready check', err);
    }

    // =====================================================================
    // Test: Run Code (Python)
    // =====================================================================
    console.log('\n--- Code Execution ---');
    try {
      const result = await sandbox.runCode('print(40 + 2)');
      assertEqual(result.output.trim(), '42', 'runCode output');
      assertEqual(result.exitCode, 0, 'runCode exitCode');
      assertEqual(result.language, 'python', 'runCode language');
      pass('runCode (python arithmetic)');
    } catch (err) {
      fail('runCode (python arithmetic)', err);
    }

    // Test: Run code with multi-line output
    try {
      const result = await sandbox.runCode('for i in range(3): print(f"line {i}")');
      const lines = result.output.trim().split('\n');
      assertEqual(lines.length, 3, 'multi-line output line count');
      assertEqual(lines[0], 'line 0', 'first line');
      assertEqual(lines[2], 'line 2', 'last line');
      pass('runCode (multi-line output)');
    } catch (err) {
      fail('runCode (multi-line output)', err);
    }

    // Test: Run code with non-zero exit
    try {
      const result = await sandbox.runCode('import sys; sys.exit(42)');
      assertEqual(result.exitCode, 42, 'non-zero exit code');
      pass('runCode (non-zero exit)');
    } catch (err) {
      fail('runCode (non-zero exit)', err);
    }

    // Test: Run code with stderr
    try {
      const result = await sandbox.runCode('import sys; print("err", file=sys.stderr)');
      assert(result.output.includes('err'), 'stderr should be in output');
      pass('runCode (stderr)');
    } catch (err) {
      fail('runCode (stderr)', err);
    }

    // =====================================================================
    // Test: Run Command
    // =====================================================================
    console.log('\n--- Command Execution ---');
    try {
      const result = await sandbox.runCommand('echo hello world');
      assertEqual(result.stdout.trim(), 'hello world', 'runCommand stdout');
      assertEqual(result.exitCode, 0, 'runCommand exitCode');
      assert(result.durationMs >= 0, 'durationMs should be >= 0');
      pass('runCommand (echo)');
    } catch (err) {
      fail('runCommand (echo)', err);
    }

    // Test: Run command with env vars
    try {
      const result = await sandbox.runCommand('echo $MY_VAR', { env: { MY_VAR: 'test_value' } });
      assertEqual(result.stdout.trim(), 'test_value', 'env var in command');
      pass('runCommand (env vars)');
    } catch (err) {
      fail('runCommand (env vars)', err);
    }

    // Test: Run command with cwd
    try {
      const result = await sandbox.runCommand('pwd', { cwd: '/tmp' });
      assertEqual(result.stdout.trim(), '/tmp', 'cwd in command');
      pass('runCommand (cwd)');
    } catch (err) {
      fail('runCommand (cwd)', err);
    }

    // Test: Run command with non-zero exit
    try {
      const result = await sandbox.runCommand('exit 7');
      assertEqual(result.exitCode, 7, 'non-zero exit code');
      pass('runCommand (non-zero exit)');
    } catch (err) {
      fail('runCommand (non-zero exit)', err);
    }

    // =====================================================================
    // Test: Filesystem Operations
    // =====================================================================
    console.log('\n--- Filesystem ---');

    // Write file
    try {
      await sandbox.filesystem.writeFile('/tmp/sdk-test.txt', 'hello from computesdk');
      pass('filesystem.writeFile');
    } catch (err) {
      fail('filesystem.writeFile', err);
    }

    // Read file
    try {
      const content = await sandbox.filesystem.readFile('/tmp/sdk-test.txt');
      assertEqual(content.trim(), 'hello from computesdk', 'readFile content');
      pass('filesystem.readFile');
    } catch (err) {
      fail('filesystem.readFile', err);
    }

    // File exists
    try {
      const exists = await sandbox.filesystem.exists('/tmp/sdk-test.txt');
      assertEqual(exists, true, 'exists for existing file');
      pass('filesystem.exists (true)');
    } catch (err) {
      fail('filesystem.exists (true)', err);
    }

    // File not exists
    try {
      const exists = await sandbox.filesystem.exists('/tmp/does-not-exist-xyz.txt');
      assertEqual(exists, false, 'exists for missing file');
      pass('filesystem.exists (false)');
    } catch (err) {
      fail('filesystem.exists (false)', err);
    }

    // Mkdir
    try {
      await sandbox.filesystem.mkdir('/tmp/sdk-test-dir/nested');
      const result = await sandbox.runCommand('test -d /tmp/sdk-test-dir/nested && echo yes');
      assertEqual(result.stdout.trim(), 'yes', 'mkdir created directory');
      pass('filesystem.mkdir');
    } catch (err) {
      fail('filesystem.mkdir', err);
    }

    // Write file in new directory
    try {
      await sandbox.filesystem.writeFile('/tmp/sdk-test-dir/nested/file.txt', 'nested content');
      const content = await sandbox.filesystem.readFile('/tmp/sdk-test-dir/nested/file.txt');
      assertEqual(content.trim(), 'nested content', 'nested file content');
      pass('filesystem.writeFile + readFile (nested)');
    } catch (err) {
      fail('filesystem.writeFile + readFile (nested)', err);
    }

    // Readdir
    try {
      // Create some files for listing
      await sandbox.filesystem.writeFile('/tmp/sdk-list-test/a.txt', 'a');
      await sandbox.filesystem.writeFile('/tmp/sdk-list-test/b.txt', 'b');
      const entries = await sandbox.filesystem.readdir('/tmp/sdk-list-test');
      assert(entries.length >= 2, `readdir should return >= 2 entries, got ${entries.length}`);
      const names = entries.map(e => e.name).sort();
      assert(names.includes('a.txt'), 'readdir should include a.txt');
      assert(names.includes('b.txt'), 'readdir should include b.txt');
      pass('filesystem.readdir');
    } catch (err) {
      fail('filesystem.readdir', err);
    }

    // Remove file
    try {
      await sandbox.filesystem.remove('/tmp/sdk-test.txt');
      const exists = await sandbox.filesystem.exists('/tmp/sdk-test.txt');
      assertEqual(exists, false, 'file should be removed');
      pass('filesystem.remove');
    } catch (err) {
      fail('filesystem.remove', err);
    }

    // =====================================================================
    // Test: Low-level file API
    // =====================================================================
    console.log('\n--- File API (low-level) ---');

    // createFile
    try {
      await sandbox.createFile('/tmp/lowlevel.txt', 'low-level content');
      pass('createFile');
    } catch (err) {
      fail('createFile', err);
    }

    // readFile (low-level)
    try {
      const content = await sandbox.readFile('/tmp/lowlevel.txt');
      assertEqual(content.trim(), 'low-level content', 'readFile content');
      pass('readFile');
    } catch (err) {
      fail('readFile', err);
    }

    // checkFileExists
    try {
      const exists = await sandbox.checkFileExists('/tmp/lowlevel.txt');
      assertEqual(exists, true, 'checkFileExists');
      pass('checkFileExists (true)');
    } catch (err) {
      fail('checkFileExists (true)', err);
    }

    // listFiles
    try {
      const result = await sandbox.listFiles('/tmp');
      assert(result.data.files.length > 0, 'listFiles should return files');
      pass('listFiles');
    } catch (err) {
      fail('listFiles', err);
    }

    // deleteFile
    try {
      await sandbox.deleteFile('/tmp/lowlevel.txt');
      const exists = await sandbox.checkFileExists('/tmp/lowlevel.txt');
      assertEqual(exists, false, 'file should be deleted');
      pass('deleteFile');
    } catch (err) {
      fail('deleteFile', err);
    }

    // =====================================================================
    // Test: Run namespace (sandbox.run.code / sandbox.run.command)
    // =====================================================================
    console.log('\n--- Run Namespace ---');

    try {
      const result = await sandbox.run.code('print("via run namespace")');
      assertEqual(result.output.trim(), 'via run namespace', 'run.code output');
      pass('sandbox.run.code');
    } catch (err) {
      fail('sandbox.run.code', err);
    }

    try {
      const result = await sandbox.run.command('echo via run namespace');
      assertEqual(result.stdout.trim(), 'via run namespace', 'run.command stdout');
      pass('sandbox.run.command');
    } catch (err) {
      fail('sandbox.run.command', err);
    }

    // =====================================================================
    // Test: Get sandbox by ID
    // =====================================================================
    console.log('\n--- Sandbox Management ---');
    try {
      const found = await compute.sandbox.getById(sandbox.sandboxId);
      assert(found !== null, 'getById should find sandbox');
      assertEqual(found!.sandboxId, sandbox.sandboxId, 'getById sandboxId');
      pass('compute.sandbox.getById');
    } catch (err) {
      fail('compute.sandbox.getById', err);
    }

    // =====================================================================
    // Test: Destroy Sandbox
    // =====================================================================
    try {
      await sandbox.destroy();
      pass('sandbox.destroy');
    } catch (err) {
      fail('sandbox.destroy', err);
    }

    // Verify sandbox is gone
    try {
      const gone = await compute.sandbox.getById(sandbox.sandboxId);
      assertEqual(gone, null, 'destroyed sandbox should not be found');
      pass('verify sandbox destroyed');
    } catch (err) {
      fail('verify sandbox destroyed', err);
    }

    // =====================================================================
    // Summary
    // =====================================================================
    console.log(`\n${'='.repeat(50)}`);
    console.log(`Results: ${passed} passed, ${failed} failed`);
    console.log('='.repeat(50));

    if (failed > 0) {
      process.exit(1);
    }
  } finally {
    cleanup();
    // Wait a bit for server to shut down
    await new Promise(r => setTimeout(r, 1000));
  }

  process.exit(0);
}

main().catch(err => {
  console.error('Fatal error:', err);
  process.exit(1);
});
