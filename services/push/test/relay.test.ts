import {env} from "cloudflare:workers";
import {applyD1Migrations,createExecutionContext,waitOnExecutionContext} from "cloudflare:test";
import {afterEach,beforeAll,beforeEach,expect,it,vi} from "vitest";
import worker,{type Env} from "../src/index";
import {resetAppleKeys} from "../src/accounts";
import {hash,now,random} from "../src/util";
import {APPLE_KEYS_URL,deviceKey,fakeApple,signIn,type Device,type Tokens} from "./apple";

let config:Env,access:string,host:Device,phone:Device;
let apple:Awaited<ReturnType<typeof fakeApple>>;
let tunnels:Map<string,{id:string;name:string;connections:unknown[]}>;
let records:Map<string,{id:string;name:string;content:string;type:string;proxied:boolean}>;
let calls:{method:string;path:string;body:any;at:number}[];
let extraTunnels=0,extraDNS=0,failures:{method:string;path:string;status:number;remaining:number;after?:boolean}|undefined;
// Requests return as soon as the handler responds; `settled` also waits for
// background work (queued Cloudflare cleanup) the response handed off.
async function send(request:Request) {
 const ctx=createExecutionContext(),response=await worker.fetch(request,config,ctx);
 return {response,settled:waitOnExecutionContext(ctx)};
}
const postRequest=(path:string,data:unknown={},token?:string)=>new Request(`https://push.test${path}`,{method:"POST",headers:{"content-type":"application/json",...(token?{authorization:`Bearer ${token}`}:{})},body:JSON.stringify(data)});
const settle=async({response,settled}:{response:Response;settled:Promise<unknown>})=>{await settled;return response;};
const post=async(path:string,data:unknown={},token?:string)=>settle(await send(postRequest(path,data,token)));
const get=async(path:string,token?:string)=>settle(await send(new Request(`https://push.test${path}`,token?{headers:{authorization:`Bearer ${token}`}}:{})));
const cron=(schedule="*/5 * * * *")=>worker.scheduled({cron:schedule} as ScheduledController,config);
// Moves every relay timestamp into the past, as if `seconds` had elapsed.
const elapse=(seconds:number)=>env.DB.prepare("UPDATE relay_hosts SET last_seen=last_seen-?1,created=created-?1,offline_since=offline_since-?1,purge_after=MAX(0,purge_after-?1)").bind(seconds).run();
async function proof(action:string,target:string,key=host,ts=now()) {
 const nonce=random();return {ts,nonce,sig:await key.sign(`${action}|${target}|${ts}|${nonce}`)};
}
async function grant(key=host) {
 const response=await post("/v1/relay/grants",{hostKey:key.publicKey},access);
 expect(response.status).toBe(200);return await response.json() as {grant:string;label:string};
}
async function connect(label:string,grant?:string,key=host) {return post(`/v1/relay/hosts/${label}/connect`,{...await proof("relay-connect",label,key),...(grant?{grant}:{})});}
async function linked() {const g=await grant();const response=await connect(g.label,g.grant);expect(response.status).toBe(200);return {...g,...await response.json() as {origin:string;token:string}};}
const count=(table:string)=>env.DB.prepare(`SELECT COUNT(*) count FROM ${table}`).first<number>("count");
const purgeActions=["sign-out","disable","account deletion"] as const;
type PurgeAction=typeof purgeActions[number];
const purgeSteps=["connections","tunnel","DNS"] as const;
const purgeCases=purgeActions.flatMap(action=>purgeSteps.map((step,index)=>({action,step,index})));
const revoke=async(action:PurgeAction,label:string)=>action==="disable"
 ?post(`/v1/relay/hosts/${label}/disable`,await proof("relay-disable",label))
 :post(action==="sign-out"?"/v1/auth/signout":"/v1/account/delete",{},access);
