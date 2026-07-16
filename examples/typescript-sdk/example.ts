async function main() {
  const baseUrl = "http://127.0.0.1:7777";

  const session = await fetch(`${baseUrl}/sessions`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ profile: "default", proxy: null }),
  }).then((r) => r.json());

  const page = await fetch(`${baseUrl}/pages`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ session_id: session.id }),
  }).then((r) => r.json());

  const nav = await fetch(`${baseUrl}/navigate`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ page_id: page.id, url: "https://example.com" }),
  }).then((r) => r.json());

  console.log({ session, page, nav });
}

main().catch(console.error);
