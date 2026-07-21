import assert from "node:assert/strict";
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
