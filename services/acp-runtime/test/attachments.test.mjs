import { test } from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { mkdtemp, rm, symlink, writeFile } from "node:fs/promises";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { promptBlocks } from "../bridge.mjs";
import { cursorAdapter, openCodeAdapter } from "../catalog.mjs";

async function attachment(t, bytes, mimeType, name) {
  const root = await mkdtemp(join(tmpdir(), "wonder-acp-attachment-"));
  t.after(() => rm(root, { recursive: true, force: true }));
  const path = join(root, "owned-file");
  await writeFile(path, bytes);
  return { type: "localFile", path, name, mimeType, byteSize: bytes.length,
    sha256: createHash("sha256").update(bytes).digest("hex") };
}

test("verified text and PDF attachments reach ACP as embedded resources", async t => {
  const text = await attachment(t, Buffer.from("# Synthetic task\n- [ ] Add a due date\n"), "text/markdown", "Todo.md");
  const pdf = await attachment(t, Buffer.from("%PDF-1.7\nsynthetic fixture"), "application/pdf", "Plan.pdf");
  const blocks = await promptBlocks([{ type: "text", text: "Read these" }, text, pdf], { embeddedContext: true });
  assert.equal(blocks[0].text, "Read these");
  assert.equal(blocks[1].type, "resource");
  assert.equal(blocks[1].resource.text, "# Synthetic task\n- [ ] Add a due date\n");
  assert.equal(blocks[1].resource.mimeType, "text/markdown");
  assert.ok(blocks[1].resource.uri.endsWith("/Todo.md"));
  assert.equal(Buffer.from(blocks[2].resource.blob, "base64").toString(), "%PDF-1.7\nsynthetic fixture");
  assert.equal(blocks[2].resource.mimeType, "application/pdf");
});

test("attachments require native support and reject changed bytes, links and audio", async t => {
  const block = await attachment(t, Buffer.from("original"), "text/plain", "Note.txt");
  await assert.rejects(promptBlocks([block], {}), /cannot read files/);
  await assert.rejects(promptBlocks([{ ...block, mimeType: "audio/wav" }], { embeddedContext: true }), /audio/);
  await assert.rejects(promptBlocks([{ ...block, byteSize: 8 * 1024 * 1024 + 1 }], { embeddedContext: true }), /size/);
  await writeFile(block.path, "modified");
  await assert.rejects(promptBlocks([block], { embeddedContext: true }), /changed/);
  const link = block.path + "-link";
  await symlink(block.path, link);
  await assert.rejects(promptBlocks([{ ...block, path: link }], { embeddedContext: true }));
});

test("images use their verified bytes and the whole attachment message is bounded", async t => {
  const bytes = Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]);
  const block = { ...(await attachment(t, bytes, "image/png", "Photo.png")), type: "localImage" };
  const [image] = await promptBlocks([block], { image: true });
  assert.equal(image.type, "image");
  assert.equal(image.mimeType, "image/png");
  assert.deepEqual(Buffer.from(image.data, "base64"), bytes);
  await assert.rejects(promptBlocks([block], {}), /cannot read images/);
  const large = await attachment(t, Buffer.alloc(8 * 1024 * 1024, 1), "application/octet-stream", "Data.bin");
  await assert.rejects(promptBlocks([large, large, large], { embeddedContext: true }), /exceed 16 MB/);
});

test("OpenCode slash commands cannot silently omit attachments", async t => {
  const image = { ...(await attachment(t, Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]), "image/png", "Photo.png")), type: "localImage" };
  const pdf = await attachment(t, Buffer.from("%PDF-1.7\nfixture"), "application/pdf", "Plan.pdf");
  const note = await attachment(t, Buffer.from("Review this plan"), "text/plain", "Note.txt");
  const capabilities = { image: true, embeddedContext: true };
  const text = text => ({ type: "text", text });
  // Native parsing joins text parts without a separator and trims whitespace.
  // Unknown commands and /compact omit binary parts too; catalogs cannot make
  // any of these combinations safe.
  for (const input of [
    [text("/review"), image], [image, text(" \n/compact ")],
    [text("/not-a-command"), pdf], [text("/"), image, text("review")],
    [text("/review "), note, image], [text("/review "), note],
  ]) {
    await assert.rejects(promptBlocks(input, capabilities), /Send the command and attachments separately/);
  }
  // Ordinary mentions and native text-resource ordering do not invoke commands.
  for (const input of [
    [text("Explain /review"), image], [text("/"), image],
    [note, text("/review"), image], [text("/review")], [image],
  ]) {
    const blocks = await promptBlocks(input, capabilities);
    assert.equal(blocks.length, input.length);
    if (input.includes(image)) assert.equal(blocks.find(b => b.type === "image").data, Buffer.from([137, 80, 78, 71, 13, 10, 26, 10]).toString("base64"));
  }
});

// Contract: even past the small-file cap, video stays a localFile reference,
// with verified bytes and no base64 payload on the ACP provider wire.
test("large video localFile reaches ACP as a verified path for the agent's tools", { timeout: 30000 }, async t => {
  const bytes = Buffer.alloc(9 * 1024 * 1024, 17);
  Buffer.from("00000018667479706d7034320000000069736f6d6d703432", "hex").copy(bytes);
  const video = await attachment(t, bytes, "video/mp4", "Screen recording.mp4");
  for (const adapter of [openCodeAdapter, cursorAdapter]) {
    const [reference] = await promptBlocks([video], {}, adapter);
    assert.equal(reference.type, "text", adapter.label);
    assert.match(reference.text, /Attached file/);
    assert.ok(reference.text.includes(JSON.stringify(video.path)), adapter.label);
    assert.ok(reference.text.includes("Screen recording.mp4"), adapter.label);
    assert.ok(Buffer.byteLength(JSON.stringify(reference)) < 1000, adapter.label);
  }
  await assert.rejects(promptBlocks([{ ...video, sha256: "0".repeat(64) }], {}), /changed/);
  await assert.rejects(promptBlocks([{ ...video, byteSize: 1024 * 1024 * 1024 + 1 }], {}), /1 GiB/);
  await assert.rejects(promptBlocks([{ type: "text", text: "/review" }, video], {}), /Send the command and attachments separately/);
});
