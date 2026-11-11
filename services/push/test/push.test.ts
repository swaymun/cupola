import {env} from "cloudflare:workers";
import {applyD1Migrations,createExecutionContext,type D1Migration} from "cloudflare:test";
import {beforeAll,afterEach,beforeEach,expect,it,vi} from "vitest";
import worker,{base64url, type Env} from "../src/index";
import {meterDatabase} from "./database-meter";
import {APPLE_KEYS_URL,deviceKey,fakeApple,signIn,type Tokens} from "./apple";
declare global {namespace Cloudflare {interface Env {DB:D1Database;TEST_MIGRATIONS:D1Migration[]}}}
let config:Env;let publicKey:string;let privateKey:CryptoKey;
let payloads:Record<string,any>[]=[];
let migrated:{receipt:unknown,limits:unknown};let apple:Awaited<ReturnType<typeof fakeApple>>;let session:Tokens;
const senderSecret="b".repeat(43),nonce="a".repeat(43),token="ab".repeat(32);
const request=(path:string,value:unknown,credential?:string)=>worker.fetch(new Request("https://push.test"+path,{method:"POST",headers:{"content-type":"application/json","CF-Connecting-IP":"192.0.2.1",...(credential?{authorization:`Bearer ${credential}`}:{})},body:JSON.stringify(value)}),config,createExecutionContext());
beforeAll(async()=>{
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS.slice(0,1));
 const expires=(Math.floor(Date.now()/3600000)+1)*3600;
 await env.DB.batch([
  env.DB.prepare("INSERT INTO deliveries VALUES('migration-device','migration-event','migration-route','completed','delivered',3,?)").bind(expires),
  env.DB.prepare("INSERT INTO limits VALUES(?,60,?)").bind(`send:migration-device:${expires/3600-2}`,expires-3600),
  env.DB.prepare("INSERT INTO limits VALUES(?,7,?)").bind(`send:migration-device:${expires/3600-1}`,expires),
  env.DB.prepare("INSERT INTO limits VALUES('ip:old:1',2,120)")
 ]);
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS.slice(1,2));
 // 0003 (accounts) deliberately drops pre-launch registrations and receipts,
 // so record what 0002 preserved before applying it.
 migrated={receipt:await env.DB.prepare("SELECT state,attempts,route_id FROM deliveries WHERE registration_id='migration-device'").first(),limits:await env.DB.prepare("SELECT key,count FROM limits").all()};
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS.slice(2));
 const pair=await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign","verify"]) as CryptoKeyPair;
 privateKey=pair.privateKey;publicKey=base64url(new Uint8Array(await crypto.subtle.exportKey("raw",pair.publicKey) as ArrayBuffer));
 const pem=btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.exportKey("pkcs8",pair.privateKey) as ArrayBuffer)));
 config={DB:env.DB,IP_RATE_LIMITER:{limit:async()=>({success:true})},APNS_TOPIC:"com.wonder.test",APNS_ENVIRONMENT:"production",APNS_KEY_ID:"TESTKEY",APNS_TEAM_ID:"TESTTEAM",APNS_PRIVATE_KEY:`-----BEGIN PRIVATE KEY-----\n${pem}\n-----END PRIVATE KEY-----`,RATE_SECRET:"test-rate-secret",APPLE_AUDIENCES:"com.swaymun.wonder",ACCOUNT_PEPPER:"test-pepper"};
 apple=await fakeApple();
});
// Apple's JWKS is served by the fake; every other fetch is an APNs send.
const appleKeys=(input:unknown)=>String(input)===APPLE_KEYS_URL?Response.json(apple.jwks):undefined;
beforeEach(async()=>{payloads=[];vi.spyOn(globalThis,"fetch").mockImplementation(async(input,init)=>{const keys=appleKeys(input);if(keys)return keys;payloads.push(JSON.parse(init?.body as string));return new Response(null,{status:200});});
 const signedIn=await signIn((path,value)=>request(path,value),apple,await deviceKey());expect(signedIn.status).toBe(200);session=await signedIn.json() as Tokens;});