function activePurge() {
 const tunnel=[...tunnels.values()][0],record=[...records.values()][0];
 tunnel.connections=[{id:"online-computer",conns:[{id:"active-1"},{id:"active-2"}]},{id:"second-connector",conns:[{id:"active-3"}]}];
 return [`/accounts/${config.CF_ACCOUNT_ID}/cfd_tunnel/${tunnel.id}/connections`,`/accounts/${config.CF_ACCOUNT_ID}/cfd_tunnel/${tunnel.id}`,`/zones/${config.CF_ZONE_ID}/dns_records/${record.id}`];
}
const deleteCalls=()=>calls.filter(c=>c.method==="DELETE");
beforeAll(async()=>{await applyD1Migrations(env.DB,env.TEST_MIGRATIONS);apple=await fakeApple();});
beforeEach(async()=>{
 resetAppleKeys();access=random();host=await deviceKey();phone=await deviceKey();tunnels=new Map();records=new Map();calls=[];extraTunnels=0;extraDNS=0;failures=undefined;
 config={DB:env.DB,IP_RATE_LIMITER:{limit:async()=>({success:true})},APNS_TOPIC:"test",APNS_ENVIRONMENT:"production",APNS_KEY_ID:"",APNS_TEAM_ID:"",APNS_PRIVATE_KEY:"",RATE_SECRET:"rate",ACCOUNT_PEPPER:"pepper",APPLE_AUDIENCES:"com.swaymun.wonder",CF_TUNNEL_TOKEN:"worker-only-test-secret",CF_ACCOUNT_ID:"a".repeat(32),CF_ZONE_ID:"b".repeat(32),RELAY_ZONE:"getcupola.app",RELAY_ALLOWED_ACCOUNT_IDS:"account"};
 await env.DB.batch([
  env.DB.prepare("INSERT INTO accounts VALUES('account',?)").bind(now()),
  env.DB.prepare("INSERT INTO sessions VALUES('session','account',?,?,?, ?,?,?)").bind(phone.publicKey,await hash(access),now()+3600,await hash(random()),now()+86400,now()),
 ]);
 vi.spyOn(globalThis,"fetch").mockImplementation(async(input,init)=>{
  if(String(input)===APPLE_KEYS_URL)return Response.json(apple.jwks);
  const url=new URL(String(input)),method=init?.method??"GET",path=url.pathname.replace("/client/v4","");
  expect(url.origin).toBe("https://api.cloudflare.com");expect(new Headers(init?.headers).get("authorization")).toBe("Bearer worker-only-test-secret");
  const body=init?.body?JSON.parse(String(init.body)):undefined;calls.push({method,path,body,at:Date.now()});
  const fail=failures && failures.method===method && path.endsWith(failures.path) && failures.remaining>0;
  if(fail){failures!.remaining--;if(!failures!.after)return Response.json({success:false,errors:[{message:"private upstream details"}]},{status:failures!.status,headers:{"Retry-After":"0"}});}
  const ok=(result:unknown,total?:number)=>Response.json({success:true,result,...(total===undefined?{}:{result_info:{total_count:total,page:1,total_pages:1}})});
  const tunnelBase=`/accounts/${config.CF_ACCOUNT_ID}/cfd_tunnel`,dnsBase=`/zones/${config.CF_ZONE_ID}/dns_records`;
  let response:Response;
  if(path===tunnelBase && method==="GET") {
   const found=[...tunnels.values()].filter(t=>!url.searchParams.has("name") || t.name===url.searchParams.get("name"));
   response=ok(found,found.length+(url.searchParams.has("name")?0:extraTunnels));
  }else if(path===tunnelBase && method==="POST") {
   const id=crypto.randomUUID();tunnels.set(id,{id,name:body.name,connections:[]});response=ok({id,name:body.name});
  }else if(path.startsWith(tunnelBase+"/")) {
   const [id,action]=path.slice(tunnelBase.length+1).split("/");const t=tunnels.get(id);
   if(!t)return Response.json({success:false},{status:404});
   if(method==="DELETE" && action==="connections"){t.connections=[];response=ok({});}
   else if(method==="DELETE" && !action){
    // Cloudflare refuses to delete a tunnel with active connectors. Cleanup
    // must remove those connections before it can revoke the public address.
    if(t.connections.some(client=>Array.isArray((client as any)?.conns) && (client as any).conns.length>0))return Response.json({success:false,errors:[{message:"Tunnel has active connections"}]},{status:400});
    tunnels.delete(id);response=ok({});
   }
   else if(action==="connections" && method==="GET")response=ok(t.connections);
   else if(action==="token")response=ok(`token-${id}`);
   else if(action==="configurations" && method==="PUT")response=ok(body);
   else throw new Error(`Unexpected tunnel API ${method} ${path}`);
  }else if(path===dnsBase && method==="GET") {
   const found=[...records.values()].filter(r=>!url.searchParams.has("name") || r.name===url.searchParams.get("name"));response=ok(found,found.length+(url.searchParams.has("name")?0:extraDNS));
  }else if(path===dnsBase && method==="POST") {
   const id=crypto.randomUUID();records.set(id,{id,...body});response=ok({id,...body});
  }else if(path.startsWith(dnsBase+"/")) {
   const id=path.slice(dnsBase.length+1);
   if(!records.has(id))return Response.json({success:false},{status:404});
   if(method==="DELETE"){records.delete(id);response=ok({});}else if(method==="PUT"){records.set(id,{id,...body});response=ok({id,...body});}else throw new Error(`Unexpected DNS API ${method}`);
  }else throw new Error(`Unexpected Cloudflare API ${method} ${path}`);
  if(fail)throw new Error("Connection lost after upstream mutation");
  return response;
 });
});
afterEach(async()=>{vi.restoreAllMocks();await env.DB.batch(["relay_nonces","relay_pending","relay_grants","relay_hosts","relay_locks","relay_capacity","deliveries","challenges","registrations","limits","sessions","identities","accounts"].map(t=>env.DB.prepare(`DELETE FROM ${t}`)));});

it("W1: creates, reuses, deletes after ten minutes offline, and recreates the same hostname and CNAME",async()=>{
 // These checks move stored timestamps by exact seconds. Keep wall time from
 // adding a second at the 599/600 boundary; request-latency tests stay live.
 vi.spyOn(Date,"now").mockReturnValue(Date.now());
 const first=await linked();expect(first.origin).toBe(`https://r-${first.label}.getcupola.app`);
 expect(tunnels.size).toBe(1);expect(records.size).toBe(1);
 const initialID=[...tunnels.keys()][0],dnsID=[...records.keys()][0];
 expect([...records.values()][0]).toMatchObject({type:"CNAME",proxied:true,content:`${initialID}.cfargotunnel.com`});
 expect(calls.find(c=>c.path.endsWith("/configurations"))!.body).toEqual({config:{ingress:[{hostname:`r-${first.label}.getcupola.app`,path:"^/api/v1/rpc/sealed$",service:"http://127.0.0.1:3777"},{service:"http_status:404"}]}});
 expect((await connect(first.label)).status).toBe(200);expect(tunnels.size).toBe(1);
 await cron();expect(tunnels.size).toBe(1); // First observed offline.
 await elapse(599);await cron();expect(tunnels.size).toBe(1);
 await elapse(1);await cron();expect(tunnels.size).toBe(0);expect(records.has(dnsID)).toBe(true);
 const recreated=await connect(first.label);expect(recreated.status).toBe(200);expect((await recreated.json() as any).origin).toBe(first.origin);
 expect([...tunnels.keys()][0]).not.toBe(initialID);expect([...records.keys()]).toEqual([dnsID]);
 expect(records.get(dnsID)!.content).toBe(`${[...tunnels.keys()][0]}.cfargotunnel.com`);
 console.info("W1 lifecycle: create/reuse PASS; observed offline 599s retained, 600s deleted; same origin and CNAME record recreated PASS");
});

