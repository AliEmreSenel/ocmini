// Development tooling only. Exercise the actual client and authoritative wire schemas.
import assert from "node:assert/strict";
import { OpenCode } from "@opencode/client";
import { OpenCodeEvent } from "@opencode/protocol/groups/event";
import { Session } from "@opencode/schema/session";
import { SessionMessage } from "@opencode/schema/session-message";
import { Model } from "@opencode/schema/model";
import { Provider } from "@opencode/schema/provider";
import { Agent } from "@opencode/schema/agent";
import { Schema, Effect, Queue, Stream } from "effect";
import { OpenCodeEvent as T3Event } from "opencode-protocol-t3/groups/event";
import { SessionMessage as T3Message } from "opencode-schema-t3/session-message";
import { OpenCode as T3OpenCode } from "opencode-client-t3/effect";
import { FetchHttpClient, HttpClient, HttpClientRequest } from "effect/http";

const baseUrl = process.env.OCMINI_TEST_URL!;
const directory = process.env.OCMINI_TEST_DIRECTORY!;
const authorization = `Basic ${Buffer.from(`opencode:${process.env.OCMINI_TEST_PASSWORD}`).toString("base64")}`;
const client = OpenCode.make({ baseUrl, headers: { Authorization: authorization } });
const location = { directory };
assert.ok((await client.server.info()).pid > 0);
for (const m of (await client.model.list({ location })).data) Schema.decodeUnknownSync(Model.Info)(m);
for (const p of (await client.provider.list({ location })).data) Schema.decodeUnknownSync(Provider.Info)(p);
for (const a of (await client.agent.list({ location })).data) Schema.decodeUnknownSync(Agent.Info)(a);
await client.command.list({ location });
await client.skill.list({ location });
const native = await client.session.create({ location, title: "SDK conformance", model: { id: "test-model", providerID: "mock" }, permissions: [{ action: "*", resource: "*", effect: "allow" }, { action: "shell", resource: "*", effect: "ask" }] });
const sessionID = native.id;
Schema.decodeUnknownSync(Session.Info)(native);
await client.experimental.session.instructions.entry.put({ sessionID, key: "t3-code", value: "SDK_INSTRUCTION" });
const response = await fetch(`${baseUrl}/api/event`, { headers: { Authorization: authorization } });
assert.equal(response.status, 200);
let resolveEnd!: () => void;
const ended = new Promise<void>((resolve) => { resolveEnd = resolve; });
const observed: any[] = [];
const reader = response.body!.getReader();
const reading = (async () => {
  let pending = "";
  const decoder = new TextDecoder();
  while (true) {
    const { value, done } = await reader.read();
    if (done) throw new Error("event stream closed before terminal event");
    pending += decoder.decode(value, { stream: true });
    let at: number;
    while ((at = pending.indexOf("\n\n")) !== -1) {
      const frame = pending.slice(0, at);
      pending = pending.slice(at + 2);
      const raw = frame.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
      if (!raw) continue;
      const event = JSON.parse(raw);
      Schema.decodeUnknownSync(OpenCodeEvent)(event);
      Schema.decodeUnknownSync(T3Event)(event);
      observed.push(event);
      if (event.type === "permission.asked" && event.data.sessionID === sessionID) {
        await client.permission.reply({ sessionID, requestID: event.data.id, decision: "once" });
      }
      if (event.type === "form.created" && event.data.form.sessionID === sessionID) {
        await client.session.form.reply({ sessionID, formID: event.data.form.id, answer: { "0": "A" } });
      }
      if (event.type === "session.execution.succeeded" && event.data.sessionID === sessionID) { resolveEnd(); return; }
    }
  }
})();
await client.session.prompt({ sessionID, id: "msg_t3_turn_conformance:1", text: "Answer" });
await Promise.race([ended, reading, new Promise((_, reject) => setTimeout(() => reject(new Error("No terminal event")), 10000))]);
const history = await client.message.list({ sessionID, order: "asc" });
for (const message of history.data) { Schema.decodeUnknownSync(SessionMessage.Info)(message); Schema.decodeUnknownSync(T3Message.Info)(message); }
assert.ok(history.data.some((message) => message.type === "assistant"));
assert.ok(observed.some((event) => event.type === "session.text.ended"));
assert.ok(observed.some((event) => event.type === "session.tool.success"));
assert.ok(observed.some((event) => event.type === "form.replied"));
assert.ok(observed.some((event) => event.type === "permission.replied"));
Schema.decodeUnknownSync(Session.Info)(await client.session.get({ sessionID }));
await Effect.runPromise(Effect.gen(function* () {
  const http = yield* HttpClient.HttpClient;
  const authenticated = HttpClient.mapRequest(http, HttpClientRequest.setHeader("Authorization", authorization));
  const t3 = yield* T3OpenCode.make({ baseUrl }).pipe(Effect.provideService(HttpClient.HttpClient, authenticated));
  yield* t3.server.info();
  yield* t3.model.list({ location });
  yield* t3.provider.list({ location });
  yield* t3.agent.list({ location });
  yield* t3.session.get({ sessionID });
  yield* t3.message.list({ sessionID });
  // Exercise T3's Effect client writes too: it serializes omitted fields as null.
  const created = yield* t3.session.create({ location, model: { id: "test-model", providerID: "mock" } });
  assert.equal(created.parentID, undefined);
  assert.equal(created.agent, "build");
  yield* t3.session.update({ sessionID: created.id, title: "T3 Effect client" });
  const child = yield* t3.session.create({ parentID: created.id });
  assert.equal(child.parentID, created.id);
  assert.equal(child.location.directory, directory);
  // Subscribe before submitting so even a quick local provider cannot race us.
  const events = yield* t3.event.subscribe().pipe(Stream.toQueue({ capacity: "unbounded" }));
  assert.equal((yield* Queue.take(events)).type, "server.connected");
  const receipt = yield* t3.session.prompt({ sessionID: created.id, text: "T3 prompt" });
  assert.ok(receipt.id.startsWith("msg_"));
  yield* Stream.fromQueue(events).pipe(
    Stream.filter((event) => event.type === "session.execution.succeeded" && event.data.sessionID === created.id),
    Stream.take(1),
    Stream.runDrain,
    Effect.timeout("10 seconds"),
  );
  const messages = yield* t3.message.list({ sessionID: created.id });
  assert.ok(messages.data.some((message) => message.type === "assistant"));
  yield* t3.session.remove({ sessionID: created.id });
}).pipe(Effect.scoped, Effect.provide(FetchHttpClient.layer)));
// Validate archive state and paged session discovery through the real client.
await client.session.update({ sessionID, archived: true });
assert.ok((await client.session.get({ sessionID })).time.archived);
const archivedPage = await client.session.list({ archived: true, limit: 1 });
assert.equal(archivedPage.data[0]?.id, sessionID);
await client.session.update({ sessionID, archived: false });

