// Writes the synthetic project folder used by session-list.mjs and the Rust
// thread_page benchmark, and prints its chat IDs (newest first) as JSON.
//   node bench/write-fixture.mjs CONFIG_DIR CWD [FILES] [EVERY]
// Every EVERYth transcript is a chat (default 18: 17 chats among 300).
import { writeSessionFixture } from "../test/session-fixture.mjs";

const [config, cwd, files = "300", every = "18"] = process.argv.slice(2);
if (!config || !cwd) throw new Error("usage: write-fixture.mjs CONFIG_DIR CWD [FILES]");
console.log(JSON.stringify((await writeSessionFixture(config, cwd, { files: Number(files), every: Number(every) })).chats));