it("W1: refuses new tunnels at the free account cap with 503 and Retry-After 600, while reusing existing tunnels",async()=>{
 const first=await linked();extraTunnels=999;
 const existing=await connect(first.label);expect(existing.status).toBe(200);
 const other=await grant(await deviceKey()),response=await connect(other.label,other.grant,await deviceKey());
 // Wrong key cannot reach the capacity check.
 expect(response.status).toBe(403);
 const key=await deviceKey(),g=await grant(key),full=await connect(g.label,g.grant,key);
 expect(full.status).toBe(503);expect(await full.json()).toEqual({error:"relay_full"});expect(full.headers.get("Retry-After")).toBe("600");
 expect(tunnels.size).toBe(1);expect(calls.filter(c=>c.method==="POST" && c.path.endsWith("/cfd_tunnel"))).toHaveLength(1);
 const capacity=await get("/v1/relay/capacity",access);expect(await capacity.json()).toMatchObject({used:1000,limit:1000,state:"full",budget:{paidTunnels:false,spendAlert:true}});
 console.info("W1 cap: 1000 total tunnels -> 503 relay_full, Retry-After 600; existing tunnel reusable PASS");
});
it("refuses new DNS names at the free zone cap but recreates a retained CNAME",async()=>{
 const first=await linked();extraDNS=199;
 const key=await deviceKey(),g=await grant(key),response=await connect(g.label,g.grant,key);
 expect(response.status).toBe(503);expect(await response.json()).toEqual({error:"relay_full"});expect(tunnels.size).toBe(1);
 tunnels.clear(); // External removal, then recreation using the existing CNAME.
 expect((await connect(first.label)).status).toBe(200);expect(records.size).toBe(1);expect(tunnels.size).toBe(1);
});
it("reports near capacity and an operator spend alert without exposing secrets or host metadata",async()=>{
 extraTunnels=800;await cron("17 * * * *");const response=await get("/healthz");const data=await response.json() as any;
 expect(data.relay).toMatchObject({configured:true,used:800,state:"near",budget:{targetMonthlyUSD:5,hardCapMonthlyUSD:25,spendAlert:true}});
 expect(JSON.stringify(data)).not.toContain(config.CF_TUNNEL_TOKEN);expect(JSON.stringify(data)).not.toContain(host.publicKey);
});
it("stays disabled with no Worker-held credential and leaves notification/account routes usable",async()=>{
 delete config.CF_TUNNEL_TOKEN;
 expect((await post("/v1/relay/grants",{hostKey:host.publicKey},access)).status).toBe(503);
 expect((await post("/v1/account",{},access)).status).toBe(200);
 expect((await (await get("/healthz")).json() as any).relay).toEqual({configured:false});expect(calls).toHaveLength(0);
});
it("W1: unapproved pending creates no tunnel; only the approved host can poll its token",async()=>{
 const response=await post("/v1/relay/pending",{hostKey:host.publicKey,...await proof("relay-pending",host.publicKey)});
 expect(response.status).toBe(202);const pending=await response.json() as {id:string;secret:string};
 const poll=async(key=host)=>get(`/v1/relay/pending/${pending.id}?${new URLSearchParams(Object.entries(await proof("relay-poll",pending.id,key)).map(([k,v])=>[k,String(v)]))}`);
 expect((await poll()).status).toBe(202);expect(calls).toHaveLength(0);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
 expect((await post(`/v1/relay/pending/${pending.id}/approve`,{secret:pending.secret})).status).toBe(401);
 expect((await post(`/v1/relay/pending/${pending.id}/approve`,{secret:random()},access)).status).toBe(403);expect(calls).toHaveLength(0);
 const approved=await post(`/v1/relay/pending/${pending.id}/approve`,{secret:pending.secret},access);expect(approved.status).toBe(200);
 const result=await approved.json() as any;expect(result.token).toBeUndefined();expect(tunnels.size).toBe(1);
 expect((await poll(await deviceKey())).status).toBe(403);
 const token=await poll();expect(token.status).toBe(200);expect(await token.json()).toMatchObject({approved:true,origin:result.origin,token:`token-${[...tunnels.keys()][0]}`});
 // Approval already belongs to the account: expiry of the approving phone's
 // session must not strand the host before its next signed token poll.
 await env.DB.prepare("UPDATE sessions SET refresh_expires=?").bind(now()-1).run();
 await worker.scheduled({cron:"17 * * * *"} as ScheduledController,config);
 expect((await poll()).status).toBe(200);expect(tunnels.size).toBe(1);expect(deleteCalls()).toHaveLength(0);
 const stored=JSON.stringify((await env.DB.prepare("SELECT * FROM relay_pending").all()).results);expect(stored).not.toContain(pending.secret);
 console.info("W1 pending: zero Cloudflare calls before signed-in approval; signed host-only token delivery PASS");
});
it("expires pending approvals without creating resources",async()=>{
 const response=await post("/v1/relay/pending",{hostKey:host.publicKey,...await proof("relay-pending",host.publicKey)});
 const pending=await response.json() as any;await env.DB.prepare("UPDATE relay_pending SET expires=?").bind(now()).run();
 expect((await post(`/v1/relay/pending/${pending.id}/approve`,{secret:pending.secret},access)).status).toBe(410);
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(await count("relay_pending")).toBe(0);expect(calls).toHaveLength(0);
});
it("requires signed-in grants, a real P-256 key, fresh signatures, action binding and non-replayed nonces",async()=>{
 expect((await post("/v1/relay/grants",{hostKey:host.publicKey})).status).toBe(401);
 expect((await post("/v1/relay/grants",{hostKey:"x".repeat(87)},access)).status).toBe(400);
 const g=await grant(),path=`/v1/relay/hosts/${g.label}/connect`;
 expect((await connect(g.label,g.grant,await deviceKey())).status).toBe(403);
 expect((await post(path,{grant:g.grant,...await proof("relay-connect",g.label,host,now()-61)})).status).toBe(403);
 expect((await post(path,{grant:g.grant,...await proof("relay-disable",g.label)})).status).toBe(403);
 const signed=await proof("relay-connect",g.label);
 expect((await post(path,{grant:g.grant,...signed})).status).toBe(200);
 expect((await post(path,signed)).status).toBe(403);
 expect((await connect(g.label,g.grant)).status).toBe(403); // Single-use grant.
 expect((await connect(g.label)).status).toBe(200);
 const text=JSON.stringify((await env.DB.prepare("SELECT * FROM relay_grants").all()).results);expect(text).not.toContain(g.grant);
});
it("does not enable a host without a grant, or redeem an expired grant",async()=>{
 const g=await grant();expect((await connect(g.label)).status).toBe(403);
 await env.DB.prepare("UPDATE relay_grants SET expires=?").bind(now()).run();
 expect((await connect(g.label,g.grant)).status).toBe(403);expect(tunnels.size).toBe(0);expect(calls).toHaveLength(0);
});
it("enforces five hosts per account and reuses a label for the same host key",async()=>{
 const first=await grant();expect((await grant()).label).toBe(first.label);
 for(let i=1;i<5;i++)await grant(await deviceKey());
 const response=await post("/v1/relay/grants",{hostKey:(await deviceKey()).publicKey},access);
 expect(response.status).toBe(409);expect(await response.json()).toEqual({error:"relay_host_limit"});expect(await count("relay_hosts")).toBe(5);expect(calls).toHaveLength(0);
});
it("does not allow another account to claim a previously registered host key",async()=>{
 await grant();const stranger=random();config.RELAY_ALLOWED_ACCOUNT_IDS="account,stranger";
 await env.DB.batch([env.DB.prepare("INSERT INTO accounts VALUES('stranger',?)").bind(now()),env.DB.prepare("INSERT INTO sessions VALUES('other','stranger',?,?,?,?,?,?)").bind((await deviceKey()).publicKey,await hash(stranger),now()+3600,await hash(random()),now()+86400,now())]);
 expect((await post("/v1/relay/grants",{hostKey:host.publicKey},stranger)).status).toBe(409);expect(await count("relay_hosts")).toBe(1);
});
it("observes active connections even after ten minutes and does not infer offline from an upstream failure",async()=>{
 await linked();const t=[...tunnels.values()][0];t.connections=[{id:"connector",conns:[{id:"active"}]}];
 await env.DB.prepare("UPDATE relay_hosts SET last_seen=?").bind(now()-1200).run();
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(tunnels.size).toBe(1);
 expect(await env.DB.prepare("SELECT last_seen FROM relay_hosts").first<number>("last_seen")).toBeGreaterThanOrEqual(now()-1);
 await env.DB.prepare("UPDATE relay_hosts SET last_seen=?").bind(now()-1200).run();t.connections=[];
 failures={method:"GET",path:"/connections",status:500,remaining:3};
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(tunnels.size).toBe(1);
});
it("retries transient API failures with bounded backoff and never leaks upstream details",async()=>{
 failures={method:"GET",path:"/cfd_tunnel",status:429,remaining:2};
 const g=await grant(),response=await connect(g.label,g.grant);expect(response.status).toBe(200);
 failures={method:"GET",path:"/connections",status:500,remaining:3};
 const failed=await connect(g.label);expect(failed.status).toBe(503);expect(await failed.text()).toBe('{"error":"relay_unavailable"}');
 expect(calls.filter(c=>c.path.endsWith("/connections"))).toHaveLength(3);
});
it("recovers a lost tunnel-create response by name without creating a second tunnel",async()=>{
 failures={method:"POST",path:"/cfd_tunnel",status:503,remaining:1,after:true};
 await linked();expect(tunnels.size).toBe(1);expect(calls.filter(c=>c.method==="POST" && c.path.endsWith("/cfd_tunnel"))).toHaveLength(1);
});
it("recovers a lost DNS-create response on reconnect without creating a second record",async()=>{
 failures={method:"POST",path:"/dns_records",status:503,remaining:1,after:true};
 const g=await grant();expect((await connect(g.label,g.grant)).status).toBe(503);expect(records.size).toBe(1);
 expect((await connect(g.label)).status).toBe(200);expect(tunnels.size).toBe(1);expect(records.size).toBe(1);
});
it("serializes concurrent creates so the last free tunnel slot is never allocated twice",async()=>{
 extraTunnels=999;const a=await grant(),key=await deviceKey(),b=await grant(key);
 const results=await Promise.all([connect(a.label,a.grant),connect(b.label,b.grant,key)]);
 expect(results.map(r=>r.status).sort()).toEqual([200,503]);expect(tunnels.size).toBe(1);
 const winner=await env.DB.prepare("SELECT label FROM relay_hosts WHERE tunnel_id IS NOT NULL").first<string>("label");
 const retry=await connect(winner===a.label?b.label:a.label,undefined,winner===a.label?key:host);
 // The losing request may have failed on the lease before consuming its grant.
 expect([403,503]).toContain(retry.status);expect(tunnels.size).toBe(1);
});
it("W1: sign-out removes the tunnel and CNAME, invalidates grants and prevents reconnect",async()=>{
 const first=await linked();const spareKey=await deviceKey();await grant(spareKey);
 const paths=activePurge();
 const response=await post("/v1/auth/signout",{},access);expect(response.status).toBe(200);
 expect(await response.json()).toEqual({signedOut:true,cleanupPending:true});
 expect(deleteCalls().map(c=>c.path)).toEqual(paths);
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect(await count("relay_hosts")).toBe(0);expect(await count("relay_grants")).toBe(0);
 expect((await connect(first.label)).status).toBe(410);
 console.info("W1 sign-out: active connections cleared -> tunnel deleted -> DNS deleted; host/grants removed, reconnect 410 PASS");
});
it("deletes account resources and keeps retriable cleanup tombstones on Cloudflare failure",async()=>{
 const first=await linked();failures={method:"DELETE",path:`/${[...tunnels.keys()][0]}`,status:500,remaining:3};
 const response=await post("/v1/account/delete",{},access);expect(response.status).toBe(200);expect(await response.json()).toEqual({deleted:true,cleanupPending:true});
 expect(await count("accounts")).toBe(0);expect(await count("sessions")).toBe(0);expect(await count("relay_hosts")).toBe(1);
 expect((await connect(first.label)).status).toBe(410);
 await elapse(240);await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect(await count("relay_hosts")).toBe(0);
});
it("disables a host only with its signed proof and removes its resources",async()=>{
 const first=await linked(),path=`/v1/relay/hosts/${first.label}/disable`;
 const paths=activePurge();
 expect((await post(path,await proof("relay-connect",first.label))).status).toBe(403);
 expect(deleteCalls()).toHaveLength(0);
 expect((await post(path,await proof("relay-disable",first.label))).status).toBe(200);
 expect(deleteCalls().map(c=>c.path)).toEqual(paths);
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect((await connect(first.label)).status).toBe(410);
});
it("deletes an account's connected tunnel before its DNS and removes credentials",async()=>{
 const first=await linked(),paths=activePurge();
 const response=await revoke("account deletion",first.label);expect(response.status).toBe(200);expect(await response.json()).toEqual({deleted:true,cleanupPending:true});
 expect(deleteCalls().map(c=>c.path)).toEqual(paths);
 expect(await count("accounts")).toBe(0);expect(await count("sessions")).toBe(0);expect(await count("relay_hosts")).toBe(0);
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect((await connect(first.label)).status).toBe(410);
});
// Observable contract: each revocation path disconnects a live host before
// deleting its tunnel and DNS. Exhausted failures preserve durable purge intent
// and block token issuance. The existing Worker/D1 suite owns this boundary.
it.each(purgeCases)("$action retries $step failures with backoff before advancing the purge",async({action,index})=>{
 const first=await linked(),paths=activePurge();
 failures={method:"DELETE",path:paths[index],status:503,remaining:2};
 const response=await revoke(action,first.label);expect(response.status).toBe(200);
 const deletes=deleteCalls();
 expect(deletes.map(c=>c.path)).toEqual([...paths.slice(0,index),paths[index],paths[index],...paths.slice(index)]);
 const attempts=deletes.filter(c=>c.path===paths[index]),delays=attempts.slice(1).map((c,i)=>c.at-attempts[i].at);
 expect(delays[0]).toBeGreaterThanOrEqual(100);expect(delays[1]).toBeGreaterThanOrEqual(200);
 expect(await count("relay_hosts")).toBe(0);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
 console.info(`Purge ${action}/${purgeSteps[index]}: ordered DELETEs PASS; measured retry delays ${delays.join(", ")} ms`);
});
it.each(purgeCases)("$action records a pending purge after exhausted $step retries and cron finishes it",async({action,index})=>{
 const first=await linked(),paths=activePurge(),tunnel=[...tunnels.values()][0];
 failures={method:"DELETE",path:paths[index],status:503,remaining:3};
 const response=await revoke(action,first.label);
 if(action==="disable") {expect(response.status).toBe(503);expect(await response.json()).toEqual({error:"relay_unavailable"});}
 else {expect(response.status).toBe(200);expect(await response.json()).toEqual({[action==="sign-out"?"signedOut":"deleted"]:true,cleanupPending:true});}
 expect(deleteCalls().map(c=>c.path)).toEqual([...paths.slice(0,index),paths[index],paths[index],paths[index]]);
 expect(await env.DB.prepare("SELECT state,tunnel_id,dns_id FROM relay_hosts WHERE label=?").bind(first.label).first()).toEqual({state:"deleting",tunnel_id:tunnel.id,dns_id:paths[2].split("/").pop()});
 expect(await count("sessions")).toBe(action==="disable"?1:0);expect(await count("accounts")).toBe(action==="account deletion"?0:1);
 expect(tunnels.size).toBe(index===2?0:1);expect(records.size).toBe(1);
 expect(tunnel.connections.length).toBe(index===0?2:0);
 expect((await connect(first.label)).status).toBe(410);
 calls=[];
 await elapse(240);await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 // Already-deleted resources return 404; retrying the whole purge is safe.
 expect(deleteCalls().map(c=>c.path)).toEqual(paths);
 expect(await count("relay_hosts")).toBe(0);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
});
it.each(purgeSteps)("recovers a lost $0 DELETE response without leaving a reachable host",async(step)=>{
 const first=await linked(),paths=activePurge(),index=purgeSteps.indexOf(step);
 failures={method:"DELETE",path:paths[index],status:503,remaining:1,after:true};
 const response=await revoke("sign-out",first.label);expect(response.status).toBe(200);expect(await response.json()).toEqual({signedOut:true,cleanupPending:true});
 expect(deleteCalls().map(c=>c.path)).toEqual([...paths.slice(0,index),paths[index],...paths.slice(index)]);
 expect(await count("relay_hosts")).toBe(0);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
});
it("reclaims expired unredeemed host slots without contacting Cloudflare",async()=>{
 await grant();await env.DB.prepare("UPDATE relay_grants SET expires=?").bind(now()).run();
 await env.DB.prepare("UPDATE relay_hosts SET created=?").bind(now()-600).run();
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);
 expect(await count("relay_hosts")).toBe(0);expect(tunnels.size).toBe(0);
 expect(calls.every(c=>c.method==="GET")).toBe(true);
});
it("keeps the large receipt sweep hourly while five-minute cron still reaps relay resources",async()=>{
 await env.DB.prepare("INSERT INTO deliveries VALUES('r','e','route','completed','delivered',1,?)").bind(now()-8*86400).run();
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(await count("deliveries")).toBe(1);
 await worker.scheduled({cron:"17 * * * *"} as ScheduledController,config);expect(await count("deliveries")).toBe(0);
});
it("treats retained clients with empty nested conns as offline, but malformed liveness as unknown",async()=>{
 await linked();const t=[...tunnels.values()][0];
 t.connections=[{id:"connector-without-shape"}];
 await cron();await elapse(600);await cron();expect(tunnels.size).toBe(1);
 t.connections=[{id:"retained-connector",conns:[]}];
 await cron();expect(tunnels.size).toBe(1);
 await elapse(600);await cron();expect(tunnels.size).toBe(0);expect(records.size).toBe(1);
});
it("preserves unrelated DNS if a reserved hostname record has been replaced externally",async()=>{
 const first=await linked(),old=[...records.values()][0];records.clear();
 records.set("other-record",{...old,id:"other-record",type:"A",content:"192.0.2.1"});
 expect((await connect(first.label)).status).toBe(503);
 const signed=await post(`/v1/relay/hosts/${first.label}/disable`,await proof("relay-disable",first.label));expect(signed.status).toBe(503);
 expect(records.get("other-record")!.content).toBe("192.0.2.1");expect(await count("relay_hosts")).toBe(1);
});
it("does not return a token when sign-out races a Cloudflare operation",async()=>{
 const original=vi.mocked(fetch).getMockImplementation()!,g=await grant();
 let revoked=false;
 vi.mocked(fetch).mockImplementation(async(input,init)=>{
  if(String(input).endsWith("/token") && !revoked) {
   revoked=true;
   const signout=await post("/v1/auth/signout",{},access);expect(signout.status).toBe(200);
  }
  return original(input,init);
 });
 const response=await connect(g.label,g.grant);expect(response.status).toBe(410);expect(await response.json()).toEqual({error:"relay_gone"});
 // The background purge met the held lease; cron retries after its backoff.
 await elapse(240);await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
});
it("does not reap a just-reserved host between its reservation and grant transaction",async()=>{
 await grant();await env.DB.prepare("DELETE FROM relay_grants").run();
 await worker.scheduled({cron:"*/5 * * * *"} as ScheduledController,config);expect(await count("relay_hosts")).toBe(1);expect(calls).toHaveLength(0);
});
// Enabled hosts belong to the account, not the phone session that approved them.
// Exercise actual cron and sign-in handlers: rotating/expiring a credential must
// preserve the address; only explicit revocation may remove its resources.
it("allows signed host reconnect after the phone session expires, before cron",async()=>{
 const first=await linked();await env.DB.prepare("UPDATE sessions SET access_expires=?,refresh_expires=?").bind(now()-1,now()-1).run();
 expect((await post("/v1/account",{},access)).status).toBe(401);
 expect((await connect(first.label)).status).toBe(200);expect(deleteCalls()).toHaveLength(0);
});
const expiryCases=["granting phone","another phone"].flatMap(expired=>["*/5 * * * *","17 * * * *"].map(cron=>({expired,cron})));
it.each(expiryCases)("keeps the address when $expired expires during $cron",async({expired,cron})=>{
 const first=await linked(),tunnelID=[...tunnels.keys()][0],dnsID=[...records.keys()][0];activePurge();
 if(expired==="another phone")await env.DB.prepare("INSERT INTO sessions VALUES('tablet','account',?,?,?,?,?,?)").bind((await deviceKey()).publicKey,await hash(random()),now()+3600,await hash(random()),now()+86400,now()).run();
 await env.DB.prepare("UPDATE sessions SET refresh_expires=? WHERE id=?").bind(now()-1,expired==="granting phone"?"session":"tablet").run();
 await worker.scheduled({cron} as ScheduledController,config);
 expect(await env.DB.prepare("SELECT state FROM relay_hosts WHERE label=?").bind(first.label).first("state")).toBe("enabled");
 expect(tunnels.has(tunnelID)).toBe(true);expect(records.has(dnsID)).toBe(true);expect(deleteCalls()).toHaveLength(0);
 const response=await connect(first.label);expect(response.status).toBe(200);expect(await response.json()).toEqual({label:first.label,origin:first.origin,token:first.token});
 expect(await count("sessions")).toBe((expired==="another phone"?2:1)-(cron==="17 * * * *"?1:0));
});
async function signedIn(key:Device,sub="relay-owner") {
 const response=await signIn(post,apple,key,sub);expect(response.status).toBe(200);
 const tokens=await response.json() as Tokens;
 const accountID=await env.DB.prepare("SELECT account_id FROM sessions WHERE id=?").bind(tokens.sessionId).first<string>("account_id");
 config.RELAY_ALLOWED_ACCOUNT_IDS+=`,${accountID}`;return tokens;
}
it.each(["expired","replaced"])("refuses unredeemed grants from a phone session that was %s",async(state)=>{
 const first=await grant();
 if(state==="expired")await env.DB.prepare("UPDATE sessions SET refresh_expires=?").bind(now()-1).run();
 else await signedIn(phone);
 expect((await connect(first.label,first.grant)).status).toBe(403);
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect(calls).toHaveLength(0);
});
it.each(["relay-owner","different-account"])("preserves a linked host when the same phone signs in again as %s",async(sub)=>{
 const initial=await signedIn(phone);access=initial.accessToken;
 const first=await linked(),tunnelID=[...tunnels.keys()][0],dnsID=[...records.keys()][0];activePurge();
 const next=await signedIn(phone,sub);
 expect(next.sessionId).not.toBe(initial.sessionId);
 expect((await post("/v1/account",{},initial.accessToken)).status).toBe(401);
 expect((await post("/v1/account",{},next.accessToken)).status).toBe(200);
 await worker.scheduled({cron:"17 * * * *"} as ScheduledController,config);
 const response=await connect(first.label);expect(response.status).toBe(200);expect(await response.json()).toEqual({label:first.label,origin:first.origin,token:first.token});
 expect(tunnels.has(tunnelID)).toBe(true);expect(records.has(dnsID)).toBe(true);expect(deleteCalls()).toHaveLength(0);
});
it.each(["/v1/auth/signout","/v1/account/delete"])("%s revokes only the acting account, even after the approving session expired",async(action)=>{
 const first=await linked(),ownerTunnel=[...tunnels.keys()][0],ownerDNS=[...records.keys()][0];activePurge();
 const originalAccess=access,otherKey=await deviceKey(),stranger=await signedIn(await deviceKey(),"stranger");access=stranger.accessToken;
 const other=await grant(otherKey);expect((await connect(other.label,other.grant,otherKey)).status).toBe(200);
 const otherTunnel=[...tunnels.keys()].find(id=>id!==ownerTunnel)!,otherDNS=[...records.keys()].find(id=>id!==ownerDNS)!;
 // The acting phone never granted this host; the original granting phone is gone.
 const acting=random();
 await env.DB.prepare("INSERT INTO sessions VALUES('second-phone','account',?,?,?,?,?,?)").bind((await deviceKey()).publicKey,await hash(acting),now()+3600,await hash(random()),now()+86400,now()).run();
 await env.DB.prepare("UPDATE sessions SET refresh_expires=? WHERE id='session'").bind(now()-1).run();
 await worker.scheduled({cron:"17 * * * *"} as ScheduledController,config);
 expect((await post("/v1/account",{},originalAccess)).status).toBe(401);
 expect((await connect(first.label)).status).toBe(200);expect(deleteCalls()).toHaveLength(0);
 expect((await post(action,{},acting)).status).toBe(200);
 expect((await connect(first.label)).status).toBe(410);expect(tunnels.has(ownerTunnel)).toBe(false);expect(records.has(ownerDNS)).toBe(false);
 expect((await connect(other.label,undefined,otherKey)).status).toBe(200);expect(tunnels.has(otherTunnel)).toBe(true);expect(records.has(otherDNS)).toBe(true);
 expect(deleteCalls().map(c=>c.path)).toEqual([`/accounts/${config.CF_ACCOUNT_ID}/cfd_tunnel/${ownerTunnel}/connections`,`/accounts/${config.CF_ACCOUNT_ID}/cfd_tunnel/${ownerTunnel}`,`/zones/${config.CF_ZONE_ID}/dns_records/${ownerDNS}`]);
});
it("fails closed for grants and QR approval without the owner account allowlist",async()=>{
 delete config.RELAY_ALLOWED_ACCOUNT_IDS;
 expect((await post("/v1/relay/grants",{hostKey:host.publicKey},access)).status).toBe(403);
 const response=await post("/v1/relay/pending",{hostKey:host.publicKey,...await proof("relay-pending",host.publicKey)}),p=await response.json() as any;
 const approve=await post(`/v1/relay/pending/${p.id}/approve`,{secret:p.secret},access);
 expect(approve.status).toBe(403);expect(await approve.json()).toEqual({error:"relay_not_available"});expect(calls).toHaveLength(0);
});
// Pre-deployment cost and timing contracts (route-w1-worker review).
it("serves capacity only to signed-in accounts, from a cached snapshot",async()=>{
 const anonymous=await get("/v1/relay/capacity");expect(anonymous.status).toBe(401);
 for(let i=0;i<3;i++)expect((await get("/healthz")).status).toBe(200);
 expect(calls).toHaveLength(0);
 extraTunnels=800;
 const first=await get("/v1/relay/capacity",access);expect(first.status).toBe(200);
 expect(await first.json()).toMatchObject({used:800,limit:1000,state:"near",dns:{used:0,limit:200}});
 expect(calls).toHaveLength(2);
 extraTunnels=900;
 expect(await (await get("/v1/relay/capacity",access)).json()).toMatchObject({used:800});
 expect((await (await get("/healthz")).json() as any).relay).toMatchObject({configured:true,used:800,state:"near"});
 expect(calls).toHaveLength(2);
 // The hourly cron refreshes the shared snapshot; requests stay read-only.
 await cron();expect(calls).toHaveLength(2);
 await cron("17 * * * *");expect(calls).toHaveLength(4);
 expect(await (await get("/v1/relay/capacity",access)).json()).toMatchObject({used:900});expect(calls).toHaveLength(4);
});
it("reports unknown capacity on health without calling Cloudflare once the snapshot is stale",async()=>{
 await cron("17 * * * *");expect(calls).toHaveLength(2);
 await env.DB.prepare("UPDATE relay_capacity SET updated=updated-3900").run();
 expect((await (await get("/healthz")).json() as any).relay).toEqual({configured:true,state:"unknown",budget:{spendAlert:true}});
 expect(calls).toHaveLength(2);
});
it.each(["/v1/auth/signout","/v1/account/delete"])("%s responds before Cloudflare cleanup, which finishes in the background",async(action)=>{
 const first=await linked();activePurge();
 const original=vi.mocked(fetch).getMockImplementation()!;let release!:()=>void;const blocked=new Promise<void>(resolve=>release=resolve);
 vi.mocked(fetch).mockImplementation(async(input,init)=>{if(init?.method==="DELETE")await blocked;return original(input,init);});
 const started=Date.now(),sent=await send(postRequest(action,{},access));
 expect(sent.response.status).toBe(200);
 expect(await sent.response.json()).toEqual({[action==="/v1/auth/signout"?"signedOut":"deleted"]:true,cleanupPending:true});
 expect(Date.now()-started).toBeLessThan(1000);
 expect(tunnels.size).toBe(1);expect((await connect(first.label)).status).toBe(410);
 release();await sent.settled;
 expect(tunnels.size).toBe(0);expect(records.size).toBe(0);expect(await count("relay_hosts")).toBe(0);
});
it("signs out without queuing cleanup when the account has no remote access",async()=>{
 const response=await post("/v1/auth/signout",{},access);expect(await response.json()).toEqual({signedOut:true});expect(calls).toHaveLength(0);
});
it("retries a queued purge with growing backoff until Cloudflare recovers",async()=>{
 const first=await linked();failures={method:"DELETE",path:"/connections",status:503,remaining:1_000};
 expect(await (await post("/v1/auth/signout",{},access)).json()).toEqual({signedOut:true,cleanupPending:true});
 const state=()=>env.DB.prepare("SELECT purge_attempts attempts,purge_after-? delay FROM relay_hosts WHERE label=?").bind(now(),first.label).first<{attempts:number;delay:number}>();
 expect(await state()).toMatchObject({attempts:1});expect((await state())!.delay).toBeGreaterThanOrEqual(239);
 calls=[];await cron();expect(deleteCalls()).toHaveLength(0); // Not due yet.
 const delays=[];
 for(let attempt=2;attempt<=6;attempt++) {
  await elapse((await state())!.delay);await cron();
  const next=await state();expect(next!.attempts).toBe(attempt);delays.push(next!.delay);
 }
 expect(delays).toEqual([480,960,1920,3600,3600].map(d=>expect.closeTo(d,1)));
 failures=undefined;await elapse(3600);await cron();
 expect(await count("relay_hosts")).toBe(0);expect(tunnels.size).toBe(0);expect(records.size).toBe(0);
});
it("deletes an idle tunnel only after ten minutes observed offline, even when it dropped just before a cron",async()=>{
 vi.spyOn(Date,"now").mockReturnValue(Date.now());
 const first=await linked(),t=[...tunnels.values()][0];t.connections=[{id:"connector",conns:[{id:"active"}]}];
 await cron();expect(tunnels.size).toBe(1);
 // Disconnects right after that tick; the next ticks follow five minutes apart.
 t.connections=[];
 await elapse(300);await cron();expect(tunnels.size).toBe(1);
 await elapse(300);await cron();expect(tunnels.size).toBe(1); // Five minutes observed offline.
 await elapse(299);await cron();expect(tunnels.size).toBe(1);
 await elapse(1);await cron();expect(tunnels.size).toBe(0);
 expect((await connect(first.label)).status).toBe(200);expect(tunnels.size).toBe(1);
});
it("restarts the offline clock when the host fetches a token to reconnect",async()=>{
 vi.spyOn(Date,"now").mockReturnValue(Date.now());
 const first=await linked();
 await cron();await elapse(599);
 expect((await connect(first.label)).status).toBe(200); // cloudflared is starting.
 await elapse(1);await cron();expect(tunnels.size).toBe(1);
 await elapse(600);await cron();expect(tunnels.size).toBe(0);
});
