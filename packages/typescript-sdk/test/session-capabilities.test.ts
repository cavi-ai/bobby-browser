import assert from "node:assert/strict";
import { createServer } from "node:http";
import { readFileSync } from "node:fs";
import test from "node:test";
import { BrowserRuntimeClient, INTERFACE_VERSION, RuntimeClientError } from "../src/index.js";

const fixture = JSON.parse(readFileSync(new URL("../../../../tests/fixtures/session-capabilities.json", import.meta.url), "utf8"));

test("session capability discovery validates the new endpoint and response contract", async () => {
  let payload = structuredClone(fixture);
  const paths: string[] = [];
  const server = createServer((request, response) => {
    paths.push(`${request.method} ${request.url}`);
    response.writeHead(200, { "content-type": "application/json", "x-interface-version": INTERFACE_VERSION });
    response.end(JSON.stringify(payload));
  });
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address();
  if (!address || typeof address === "string") throw new Error("fixture must listen");
  try {
    const client = new BrowserRuntimeClient({ baseUrl: `http://127.0.0.1:${address.port}`, bearerToken: "test-token" });
    assert.deepEqual(await client.sessionCapabilities(fixture.sessionId), fixture);
    assert.equal(paths[0], `GET /v1/sessions/${fixture.sessionId}/capabilities`);
    for (const invalid of [
      { ...fixture, schemaVersion: 2 }, { ...fixture, connection: "fabricated" },
      { ...fixture, features: [] }, { ...fixture, secret: "unrecognized" },
      { ...fixture, sessionId: "00000000-0000-4000-8000-000000000006" },
      { ...fixture, features: [fixture.features[0], fixture.features[0]] },
    ]) {
      payload = invalid;
      await assert.rejects(client.sessionCapabilities(fixture.sessionId), RuntimeClientError);
    }
    const before = paths.length;
    await assert.rejects(client.sessionCapabilities("invalid/session"), RuntimeClientError);
    assert.equal(paths.length, before);
  } finally {
    await new Promise<void>((resolve, reject) => server.close(error => error ? reject(error) : resolve()));
  }
});
