import {env} from "cloudflare:workers";
import {applyD1Migrations,createExecutionContext} from "cloudflare:test";
import {afterEach,beforeAll,beforeEach,expect,it,vi} from "vitest";
import worker,{type Env} from "../src/index";
import {resetAppleKeys} from "../src/accounts";
import {APPLE_KEYS_URL,deviceKey,fakeApple,nonceHash,signIn,type Device,type Tokens} from "./apple";
import {base64url} from "../src/index";

let config:Env;let apple:Awaited<ReturnType<typeof fakeApple>>;let jwks:unknown;let keyFetches=0;
let pushes:{url:string,body:any}[]=[];
const request=(path:string,value:unknown,credential?:string,ip="192.0.2.1")=>worker.fetch(new Request("https://push.test"+path,{method:"POST",headers:{"content-type":"application/json","CF-Connecting-IP":ip,...(credential?{authorization:`Bearer ${credential}`}:{})},body:JSON.stringify(value)}),config,createExecutionContext());
const post=(path:string,value:unknown)=>request(path,value);
const count=(table:string)=>env.DB.prepare(`SELECT COUNT(*) count FROM ${table}`).first<number>("count");
beforeAll(async()=>{
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS);
 apple=await fakeApple();
 config={DB:env.DB,IP_RATE_LIMITER:{limit:async()=>({success:true})},APNS_TOPIC:"com.wonder.test",APNS_ENVIRONMENT:"production",APNS_KEY_ID:"",APNS_TEAM_ID:"",APNS_PRIVATE_KEY:"",RATE_SECRET:"test-rate-secret",APPLE_AUDIENCES:"com.swaymun.wonder",ACCOUNT_PEPPER:"test-pepper"};
 // APNs needs a real-shaped ES256 key; the request itself is faked below.
 const pair=await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign"]) as CryptoKeyPair;
 config.APNS_KEY_ID="TESTKEY";config.APNS_TEAM_ID="TESTTEAM";
 config.APNS_PRIVATE_KEY=btoa(String.fromCharCode(...new Uint8Array(await crypto.subtle.exportKey("pkcs8",pair.privateKey) as ArrayBuffer)));
});
beforeEach(()=>{
 resetAppleKeys();jwks=apple.jwks;keyFetches=0;pushes=[];
 vi.spyOn(globalThis,"fetch").mockImplementation(async(input,init)=>{
  if(String(input)===APPLE_KEYS_URL){keyFetches++;return Response.json(jwks);}
  pushes.push({url:String(input),body:JSON.parse(init?.body as string)});return new Response(null,{status:200});
 });
});
afterEach(async()=>{vi.restoreAllMocks();await env.DB.batch(["deliveries","challenges","registrations","limits","sessions","identities","accounts"].map(t=>env.DB.prepare(`DELETE FROM ${t}`)));});

async function signedIn(device:Device,sub?:string) {
 const response=await signIn(post,apple,device,sub);expect(response.status).toBe(200);return await response.json() as Tokens;
}
// Full enrollment: APNs-delivered challenge, then the registration key's proof.
async function enroll(tokens:Tokens,apnsToken="ab".repeat(32)) {
 const key=await deviceKey(),nonce="a".repeat(43),senderSecret=base64url(crypto.getRandomValues(new Uint8Array(32)));
 const challenged=await request("/v1/challenges",{token:apnsToken,publicKey:key.publicKey,nonce},tokens.accessToken);expect(challenged.status).toBe(202);
 const {id}=await challenged.json() as {id:string};const proof=pushes.at(-1)!.body.wonderPush;
 const accepted=await request("/v1/registrations",{id,challenge:proof.challenge,senderSecret,signature:await key.sign(`register|${id}|${proof.challenge}|${senderSecret}`)});
 expect(accepted.status).toBe(200);return {id,senderSecret};
}
const send=(id:string,senderSecret:string)=>request(`/v1/registrations/${id}/send`,{eventId:crypto.randomUUID(),routeId:crypto.randomUUID(),kind:"completed"},senderSecret);

