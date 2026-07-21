import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

import {
  MAX_NATIVE_MESSAGE_BYTES,
  NativeCompanionTransport,
} from "../src/native-transport.js";

class ListenerSet<T extends (...arguments_: never[]) => unknown> {
  listeners: T[] = [];

  addListener(listener: T): void {
    this.listeners.push(listener);
  }

  emit(...arguments_: Parameters<T>): void {
    for (const listener of this.listeners) listener(...arguments_);
  }
}

class FakePort {
  readonly sent: unknown[] = [];
  readonly onMessage = new ListenerSet<(message: unknown) => void>();
  readonly onDisconnect = new ListenerSet<() => void>();
  disconnected = false;

  postMessage(message: unknown): void {
    this.sent.push(message);
  }

  disconnect(): void {
    this.disconnected = true;
  }
}

async function urlSecurityFixtures(): Promise<{ benign: string[]; secret: string[] }> {
  return JSON.parse(
    await readFile(
      new URL(
        "../../../crates/companion-core/tests/fixtures/extension-url-security.json",
        import.meta.url,
      ),
      "utf8",
    ),
  ) as { benign: string[]; secret: string[] };
}

function completedWithUrl(url: string): unknown {
  return {
    kind: "actionCompleted",
    output: {
      commandId: "4c4dfe8c-7c69-4b33-a13e-1fcdf18f2952",
      interactionPath: "extensionApi",
      output: { url },
    },
  };
}

test("native transport connects only to the approved host and enforces message direction", () => {
  const port = new FakePort();
  const hosts: string[] = [];
  const received: unknown[] = [];
  const transport = new NativeCompanionTransport({
    connectNative(hostName) {
      hosts.push(hostName);
      return port;
    },
  });

  transport.start((message) => {
    received.push(message);
  });
  transport.send({ kind: "pong" });
  port.onMessage.emit({ kind: "ping" });
  port.onMessage.emit({ kind: "notAProtocolMessage" });

  assert.deepEqual(hosts, ["com.bobby_browser.companion"]);
  assert.deepEqual(port.sent, [{ kind: "pong" }]);
  assert.deepEqual(received, [{ kind: "ping" }]);
});

test("native transport rejects requests outbound and events inbound", () => {
  const port = new FakePort();
  const received: unknown[] = [];
  const transport = new NativeCompanionTransport({ connectNative: () => port });
  transport.start((message) => {
    received.push(message);
  });

  assert.throws(() => transport.send({ kind: "ping" }), /direction|outbound/i);
  port.onMessage.emit({ kind: "pong" });

  assert.deepEqual(port.sent, []);
  assert.deepEqual(received, []);
});

test("shared URL security fixtures match the TypeScript extension boundary", async () => {
  const port = new FakePort();
  const transport = new NativeCompanionTransport({ connectNative: () => port });
  transport.start(() => {});
  const fixtures = await urlSecurityFixtures();

  for (const url of fixtures.benign) {
    assert.doesNotThrow(() => transport.send(completedWithUrl(url)), url);
  }
  for (const url of fixtures.secret) {
    assert.throws(() => transport.send(completedWithUrl(url)), /secret|URL/i, url);
  }
});

test("native pair metadata is exact and recursively secret free", () => {
  const port = new FakePort();
  const transport = new NativeCompanionTransport({ connectNative: () => port });
  transport.start(() => {});
  const base = {
    kind: "pair",
    input: {
      protocolVersion: 1,
      companionId: "companion-1",
      profileId: "profile-1",
      identity: {
        engine: "firefox",
        browserName: "Firefox",
        browserVersion: "stable",
        os: "macos",
        profileLabel: "default-release",
      },
      capabilities: {
        observe: true,
        navigate: true,
        nativeInput: false,
        tabs: true,
        frames: true,
        nativeDialogs: false,
      },
    },
  } as const;

  assert.throws(
    () =>
      transport.send({
        ...base,
        input: {
          ...base.input,
          identity: { ...base.input.identity, profileLabel: "Bearer private-token" },
        },
      }),
    /secret|pair/i,
  );
  assert.throws(
    () =>
      transport.send({
        ...base,
        input: {
          ...base.input,
          identity: { ...base.input.identity, endpoint: "ws://127.0.0.1:9000/private" },
        },
      }),
    /shape|secret|pair/i,
  );
  assert.deepEqual(port.sent, []);
});

test("paired events are delivered and listener failures are contained", async () => {
  const port = new FakePort();
  const errors: unknown[] = [];
  const transport = new NativeCompanionTransport({
    connectNative: () => port,
    onListenerError: (error: unknown) => errors.push(error),
  });
  transport.start(async (message) => {
    assert.deepEqual(message, {
      kind: "paired",
      output: { companionId: "companion-1", profileId: "profile-1" },
    });
    throw new Error("listener exploded");
  });

  port.onMessage.emit({
    kind: "paired",
    output: { companionId: "companion-1", profileId: "profile-1" },
  });
  await new Promise((resolve) => setImmediate(resolve));

  assert.equal(errors.length, 1);
  assert.match(String(errors[0]), /listener exploded/);
});

