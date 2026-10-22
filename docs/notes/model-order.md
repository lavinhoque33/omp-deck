# Findings: model release dates and a "newest" sort key for OmpDeck

**Q1: There is no release date and no "newest" catalog order that omp exposes.** The closest things are an implicit sort in omp's own picker and a hidden `identity.revision` field. Neither is a date.

## Q1 evidence

- **CLI.** `omp models --help` lists only `--json`, `--kind=<value>`, `-e`, `--no-extensions` and `--config`. There is no `--verbose` or `--all`. The `--kind` flag takes a catalog kind (`omp models --kind all` is an example in the help). Every one of the 168 models from `omp models --json` is `kind:"chat"`, and the JSON has no extra fields.
- **Cache.** `~/.omp/agent/models.db` has tables `model_cache(provider_id, version, updated_at, models JSON, …)` and `model_cache_refresh`. I grepped every cached `models` blob for keys matching `release`, `date`, `cutoff`, `created`, `knowledge`, `lastUpdated`. The only hit was `supportsConfigurationUpdate`, so there are no date fields.
- **Hidden field.** Cached records carry `"identity":{"class":"anthropic","family":"haiku","revision":"5.5.0"}`. This is a revision string, not a date. It is not in `omp models --json`; only reading `models.db` directly would give it to you. Reading it directly is brittle, and I recommend against it.
- **Bundle (`strings -n 6 ~/.local/bin/omp`).**
  - The only `release_date` / `releaseDate` hits are unrelated: an HTML music tag parser and a skill/plugin manifest validator.
  - `last_updated` is a registry-package field.
  - The models.dev importer (`mapModelsDevToModels`, `modelsDevKey`) maps no date fields into models.
  - The `isNew` / `isBeta` flags used for the "new" badge in the picker are a protobuf field on a different object. [UNVERIFIED] whether any model object ever carries `isNew`; the key is absent from the cache.
- **omp's own picker order.** Items go through `kf(items, {roles, mruOrder})` → `ne()`, found in the bundle near line 607695 of the `strings` output; the chunk file name is not pinned down. The 18.4.10 source has the same logic at `~/PersProject/oh-my-pi/packages/tui/src/overlays/model-browser.ts:303-306`. Comparator keys in order:
  1. Rank of the role the model is assigned to (`Tp` list: default, smol, etc.).
  2. MRU position (`mruOrder` index).
  3. `provider.localeCompare`.
  4. `model.priority` (ascending).
  5. Version `Y(id)` descending.
  6. Alias (`-latest` or dated id) before plain id.
  7. Dated ids by `-(\d{8})$` descending.
  8. `id.localeCompare`.
  
  The version function `Y(id)` is `parseFloat` of the first of: `(?:^|[-_])(\d+\.\d+)`; then `(\d{1,2})-(\d{1,2})(?=-|$)` read as "major.minor"; then the first integer; else 0.
  
  Two weaknesses of `Y`: a float compare treats `5.10` as `5.1`, and its tie-breaks are plain `localeCompare`. Reusing the idea is right, but a tuple compare is better.

## Q2: Proposed newness sort key

The key is a pure function of `(provider, id, name)`.

**Parse `id` (then lower-case):**
1. Take the last `/` segment, `s`.
2. **Date suffix**, strip the first matching form from the end:
   - `-YYYY-MM-DD` → YYYYMMDD
   - `-YYYYMMDD` → YYYYMMDD
   - `-MMDD` where MM is 01–12 and DD is 01–31 → value below 1232, so it is less than any full date. Year unknown; the only catalog hit is `Qwen3.8-Max-0902`.
   
   If none match, the date is `None`.