it("creates one account per Apple user across devices and stores no Apple user ID",async()=>{
 const phone=await deviceKey(),tablet=await deviceKey();
 const first=await signedIn(phone,"001234.secret-apple-sub");expect(first.created).toBe(true);
 expect(first.accessExpiresIn).toBe(3600);
 const again=await signIn(post,apple,tablet,"001234.secret-apple-sub");
 expect(again.status).toBe(200);expect((await again.json() as Tokens).created).toBe(false);
 expect(await count("accounts")).toBe(1);expect(await count("identities")).toBe(1);expect(await count("sessions")).toBe(2);
 const everything=JSON.stringify(await Promise.all(["accounts","identities","sessions"].map(t=>env.DB.prepare(`SELECT * FROM ${t}`).all())));
 expect(everything).not.toContain("secret-apple-sub");expect(everything).not.toContain(first.accessToken);expect(everything).not.toContain(first.refreshToken);
 expect(keyFetches).toBe(1);
});

it("rejects identity tokens that fail Apple verification",async()=>{
 const device=await deviceKey(),at=Math.floor(Date.now()/1000);
 for(const overrides of [
  {aud:"com.example.other"},
  {iss:"https://evil.example"},
  {exp:at-120},
  {iat:at+600},
  {nonce:await nonceHash("not this request's nonce")},
  {sub:""},
 ])expect((await signIn(post,apple,device,undefined,overrides)).status).toBe(401);
 // Signed by a different key that claims Apple's key ID.
 const forger=await fakeApple("test-kid");jwks=apple.jwks;
 expect((await signIn(post,forger,device)).status).toBe(401);
 // Not RS256.
 const nonce="n".repeat(43);
 const unsigned=await apple.token({iss:"https://appleid.apple.com",aud:"com.swaymun.wonder",sub:"x",iat:at,exp:at+600,nonce:await nonceHash(nonce)},{alg:"none",kid:"test-kid"});
 expect((await request("/v1/auth/apple",{identityToken:unsigned,nonce,deviceKey:device.publicKey,signature:await device.sign(`signin|apple|${nonce}`)})).status).toBe(401);
 expect(await count("accounts")).toBe(0);expect(await count("sessions")).toBe(0);
});

it("requires the device key's signature over the nonce",async()=>{
 const device=await deviceKey(),other=await deviceKey(),nonce=base64url(crypto.getRandomValues(new Uint8Array(32))),at=Math.floor(Date.now()/1000);
 const identityToken=await apple.token({iss:"https://appleid.apple.com",aud:"com.swaymun.wonder",sub:"s",iat:at,exp:at+600,nonce:await nonceHash(nonce)});
 expect((await request("/v1/auth/apple",{identityToken,nonce,deviceKey:device.publicKey,signature:await other.sign(`signin|apple|${nonce}`)})).status).toBe(403);
 expect(await count("sessions")).toBe(0);
});

it("follows Apple key rotation by refetching an unknown key ID once",async()=>{
 await signedIn(await deviceKey());expect(keyFetches).toBe(1);
 const rotated=await fakeApple("rotated-kid");jwks={keys:[...apple.jwks.keys,...rotated.jwks.keys]};
 expect((await signIn(post,rotated,await deviceKey())).status).toBe(200);expect(keyFetches).toBe(2);
 vi.mocked(fetch).mockImplementation(async()=>new Response(null,{status:500}));
 const unknown=await fakeApple("never-published");
 expect((await signIn(post,unknown,await deviceKey())).status).toBe(503);
});

it("is unavailable until the account pepper is configured",async()=>{
 const pepper=config.ACCOUNT_PEPPER;config.ACCOUNT_PEPPER="";
 try {expect((await signIn(post,apple,await deviceKey())).status).toBe(503);}finally{config.ACCOUNT_PEPPER=pepper;}
});

