import {type Env,Failure,now} from "./util";
/** The one path a relay tunnel forwards: an anchored ingress regex. */
export const RELAY_SEALED_PATH="^/api/v1/rpc/sealed$";

const API="https://api.cloudflare.com/client/v4";
export const TUNNEL_LIMIT=1000, DNS_LIMIT=200;
type Envelope<T>={success:boolean;result:T;result_info?:{total_count?:number;total_pages?:number}};
export interface Tunnel {id:string;name:string}
export interface DNSRecord {id:string;name:string;type:string;content:string;proxied:boolean}
// Snapshot age that health and signed-in capacity reads accept: the hourly
// cron plus slack. Allocation decisions always recount live.
export const CAPACITY_FRESH_SECONDS=3900;
type Capacity=Awaited<ReturnType<CloudflareAPI["capacity"]>>;
export async function storedCapacity(env:Env):Promise<Capacity|undefined> {
 const row=await env.DB.prepare("SELECT value FROM relay_capacity WHERE id='current' AND updated>?").bind(now()-CAPACITY_FRESH_SECONDS).first<string>("value");
 return row?JSON.parse(row):undefined;
}
export const relayConfigured=(env:Env)=>!!(env.CF_TUNNEL_TOKEN && /^[a-f0-9]{32}$/.test(env.CF_ACCOUNT_ID??"") && /^[a-f0-9]{32}$/.test(env.CF_ZONE_ID??"") && env.RELAY_ZONE && /^(?:[a-z0-9](?:[a-z0-9-]*[a-z0-9])?\.)+[a-z]{2,}$/.test(env.RELAY_ZONE));
export class CloudflareAPI {
 readonly tunnels:string;readonly dns:string;
 constructor(readonly env:Env,readonly fence?:()=>Promise<void>) {
  if(!relayConfigured(env))throw new Failure(503,"relay_not_configured");
  this.tunnels=`/accounts/${env.CF_ACCOUNT_ID}/cfd_tunnel`;
  this.dns=`/zones/${env.CF_ZONE_ID}/dns_records`;
 }
 hostname(label:string) {return `r-${label}.${this.env.RELAY_ZONE}`;}
 async request<T>(path:string,method="GET",body?:unknown):Promise<Envelope<T>|null> {
  // POST is never blindly retried. A lost create response is recovered by name
  // before another create, including across Worker restarts.
  const attempts=method==="POST"?1:3;
  for(let attempt=0;attempt<attempts;attempt++) {
   await this.fence?.();
   let response:Response|undefined;
   try {
    response=await fetch(API+path,{method,signal:AbortSignal.timeout(5000),headers:{authorization:`Bearer ${this.env.CF_TUNNEL_TOKEN}`,"content-type":"application/json"},...(body===undefined?{}:{body:JSON.stringify(body)})});
    if(response.status===404)return null;
    if(response.ok) {
     const envelope=await response.json() as Envelope<T>;
     if(envelope.success===true)return envelope;
     throw new Failure(503,"relay_unavailable");
    }
    if(response.status!==429 && response.status<500)throw new Failure(503,"relay_unavailable");
   }catch(error){if(error instanceof Failure)throw error;}
   if(attempt===attempts-1)break;
   const retryAfter=Number(response?.headers.get("Retry-After")??0)*1000;
   // Long server backoff is delegated to the host (Retry-After), rather than
   // keeping a request or the global creation lease alive indefinitely.
   if(retryAfter>2000)break;
   await new Promise(resolve=>setTimeout(resolve,Math.max(retryAfter,100*2**attempt+Math.random()*100)));
  }
  throw new Failure(503,"relay_unavailable");
 }
 async list<T>(path:string) {
  const result=await this.request<T[]>(path);
  if(!result || !Array.isArray(result.result))throw new Failure(503,"relay_unavailable");
  return result;
 }
 async capacity() {
  const tunnels=await this.list<Tunnel>(`${this.tunnels}?is_deleted=false&per_page=1`);
  const dns=await this.list<DNSRecord>(`${this.dns}?per_page=1`);
  const used=tunnels.result_info?.total_count,dnsUsed=dns.result_info?.total_count;
  if(!Number.isInteger(used) || (used as number)<0 || !Number.isInteger(dnsUsed) || (dnsUsed as number)<0)throw new Failure(503,"relay_unavailable");
  const state=used!>=TUNNEL_LIMIT || dnsUsed!>=DNS_LIMIT?"full":used!>=800 || dnsUsed!>=160?"near":"ok";
  const value={used:used!,limit:TUNNEL_LIMIT,state,dns:{used:dnsUsed!,limit:DNS_LIMIT},budget:{targetMonthlyUSD:5,hardCapMonthlyUSD:25,paidTunnels:false,spendAlert:state!=="ok"}};
  // Every live count refreshes the shared snapshot at no extra API cost.
  await this.env.DB.prepare("INSERT INTO relay_capacity VALUES('current',?,?) ON CONFLICT(id) DO UPDATE SET value=excluded.value,updated=excluded.updated").bind(JSON.stringify(value),now()).run();
  return value;
 }
 async findTunnel(label:string) {
  const result=await this.list<Tunnel>(`${this.tunnels}?is_deleted=false&name=${encodeURIComponent(`wonder-${label}`)}&per_page=100`);
  const matches=result.result.filter(t=>t.name===`wonder-${label}`);
  if(matches.length>1 || (result.result_info?.total_pages??1)>1)throw new Failure(503,"relay_conflict");
  return matches[0];
 }
 async findDNS(label:string) {
  const result=await this.list<DNSRecord>(`${this.dns}?name=${encodeURIComponent(this.hostname(label))}&per_page=100`);
  if(result.result.length>1 || (result.result_info?.total_pages??1)>1)throw new Failure(503,"relay_conflict");
  return result.result[0];
 }
 async createTunnel(label:string) {
  // Recheck both the deterministic name and authoritative account counts before
  // each POST. The caller holds the shared D1 lease over this whole operation.
  for(let attempt=0;attempt<2;attempt++) {
   const found=await this.findTunnel(label);if(found)return found;
   const capacity=await this.capacity();
   if(capacity.used>=TUNNEL_LIMIT)throw new Failure(503,"relay_full");
   try {
    const created=await this.request<Tunnel>(this.tunnels,"POST",{name:`wonder-${label}`,config_src:"cloudflare"});
    if(created?.result?.id)return created.result;
   }catch(error){if(attempt===1)throw error;}
   await new Promise(resolve=>setTimeout(resolve,150+Math.random()*100));
  }
  throw new Failure(503,"relay_unavailable");
 }
 async configure(tunnelID:string,label:string) {
  // The edge ends TLS, so it forwards only the host's sealed RPC channel; the
  // host also refuses every other path on this address (relay.rs SEALED_PATH).
  if(!await this.request(`${this.tunnels}/${tunnelID}/configurations`,"PUT",{config:{ingress:[{hostname:this.hostname(label),path:RELAY_SEALED_PATH,service:"http://127.0.0.1:3777"},{service:"http_status:404"}]}}))throw new Failure(503,"relay_unavailable");
 }
 async upsertDNS(label:string,tunnelID:string,expectedID:string|null) {
  const existing=await this.findDNS(label);
  if(existing && (existing.type!=="CNAME" || (expectedID?existing.id!==expectedID:existing.content!==`${tunnelID}.cfargotunnel.com`)))throw new Failure(503,"relay_conflict");
  if(!existing && (await this.capacity()).dns.used>=DNS_LIMIT)throw new Failure(503,"relay_full");
  const body={type:"CNAME",name:this.hostname(label),content:`${tunnelID}.cfargotunnel.com`,proxied:true,ttl:1};
  const result=await this.request<DNSRecord>(this.dns+(existing?`/${existing.id}`:""),existing?"PUT":"POST",body);
  if(!result?.result?.id)throw new Failure(503,"relay_unavailable");
  return result.result.id;
 }
 async token(id:string) {
  const result=await this.request<string>(`${this.tunnels}/${id}/token`);
  if(!result || typeof result.result!=="string" || !result.result)throw new Failure(503,"relay_unavailable");
  return result.result;
 }
 async connected(id:string) {
  const result=await this.request<{conns?:unknown[]}[]>(`${this.tunnels}/${id}/connections`);
  if(!result)return null;
  if(!Array.isArray(result.result))throw new Failure(503,"relay_unavailable");
  // This endpoint returns connector clients, each with a nested conns list.
  // A retained client with zero connections is offline; malformed data is
  // unknown, never proof of inactivity.
  if(result.result.some(client=>Array.isArray(client?.conns) && client.conns.length>0))return true;
  if(result.result.some(client=>!Array.isArray(client?.conns)))throw new Failure(503,"relay_unavailable");
  return false;
 }
 async deleteTunnel(id:string) {
  // Cloudflare refuses tunnel deletion while any connector is active. Keep
  // this order at the shared deletion boundary, including the idle reaper:
  // a connector may have reconnected since the last liveness check.
  await this.request(`${this.tunnels}/${id}/connections`,"DELETE");
  await this.request(`${this.tunnels}/${id}`,"DELETE");
 }
 async deleteDNS(id:string) {await this.request(`${this.dns}/${id}`,"DELETE");}
}

export async function withRelayLock<T>(env:Env,run:(fence:()=>Promise<void>)=>Promise<T>) {
 const owner=crypto.randomUUID();
 const locked=await env.DB.prepare("INSERT INTO relay_locks VALUES('control',?,?) ON CONFLICT(id) DO UPDATE SET owner=excluded.owner,expires=excluded.expires WHERE expires<=? RETURNING owner").bind(owner,now()+180,now()).first();
 if(!locked)throw new Failure(503,"relay_busy");
 const fence=async()=>{
  const held=await env.DB.prepare("UPDATE relay_locks SET expires=? WHERE id='control' AND owner=? AND expires>? RETURNING owner").bind(now()+180,owner,now()).first();
  if(!held)throw new Failure(503,"relay_busy");
 };
 try{return await run(fence);}finally{await env.DB.prepare("DELETE FROM relay_locks WHERE id='control' AND owner=?").bind(owner).run();}
}