// Keep decoding through deletion: its durable schema is version 2 in both clients.
const deletionResponse = await fetch(`${baseUrl}/api/event?types=session.deleted`, { headers: { Authorization: authorization } });
const deletionReader = deletionResponse.body!.getReader();
const deleted = (async () => {
  let pending = "";
  const decoder = new TextDecoder();
  while (true) {
    const { value, done } = await deletionReader.read();
    assert.ok(!done, "Event stream closed before deletion");
    pending += decoder.decode(value, { stream: true });
    let at: number;
    while ((at = pending.indexOf("\n\n")) !== -1) {
      const frame = pending.slice(0, at); pending = pending.slice(at + 2);
      const raw = frame.split("\n").filter((line) => line.startsWith("data:")).map((line) => line.slice(5).trimStart()).join("\n");
      if (!raw) continue;
      const event = JSON.parse(raw);
      Schema.decodeUnknownSync(OpenCodeEvent)(event);
      Schema.decodeUnknownSync(T3Event)(event);
      if (event.type === "session.deleted" && event.data.sessionID === sessionID) return;
    }
  }
})();
await client.session.remove({ sessionID });
await Promise.race([deleted, new Promise((_, reject) => setTimeout(() => reject(new Error("No deletion event")), 10000))]);
await deletionReader.cancel();
await reader.cancel();
console.log("Official OpenCode v2.2.5 and T3-pinned v2.0.28 clients and schemas passed.");
process.exit(0);