it("rotates device-bound refresh tokens and rejects reuse, other devices and stale proofs",async()=>{
 const device=await deviceKey(),thief=await deviceKey();const tokens=await signedIn(device);
 const at=Math.floor(Date.now()/1000);
 const refresh=async(refreshToken:string,signer:Device,timestamp=at)=>request("/v1/auth/refresh",{sessionId:tokens.sessionId,refreshToken,timestamp,signature:await signer.sign(`refresh|${tokens.sessionId}|${timestamp}`)});
 expect((await refresh(tokens.refreshToken,thief)).status).toBe(403);
 expect((await refresh(tokens.refreshToken,device,at-600)).status).toBe(403);
 const renewed=await refresh(tokens.refreshToken,device);expect(renewed.status).toBe(200);
 const next=await renewed.json() as Tokens;expect(next.sessionId).toBe(tokens.sessionId);expect(next.accessToken).not.toBe(tokens.accessToken);
 expect((await refresh(tokens.refreshToken,device)).status).toBe(401);
 expect((await request("/v1/account",{},tokens.accessToken)).status).toBe(401);
 expect((await request("/v1/account",{},next.accessToken)).status).toBe(200);
 await env.DB.prepare("UPDATE sessions SET access_expires=?").bind(at-1).run();
 expect((await request("/v1/account",{},next.accessToken)).status).toBe(401);
});

it("refuses push enrollment and delivery without a signed-in session",async()=>{
 const key=await deviceKey();
 for(const credential of [undefined,"x".repeat(43)])
  expect((await request("/v1/challenges",{token:"ab".repeat(32),publicKey:key.publicKey,nonce:"a".repeat(43)},credential)).status).toBe(401);
 expect(pushes).toHaveLength(0);expect(await count("challenges")).toBe(0);
 const tokens=await signedIn(await deviceKey());const {id,senderSecret}=await enroll(tokens);
 const status=await request(`/v1/registrations/${id}/status`,{},senderSecret);
 expect(await status.json()).toEqual({id,account:true});
 // Removing the session row directly (no Worker cleanup) still refuses delivery.
 await env.DB.prepare("DELETE FROM sessions WHERE id=?").bind(tokens.sessionId).run();
 expect((await send(id,senderSecret)).status).toBe(410);
 expect((await request(`/v1/registrations/${id}/status`,{},senderSecret)).status).toBe(410);
 expect(pushes.filter(p=>p.body.aps?.alert)).toHaveLength(0);
});

it("signs out one device and removes only that device's registrations",async()=>{
 const phone=await signedIn(await deviceKey()),ipad=await signedIn(await deviceKey());
 const onPhone=await enroll(phone,"ab".repeat(32)),onIpad=await enroll(ipad,"cd".repeat(32));
 expect((await send(onPhone.id,onPhone.senderSecret)).status).toBe(200);
 expect((await request("/v1/auth/signout",{},phone.accessToken)).status).toBe(200);
 expect((await send(onPhone.id,onPhone.senderSecret)).status).toBe(410);
 expect((await request(`/v1/registrations/${onPhone.id}/activity`,{eventId:crypto.randomUUID(),token:"ef".repeat(32),event:"update",state:{phase:"running",running:1,needsInput:0},staleDate:Math.floor(Date.now()/1000)+600},onPhone.senderSecret)).status).toBe(410);
 expect((await send(onIpad.id,onIpad.senderSecret)).status).toBe(200);
 expect(await env.DB.prepare("SELECT COUNT(*) count FROM deliveries WHERE registration_id=?").bind(onPhone.id).first("count")).toBe(0);
 expect(await count("accounts")).toBe(1);
});

