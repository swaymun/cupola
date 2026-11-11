// One Live Activity per phone, merged here from every computer's rows.
import {env} from "cloudflare:workers";
import {applyD1Migrations,createExecutionContext,type D1Migration} from "cloudflare:test";
import {beforeAll,afterEach,beforeEach,expect,it,vi} from "vitest";
import worker,{base64url,type Env} from "../src/index";
import {FINISHED_TTL,RUNNING_TTL,START_GRACE,WAITING_TTL,aggregate,clip,type Row} from "../src/activity";
import {APPLE_KEYS_URL,deviceKey,fakeApple,signIn,type Tokens} from "./apple";
declare global {namespace Cloudflare {interface Env {DB:D1Database;TEST_MIGRATIONS:D1Migration[]}}}
interface Sent {url:string;headers:Record<string,string>;body:any}
let config:Env,apple:Awaited<ReturnType<typeof fakeApple>>,session:Tokens;
let sent:Sent[]=[],apns={status:200,reason:""};
const phoneToken="ab".repeat(32),updateToken="cd".repeat(32),startToken="ef".repeat(32),nonce="a".repeat(43);
const request=(path:string,value:unknown,credential?:string)=>worker.fetch(new Request("https://push.test"+path,{method:"POST",headers:{"content-type":"application/json","CF-Connecting-IP":"192.0.2.1",...(credential?{authorization:`Bearer ${credential}`}:{})},body:JSON.stringify(value)}),config,createExecutionContext());
beforeAll(async()=>{
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS);
 const pair=await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign","verify"]) as CryptoKeyPair;
 const pem=btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.exportKey("pkcs8",pair.privateKey) as ArrayBuffer)));
 config={DB:env.DB,IP_RATE_LIMITER:{limit:async()=>({success:true})},APNS_TOPIC:"com.wonder.test",APNS_ENVIRONMENT:"production",APNS_KEY_ID:"TESTKEY",APNS_TEAM_ID:"TESTTEAM",APNS_PRIVATE_KEY:`-----BEGIN PRIVATE KEY-----\n${pem}\n-----END PRIVATE KEY-----`,RATE_SECRET:"test-rate-secret",APPLE_AUDIENCES:"com.swaymun.wonder",ACCOUNT_PEPPER:"test-pepper"};
 apple=await fakeApple();
});
beforeEach(async()=>{
 sent=[];apns={status:200,reason:""};
 vi.spyOn(globalThis,"fetch").mockImplementation(async(input,init)=>{
  if(String(input)===APPLE_KEYS_URL)return Response.json(apple.jwks);
  sent.push({url:String(input),headers:init?.headers as Record<string,string>,body:JSON.parse(init?.body as string)});
  return apns.status===200?new Response(null,{status:200}):Response.json({reason:apns.reason},{status:apns.status});
 });
 const signedIn=await signIn((path,value)=>request(path,value),apple,await deviceKey());expect(signedIn.status).toBe(200);session=await signedIn.json() as Tokens;
});
afterEach(async()=>{vi.restoreAllMocks();await env.DB.batch(["challenges","registrations","deliveries","limits","sessions","identities","accounts","activity_rows","activity_cards"].map(t=>env.DB.prepare(`DELETE FROM ${t}`)));});

