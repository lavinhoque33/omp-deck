// Loaded only by the headless account-policy operation. No credentials or session pins are read or changed.
import { YAML } from "bun";
import * as fs from "node:fs/promises";
import { homedir } from "node:os";
import * as path from "node:path";
import { cfgAuthAccountPolicies, cfgDisabledProviders } from "@oh-my-pi/pi-coding-agent/config/model-settings";
import { Settings } from "@oh-my-pi/pi-coding-agent/config/settings";
import { writeFileAtomically } from "@oh-my-pi/pi-coding-agent/utils/atomic-file";
import { getProjectAgentDir, normalizePathForComparison } from "@oh-my-pi/pi-utils";
import { withFileLock } from "@oh-my-pi/pi-utils/file-lock";
import { stringifyYamlConfig } from "@oh-my-pi/pi-utils/yaml-config";

const baseFields = ["email", "accountId", "projectId"] as const;
const identityFields = [...baseFields, "orgId"] as const;
type Account = Partial<Record<(typeof identityFields)[number], string>>;
type Mapping = Record<string, unknown>;

function isMapping(v: unknown): v is Mapping {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

function nonempty(v: unknown): v is string {
  return typeof v === "string" && v.trim().length > 0;
}

function isAccount(v: unknown): v is Account {
  return isMapping(v) && baseFields.some(k => nonempty(v[k])) &&
    identityFields.every(k => v[k] === undefined || nonempty(v[k]));
}

function accountPolicies(settings: unknown): unknown {
  return isMapping(settings) && isMapping(settings.auth) ? settings.auth.accountPolicies : undefined;
}

function promoteAccount(current: unknown, provider: string, account: Account): unknown[] {
  if (current != null && !Array.isArray(current)) throw new Error("auth.accountPolicies must be an array");
  const policies: unknown[] = Array.isArray(current) ? current.slice() : [];
  let first = -1;
  let selected: { index: number; rule: Mapping } | undefined;
  let maxOther = 0; // Accounts with no rule have priority zero.
  for (let i = 0; i < policies.length; i++) {
    const rule = policies[i];
    if (!isMapping(rule) || rule.provider !== provider) continue;
    if (first === -1) first = i;
    const selector = rule.account;
    if (isAccount(selector) && identityFields.every(k => selector[k] === undefined || selector[k] === account[k])) {
      if (selected) throw new Error("More than one account policy matches the selected account");
      selected = { index: i, rule };
    } else if (typeof rule.priority === "number" && Number.isFinite(rule.priority)) {
      maxOther = Math.max(maxOther, rule.priority);
    }
  }

  // Keep the original selector (possibly shorter than the full identity), along with reserves
  // and any unmodeled fields. No other rule's position relative to its peers or priority changes.
  let rule = selected?.rule ?? { provider, account };
  if (typeof rule.priority !== "number" || !(rule.priority > maxOther)) {
    const priority = maxOther + 1 > maxOther ? maxOther + 1 : maxOther * 2;
    if (!Number.isFinite(priority)) throw new Error("The configured account priority cannot be exceeded");
    rule = { ...rule, priority };
  }
  if (selected) policies.splice(selected.index, 1);
  policies.splice(first === -1 ? policies.length : first, 0, rule);
  return policies;
}

async function projectYaml(file: string): Promise<Mapping> {
  let source: string;
  try {
    if (!(await fs.lstat(file)).isFile()) throw new Error("Project .omp/config.yml must be a regular file");
    source = await fs.readFile(file, "utf8");
  } catch (e) {
    if (isMapping(e) && e.code === "ENOENT") return {};
    throw e;
  }
  let cfg: unknown;
  try {
    cfg = YAML.parse(source) ?? {};
  } catch {
    throw new Error("Project .omp/config.yml is invalid YAML; no account policy was written");
  }
  if (!isMapping(cfg) || (cfg.auth !== undefined && !isMapping(cfg.auth))) {
    throw new Error("Project .omp/config.yml must contain a mapping, including its auth settings if present");
  }
  return cfg;
}

export async function preferAccount(request: unknown): Promise<{ value: unknown[] }> {
  if (!isMapping(request) || !nonempty(request.provider) || request.provider.trim() !== request.provider ||
      !isAccount(request.account)) {
    throw new Error("A provider and a valid OAuth account identity are required");
  }
  const { provider, account, project } = request;
  if (project === null) {
    const settings = await Settings.loadIsolated();
    const value = promoteAccount(accountPolicies(settings.getGlobalSettings()), provider, account);
    settings.writeValue(cfgAuthAccountPolicies, value, "global");
    await settings.flush();
    return { value };
  }

  if (!nonempty(project)) throw new Error("A project directory is required");
  const cwd = await fs.realpath(project);
  if (normalizePathForComparison(cwd) === normalizePathForComparison(homedir())) {
    throw new Error("omp does not load project settings from the home directory");
  }
  const dir = getProjectAgentDir(cwd);
  await fs.mkdir(dir, { recursive: true });
  if (!(await fs.lstat(dir)).isDirectory()) throw new Error("Project .omp must be a real directory");
  const file = path.join(dir, "config.yml");

  return withFileLock(file, async () => {
    const cfg = await projectYaml(file);
    const settings = await Settings.loadReadOnly({ cwd });
    if (cfgDisabledProviders.get(settings).includes("native")) {
      throw new Error("Native omp project settings are disabled here");
    }
    const oldProject = accountPolicies(settings.getProjectSettings());
    // omp loads settings.json before config.yml. Fail instead of writing a hidden override when
    // another discovery provider owns this key.
    if (oldProject !== undefined && !Bun.deepEquals(oldProject, accountPolicies(cfg))) {
      const json = Bun.file(path.join(dir, "settings.json"));
      if (!(await json.exists()) || !Bun.deepEquals(oldProject, accountPolicies(await json.json()))) {
        throw new Error("Another project settings source owns auth.accountPolicies");
      }
    }
    // This setting is a replacing array, not merged with the global layer. On the first edit
    // seed all inherited rules, not just the provider being promoted.
    const before = oldProject === undefined ? accountPolicies(settings.getGlobalSettings()) : oldProject;
    const value = promoteAccount(before, provider, account);
    if (!Bun.deepEquals(value, accountPolicies(cfg))) {
      cfg.auth = { ...(isMapping(cfg.auth) ? cfg.auth : {}), accountPolicies: value };
      const content = stringifyYamlConfig(cfg);
      if (!Bun.deepEquals(YAML.parse(content), cfg)) {
        throw new Error("Project .omp/config.yml cannot be safely rewritten");
      }
      await writeFileAtomically(file, content);
    }
    return { value };
  });
}
