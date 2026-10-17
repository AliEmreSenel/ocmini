# ocmini contract — minimal opencode-serve drop-in (targets opencode v1.19.2, T3 Code @3b99fb2d)

Ground truth sources (read-only):
- /home/aliemre/.t3/scratch/2026-10-16-why-is-my-t3code-using-e76a83d5/src/opencode (tag v1.19.2)
- /home/aliemre/.t3/scratch/2026-10-16-why-is-my-t3code-using-e76a83d5/src/t3code (packages/provider-opencode)
- openapi.json in the same scratch dir (162 paths; we implement the 22-endpoint subset)

## CLI (PATH shim for `opencode`)
- `ocmini serve --hostname=H --port=P` → bind H:P, print exactly `opencode server listening on http://H:P` to stdout, flush, run forever.
- `ocmini --version` → `1.19.2` (T3 version gate parses semver; keep 1.x so T3 uses the v1 SDK client).
- `ocmini models --verbose` → JSON array of catalog models (from config providers + auth + models.dev subset).
- `ocmini agent list` → JSON array. `ocmini debug skill` → JSON array.

## HTTP
- Auth: if env OPENCODE_SERVER_PASSWORD set → require `Authorization: Basic base64("opencode:"+pw)` on ALL routes (401 otherwise, constant-time compare).
- Header `x-opencode-directory`: absolute dir for the request's project scope (session.directory source).
- `GET /api/info` → 404 (forces T3 v1 routing). `GET /global/health` → `{"healthy":true,"version":"1.19.2"}`.

## Endpoints (v1 SDK subset; JSON keys camelCase, ids: ses_/msg_/prt_/evt_ + hex)
Session JSON: `{"id","slug","projectID":"global","directory","title","version":"0.0.0","time":{"created","updated"},"parentID"?,"permission"?}` (time = epoch ms).
- `POST /session` {title?,parentID?,permission?} → Session
- `GET /session/:id`, `PATCH /session/:id` {title?,permission?} → Session, `DELETE /session/:id`
- `GET /session/:id/children` → [Session]
- `GET /session/status` → `{"<sessionID>":"busy"}` (busy sessions map)
- `POST /session/:id/prompt_async` {modelID,providerID,parts:[{type:"text",text}],agent?,messageID?} → `{}`; runs turn, streams events.
- `POST /session/:id/message` (sync) → `{info:Message,parts:[Part]}` (used for title-gen; must complete).
- `POST /session/:id/abort` → `{}` (cancel running turn → session.idle)
- `POST /session/:id/command` {command,arguments,modelID,providerID,parts} → like prompt_async
- `POST /session/:id/summarize` {providerID,modelID} → `{}` (compact: add compaction marker; emit session.compacted)
- `POST /session/:id/fork` {messageID?} → Session (copy messages up to messageID)
- `GET /session/:id/message` → `[{"info":Message,"parts":[Part]}]`, newest-last.
Message.info: `{"id","sessionID","role":"user"|"assistant","time":{"created","completed"?},"modelID"?,"providerID"?,"cost"?,"tokens"?:{"input","output","reasoning","cache":{"read","write"}},"finish"?}`
Part union (keep JSON `type`): `text{id,sessionID,messageID,text}`, `reasoning{...text}`, `tool{id,sessionID,messageID,tool,callID,state:{"status":"pending|running|completed|error","input","output"?,"title"?,"time":{"start","end"?},"error"?}}`, `step-start`, `step-finish{cost,tokens}`, `todo{todos:[{content,status,priority}]}`, `patch{...}`, `compaction{}`, `retry{}`, `subtask{...}`.