interface Computer {id:string;secret:string}
let computers=0;
// One phone, several computers: each computer is its own registration (its own
// device key and sender capability) under the phone's one signed-in session.
async function enroll():Promise<Computer> {
 const pair=await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign","verify"]) as CryptoKeyPair;
 const publicKey=base64url(new Uint8Array(await crypto.subtle.exportKey("raw",pair.publicKey) as ArrayBuffer));
 const senderSecret=base64url(crypto.getRandomValues(new Uint8Array(32)));computers++;
 const challenged=await request("/v1/challenges",{token:phoneToken,publicKey,nonce},session.accessToken);expect(challenged.status).toBe(202);
 const {id}=await challenged.json() as {id:string};
 const proof=sent.at(-1)!.body.wonderPush;
 const signature=base64url(new Uint8Array(await crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"},pair.privateKey,new TextEncoder().encode(`register|${id}|${proof.challenge}|${senderSecret}`))));
 expect((await request("/v1/registrations",{id,challenge:proof.challenge,senderSecret,signature})).status).toBe(200);
 sent=[];return {id,secret:senderSecret};
}
const at=()=>Math.floor(Date.now()/1000);
let order=0;
const chat=(title:string,status="running",project="Cupola")=>({routeId:crypto.randomUUID(),title,project,status,startedAt:at()-600+order++});
type Chat=ReturnType<typeof chat>;
const report=(c:Computer,rows:Chat[],extra:Record<string,unknown>={})=>request(`/v1/registrations/${c.id}/activity`,{rows,...extra},c.secret);
const phone=(c:Computer,kind:string,token?:string)=>request(`/v1/registrations/${c.id}/activity-state`,{kind,...(token?{token}:{})},c.secret);
const pushes=()=>sent.filter(s=>s.body.aps?.event);
const last=()=>pushes().at(-1)!;
const rows=(state:any)=>state.threads.map((r:any)=>`${r.title}:${r.status}`);
const card=()=>env.DB.prepare("SELECT * FROM activity_cards").first<any>();

it("shows one card for chats on two computers, with each state and the finished window",async()=>{
 const a=await enroll(),b=await enroll();
 expect(await (await phone(a,"update",updateToken)).json()).toEqual({registered:true});
 const [a1,a2,b1]=[chat("Fix login"),chat("Write docs"),chat("Deploy","running","Studio")];
 expect((await report(a,[a1,a2])).status).toBe(200);
 expect(rows(last().body.aps["content-state"]).sort()).toEqual(["Fix login:running","Write docs:running"]);
 // The second computer adds its chat; it neither replaces nor is replaced.
 expect((await report(b,[b1])).status).toBe(200);
 let state=last().body.aps["content-state"];
 expect(state).toMatchObject({phase:"running",running:3,needsInput:0});
 expect(state.more).toBeUndefined();
 expect(state.threads.map((r:any)=>[r.title,r.registrationId===a.id?"A":"B"]).sort()).toEqual([["Deploy","B"],["Fix login","A"],["Write docs","A"]]);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(3);
 expect(pushes()).toHaveLength(2);
 expect(last().headers).toMatchObject({"apns-push-type":"liveactivity","apns-topic":"com.wonder.test.push-type.liveactivity"});
 expect(last().body.aps.alert).toBeUndefined();
 // A replay of the same report repaints nothing and never alerts.
 expect((await report(a,[a1,a2])).status).toBe(200);
 expect(pushes()).toHaveLength(2);
 // A chat finishes: Done stays in its row while the others still run.
 expect((await report(a,[{...a1,status:"done"},a2])).status).toBe(200);
 state=last().body.aps["content-state"];
 expect(last().body.aps.event).toBe("update");
 expect(state).toMatchObject({phase:"running",running:2});
 expect(rows(state)).toEqual(["Write docs:running","Deploy:running","Fix login:done"]);
 // Reporting the same finished chat again does not extend its window.
 await env.DB.prepare("UPDATE activity_rows SET updated=updated-? WHERE title='Fix login'").bind(FINISHED_TTL+1).run();
 expect((await report(a,[{...a1,status:"done"},a2])).status).toBe(200);
 expect(rows(last().body.aps["content-state"])).toEqual(["Write docs:running","Deploy:running"]);
 // Waiting on approval leads the card.
 expect((await report(b,[{...b1,status:"needsApproval"}])).status).toBe(200);
 state=last().body.aps["content-state"];
 expect(state).toMatchObject({phase:"needsApproval",running:2,needsInput:1});
 expect(rows(state)[0]).toBe("Deploy:needsApproval");
 expect(last().headers["apns-priority"]).toBe("10");
 // Everything finishes: one end push, dismissed after the finished window, token spent.
 expect((await report(b,[{...b1,status:"done"}])).status).toBe(200);
 expect((await report(a,[{...a2,status:"failed"}])).status).toBe(200);
 expect(last().body.aps).toMatchObject({event:"end","content-state":{phase:"failed",running:0,needsInput:0}});
 expect(last().body.aps["dismissal-date"]).toBeGreaterThan(at()+FINISHED_TTL-60);
 expect(rows(last().body.aps["content-state"]).sort()).toEqual(["Deploy:done","Write docs:failed"]);
 expect((await card()).update_token).toBeNull();
 const ended=pushes().length;
 expect((await report(a,[{...a2,status:"failed"}])).status).toBe(200);
 expect(pushes()).toHaveLength(ended);
});

it("caps the card at five rows, most urgent first, and names the rest",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 const list=[chat("one"),chat("two"),chat("three"),chat("four"),chat("five"),chat("six"),chat("seven")];
 list[6].status="needsInput";list[5].status="needsApproval";
 expect((await report(a,list)).status).toBe(200);
 const state=last().body.aps["content-state"];
 expect(state).toMatchObject({phase:"needsApproval",running:7,needsInput:2,more:2});
 expect(rows(state)).toEqual(["six:needsApproval","seven:needsInput","one:running","two:running","three:running"]);
});

it("upserts rows, drops working chats a computer stops reporting and keeps finished ones",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 const [x,y,z]=[chat("x"),chat("y"),chat("z")];
 await report(a,[x,y,z]);
 await report(a,[{...x,title:"x renamed"},{...y,status:"needsInput"}]);
 const stored=(await env.DB.prepare("SELECT title,status FROM activity_rows ORDER BY title").all()).results;
 expect(stored).toEqual([{title:"x renamed",status:"running"},{title:"y",status:"needsInput"}]);
 await report(a,[{...y,status:"stopped"}]);
 expect((await env.DB.prepare("SELECT title,status FROM activity_rows ORDER BY title").all()).results).toEqual([{title:"y",status:"done"}]);
 // A finished chat that is no longer reported stays for its window.
 await report(a,[]);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(1);
});