it("deletes the account with every session, registration, receipt, challenge and identity",async()=>{
 const phone=await signedIn(await deviceKey(),"owner"),ipad=await signedIn(await deviceKey(),"owner");
 const stranger=await signedIn(await deviceKey(),"someone-else");
 const a=await enroll(phone,"ab".repeat(32)),b=await enroll(ipad,"cd".repeat(32)),kept=await enroll(stranger,"ef".repeat(32));
 for(const r of [a,b,kept])expect((await send(r.id,r.senderSecret)).status).toBe(200);
 // A pending challenge too.
 expect((await request("/v1/challenges",{token:"12".repeat(32),publicKey:(await deviceKey()).publicKey,nonce:"a".repeat(43)},phone.accessToken)).status).toBe(202);
 expect((await request("/v1/account/delete",{},ipad.accessToken)).status).toBe(200);
 for(const credential of [phone.accessToken,ipad.accessToken])expect((await request("/v1/account",{},credential)).status).toBe(401);
 for(const r of [a,b])expect((await send(r.id,r.senderSecret)).status).toBe(410);
 expect(await count("accounts")).toBe(1);expect(await count("identities")).toBe(1);expect(await count("sessions")).toBe(1);
 expect(await count("registrations")).toBe(1);expect(await env.DB.prepare("SELECT COUNT(*) count FROM challenges WHERE session_id<>?").bind(stranger.sessionId).first("count")).toBe(0);
 expect(await env.DB.prepare("SELECT COUNT(*) count FROM deliveries WHERE registration_id<>?").bind(kept.id).first("count")).toBe(0);
 expect((await send(kept.id,kept.senderSecret)).status).toBe(200);
 // Signing in again starts a new, empty account.
 expect((await signIn(post,apple,await deviceKey(),"owner")).status).toBe(200);expect(await count("accounts")).toBe(2);
});

it("replaces a device's previous session and registrations when it signs in again",async()=>{
 const device=await deviceKey();const first=await signedIn(device,"a");const old=await enroll(first);
 const second=await signedIn(device,"b");
 expect((await request("/v1/account",{},first.accessToken)).status).toBe(401);
 expect((await send(old.id,old.senderSecret)).status).toBe(410);
 expect(await count("sessions")).toBe(1);
 expect((await request("/v1/account",{},second.accessToken)).status).toBe(200);
});

it("rate-limits sign-in per IP",async()=>{
 const device=await deviceKey();
 for(let i=0;i<20;i++)expect((await signIn((p,v)=>request(p,v,undefined,"198.51.100.7"),apple,device)).status).toBe(200);
 expect((await signIn((p,v)=>request(p,v,undefined,"198.51.100.7"),apple,device)).status).toBe(429);
 expect((await signIn((p,v)=>request(p,v,undefined,"198.51.100.8"),apple,device)).status).toBe(200);
});

it("spends the global sign-in quota only on verified Apple sign-ins",async()=>{
 // One sign-in left this hour for everyone.
 const expires=(Math.floor(Date.now()/1000/3600)+1)*3600;
 await env.DB.prepare("INSERT INTO limits VALUES('signin-global',1999,?)").bind(expires).run();
 const attacker=await deviceKey();
 for(let i=0;i<5;i++)expect((await signIn((p,v)=>request(p,v,undefined,`203.0.113.${i}`),apple,attacker,undefined,{aud:"com.example.other"})).status).not.toBe(200);
 expect((await signIn((p,v)=>request(p,v,undefined,"198.51.100.9"),apple,await deviceKey())).status).toBe(200);
});

it("expires sessions after their refresh window together with their registrations",async()=>{
 const tokens=await signedIn(await deviceKey());const r=await enroll(tokens);
 await env.DB.prepare("UPDATE sessions SET refresh_expires=?").bind(Math.floor(Date.now()/1000)-1).run();
 await worker.scheduled({} as ScheduledController,config);
 expect(await count("sessions")).toBe(0);expect(await count("registrations")).toBe(0);
 expect((await send(r.id,r.senderSecret)).status).toBe(410);
 expect(await count("accounts")).toBe(1);
});

it("serves the privacy page",async()=>{
 const response=await worker.fetch(new Request("https://push.test/privacy"),config,createExecutionContext());
 expect(response.headers.get("Cache-Control")?.split(/,\s*/)).toContain("no-transform");
 expect(response.status).toBe(200);expect(response.headers.get("content-type")).toContain("text/html");
 const text=await response.text();expect(text).toContain("What is stored");expect(text).toContain("end-to-end encrypted");
});