test("native transport rejects outbound messages larger than 1 MiB", () => {
  const port = new FakePort();
  const transport = new NativeCompanionTransport({ connectNative: () => port });
  transport.start(() => {});

  assert.throws(
    () =>
      transport.send({
        kind: "actionFailed",
        output: {
          commandId: "command-1",
          code: "failed",
          message: "x".repeat(MAX_NATIVE_MESSAGE_BYTES),
          effectUncertain: false,
        },
      }),
    /1 MiB/,
  );
  assert.deepEqual(port.sent, []);
});

test("native transport reconnects after a native host disconnect", () => {
  const ports = [new FakePort(), new FakePort()];
  const connected: FakePort[] = [];
  const delays: number[] = [];
  const scheduled: Array<() => void> = [];
  const transport = new NativeCompanionTransport({
    connectNative() {
      const port = ports[connected.length];
      assert.ok(port);
      connected.push(port);
      return port;
    },
    scheduleReconnect(callback, delayMs) {
      delays.push(delayMs);
      scheduled.push(callback);
      return 1;
    },
    cancelReconnect() {},
  });

  transport.start(() => {});
  const pairRequest = {
    kind: "pair",
    input: {
      protocolVersion: 1,
      companionId: "companion-1",
      profileId: "profile-1",
      identity: {
        engine: "firefox",
        browserName: "Firefox",
        browserVersion: "stable",
        os: "macos",
        profileLabel: "default-release",
      },
      capabilities: {
        observe: true,
        navigate: true,
        nativeInput: false,
        tabs: true,
        frames: true,
        nativeDialogs: false,
      },
    },
  } as const;
  transport.send(pairRequest);
  assert.equal(scheduled.length, 0);
  connected[0]?.onDisconnect.emit();
  assert.deepEqual(delays, [100]);
  scheduled[0]?.();

  assert.deepEqual(connected, ports);
  assert.deepEqual(connected[1]?.sent, [pairRequest]);
});

test("terminal invalid-auth status prevents native host respawn", () => {
  const port = new FakePort();
  const delays: number[] = [];
  const transport = new NativeCompanionTransport({
    connectNative: () => port,
    scheduleReconnect(_callback, delayMs) {
      delays.push(delayMs);
      return 1;
    },
    cancelReconnect() {},
  });
  transport.start(() => {});

  port.onMessage.emit({
    kind: "nativeStatus",
    output: { state: "invalidAuth" },
  });
  port.onDisconnect.emit();
  transport.start(() => {});

  assert.deepEqual(delays, []);
});

test("failed native launches use bounded exponential backoff and reset after success", () => {
  const delays: number[] = [];
  const scheduled: Array<() => void> = [];
  const successfulPort = new FakePort();
  let attempts = 0;
  const transport = new NativeCompanionTransport({
    connectNative() {
      attempts += 1;
      if (attempts <= 7) throw new Error("host offline");
      return successfulPort;
    },
    scheduleReconnect(callback, delayMs) {
      delays.push(delayMs);
      scheduled.push(callback);
      return attempts;
    },
    cancelReconnect() {},
  });
  transport.start(() => {});
  while (attempts <= 7) {
    const callback = scheduled.shift();
    assert.ok(callback);
    callback();
  }

  assert.deepEqual(delays, [100, 200, 400, 800, 1_600, 3_200, 5_000]);
  successfulPort.onMessage.emit({
    kind: "paired",
    output: { companionId: "companion-1", profileId: "profile-1" },
  });
  successfulPort.onDisconnect.emit();
  assert.equal(delays.at(-1), 100);
});

test("silent native ports do not reset reconnect backoff before a validated message", () => {
  const ports = Array.from({ length: 9 }, () => new FakePort());
  const delays: number[] = [];
  const scheduled: Array<() => void> = [];
  let connections = 0;
  const transport = new NativeCompanionTransport({
    connectNative() {
      const port = ports[connections];
      assert.ok(port);
      connections += 1;
      return port;
    },
    scheduleReconnect(callback, delayMs) {
      delays.push(delayMs);
      scheduled.push(callback);
      return connections;
    },
    cancelReconnect() {},
  });
  transport.start(() => {});

  for (let index = 0; index < 8; index += 1) {
    ports[index]?.onDisconnect.emit();
    const reconnect = scheduled.shift();
    assert.ok(reconnect);
    reconnect();
  }

  assert.deepEqual(delays, [100, 200, 400, 800, 1_600, 3_200, 5_000, 5_000]);
  ports[8]?.onMessage.emit({
    kind: "paired",
    output: { companionId: "companion-1", profileId: "profile-1" },
  });
  ports[8]?.onDisconnect.emit();
  assert.equal(delays.at(-1), 100);
});