it("expires running rows after 2 hours, waiting rows after 24 and finished rows after 15 minutes",()=>{
 const t=1_800_000_000;
 const row=(status:string,age:number):Row=>({registration_id:"r",route_id:crypto.randomUUID(),session_id:"s",title:status,project:"p",status,started:t-9000,updated:t-age});
 const live=(rows:Row[])=>aggregate(rows,t)?.state.threads.map(r=>r.status)??[];
 expect(live([row("running",RUNNING_TTL-1)])).toEqual(["running"]);
 expect(live([row("running",RUNNING_TTL)])).toEqual([]);
 expect(live([row("needsApproval",WAITING_TTL-1),row("needsInput",WAITING_TTL)])).toEqual(["needsApproval"]);
 expect(live([row("done",FINISHED_TTL-1),row("failed",FINISHED_TTL)])).toEqual(["done"]);
 // Only finished rows: a terminal card (failed wins), with nothing running.
 const terminal=aggregate([row("done",10),row("failed",20)],t)!;
 expect(terminal.state).toMatchObject({phase:"failed",running:0,needsInput:0});
 expect(terminal.working).toBe(0);
});

it("ends the card when a computer goes quiet and its running rows expire",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 await report(a,[chat("stuck")]);
 await env.DB.prepare("UPDATE activity_rows SET updated=updated-?").bind(RUNNING_TTL+1).run();
 await env.DB.prepare("UPDATE activity_cards SET sent_at=sent_at-1000").run();
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 expect(last().body.aps).toMatchObject({event:"end","content-state":{phase:"done",running:0,threads:[]}});
 expect((await card()).update_token).toBeNull();
});

