// Headless OmpDeck authentication bridge. Only omp opens/writes its credential store.
// Static host imports are required by the compiled omp binary; all work runs in the factory.
import * as fs from "node:fs";
import * as path from "node:path";
import { discoverAuthStorage, ModelRegistry, Settings } from "@oh-my-pi/pi-coding-agent";
import { cfgDisabledProviders } from "@oh-my-pi/pi-coding-agent/config/model-settings";
import { listLogoutAccounts } from "@oh-my-pi/pi-coding-agent/slash-commands/helpers/logout";
import type { AuthStorage } from "@oh-my-pi/pi-coding-agent/session/auth-storage";
import { getOAuthProviders, getOAuthCredentialProvider } from "@oh-my-pi/pi-ai/oauth";
import { getProjectDir } from "@oh-my-pi/pi-utils";

interface Prompt { message: string; placeholder?: string; allowEmpty?: boolean; secret?: boolean }
interface AuthInfo { url: string; launchUrl?: string; instructions?: string }
interface PendingAnswer { resolve(value: string): void; reject(reason: Error): void }

function record(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null;
}

export default async function ompdeckAuth(_api: unknown) {
  const requestText = process.env.OMPDECK_AUTH_REQ;
  const output = process.env.OMPDECK_AUTH_OUT;
  const directory = process.env.OMPDECK_AUTH_DIR;
  if (!requestText || !output || !directory) return;

  // Each event is durable before the callback returns. Neither answers nor credentials are reported.
  const report = (event: unknown) => {
    const fd = fs.openSync(output, "a", 0o600);
    try {
      fs.writeSync(fd, JSON.stringify(event) + "\n");
      fs.fsyncSync(fd);
    } finally { fs.closeSync(fd); }
  };
  const controller = new AbortController();
  const pending = new Map<number, PendingAnswer>();
  let sequence = 0;
  let auth: AuthStorage | undefined;
  let outcome: unknown;
  let timer: NodeJS.Timeout | undefined;
  const cap = setTimeout(() => controller.abort(new Error("Login timed out")), 15 * 60 * 1000);
  const abortPending = () => {
    for (const answer of pending.values()) answer.reject(new Error("Login cancelled"));
    pending.clear();
  };
  controller.signal.addEventListener("abort", abortPending);

  const ask = (event: { t: "manual" } | { t: "prompt"; message: string; placeholder?: string; allowEmpty: boolean; secret: boolean }) => {
    if (controller.signal.aborted) return Promise.reject(new Error("Login cancelled"));
    const seq = ++sequence;
    const { promise, resolve, reject } = Promise.withResolvers<string>();
    pending.set(seq, { resolve, reject });
    report({ ...event, seq });
    return promise;
  };

  try {
    const request: unknown = JSON.parse(requestText);
    if (!record(request)) throw new Error("Invalid authentication request");
    const settings = await Settings.init({ cwd: getProjectDir() });
    auth = await discoverAuthStorage(undefined, { settings });
    const registry = new ModelRegistry(auth);
    if (request.op === "list") {
      const disabled = new Set(cfgDisabledProviders.get(settings));
      const providers = [];
      for (const provider of getOAuthProviders()) {
        const store = provider.storeCredentialsAs ?? provider.id;
        const accounts = await listLogoutAccounts(auth, provider.id, "ompdeck");
        const source = auth.keys.source(store);
        providers.push({
          id: provider.id, name: provider.name, available: provider.available, store,
          disabled: disabled.has(provider.id) || disabled.has(store),
          origin: source ? { kind: source.kind, envVar: "envVar" in source ? source.envVar : undefined } : null,
          accounts: accounts.map(account => ({
            credentialId: account.credentialId, label: account.label, detail: account.detail,
            kind: account.type, active: account.active,
          })),
        });
      }
      outcome = { t: "providers", providers };
    } else if (request.op === "logout") {
      if (typeof request.provider !== "string" || typeof request.credentialId !== "number" || !Number.isSafeInteger(request.credentialId)) {
        throw new Error("Provider and credentialId are required");
      }
      const store = getOAuthCredentialProvider(request.provider);
      await auth.credentials.reload();
      const removed = await auth.credentials.removeById(store, request.credentialId);
      await registry.refreshProvider(store, "online");
      outcome = { t: "logged_out", removed, remaining: auth.keys.describe(store, "ompdeck") ?? null };
    } else if (request.op === "login") {
      if (typeof request.provider !== "string" || !request.provider) throw new Error("Provider is required");
      const provider = getOAuthProviders().find(provider => provider.id === request.provider);
      if (!provider) throw new Error(`Unknown login provider: ${request.provider}`);
      timer = setInterval(() => {
        try {
          if (fs.existsSync(path.join(directory, "cancel"))) { controller.abort(); return; }
          for (const [seq, answer] of pending) {
            const file = path.join(directory, `answer-${seq}.json`);
            if (!fs.existsSync(file)) continue;
            const value: unknown = JSON.parse(fs.readFileSync(file, "utf8"));
            fs.unlinkSync(file);
            if (!record(value) || typeof value.value !== "string") throw new Error("Invalid login answer");
            pending.delete(seq);
            answer.resolve(value.value);
          }
        } catch (error) {
          controller.abort(error);
        }
      }, 100);
      const identity: unknown = await auth.oauth.login(provider.id, {
        signal: controller.signal,
        onAuth: (info: AuthInfo) => report({ t: "auth", url: info.url, launchUrl: info.launchUrl, instructions: info.instructions }),
        onPrompt: (prompt: Prompt) => ask({ t: "prompt", message: prompt.message, placeholder: prompt.placeholder,
          allowEmpty: prompt.allowEmpty === true, secret: prompt.secret === true }),
        onManualCodeInput: () => ask({ t: "manual" }),
        onProgress: (message: string) => report({ t: "progress", message }),
      });
      const store = provider.storeCredentialsAs ?? provider.id;
      await registry.refreshProvider(store, "online");
      let label: string | undefined;
      if (record(identity)) {
        for (const key of ["email", "accountId", "orgName"]) {
          const value = identity[key];
          if (typeof value === "string" && value.trim()) { label = value.trim(); break; }
        }
      }
      outcome = { t: "done", store, label };
    } else {
      throw new Error("Unknown authentication operation");
    }
  } catch (error) {
    outcome = { t: "error", message: error instanceof Error ? error.message : String(error), cancelled: controller.signal.aborted };
  } finally {
    clearTimeout(cap);
    clearInterval(timer);
    controller.signal.removeEventListener("abort", abortPending);
    // A callback-based flow may finish while its manual-code promise is still pending.
    abortPending();
    try { auth?.close(); }
    catch (error) { outcome = { t: "error", message: error instanceof Error ? error.message : String(error), cancelled: controller.signal.aborted }; }
  }
  report(outcome);
}
