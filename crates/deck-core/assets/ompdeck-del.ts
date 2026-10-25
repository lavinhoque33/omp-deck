// ompdeck-del.ts — OmpDeck session-delete extension (batch).
// Run headless: env OMPDECK_DEL_TARGETS=<file, one NDJSON {"path":"<absolute .jsonl>","id":"<expected session id>"} per line> OMPDECK_DEL_OUT=<result file>
//   omp --mode rpc --no-session --no-extensions -e <path>
// Inert when either variable is unset. Appends one JSON line per target to OMPDECK_DEL_OUT, as soon as
// that target is done (each target under its own session lease):
//   {"target":"…","ok":true,"id":"…"} | {"target":"…","ok":false,"error":"live"|"changed"|"<message>"}
// A target without a line means failure (e.g. an omp-internal specifier below no longer resolves).
import * as fs from "node:fs";

const TARGETS = process.env.OMPDECK_DEL_TARGETS;
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
  if (!TARGETS || !OUT) return; // not launched by OmpDeck -> inert
  let targets: { path: string; id: string }[];
  try {
    targets = fs.readFileSync(TARGETS, "utf8").split("\n").filter((t) => t.length > 0).map((t) => JSON.parse(t))
      .filter((t) => typeof t?.path === "string" && typeof t?.id === "string" && t.id.length > 0);
  } catch (e) {
    return;
  }
  let storage: any;
  let acquire: any;
  try {
    const mod = await import("@oh-my-pi/pi-coding-agent/session/session-storage");
    storage = new mod.FileSessionStorage();
    acquire = mod.tryAcquireSessionLease;
  } catch (e) {
    // No target gets a line: the host reports omp's stderr.
    return;
  }
  for (const { path: target, id: expected } of targets) {
    try {
      const lease = acquire(expected);
      if (!lease) { report({ target, ok: false, error: "live", id: expected }); continue; }
      let deleted = false;
      try {
        // Re-read the header id under the lease: the file at `target` may have been replaced since selection.
        if (headerId(target) === expected) {
          await storage.deleteSessionWithArtifacts(target);
          deleted = true;
        }
      } finally {
        try { await lease.release(); } catch {}
      }
      report(deleted ? { target, ok: true, id: expected } : { target, ok: false, error: "changed", id: expected });
    } catch (e) {
      report({ target, ok: false, error: String((e as any)?.message ?? e) });
    }
  }
}