it("stops every push after the owner dismisses the card, until Cupola is opened again",async()=>{
 const a=await enroll(),b=await enroll();
 await phone(a,"start",startToken);await phone(a,"update",updateToken);
 const first=chat("first");await report(a,[first]);
 expect(pushes()).toHaveLength(1);
 expect(await (await phone(b,"dismissed")).json()).toEqual({registered:false,dismissed:true});
 expect(await card()).toMatchObject({update_token:null,dismissed:1});
 const quiet=pushes().length;
 // A chat finishes and another starts, on either computer: still no card, no update, no push-to-start.
 await report(a,[{...first,status:"done"}]);
 const second=chat("second");await report(b,[second]);
 await env.DB.prepare("UPDATE activity_cards SET work_since=work_since-100").run();
 await report(b,[second]);await report(a,[{...first,status:"done"},chat("third")]);
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 expect(pushes()).toHaveLength(quiet);
 // A late update-token registration cannot revive the dismissed card.
 expect(await (await phone(a,"update","12".repeat(32))).json()).toEqual({registered:false,dismissed:true});
 expect((await card()).update_token).toBeNull();
 // Opening Cupola lifts the gate; with chats running, the next report starts a card.
 expect(await (await phone(a,"foreground")).json()).toEqual({dismissed:false});
 expect((await report(b,[second])).status).toBe(200);
 await env.DB.prepare("UPDATE activity_cards SET work_since=work_since-100").run();
 expect(await (await report(b,[second])).json()).toEqual({stored:true});
 expect(last().body.aps).toMatchObject({event:"start",attributes:{registrationId:b.id},alert:{title:"Cupola"}});
 expect(last().body.aps["content-state"].running).toBe(2);
});

it("treats a card iOS no longer has (swiped while Cupola was suspended, or the system's 8 hour end) as dismissed",async()=>{
 const a=await enroll();await phone(a,"start",startToken);await phone(a,"update",updateToken);
 apns={status:410,reason:"Unregistered"};
 expect((await report(a,[chat("one")])).status).toBe(200);
 expect(await card()).toMatchObject({update_token:null,dismissed:1});
 apns={status:200,reason:""};sent=[];
 await env.DB.prepare("UPDATE activity_cards SET work_since=1").run();
 await report(a,[chat("one"),chat("two")]);
 expect(pushes()).toHaveLength(0);
});

it("starts a card by push-to-start only for waiting work, after the phone had time to start its own",async()=>{
 const a=await enroll();await phone(a,"start",startToken);
 const first=chat("first");
 const waiting=await (await report(a,[first])).json() as {revisit?:number};
 expect(waiting.revisit).toBeGreaterThan(0);expect(waiting.revisit).toBeLessThanOrEqual(START_GRACE+1);
 expect(pushes()).toHaveLength(0);
 await env.DB.prepare("UPDATE activity_cards SET work_since=work_since-?").bind(START_GRACE+1).run();
 expect(await (await report(a,[first])).json()).toEqual({stored:true});
 expect(last().body.aps).toMatchObject({event:"start","attributes-type":"AgentActivityAttributes",attributes:{registrationId:a.id},alert:{body:"Agents are working on your computer."}});
 expect(last().headers["apns-priority"]).toBe("10");
 // The same stretch is not started twice while the phone registers the new card.
 await report(a,[first,chat("second")]);
 expect(pushes()).toHaveLength(1);
 // The phone's own card (update token) means no push-to-start.
 await phone(a,"update",updateToken);
 await report(a,[first,chat("second"),chat("third")]);
 expect(pushes().map(p=>p.body.aps.event)).toEqual(["start","update"]);
 // Registering a token returns the combined state for the phone to apply at once.
 const again=await (await phone(a,"update","34".repeat(32))).json() as any;
 expect(again.registered).toBe(true);expect(again.state).toMatchObject({phase:"running",running:3});
 // Work that is only finished never starts a card.
 await phone(a,"ended","34".repeat(32));await env.DB.prepare("UPDATE activity_cards SET start_sent_at=0").run();sent=[];
 await report(a,[{...first,status:"done"}]);
 expect(pushes()).toHaveLength(0);
});