3. **Version tuple.** Find the leftmost digit run in `s`, skipping runs that match `\d+(\.\d+)?[bm](?=[-_:]|$)` (parameter sizes like `27b`). At that position, try in order:
   - `\d+(\.\d+){1,2}` → tuple, e.g. `3.8` → (3,8).
   - `(\d{1,2})-(\d{1,2})(?=[-_:]|$)`, only if the previous char is `-` or `_` → (a,b), e.g. `4-5` → (4,5).
   - Plain integer → (n).
   
   If there is no digit, the version is empty (unknown). Drop trailing zeros while the length is above 1, so (4,0) equals (4).
4. **Series** is the first `[a-z]+` run of the last segment (`claude`, `gpt`, `qwen`, `glm`, `minimax`, `kimi`, `mimo`, `muse`, `hy`, …). It is used only for ranking.

**Sort keys, ascending, in this order:**
1. `rank`:
   - Within a provider list, collect the distinct non-empty versions of each series and sort them descending.
   - `rank` = a model's index in that list (0 = that series' newest version).
   - A model with an empty version gets `u32::MAX`.
   - This makes a mixed-vendor aggregator like `commandcode` show every vendor's latest first, instead of pretending `Kimi-K3` and `gpt-6` are comparable. For single-series providers it is identical to a plain descending version sort.
2. Unversioned (empty version) after versioned. This is already implied by `rank`.
3. Version tuple, descending.
4. Undated before dated, so the alias comes before its snapshot.
5. Date, descending.
6. `id` lower-case, ascending, then `name` lower-case, ascending.

**Rust sketch:**
```rust
struct Parsed { ver: Vec<u32>, date: Option<u32>, series: String }
fn parse(id: &str) -> Parsed { /* steps 1-4 above */ }
// key per model, after computing rank from the provider's list:
// (rank, ver.is_empty(), Reverse(ver), date.is_some(), Reverse(date.unwrap_or(0)), id_lc, name_lc)
```
Vec comparison is lexicographic, so (5,10) > (5,9) and (5,1) > (5). A shorter tuple that is a prefix of a longer one compares lower, which is correct here.

I ran this algorithm over all 168 models in a Python prototype and then deleted it.

## Resulting order per provider

The catalog is 28 `anthropic`, 96 `commandcode`, 7 `openai-codex` and 37 `opencode-go`; that makes the 168.

**anthropic (28):**
```
claude-haiku-5-5, claude-opus-5-5, claude-sonnet-5-5, claude-fable-5-1, claude-mythos-5-1,
claude-fable-5, claude-mythos-5, claude-opus-5, claude-sonnet-5, claude-opus-4-8, claude-opus-4-7,
claude-opus-4-6, claude-sonnet-4-6, claude-haiku-4-5, claude-opus-4-5, claude-sonnet-4-5,
claude-opus-4-5-20251101, claude-haiku-4-5-20251001, claude-sonnet-4-5-20250929,
claude-opus-4-1, claude-opus-4-1-20250805, claude-opus-4-0, claude-sonnet-4-0,
claude-opus-4-20250514, claude-sonnet-4-20250514,
claude-3-5-sonnet-20241022, claude-3-5-sonnet-20240620, claude-3-haiku-20240307
```

**openai-codex (7):**
```
gpt-6.1-sol, gpt-6-astra, gpt-6-luna, gpt-6-sol, gpt-5.6-luna, gpt-5.6-sol, gpt-5.6-terra
```

**opencode-go (37):** rank 0 across series, then rank 1, and so on.
```
gpt-6-luna, claude-haiku-5-5, glm-5.3, glm-5.3-flash, step-5-preview, grok-4.7, deepseek-v4.1-flash,
gemini-4-argon-contributor, hy4-preview, qwen3.8-flash, qwen3.8-max, minimax-m3.1-flash-free, kimi-k3,
mimo-v2.6-flash, mimo-v2.6-pro, longcat-2.5-preview-free, muse-spark-1.3-contributor,
gpt-5.6-luna, glm-5.2, grok-4.6, deepseek-v4-pro, qwen3.7-max, qwen3.7-plus, hy3, minimax-m3,
kimi-k2.7-code, mimo-v2.5, mimo-v2.5-pro, longcat-2.0, muse-spark-1.2-contributor,
glm-5.1, qwen3.6-plus, minimax-m2.7, kimi-k2.6, minimax-m2.5, beam-free, omen-alpha
```

