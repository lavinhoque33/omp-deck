// ompdeck-del.ts — OmpDeck session-delete extension.
// Run headless: env OMPDECK_DEL_TARGET=<abs .jsonl> OMPDECK_DEL_OUT=<result file> omp --mode rpc --no-session --no-extensions -e <path>
// Inert when either variable is unset. Appends exactly one JSON line to OMPDECK_DEL_OUT:
//   {"ok":true,"id":"…"} | {"ok":false,"error":"live"|"no session header"|"<message>"}
// A missing line means failure (e.g. an omp-internal specifier below no longer resolves).
import * as fs from "node:fs";

const TARGET = process.env.OMPDECK_DEL_TARGET;
const OUT = process.env.OMPDECK_DEL_OUT;

const report = (r: unknown) => {
  try { fs.appendFileSync(OUT!, JSON.stringify(r) + "\n"); } catch {}
};

// The session id is the `id` of the header (type "session"): line 1, or line 2 when omp put a padded
// `title` line first. Only the first 16 KiB / 3 lines are examined.
function headerId(file: string): string | undefined {
  let fd: number | undefined;
  try {
    fd = fs.openSync(file, "r");
    const buf = Buffer.alloc(16 * 1024);
    const n = fs.readSync(fd, buf, 0, buf.length, 0);
    const lines = buf.subarray(0, n).toString("utf8").split("\n").slice(0, 3);
    for (const line of lines) {
      let h: any;
      try { h = JSON.parse(line); } catch { continue; }
      if (h?.type === "session") return typeof h.id === "string" && h.id ? h.id : undefined;
    }
    return undefined;
  } catch {
    return undefined;
  } finally {
    if (fd !== undefined) try { fs.closeSync(fd); } catch {}
  }
}

export default async function ompdeckDel(_api: any) {
  if (!TARGET || !OUT) return; // not launched by OmpDeck -> inert
  try {
    const { FileSessionStorage, tryAcquireSessionLease } = await import(
      "@oh-my-pi/pi-coding-agent/session/session-storage"
    );
    const id = headerId(TARGET);
    if (!id) return report({ ok: false, error: "no session header" });
    const lease = tryAcquireSessionLease(id);
    if (!lease) return report({ ok: false, error: "live", id });
    try {
      await new FileSessionStorage().deleteSessionWithArtifacts(TARGET);
    } finally {
      try { await lease.release(); } catch {}
    }
    report({ ok: true, id });
  } catch (e) {
    report({ ok: false, error: String((e as any)?.message ?? e) });
  }
}
