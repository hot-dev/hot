// Tiny Node smoke test for ::hot::box/start mounts.
// Reads HOT_NAME from env (defaults to "world"), echoes a structured JSON
// document to stdout, then exits. Intentionally has zero dependencies so
// the bundle can be small and the container needs nothing beyond `node`.

const name = process.env.HOT_NAME || "world";
const ts = new Date().toISOString();
const payload = {
  source: "hot/src-offline/frontpage-demos/resources/box-demos/node-hello/server.js",
  message: `hello, ${name}`,
  ts,
  // Echo the file contents back so the test can assert the bind actually
  // landed on disk inside the container, not just the env var.
  self: require("fs").statSync(__filename).size,
};
process.stdout.write(JSON.stringify(payload) + "\n");
