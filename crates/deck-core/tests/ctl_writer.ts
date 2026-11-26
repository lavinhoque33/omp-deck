// Exercise the production writer with controlled numeric (Bun) and boolean (node:net) writes.
import { strict as assert } from "node:assert";
const source = await Bun.file(new URL("../assets/ompdeck-ctl.ts", import.meta.url)).text();
const body = source.slice(source.indexOf("      const MAX_BACKLOG ="), source.indexOf("      // Only this main-session factory registers tools."));
assert(body.includes("const drain =") && body.includes("const write ="));
const code = `
const PIPE = false;
let closed = 0;
const onClose = s => {
  if (!s.data.writer.closed) closed++;
  s.data.writer.closed = true;
  s.data.writer.queue.length = 0;
  s.data.writer.bytes = 0;
  s.data.writer.snapshot = undefined;
};
${body}
return { write, drain, closed: () => closed };
`;
const makeWriter = () => new Function(new Bun.Transpiler({ loader: "ts" }).transformSync(code))();
const makeSocket = (returns: (number | boolean | Error)[]) => {
  const chunks: Buffer[] = [];
  let calls = 0, terminated = false;
  return {
    data: { writer: { queue: [], bytes: 0, closed: false } },
    chunks,
    calls: () => calls,
    terminated: () => terminated,
    terminate() { terminated = true; },
    write(bytes: Uint8Array) {
      calls++;
      const result = returns.shift() ?? bytes.length;
      if (result instanceof Error) throw result;
      if (typeof result === "boolean") chunks.push(Buffer.from(bytes));
      else if (result >= 0) chunks.push(Buffer.from(bytes.subarray(0, result)));
      return result;
    },
  };
};
{
  const writer = makeWriter(), socket = makeSocket([1, 0, 2, 0]);
  writer.write(socket, "界\n", true); // stop inside a UTF-8 code point
  writer.write(socket, "old snapshot\n", true);
  writer.write(socket, "reply\n");
  writer.write(socket, "latest snapshot\n", true);
  writer.write(socket, "tool\n");
  writer.write(socket, "tool_cancel\n");
  assert.equal(socket.data.writer.bytes, Buffer.byteLength("界\nreply\nlatest snapshot\ntool\ntool_cancel\n") - 1);
  assert.equal(socket.calls(), 1, "new lines must not retry a blocked socket");
  writer.drain(socket); // zero bytes: preserve the tail
  writer.drain(socket); // another partial code point
  writer.drain(socket); // zero bytes again
  writer.drain(socket);
  assert.equal(Buffer.concat(socket.chunks).toString(), "界\nreply\nlatest snapshot\ntool\ntool_cancel\n");
  assert.equal(socket.data.writer.bytes, 0);
  assert.equal(socket.data.writer.queue.length, 0);
}
{
  const writer = makeWriter(), socket = makeSocket([0]);
  writer.write(socket, "old\n", true);
  writer.write(socket, "new\n", true); // a zero-byte write has not started the snapshot
  writer.drain(socket);
  assert.equal(Buffer.concat(socket.chunks).toString(), "new\n");
}
for (const result of [-1, new Error("closed")]) {
  const writer = makeWriter(), socket = makeSocket([result]);
  writer.write(socket, "tool\n");
  assert(socket.terminated());
  assert.equal(writer.closed(), 1);
  writer.write(socket, "ignored\n");
  writer.drain(socket);
  assert.equal(socket.calls(), 1);
}
{
  const writer = makeWriter(), socket = makeSocket([0]);
  writer.write(socket, "x".repeat(64 * 1024 * 1024));
  assert(!socket.terminated(), "exactly 64 MiB is allowed");
  writer.write(socket, "x");
  assert(socket.terminated(), "more than 64 MiB must destroy the connection");
  assert.equal(writer.closed(), 1);
}
{
  const writer = makeWriter(), socket = makeSocket([false, true, false]);
  writer.write(socket, "hello\n");
  writer.write(socket, "界\n", true);
  writer.write(socket, "reply\n");
  assert.equal(Buffer.concat(socket.chunks).toString(), "hello\n界\nreply\n");
  assert.equal(socket.data.writer.bytes, 0, "node buffers false writes itself");
}
console.log(`Bun ${Bun.version}: writer partial/zero/UTF-8/FIFO/coalescing/cap/closed/node regressions passed`);