it("keeps every push under ActivityKit's 4 KB limit with the widest titles",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 const wide=(i:number)=>({...chat("😀".repeat(110)),project:"界".repeat(100),routeId:crypto.randomUUID(),startedAt:at()-i});
 expect((await report(a,Array.from({length:8},(_,i)=>wide(i)))).status).toBe(200);
 const body=JSON.stringify(last().body);
 expect(new TextEncoder().encode(body).length).toBeLessThan(4096);
 const state=last().body.aps["content-state"];
 expect(state.threads).toHaveLength(5);expect(state.more).toBe(3);
 expect([...state.threads[0].title]).toHaveLength(40);expect([...state.threads[0].project]).toHaveLength(24);
 expect(clip("a\n\tb  c\u0007d",40)).toBe("a b c d");
 // Even the start push, which also carries attributes and an alert.
 await phone(a,"ended",updateToken);await phone(a,"start",startToken);
 await env.DB.prepare("UPDATE activity_cards SET work_since=1").run();
 await report(a,Array.from({length:8},(_,i)=>wide(i)));
 expect(last().body.aps.event).toBe("start");
 expect(new TextEncoder().encode(JSON.stringify(last().body)).length).toBeLessThan(4096);
});

it("accepts only the documented row fields, from the registration's own capability",async()=>{
 const a=await enroll(),other=await enroll();await phone(a,"update",updateToken);
 const good=chat("ok");
 const bad:unknown[]=[
  {rows:[{...good,conversationId:"thread-1"}]},{rows:[{...good,text:"private"}]},{rows:[{...good,routeId:"conversation-id"}]},
  {rows:[{...good,status:"Writing payroll.xlsx"}]},{rows:[{...good,startedAt:1}]},{rows:[{...good,title:7}]},{rows:[{...good,title:"x".repeat(401)}]},
  {rows:[good,good]},{rows:Array.from({length:21},()=>chat("many"))},{rows:"none"},{},{rows:[good],provider:"codex"},
 ];
 for(const body of bad)expect((await request(`/v1/registrations/${a.id}/activity`,body,a.secret)).status).toBe(400);
 expect((await request(`/v1/registrations/${a.id}/activity`,{rows:[good]},other.secret)).status).toBe(403);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 for(const body of [{kind:"update"},{kind:"update",token:"short"},{kind:"nope"},{kind:"dismissed",token:updateToken},{kind:"foreground",extra:1},{kind:"start",token:"zz".repeat(32)}])
  expect((await request(`/v1/registrations/${a.id}/activity-state`,body,a.secret)).status).toBe(400);
 expect(JSON.stringify(sent)).not.toMatch(/payroll|private|conversation-id|thread-1|codex/);
});

it("keeps rows only while a phone has a card, and deletes them when it turns off, unpairs or signs out",async()=>{
 const a=await enroll(),b=await enroll();
 // No card yet: nothing is stored.
 expect(await (await report(a,[chat("early")])).json()).toEqual({stored:false});
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 await phone(a,"update",updateToken);
 await report(a,[chat("a1")]);await report(b,[chat("b1")]);
 // Unpairing one computer removes only its rows.
 expect((await request(`/v1/registrations/${b.id}/revoke`,{},b.secret)).status).toBe(200);
 expect((await env.DB.prepare("SELECT title FROM activity_rows").all()).results).toEqual([{title:"a1"}]);
 await report(a,[]);
 // Turning Live Activities off forgets rows and tokens.
 await report(a,[chat("a2")]);
 expect(await (await phone(a,"off")).json()).toEqual({registered:false});
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_cards").first("c")).toBe(0);
 // Signing out removes the session's rows and card too.
 await phone(a,"update",updateToken);await report(a,[chat("a3")]);
 expect((await request("/v1/auth/signout",{},session.accessToken)).status).toBe(200);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_cards").first("c")).toBe(0);
});

