import {authenticate} from "./accounts";
import {CloudflareAPI,relayConfigured,storedCapacity,withRelayLock} from "./cloudflare";
import {type Env,Failure,decode,hash,isPublicKey,json,limit,now,random,requireValue,secret,uuid,verify} from "./util";

interface Host {id:string;account_id:string;label:string;host_key:string;tunnel_id:string|null;dns_id:string|null;state:"granted"|"enabled"|"deleting";last_seen:number;created:number;offline_since:number|null;purge_attempts:number;purge_after:number}
interface Pending {id:string;host_key:string;secret_hash:string;label:string|null;account_id:string|null;session_id:string|null;expires:number}
function newLabel() {
 const bytes=crypto.getRandomValues(new Uint8Array(16)),alphabet="abcdefghijklmnopqrstuvwxyz234567";
 let bits=0,value=0,result="";
 for(const byte of bytes){value=(value<<8)|byte;bits+=8;while(bits>=5){bits-=5;result+=alphabet[(value>>>bits)&31];}}
 return result+alphabet[(value<<(5-bits))&31];
}
async function validKey(key:unknown):Promise<string> {
 requireValue(isPublicKey(key),"invalid_key");
 try {await crypto.subtle.importKey("raw",decode(key),{name:"ECDSA",namedCurve:"P-256"},false,["verify"]);}catch{throw new Failure(400,"invalid_key");}
 return key;
}
async function proof(env:Env,key:string,action:string,target:string,data:Record<string,unknown>) {
 const {ts,nonce,sig}=data;
 requireValue(typeof ts==="number" && Number.isInteger(ts) && typeof nonce==="string" && secret.test(nonce));
 if(Math.abs(ts-now())>60 || !await verify(key,`${action}|${target}|${ts}|${nonce}`,sig))throw new Failure(403,"invalid_proof");
 const used=await env.DB.prepare("INSERT INTO relay_nonces(hash,expires) VALUES(?,?) ON CONFLICT DO NOTHING RETURNING hash").bind(await hash(`${key}|${nonce}`),now()+121).first();
 if(!used)throw new Failure(403,"replayed_proof");
}
function requireRelayAccount(env:Env,accountID:string) {
 // Relay traffic is sealed end to end and the tunnel forwards only the sealed
 // path (crates/wonderd/RELAY.md). "*" lets any signed-in account use it; a
 // comma-separated list limits it to those IDs. Missing or empty fails closed.
 const allowed=env.RELAY_ALLOWED_ACCOUNT_IDS?.split(",").map(id=>id.trim()).filter(Boolean)??[];
 if(!allowed.includes("*") && !allowed.includes(accountID))throw new Failure(403,"relay_not_available");
}
const getHost=(env:Env,label:string)=>env.DB.prepare("SELECT * FROM relay_hosts WHERE label=?").bind(label).first<Host>();
async function liveHost(env:Env,label:string) {
 const host=await env.DB.prepare("SELECT h.* FROM relay_hosts h JOIN accounts a ON a.id=h.account_id WHERE h.label=? AND h.state<>'deleting'").bind(label).first<Host>();
 if(!host)throw new Failure(410,"relay_gone");return host;
}
async function reserve(env:Env,accountID:string,sessionID:string,key:string) {
 const existing=await env.DB.prepare("SELECT * FROM relay_hosts WHERE host_key=?").bind(key).first<Host>();
 if(existing) {
  if(existing.account_id!==accountID || existing.state==="deleting")throw new Failure(409,"relay_conflict");
  return existing;
 }
 const label=newLabel();
 const inserted=await env.DB.prepare("INSERT INTO relay_hosts(id,account_id,label,host_key,state,last_seen,created) SELECT ?,?,?,?,'granted',?,? WHERE EXISTS(SELECT 1 FROM sessions WHERE id=? AND account_id=? AND access_expires>?) AND (SELECT COUNT(*) FROM relay_hosts WHERE account_id=?)<5 RETURNING *").bind(crypto.randomUUID(),accountID,label,key,now(),now(),sessionID,accountID,now(),accountID).first<Host>();
 if(!inserted)throw new Failure(409,"relay_host_limit");return inserted;
}
// Caller holds the global lease. Keep row IDs before subsequent external calls
// so retries can finish a partial create, DNS upsert or deletion.
async function provision(env:Env,api:CloudflareAPI,host:Host) {
 host=await liveHost(env,host.label);
 if(host.state!=="enabled")throw new Failure(403,"grant_required");
 let tunnelID=host.tunnel_id;
 if(tunnelID && await api.connected(tunnelID)===null)tunnelID=null;
 if(!tunnelID) {
  const known=await api.findTunnel(host.label);
  if(!known) {
   const capacity=await api.capacity();
   if(capacity.state==="full") {
    // A retained CNAME consumes no additional DNS slot during recreation.
    if(capacity.used>=capacity.limit || !await api.findDNS(host.label))throw new Failure(503,"relay_full");
   }
  }
  tunnelID=(known??await api.createTunnel(host.label)).id;
  await env.DB.prepare("UPDATE relay_hosts SET tunnel_id=?,last_seen=? WHERE label=?").bind(tunnelID,now(),host.label).run();
 }
 await api.configure(tunnelID,host.label);
 const dnsID=await api.upsertDNS(host.label,tunnelID,host.dns_id);
 // A host fetching its token is starting cloudflared: restart the idle clock
 // so the reaper cannot delete the tunnel while the connector comes up.
 await env.DB.prepare("UPDATE relay_hosts SET dns_id=?,last_seen=?,offline_since=NULL WHERE label=?").bind(dnsID,now(),host.label).run();
 const token=await api.token(tunnelID);
 // Account revocation can run while Cloudflare is responding. Never return a
 // token after that revocation; the durable tombstone lets cron clean it up.
 await liveHost(env,host.label);
 return {token,origin:`https://${api.hostname(host.label)}`,label:host.label};
}
async function remove(env:Env,api:CloudflareAPI,host:Host) {
 const tunnel=host.tunnel_id?{id:host.tunnel_id}:await api.findTunnel(host.label);
 if(tunnel)await api.deleteTunnel(tunnel.id);
 // Resolve DNS by name too, recovering a lost POST response before its ID was
 // stored. Only this reserved random hostname is ever touched.
 const dns=await api.findDNS(host.label);
 if(dns) {
  if(dns.type!=="CNAME" || (host.dns_id?dns.id!==host.dns_id:dns.content!==`${tunnel?.id}.cfargotunnel.com`))throw new Failure(503,"relay_conflict");
  await api.deleteDNS(dns.id);
 }
 await env.DB.batch([
  env.DB.prepare("DELETE FROM relay_grants WHERE label=?").bind(host.label),
  env.DB.prepare("DELETE FROM relay_pending WHERE label=?").bind(host.label),
  env.DB.prepare("DELETE FROM relay_hosts WHERE label=? AND state='deleting'").bind(host.label),
 ]);
}
// Tombstones are the purge queue. Sign-out and account deletion hand their own
// rows to a background attempt; cron retries due rows with backoff (4, 8, 16,
// 32, then every 60 minutes), so a permanently failing purge cannot starve
// others. Credentials are revoked before this runs, whatever Cloudflare does.
export async function cleanupRelay(env:Env,accountID?:string) {
 const scope=accountID===undefined?"":" AND account_id=?",values=accountID===undefined?[]:[accountID];
 const rows=await env.DB.prepare(`SELECT label FROM relay_hosts WHERE state='deleting' AND purge_after<=?${scope} ORDER BY purge_after LIMIT 20`).bind(now(),...values).all<{label:string}>();
 if(rows.results.length===0 || !relayConfigured(env))return;
 for(const {label} of rows.results) {
  try {await withRelayLock(env,async fence=>{const host=await getHost(env,label);if(host?.state==="deleting")await remove(env,new CloudflareAPI(env,fence),host);});}
  catch {await env.DB.prepare("UPDATE relay_hosts SET purge_attempts=purge_attempts+1,purge_after=?+MIN(3600,240<<MIN(purge_attempts,4)) WHERE label=? AND state='deleting'").bind(now(),label).run();}
 }
}
export async function reapRelay(env:Env,hourly:boolean) {
 // Mark abandoned grants and orphaned accounts for cleanup before removing any
 // metadata. Expired pending approvals themselves never own resources.
 await env.DB.batch([
  env.DB.prepare("UPDATE relay_hosts SET state='deleting' WHERE state<>'deleting' AND (NOT EXISTS(SELECT 1 FROM accounts a WHERE a.id=relay_hosts.account_id) OR (state='granted' AND created<=? AND NOT EXISTS(SELECT 1 FROM relay_grants g WHERE g.label=relay_hosts.label AND g.expires>?)))").bind(now()-600,now()),
  env.DB.prepare("DELETE FROM relay_grants WHERE expires<=?").bind(now()),
  env.DB.prepare("DELETE FROM relay_pending WHERE expires<=?").bind(now()),
  env.DB.prepare("DELETE FROM relay_nonces WHERE expires<=?").bind(now()),
 ]);
 await cleanupRelay(env);
 if(!relayConfigured(env))return;
 // Keeps the shared capacity snapshot fresh for health and signed-in reads.
 if(hourly)try {await new CloudflareAPI(env).capacity();}catch{/* Reported as unknown once stale. */}
 const rows=await env.DB.prepare("SELECT label FROM relay_hosts WHERE state='enabled' AND tunnel_id IS NOT NULL").all<{label:string}>();
 for(const {label} of rows.results) {
  try {
   await withRelayLock(env,async fence=>{
    const host=await getHost(env,label);if(!host || host.state!=="enabled" || !host.tunnel_id)return;
    const api=new CloudflareAPI(env,fence),connected=await api.connected(host.tunnel_id);
    // Idle deletion follows 10 minutes offline, counted from the first cron
    // that observed it (10-15 minutes in practice), never from last_seen.
    if(connected)await env.DB.prepare("UPDATE relay_hosts SET last_seen=?,offline_since=NULL WHERE label=?").bind(now(),label).run();
    else if(connected===false && host.offline_since===null)await env.DB.prepare("UPDATE relay_hosts SET offline_since=? WHERE label=?").bind(now(),label).run();
    else if(connected===null || host.offline_since!<=now()-600) {
     if(connected!==null)await api.deleteTunnel(host.tunnel_id);
     await env.DB.prepare("UPDATE relay_hosts SET tunnel_id=NULL,offline_since=NULL WHERE label=? AND tunnel_id=?").bind(label,host.tunnel_id).run();
    }
   });
  }catch{/* Unknown liveness is never offline. Retry on the next cron. */}
 }
}