**commandcode (96):** each line is one rank group, with the rank-0 line first.
```
rank0: gpt-6.1-sol; claude-{haiku,opus,sonnet}-5-5; z-ai/glm-5.3-flash, z-ai/glm-5.3-flashx, zai-org/GLM-5.3;
       Step-5-Preview; grok-4.7; deepseek-v4.1-flash(+fast); gemini-4-argon; mistral-large-4; hy4-preview;
       Qwen3.8-27B/Flash/Max/Max-Prime/Omni-Flash/Max-0902; nemotron-3.5-super-vl; ling-3.1-flash;
       minimax-m3.1-flash(+:free); Kimi-K3; mimo-v2.6-flash/pro/pro-ultraspeed; laguna-s-2.1-free;
       LongCat-2.0; ornith-1.5-122b(+:free); muse-spark-1.3(+contributor)
rank1: gpt-6-astra/luna/sol; GLM-5.2(+Fast); claude-fable-5-1; grok-4.6; deepseek-v4-flash(+fast,+vision-exp), v4-pro;
       gemini-3.8-flash; Qwen3.7-Flash/Max/Plus; Step-3.7-Flash; MiniMax-M3; nemotron-3-ultra; hy3-paid;
       Kimi-K2.7-Code(+Highspeed); mimo-v2.5/pro; muse-spark-1.2(+contributor)
rank2: gpt-5.6-*; GLM-5.1; claude-fable-5/opus-5/sonnet-5; grok-4.5; gemini-3.7-flash; Qwen3.6-*; Step-3.5-Flash;
       MiniMax-M2.7; Kimi-K2.6; muse-spark-1.1
rank3+: gpt-5.5; GLM-5; claude-opus-4-8; gemini-3.6; MiniMax-M2.5; Kimi-K2.5; muse-wonder-1.0; gpt-5.4(+mini);
        claude-opus-4-7; gemini-3.5-flash(+lite); gpt-5.3-codex; claude-sonnet-4-6; gemini-3.1-flash-lite;
        claude-haiku-4-5-20251001
unversioned (last): muse-spark-open, beam:free, fugu-ultra, glyph-cluster:free, inkling, inkling-small
```

## Cases that look wrong or ambiguous

- **Across vendors there is no true "newest".** `commandcode` aggregates ~30 vendors, so the interleave is a heuristic. A plain descending version sort is worse there, because it would put `gpt-6` above `Kimi-K3` just because 6 > 3. The interleave only reorders `commandcode`.
- **Names and ids that group poorly:**
  - `z-ai/glm-5.3-flash` and `zai-org/GLM-5.3` still group correctly because the series is `glm`.
  - `muse-wonder-1.0` is classed as series `muse` (version 1.0 → (1)).
  - `tencent/hy3-paid` and `hy4-preview` are series `hy`.
- **Variants of one version** (`-flash`, `-fast`, `-free`, `-contributor`, `-preview`) sort by id alphabetically, not by tier. For example `claude-haiku-5-5` comes before `opus` and `sonnet` at the same version. I judged this acceptable; adding a family-tier weight would be a hand-maintained table.
- **MMDD suffix.** `Qwen3.8-Max-0902` lands after the undated `Qwen3.8-Max-Prime` / `Omni-Flash` at the same version. This is consistent with the "alias before snapshot" rule.
- **Unversioned ids** (`omen-alpha`, `inkling`, `beam-free`, `fugu-ultra`, `glyph-cluster`, `muse-spark-open`) go last. Their age is unknowable from the id.
- **Version-less size tokens.** The `[bm]` skip handles `27b`. Ids like `gpt-4o` and `o3-mini` pick up 4 and 3; none are in this catalog.
- **A date-less snapshot, `claude-opus-4-0`,** is treated as plain version (4), the same as `claude-opus-4`. It sorts above `claude-opus-4-20250514`, and between itself and its siblings the id decides.
- **`isNew`** was not observed on any real model, so there is no badge-based sort.