it("ignores rows of a replaced registration, and the hourly reaper deletes expired and orphaned rows",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 await report(a,[chat("kept")]);
 // The same computer enrolls again (a new registration under the same key is a new ID).
 await env.DB.prepare("INSERT INTO activity_rows VALUES('gone-registration',?,(SELECT session_id FROM activity_rows LIMIT 1),'orphan','p','running',1,?)").bind(crypto.randomUUID(),at()).run();
 await env.DB.prepare("INSERT INTO activity_rows SELECT registration_id,?,session_id,'old','p','done',1,? FROM activity_rows LIMIT 1").bind(crypto.randomUUID(),at()-2*86400).run();
 await report(a,[chat("kept")]);
 expect(rows(last().body.aps["content-state"]).every((r:string)=>!r.startsWith("orphan"))).toBe(true);
 await worker.scheduled({cron:"17 * * * *"} as ScheduledController,config);
 expect((await env.DB.prepare("SELECT title FROM activity_rows").all()).results.map((r:any)=>r.title)).toEqual(["kept"]);
});

it("deletes finished rows after 15 minutes and running rows after 2 hours, not at the next daily sweep",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 await report(a,[chat("keep")]);
 const session_id=await env.DB.prepare("SELECT session_id FROM activity_rows").first<string>("session_id");
 const insert=(title:string,status:string,age:number)=>env.DB.prepare("INSERT INTO activity_rows VALUES(?,?,?,?,'p',?,1,?)").bind(a.id,crypto.randomUUID(),session_id,title,status,at()-age).run();
 await insert("done-old","done",FINISHED_TTL+60);await insert("failed-old","failed",FINISHED_TTL+60);await insert("done-fresh","done",FINISHED_TTL-120);
 await insert("running-old","running",RUNNING_TTL+60);await insert("running-fresh","running",RUNNING_TTL-120);
 await insert("waiting-fresh","needsApproval",RUNNING_TTL+3600);await insert("waiting-old","needsInput",WAITING_TTL+60);
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 expect((await env.DB.prepare("SELECT title FROM activity_rows ORDER BY title").all()).results.map((r:any)=>r.title)).toEqual(["done-fresh","keep","running-fresh","waiting-fresh"]);
});

it("stores nothing when Live Activities are off, however often Cupola is reopened",async()=>{
 const a=await enroll();
 await phone(a,"update",updateToken);await report(a,[chat("one")]);
 expect(await (await phone(a,"off")).json()).toEqual({registered:false});
 // Foreground, dismissed and ended are about a card; with none they create nothing.
 for(const kind of ["foreground","dismissed"])await phone(a,kind);
 await phone(a,"ended",updateToken);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_cards").first("c")).toBe(0);
 expect(await (await report(a,[chat("two","needsApproval")])).json()).toEqual({stored:false});
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 expect(pushes()).toHaveLength(1);
});

it("does not send a push-to-start that was in flight when the owner dismissed the card",async()=>{
 const a=await enroll();await phone(a,"start",startToken);
 const first=chat("first");await report(a,[first]);
 await env.DB.prepare("UPDATE activity_cards SET work_since=work_since-100").run();
 // Hold the report after it read the card (not yet dismissed) while the dismissal completes.
 let reached!:()=>void,release!:()=>void;
 const paused=new Promise<void>(resolve=>reached=resolve),gate=new Promise<void>(resolve=>release=resolve);
 const real=config.DB;
 config={...config,DB:new Proxy(real,{get:(target,key)=>key==="prepare"?(sql:string)=>{
  const statement=target.prepare(sql);
  if(!sql.startsWith("SELECT r.* FROM activity_rows"))return statement;
  return new Proxy(statement,{get:(inner,name)=>name==="bind"?(...values:unknown[])=>{const bound=inner.bind(...values);return new Proxy(bound,{get:(b,method)=>method==="all"?async()=>{reached();await gate;return b.all();}:Reflect.get(b,method).bind?.(b)??Reflect.get(b,method)});}:Reflect.get(inner,name)});
 }:Reflect.get(target,key).bind(target)}) as D1Database};
 const racing=report(a,[first,chat("second")]);
 await paused;
 config={...config,DB:real};
 await phone(a,"dismissed");
 release();
 expect((await racing).status).toBe(200);
 expect(pushes()).toHaveLength(0);
 expect(await card()).toMatchObject({dismissed:1,update_token:null});
 // A token that registers after the dismissal stays unset too.
 await phone(a,"update",updateToken);
 expect((await card()).update_token).toBeNull();
});