// Anonymous health never calls Cloudflare: it reads the shared snapshot that
// cron and live capacity checks keep, and reports unknown once that is stale.
export async function relayHealth(env:Env) {
 if(!relayConfigured(env))return {configured:false};
 const stored=await storedCapacity(env);
 return stored?{configured:true,...stored}:{configured:true,state:"unknown",budget:{spendAlert:true}};
}
export async function handleRelay(path:string,request:Request,env:Env,data:Record<string,unknown>,ip:string) {
 if(!path.startsWith("/v1/relay/"))return undefined;
 if(!relayConfigured(env))throw new Failure(503,"relay_not_configured");
 if(path==="/v1/relay/capacity" && request.method==="GET") {
  await authenticate(request,env);
  return json(await storedCapacity(env)??await new CloudflareAPI(env).capacity());
 }
 if(path==="/v1/relay/grants" && request.method==="POST") {
  const session=await authenticate(request,env),{hostKey,...extra}=data;
  requireRelayAccount(env,session.account_id);
  requireValue(Object.keys(extra).length===0);const key=await validKey(hostKey);
  await limit(env,`relay-grant:${session.account_id}`,3600,30);
  return withRelayLock(env,async()=>{
   const host=await reserve(env,session.account_id,session.id,key),grant=random();
   await env.DB.batch([env.DB.prepare("DELETE FROM relay_grants WHERE label=?").bind(host.label),env.DB.prepare("INSERT INTO relay_grants VALUES(?,?,?,?)").bind(await hash(grant),host.label,session.id,now()+600)]);
   return json({grant,label:host.label,expiresIn:600});
  });
 }
 const hostMatch=path.match(/^\/v1\/relay\/hosts\/([a-z2-7]{26})\/(connect|disable)$/);
 if(hostMatch && request.method==="POST") {
  const [,label,action]=hostMatch,{grant,ts,nonce,sig,...extra}=data;requireValue(Object.keys(extra).length===0);
  const host=await liveHost(env,label);
  await proof(env,host.host_key,`relay-${action}`,label,{ts,nonce,sig});
  await limit(env,`relay-host:${label}`,3600,120);
  return withRelayLock(env,async fence=>{
   const current=await liveHost(env,label),cf=new CloudflareAPI(env,fence);
   if(action==="disable") {
    await env.DB.prepare("UPDATE relay_hosts SET state='deleting' WHERE label=?").bind(label).run();await remove(env,cf,current);return json({disabled:true});
   }
   if(grant!==undefined) {
    requireValue(typeof grant==="string" && secret.test(grant));
    const grantHash=await hash(grant);
    const [enabled]=await env.DB.batch([
     env.DB.prepare("UPDATE relay_hosts SET state='enabled' WHERE label=? AND state<>'deleting' AND EXISTS(SELECT 1 FROM relay_grants g JOIN sessions s ON s.id=g.session_id WHERE g.hash=? AND g.label=relay_hosts.label AND g.expires>? AND s.refresh_expires>?) RETURNING label").bind(label,grantHash,now(),now()),
     env.DB.prepare("DELETE FROM relay_grants WHERE hash=? AND label=?").bind(grantHash,label),
    ]);
    if(enabled.results.length===0)throw new Failure(403,"invalid_grant");
   }else if(current.state!=="enabled")throw new Failure(403,"grant_required");
   return json(await provision(env,cf,current));
  });
 }
 if(path==="/v1/relay/pending" && request.method==="POST") {
  const {hostKey,ts,nonce,sig,...extra}=data;requireValue(Object.keys(extra).length===0);const key=await validKey(hostKey);
  await limit(env,`relay-pending:${ip}`,3600,10);await limit(env,"relay-pending-global",3600,200);
  await proof(env,key,"relay-pending",key,{ts,nonce,sig});
  const id=crypto.randomUUID(),pendingSecret=random();
  await env.DB.prepare("INSERT INTO relay_pending(id,host_key,secret_hash,expires) VALUES(?,?,?,?)").bind(id,key,await hash(pendingSecret),now()+600).run();
  return json({id,secret:pendingSecret,expiresIn:600},202);
 }
 const pendingMatch=path.match(/^\/v1\/relay\/pending\/([0-9a-f-]{36})(\/approve)?$/);
 if(pendingMatch) {
  const [,id,approve]=pendingMatch;requireValue(uuid.test(id));
  const pending=await env.DB.prepare("SELECT * FROM relay_pending WHERE id=? AND expires>?").bind(id,now()).first<Pending>();
  if(!pending)throw new Failure(410,"pending_gone");
  if(approve && request.method==="POST") {
   const session=await authenticate(request,env),{secret:approvalSecret,...extra}=data;
   requireRelayAccount(env,session.account_id);
   requireValue(Object.keys(extra).length===0 && typeof approvalSecret==="string" && secret.test(approvalSecret));
   if(pending.secret_hash!==await hash(approvalSecret) || (pending.account_id && pending.account_id!==session.account_id))throw new Failure(403,"invalid_proof");
   await limit(env,`relay-approve:${session.account_id}`,3600,30);
   return withRelayLock(env,async fence=>{
    // Re-read after waiting for the lease; another account cannot take over a
    // pending host approved concurrently from a second device.
    const current=await env.DB.prepare("SELECT * FROM relay_pending WHERE id=? AND expires>?").bind(id,now()).first<Pending>();
    if(!current)throw new Failure(410,"pending_gone");
    if(current.account_id && current.account_id!==session.account_id)throw new Failure(403,"invalid_proof");
    const host=await reserve(env,session.account_id,session.id,current.host_key);
    await env.DB.batch([env.DB.prepare("UPDATE relay_hosts SET state='enabled' WHERE label=? AND state='granted'").bind(host.label),env.DB.prepare("UPDATE relay_pending SET label=?,account_id=?,session_id=? WHERE id=?").bind(host.label,session.account_id,session.id,id)]);
    const result=await provision(env,new CloudflareAPI(env,fence),host);
    return json({approved:true,label:host.label,origin:result.origin});
   });
  }
  if(!approve && request.method==="GET") {
   const {ts,nonce,sig,...extra}=data;requireValue(Object.keys(extra).length===0);
   await proof(env,pending.host_key,"relay-poll",id,{ts,nonce,sig});
   await limit(env,`relay-poll:${id}`,600,120);
   if(!pending.label)return json({approved:false},202);
   return withRelayLock(env,async fence=>json({approved:true,...await provision(env,new CloudflareAPI(env,fence),await liveHost(env,pending.label!))}));
  }
 }
 throw new Failure(404,"not_found");
}