## Test table

Each row asserts that the first id sorts before the second, using the real ids in that provider's list. I ran all 38 rows against the prototype: **38 OK, 0 BAD**.

| Provider | A (sorts first) | B |
|---|---|---|
| anthropic | claude-opus-5-5 | claude-opus-4-8 |
| anthropic | claude-opus-4-8 | claude-opus-4-7 |
| anthropic | claude-sonnet-5-5 | claude-sonnet-5 |
| anthropic | claude-sonnet-5 | claude-sonnet-4-6 |
| anthropic | claude-sonnet-4-5 | claude-sonnet-4-5-20250929 |
| anthropic | claude-opus-4-5-20251101 | claude-haiku-4-5-20251001 |
| anthropic | claude-haiku-4-5-20251001 | claude-sonnet-4-5-20250929 |
| anthropic | claude-opus-4-1 | claude-opus-4-0 |
| anthropic | claude-opus-4-0 | claude-opus-4-20250514 |
| anthropic | claude-opus-4-20250514 | claude-3-5-sonnet-20241022 |
| anthropic | claude-3-5-sonnet-20241022 | claude-3-5-sonnet-20240620 |
| anthropic | claude-3-5-sonnet-20240620 | claude-3-haiku-20240307 |
| anthropic | claude-fable-5-1 | claude-fable-5 |
| anthropic | claude-haiku-5-5 | claude-fable-5-1 |
| openai-codex | gpt-6.1-sol | gpt-6-sol |
| openai-codex | gpt-6-sol | gpt-5.6-sol |
| openai-codex | gpt-5.6-sol | gpt-5.6-terra |
| commandcode | gpt-5.5 | gpt-5.4 |
| commandcode | gpt-5.4 | gpt-5.4-mini |
| commandcode | gpt-5.4-mini | gpt-5.3-codex |
| commandcode | Qwen/Qwen3.8-Max | Qwen/Qwen3.8-Max-0902 |
| commandcode | Qwen/Qwen3.8-Max-0902 | Qwen/Qwen3.7-Max |
| commandcode | google/gemini-3.8-flash | google/gemini-3.5-flash |
| commandcode | google/gemini-4-argon | google/gemini-3.8-flash |
| commandcode | moonshotai/Kimi-K3 | moonshotai/Kimi-K2.7-Code |
| commandcode | deepseek/deepseek-v4.1-flash | deepseek/deepseek-v4-pro |
| commandcode | meta/muse-spark-1.3 | meta/muse-spark-open |
| commandcode | zai-org/GLM-5.3 | zai-org/GLM-5 |
| commandcode | gpt-6.1-sol | claude-haiku-5-5 |
| commandcode | claude-haiku-5-5 | gpt-6-sol |
| commandcode | xai/grok-4.7 | xai/grok-4.6 |
| opencode-go | glm-5.3 | glm-5.2 |
| opencode-go | kimi-k3 | kimi-k2.7-code |
| opencode-go | qwen3.8-max | qwen3.7-max |
| opencode-go | longcat-2.5-preview-free | longcat-2.0 |
| opencode-go | minimax-m3 | minimax-m2.7 |
| opencode-go | mimo-v2.6-pro | mimo-v2.5-pro |
| opencode-go | hy4-preview | hy3 |
| opencode-go | muse-spark-1.3-contributor | omen-alpha |

I deleted all my throwaway files (`/tmp/mo`, `/tmp/m.json`, `/tmp/omp.strings`). No tmux server was started, no files under `~/.omp` were written, and nothing in the repo was edited.