it("stores no titles from a report that read the card before Live Activities were turned off",async()=>{
 const a=await enroll();await phone(a,"update",updateToken);
 // Hold the report right after it saw the card, while the owner turns the feature off.
 let reached!:()=>void,release!:()=>void,held=false;
 const paused=new Promise<void>(resolve=>reached=resolve),gate=new Promise<void>(resolve=>release=resolve);
 const real=config.DB;
 config={...config,DB:new Proxy(real,{get:(target,key)=>key==="prepare"?(sql:string)=>{
  const statement=target.prepare(sql);
  if(held || !sql.startsWith("SELECT 1 AS present FROM activity_cards"))return statement;
  return new Proxy(statement,{get:(inner,name)=>name==="bind"?(...values:unknown[])=>{const bound=inner.bind(...values);return new Proxy(bound,{get:(b,method)=>method==="first"?async(...args:unknown[])=>{const value=await (b.first as any)(...args);held=true;reached();await gate;return value;}:Reflect.get(b,method).bind?.(b)??Reflect.get(b,method)});}:Reflect.get(inner,name)});
 }:Reflect.get(target,key).bind(target)}) as D1Database};
 const racing=report(a,[chat("private title","running","Secret project")]);
 await paused;
 config={...config,DB:real};
 expect(await (await phone(a,"off")).json()).toEqual({registered:false});
 release();
 const response=await racing;
 expect(response.status).toBe(200);
 expect(await response.json()).toEqual({stored:false});
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_rows").first("c")).toBe(0);
 expect(await env.DB.prepare("SELECT COUNT(*) c FROM activity_cards").first("c")).toBe(0);
 expect(pushes()).toHaveLength(0);
});

it("with alerts off a registration gets no banner push but still gets Live Activity updates",async()=>{
 const a=await enroll();
 expect(await (await phone(a,"update",updateToken)).json()).toEqual({registered:true});
 const alerts=(enabled:unknown,secret=a.secret)=>request(`/v1/registrations/${a.id}/alerts`,{enabled},secret);
 expect((await alerts("no")).status).toBe(400);
 expect((await alerts(false,"z".repeat(43))).status).toBe(403);
 expect(await (await alerts(false)).json()).toEqual({alerts:false});
 const send=(kind="completed")=>request(`/v1/registrations/${a.id}/send`,{eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind},a.secret);
 sent=[];
 const suppressed=await send();expect(suppressed.status).toBe(200);expect(await suppressed.json()).toEqual({delivered:true,suppressed:true});
 expect(sent.filter(s=>s.body.aps?.alert)).toHaveLength(0);expect(sent).toHaveLength(0);
 // Live Activity updates are unaffected.
 expect((await report(a,[chat("Fix login")])).status).toBe(200);
 expect(pushes()).toHaveLength(1);expect(last().body.aps.alert).toBeUndefined();
 // Turning Notifications back on restores banners.
 expect(await (await alerts(true)).json()).toEqual({alerts:true});
 expect((await send()).status).toBe(200);
 expect(sent.filter(s=>s.body.aps?.alert)).toHaveLength(1);
});
