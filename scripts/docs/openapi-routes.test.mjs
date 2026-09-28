import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

const root = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "../..");

function axumRoutes(source) {
  const routes = [];
  for (const match of source.matchAll(/\.route\(\s*"(\/[^"]+)"\s*,/gu)) {
    let depth = 0;
    let end = -1;
    for (let index = source.indexOf("(", match.index); index < source.length; index += 1) {
      if (source[index] === "(") depth += 1;
      if (source[index] === ")" && --depth === 0) {
        end = index;
        break;
      }
    }
    assert.notEqual(end, -1, `unclosed Axum route ${match[1]}`);
    const declaration = source.slice(match.index, end + 1);
    const methods = [...declaration.matchAll(/\b(get|post|put|patch|delete)\s*\(/gu)];
    assert.ok(methods.length, `route ${match[1]} has no HTTP method`);
    for (const method of methods) routes.push(`${method[1].toUpperCase()} ${match[1]}`);
  }
  return routes;
}

function documentedRoutes(yaml) {
  const routes = [];
  let currentPath = null;
  for (const line of yaml.split(/\r?\n/u)) {
    const pathMatch = /^  (\/[^:]+):\s*$/u.exec(line);
    if (pathMatch) currentPath = pathMatch[1];
    else if (/^\S/u.test(line)) currentPath = null;
    const methodMatch = /^    (get|post|put|patch|delete):\s*$/u.exec(line);
    if (currentPath && methodMatch) routes.push(`${methodMatch[1].toUpperCase()} ${currentPath}`);
  }
  return routes;
}

test("OpenAPI operations match the broker's HTTP routes", async () => {
  const [routesSource, brokerSource, openapi] = await Promise.all([
    readFile(path.join(root, "crates/broker/src/routes.rs"), "utf8"),
    readFile(path.join(root, "crates/broker/src/lib.rs"), "utf8"),
    readFile(path.join(root, "docs/bobby-browser/source/openapi/v1.yaml"), "utf8"),
  ]);
  const implemented = [...axumRoutes(routesSource), ...axumRoutes(brokerSource)].sort();
  const documented = documentedRoutes(openapi).sort();
  assert.deepEqual(documented, implemented);
});