afterEach(async()=>{vi.restoreAllMocks();config.DB=env.DB;await env.DB.batch(["challenges","registrations","deliveries","limits","sessions","identities","accounts","activity_rows","activity_cards"].map(t=>env.DB.prepare(`DELETE FROM ${t}`)));});
async function enroll() {
 const challenged=await request("/v1/challenges",{token,publicKey,nonce},session.accessToken);expect(challenged.status).toBe(202);
 const {id}=await challenged.json() as {id:string};
 const proof=payloads.at(-1)!.wonderPush;
 const signature=base64url(new Uint8Array(await crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"},privateKey,new TextEncoder().encode(`register|${id}|${proof.challenge}|${senderSecret}`))));
 const accepted=await request("/v1/registrations",{id,challenge:proof.challenge,senderSecret,signature});expect(accepted.status).toBe(200);return id;
}
it("migrates receipts and the newest quota bucket without resetting delivery state",async()=>{
 expect(migrated.receipt).toEqual({state:"delivered",attempts:3,route_id:"migration-route"});
 expect(migrated.limits).toMatchObject({results:[{key:"send:migration-device",count:7}]});
});
it("requires APNs possession proof and the device signature",async()=>{
 const result=await request("/v1/challenges",{token,publicKey,nonce},session.accessToken);const {id}=await result.json() as {id:string};
 expect((await request("/v1/registrations",{id,challenge:payloads[0].wonderPush.challenge,senderSecret,signature:"bad"})).status).toBe(403);
 expect(await env.DB.prepare("SELECT COUNT(*) count FROM registrations").first("count")).toBe(0);
 expect(payloads[0].aps).toEqual({"content-available":1});
});
it("delivers generic alerts once and rejects changed duplicate routing",async()=>{
 const id=await enroll(), eventId=crypto.randomUUID(),routeId=crypto.randomUUID();
 expect((await request(`/v1/registrations/${id}/send`,{eventId,routeId,kind:"completed",text:"never forward private text"},senderSecret)).status).toBe(200);
 const duplicate=await request(`/v1/registrations/${id}/send`,{eventId,routeId,kind:"completed"},senderSecret);expect(await duplicate.json()).toEqual({delivered:true,duplicate:true});
 expect(payloads.length).toBe(2);expect(JSON.stringify(payloads[1])).not.toContain("private text");expect(payloads[1].wonderPush).toEqual({registrationId:id,routeId});
 // Shown as is when the phone cannot rewrite the alert: say what the app says.
 expect(payloads[1].aps.alert).toEqual({title:"Cupola",body:"A reply finished."});
 expect((await request(`/v1/registrations/${id}/send`,{eventId:crypto.randomUUID(),routeId,kind:"attention"},senderSecret)).status).toBe(200);
 expect(payloads.at(-1)!.aps.alert).toEqual({title:"Cupola",body:"A task needs your attention."});
 expect((await request(`/v1/registrations/${id}/send`,{eventId,routeId:crypto.randomUUID(),kind:"completed"},senderSecret)).status).toBe(409);
});
it("rejects unauthorized send and makes revocation terminal",async()=>{
 const id=await enroll();expect((await request(`/v1/registrations/${id}/send`,{},"c".repeat(43))).status).toBe(403);
 expect((await request(`/v1/registrations/${id}/revoke`,{},senderSecret)).status).toBe(200);
 expect((await request(`/v1/registrations/${id}/send`,{},senderSecret)).status).toBe(410);
});
it("removes stale APNs tokens and never claims delivery",async()=>{
 const id=await enroll();vi.mocked(fetch).mockResolvedValueOnce(Response.json({reason:"Unregistered"},{status:410}));
 expect((await request(`/v1/registrations/${id}/send`,{eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"attention"},senderSecret)).status).toBe(410);
 expect(await env.DB.prepare("SELECT COUNT(*) count FROM registrations").first("count")).toBe(0);
});
it("bounds token enrollment abuse before sending a fourth challenge",async()=>{
 for(let i=0;i<3;i++)expect((await request("/v1/challenges",{token,publicKey,nonce},session.accessToken)).status).toBe(202);
 expect((await request("/v1/challenges",{token,publicKey,nonce},session.accessToken)).status).toBe(429);expect(payloads.length).toBe(3);
});
it("bounds one account's enrollments so it cannot spend everyone's quota",async()=>{
 const tokens=Array.from({length:21},(_,i)=>i.toString(16).padStart(2,"0").repeat(32));
 for(const each of tokens.slice(0,20))expect((await request("/v1/challenges",{token:each,publicKey,nonce},session.accessToken)).status).toBe(202);
 expect((await request("/v1/challenges",{token:tokens[20],publicKey,nonce},session.accessToken)).status).toBe(429);
 expect(await env.DB.prepare("SELECT count FROM limits WHERE key='enroll-global'").first("count")).toBe(20);
});
it("bounds retries and deduplicates concurrent delivery",async()=>{
 const id=await enroll();const body={eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed"};
 const results=await Promise.all([request(`/v1/registrations/${id}/send`,body,senderSecret),request(`/v1/registrations/${id}/send`,body,senderSecret)]);
 expect(results.some(r=>r.status===200)).toBe(true);expect(payloads.length).toBe(2);
});

it("verifies the sender capability and preserves only the renewed registration",async()=>{
 const old=await enroll();
 expect((await request(`/v1/registrations/${old}/status`,{},senderSecret)).status).toBe(200);
 expect((await request(`/v1/registrations/${old}/status`,{},"z".repeat(43))).status).toBe(403);
 const current=await enroll();expect(current).not.toBe(old);
 expect((await request(`/v1/registrations/${old}/status`,{},senderSecret)).status).toBe(410);
 expect((await request(`/v1/registrations/${current}/status`,{},senderSecret)).status).toBe(200);
 const proof=payloads.at(-1)!.wonderPush;
 const signature=base64url(new Uint8Array(await crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"},privateKey,new TextEncoder().encode(`register|${current}|${proof.challenge}|${senderSecret}`))));
 expect((await request(`/v1/registrations/${current}/revoke`,{},senderSecret)).status).toBe(200);
 expect((await request("/v1/registrations",{id:current,challenge:proof.challenge,senderSecret,signature})).status).toBe(403);
});
it("stops retrying after five upstream failures",async()=>{
 const id=await enroll(), event={eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"attention"};
 vi.mocked(fetch).mockImplementation(async()=>Response.json({reason:"InternalServerError"},{status:500}));
 for(let i=0;i<5;i++)expect((await request(`/v1/registrations/${id}/send`,event,senderSecret)).status).toBe(503);
 expect((await request(`/v1/registrations/${id}/send`,event,senderSecret)).status).toBe(429);
 expect(await env.DB.prepare("SELECT attempts FROM deliveries WHERE registration_id=?").bind(id).first("attempts")).toBe(5);
});

it("rejects an edge-limited request before accessing D1 or APNs",async()=>{
 vi.spyOn(config.IP_RATE_LIMITER,"limit").mockResolvedValueOnce({success:false});
 const prepared=vi.spyOn(config.DB,"prepare");
 expect((await request("/v1/challenges",{token,publicKey,nonce},session.accessToken)).status).toBe(429);
 expect(prepared).not.toHaveBeenCalled();expect(payloads).toHaveLength(0);
});

it("enforces exact device and global quotas, reusing rows after rollover",async()=>{
 const id=await enroll();const current=Math.floor(Date.now()/1000);
 await env.DB.prepare("INSERT INTO limits VALUES(?,60,?)").bind(`send:${id}`,current+3600).run();
 const event=()=>({eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed"});
 expect((await request(`/v1/registrations/${id}/send`,event(),senderSecret)).status).toBe(429);
 await env.DB.prepare("UPDATE limits SET expires=? WHERE key=?").bind(current-1,`send:${id}`).run();
 expect((await request(`/v1/registrations/${id}/send`,event(),senderSecret)).status).toBe(200);
 expect(await env.DB.prepare("SELECT count FROM limits WHERE key=?").bind(`send:${id}`).first("count")).toBe(1);
 await env.DB.prepare("UPDATE limits SET count=30000 WHERE key='send-global'").run();
 expect((await request(`/v1/registrations/${id}/send`,event(),senderSecret)).status).toBe(429);
 expect(payloads).toHaveLength(2);
});

it("allows only one concurrent send into the final device quota slot",async()=>{
 const id=await enroll();
 await env.DB.prepare("INSERT INTO limits VALUES(?,59,?)").bind(`send:${id}`,Math.floor(Date.now()/1000)+3600).run();
 const responses=await Promise.all(Array.from({length:5},()=>request(`/v1/registrations/${id}/send`,{eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed"},senderSecret)));
 expect(responses.filter(r=>r.status===200)).toHaveLength(1);
 expect(responses.filter(r=>r.status===429)).toHaveLength(4);
 expect(payloads).toHaveLength(2);
});

it("uses four D1 writes per send, none for duplicates, and one per expired receipt",async()=>{
 const id=await enroll();const metered=meterDatabase(env.DB);config.DB=metered.db;
 const event={eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed"};
 expect((await request(`/v1/registrations/${id}/send`,event,senderSecret)).status).toBe(200);
 expect(metered.totals.written).toBe(4);
 expect((await request(`/v1/registrations/${id}/send`,event,senderSecret)).status).toBe(200);
 expect(metered.totals.written).toBe(4);
 for(let i=1;i<20;i++)expect((await request(`/v1/registrations/${id}/send`,{...event,eventId:crypto.randomUUID()},senderSecret)).status).toBe(200);
 expect(metered.totals.written).toBe(80);
 await env.DB.prepare("UPDATE deliveries SET updated=?").bind(Math.floor(Date.now()/1000)-8*86400).run();
 await worker.scheduled({} as ScheduledController,config);
 expect(metered.totals.written).toBe(100);
 expect(await env.DB.prepare("SELECT COUNT(*) count FROM deliveries").first("count")).toBe(0);
 console.info("Push D1 cost: 20 sends + 1 duplicate + receipt expiry =",metered.totals);
});
it("forwards bounded encrypted previews without persisting them or plaintext",async()=>{
 const id=await enroll(),eventId=crypto.randomUUID(),routeId=crypto.randomUUID();
 const preview=base64url(crypto.getRandomValues(new Uint8Array(2000)));
 expect((await request(`/v1/registrations/${id}/send`,{eventId,routeId,kind:"attention",preview,text:"must stay private"},senderSecret)).status).toBe(200);
 const payload=payloads.at(-1)!;
 expect(payload.aps["mutable-content"]).toBe(1);
 expect(payload.wonderPush).toEqual({registrationId:id,routeId,eventId,preview});
 expect(JSON.stringify(payload)).not.toContain("must stay private");
 expect(new TextEncoder().encode(JSON.stringify(payload)).length).toBeLessThan(4096);
 const receipt=await env.DB.prepare("SELECT * FROM deliveries WHERE event_id=?").bind(eventId).first();
 expect(JSON.stringify(receipt)).not.toContain(preview);
});
it("rejects malformed or oversized previews before claiming delivery",async()=>{
 const id=await enroll();
 for(const preview of ["bad+base64", "a".repeat(3241), {}, ""]) {
  expect((await request(`/v1/registrations/${id}/send`,{eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed",preview},senderSecret)).status).toBe(400);
 }
 expect(payloads).toHaveLength(1);
 expect(await env.DB.prepare("SELECT COUNT(*) AS count FROM deliveries").first()).toEqual({count:0});
});