## Events (SSE `GET /event`, `data: {"id":"evt_…","type":T,"properties":P}\n\n`)
First: `server.connected {}`. Heartbeat event `server.heartbeat {}` every 10s. During turns emit:
`message.updated {info}`, `message.part.updated {part}`, `message.part.delta {partID,messageID,sessionID,delta,field:"text"}`, `session.status {sessionID,status:"busy"|"idle"}`, `session.idle {sessionID}`, `session.error {sessionID,error}`, `todo.updated {sessionID,todos}`, `permission.asked {id,sessionID,permission,pattern,metadata}`, `permission.replied {sessionID,requestID,reply}`, `question.asked {id,sessionID,questions:[{question,header,options:[{label,description}]}]}`, `question.replied {sessionID,requestID,answers}`, `question.rejected {sessionID,requestID}`, `session.compacted {sessionID}`.
- `POST /permission/:id/reply` {response:"once"|"always"|"reject"} → true
- `GET /permission` → pending [PermissionRequest]
- `POST /question/:id/reply` {answers:[[string]]} → true; `POST /question/:id/reject` → true; `GET /question` → pending list
- `GET /mcp` → `{"<name>":{"status":"connected"}}` map; `POST /mcp` {name,config:{type:"remote",url,headers?,oauth:false} | {type:"local",command:[..],enabled}} → true (register + connect, expose tools as `name_tool`).
- `GET /provider` → `{"all":[Provider],"default":{},"connected":[ids]}`; Provider: `{"id","name","env":[],"source":"config","options":{"baseURL"?},"models":{"<id>":{"id","name","limit":{"context","output"},"modalities"?}}}`.
- `GET /agent` → `[{"id","description","mode":"primary"|"subagent","prompt"?}]` (built-ins: build, general + config `agent` entries + markdown agents dir)
- `GET /skill` → `[{"name","description"}]` (from ~/.config/opencode/skills/*/SKILL.md frontmatter + config skills paths)
- `GET /command` → `[{"name","description","template","hints"}]` (config `command` entries)

## Config / auth (file-compatible; enables lossless revert to real opencode)
- Read `$XDG_CONFIG_HOME/opencode/opencode.json` (default ~/.config/opencode), overlay env `OPENCODE_CONFIG_CONTENT` (JSON, deep-merge, env wins). Keys used: `provider` (custom), `model` ("prov/mid"), `permission` (map name→allow|ask|deny), `instructions` (AGENTS.md files, cwd+global), `mcp`, `agent`, `command`, `skills`.
- auth: `$XDG_DATA_HOME/opencode/auth.json` (default ~/.local/share), env `OPENCODE_AUTH_CONTENT` overrides. Entries: `{"type":"api","key","metadata"?}`, `{"type":"oauth","refresh","access","expires","accountId"?,"enterpriseUrl"?}`, `{"type":"wellknown","key","token"}`.

## Providers (HTTP, no npm)
- `npm:"@ai-sdk/openai-compatible"` + openrouter + github-copilot: OpenAI `/chat/completions` stream SSE. baseURL precedence: options.baseURL > auth wellknown token url > openrouter `https://openrouter.ai/api/v1` > copilot `https://api.githubcopilot.com` (Bearer = oauth.access; refresh via device-flow plugin when expired) > vllm localhost:8000/v1.
- anthropic: `/v1/messages`, headers x-api-key/anthropic-version: 2023-06-01, system prompt top-level, SSE stream events (content_block_delta etc).
- OpenAI-compat tool calling: map tools → `tools:[{type:"function",function:{name,description,parameters}}]`, stream `tool_calls` deltas.

## Permission engine
Rules list `[{permission,pattern,action:"allow"|"ask"|"deny"}]`, wildcard `*` matches prefix/glob. Names: `bash`,`edit`,`webfetch`,`websearch`,`codesearch`,`external_directory`,`doom_loop`,`task`,`todowrite`,`question`,`skill`,`read`,`glob`,`grep`. Tool→name: bash→bash; write/edit/apply_patch→edit; webfetch→webfetch; task→task; read/glob/grep→read (always allow). Config `permission` map seeds default rules. `ask` → permission.asked + block on reply (once/always/reject).
`external_directory`: bash/read/write outside session.directory → name external_directory.

## Tools (schemas mirror opencode tool/*.txt; params camelCase)
bash{command,cwd?,timeout?}, read{filePath,offset?,limit?}, write{filePath,content}, edit{filePath,oldString,newString,replaceAll?}, apply_patch{patch}, glob{pattern,path?}, grep{pattern,path?,include?}, webfetch{url,format?}, todowrite{todos}, question{questions}, task{description,prompt,subagent_type}, skill{name}. bash: `sh -lc`, 120s timeout, 50KB output truncate. edit: exact unique match (error if 0/multiple).

## Agent loop
system prompt: base (concise coding agent) + AGENTS.md/instructions files + skills list + tool list. Loop: provider → text/reasoning parts (delta events) → tool calls (tool part pending→running→completed, permission gate) → follow-up turns until no tool calls; max 100; doom_loop guard: same tool+args 3x → permission.asked doom_loop. Token usage from provider usage obj → tokens{input,output,reasoning,cache}. Compact/summarize: replace old messages with summary text part, keep last N, emit session.compacted.

## Storage (anti-bloat)
SQLite WAL at `$XDG_DATA_HOME/ocmini/ocmini.db`: sessions(id PK, json, time_created, time_updated, parent_id, busy), messages(id PK, session_id, json, time_created), parts(id PK, message_id, session_id, json, time_created), indexes on session_id. WAL + synchronous=NORMAL, busy_timeout=5000. Retention: on startup delete sessions idle > 30d (T3 keeps authoritative copy).
Usage widget compat: ALSO write `~/.local/share/opencode/opencode-ocmini.db` (matches T3 scan regex) with table `message(id, session_id, data TEXT, time_created INTEGER)` — data JSON: `{"role":"assistant","tokens":{"input","output","reasoning","cache":{"read","write"}},"model":{"providerID","modelID"}}`. Never touch real opencode.db.

## Ownership (subagent boundaries — edit ONLY your files)
- CORE (parent): Cargo.toml, SPEC, types.rs, bus.rs, store.rs, config.rs, auth.rs, permission.rs, usage_db.rs, server.rs, main.rs, engine.rs (traits), questions/permissions queues.
- QWEN: provider/mod.rs, provider/openai_compat.rs, provider/anthropic.rs, engine_impl.rs, tools/* (all tool impls).
- GEMINI: mcp/mod.rs (stdio+remote client, JSON-RPC 2.0, tools/list), src/bin/fuzz.rs (differential shape fuzzer per SPEC §Endpoints: replay GETs + prompt sequences vs oracle, normalize ids/times), tests/contract.rs (axum-test-free: spawn server on ephemeral port, assert 21-endpoint shapes).

Rules: no new deps (request via report), no `cargo build` (parent builds), idiomatic Rust, no comments, match SPEC JSON exactly (serde rename).

## Verified facts (2026-10-17 smoke test, live machine)
- User `opencode.json` is JSONC (comments + trailing commas) → config.rs `strip_jsonc` handles it. Providers: vllm, pllm5 (`http://<your-llm-proxy>:32401/v1`, apiKey in options), EvAI, ollama. Options key is `apiKey` (serde rename).
- prompt body: `{"model":{"providerID","modelID"},"parts":[...]}` nested (types.rs ModelRef).
- axum 0.8 routes use `{id}` syntax. Binary lands in shared /mnt/data/cargo-target/debug/ocmini (global cargo config).
- Verified working: banner stdout, health, /api/info 404, Basic auth, session CRUD, prompt_async → pllm5 → PONG, events (server.connected/heartbeat, session.status busy→idle, session.idle, message.updated, message.part.updated/delta), usage sidecar.
- T3 config interface (docs/user/providers-opencode.md + OpenCodeRuntime.ts): T3 never writes opencode config; passes OPENCODE_CONFIG_CONTENT (default `{}`) + OPENCODE_SERVER_PASSWORD env only; external-server mode exists (Server URL+password in Settings). Plan mode uses agent id `plan`. Revert = restore real binary at /usr/bin/opencode; zero file changes.
- Live opencode remote connections: self-hosted provider proxy only; GitHub HTTPS seen only via agent bash `gh` calls. ocmini replicates exactly: providers + MCP stdio children.
- Skill dirs to scan: ~/.config/opencode/skill(s)/, project/.opencode/skill(s)/ (+config `skills` paths).
