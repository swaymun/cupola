import {handleActivity,reapActivity} from "./activity";
import {handleRelay,reapRelay,relayHealth} from "./relay";
import {PRIVACY_HTML,authenticate,handleAccount,purgeSessions} from "./accounts";
import {type Env,Failure,base64url,body,decode,encoder,hash,isPublicKey,json,limit,now,random,requireValue,secret,uuid,verify} from "./util";
export {base64url,hash,type Env};
let jwtCache:{key:string,expires:number,value:string}|undefined;
async function jwt(env:Env) {
 const cacheKey=`${env.APNS_TEAM_ID}:${env.APNS_KEY_ID}:${await hash(env.APNS_PRIVATE_KEY)}`;
 if(jwtCache?.key===cacheKey && jwtCache.expires>now())return jwtCache.value;
 const header=base64url(encoder.encode(JSON.stringify({alg:"ES256",kid:env.APNS_KEY_ID})));
 const claims=base64url(encoder.encode(JSON.stringify({iss:env.APNS_TEAM_ID,iat:now()})));
 const key=await crypto.subtle.importKey("pkcs8",decode(env.APNS_PRIVATE_KEY.replace(/-----[^-]+-----/g,"").replace(/\s/g,"")),{name:"ECDSA",namedCurve:"P-256"},false,["sign"]);
 const signature=await crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"},key,encoder.encode(`${header}.${claims}`));
 const value=`${header}.${claims}.${base64url(new Uint8Array(signature))}`;
 jwtCache={key:cacheKey,expires:now()+2400,value};return value;
}
export async function apns(env:Env,token:string,payload:unknown,eventID:string,background=false) {
 if(!env.APNS_PRIVATE_KEY || !env.APNS_KEY_ID || !env.APNS_TEAM_ID)throw new Failure(503,"push_not_configured");
 requireValue(env.APNS_ENVIRONMENT==="production" || env.APNS_ENVIRONMENT==="sandbox","invalid_environment");
 const host=env.APNS_ENVIRONMENT==="production"?"api.push.apple.com":"api.sandbox.push.apple.com";
 const response=await fetch(`https://${host}/3/device/${token}`,{method:"POST",signal:AbortSignal.timeout(10000),headers:{authorization:`bearer ${await jwt(env)}`,"content-type":"application/json","apns-topic":env.APNS_TOPIC,"apns-push-type":background?"background":"alert","apns-priority":background?"5":"10","apns-expiration":String(now()+3600),"apns-id":eventID,"apns-collapse-id":eventID},body:JSON.stringify(payload)});
 // Do not log tokens, payloads, keys, or upstream bodies.
 let reason="";if(!response.ok){try{reason=(await response.json() as {reason?:string}).reason??"";}catch{}}
 return {status:response.status,reason};
}
// Live Activity pushes: the content is rebuilt in activity.ts from validated rows.
export async function activity(env:Env,token:string,payload:unknown,priority:"5"|"10") {
 if(!env.APNS_PRIVATE_KEY || !env.APNS_KEY_ID || !env.APNS_TEAM_ID)throw new Failure(503,"push_not_configured");
 requireValue(env.APNS_ENVIRONMENT==="production" || env.APNS_ENVIRONMENT==="sandbox","invalid_environment");
 const host=env.APNS_ENVIRONMENT==="production"?"api.push.apple.com":"api.sandbox.push.apple.com";
 const response=await fetch(`https://${host}/3/device/${token}`,{method:"POST",signal:AbortSignal.timeout(10000),headers:{authorization:`bearer ${await jwt(env)}`,"content-type":"application/json","apns-topic":`${env.APNS_TOPIC}.push-type.liveactivity`,"apns-push-type":"liveactivity","apns-priority":priority,"apns-expiration":String(now()+900)},body:JSON.stringify(payload)});
 let reason="";if(!response.ok){try{reason=(await response.json() as {reason?:string}).reason??"";}catch{}}
 return {status:response.status,reason};
}
interface Challenge {id:string;token:string;token_hash:string;public_key:string;nonce:string;challenge_hash:string;expires:number}
interface Registration {id:string;token:string;token_hash:string;public_key:string;sender_hash:string;account_id:string;session_id:string;updated:number;alerts:number}

async function handle(request:Request,env:Env,ctx:ExecutionContext) {
 const path=new URL(request.url).pathname;
 if(request.method==="GET" && path==="/healthz")return json({status:"ok",configured:!!env.APNS_KEY_ID && !!env.APNS_PRIVATE_KEY,accounts:!!env.ACCOUNT_PEPPER && !!env.APPLE_AUDIENCES,topic:env.APNS_TOPIC,environment:env.APNS_ENVIRONMENT,relay:await relayHealth(env)});
 // Preserve the privacy page on custom domains too: Cloudflare's automatic
 // Web Analytics must not inject its beacon into this service's HTML.
 if(request.method==="GET" && path==="/privacy")return new Response(PRIVACY_HTML,{headers:{"Content-Type":"text/html; charset=utf-8","Cache-Control":"public, max-age=3600, no-transform"}});
 const relayGET=request.method==="GET" && (path==="/v1/relay/capacity" || /^\/v1\/relay\/pending\/[0-9a-f-]{36}$/.test(path));
 if(request.method!=="POST" && !relayGET)throw new Failure(405,"method_not_allowed");
 if(!env.RATE_SECRET)throw new Failure(503,"push_not_configured");
 // Salted short-lived IP hashes only; a user cannot choose Cloudflare's IP header.
 const ip=await hash(`${env.RATE_SECRET}:${request.headers.get("CF-Connecting-IP")??"unknown"}`);
 // This inexpensive edge gate is approximate and per location. The device,
 // enrollment and global delivery quotas below remain exact in D1.
 if(!(await env.IP_RATE_LIMITER.limit({key:ip})).success)throw new Failure(429,"rate_limited");
 const params=new URL(request.url).searchParams;
 const data:Record<string,unknown>=relayGET?(path==="/v1/relay/capacity"?{}:{...Object.fromEntries(params),ts:params.has("ts")?Number(params.get("ts")):undefined}):await body(request,8192);
 const relay=await handleRelay(path,request,env,data,ip);
 if(relay)return relay;
 const account=await handleAccount(path,request,env,ctx,data,ip);
 if(account)return account;
 if(path==="/v1/challenges") {
  // Push belongs to an account: only a signed-in phone session may enroll.
  const session=await authenticate(request,env);
  const {token,publicKey,nonce}=data;
  requireValue(typeof token==="string" && /^[0-9a-f]{32,512}$/.test(token));
  requireValue(isPublicKey(publicKey) && typeof nonce==="string" && secret.test(nonce));
  try {await crypto.subtle.importKey("raw",decode(publicKey),{name:"ECDSA",namedCurve:"P-256"},false,["verify"]);}catch{throw new Failure(400,"invalid_key");}
  const tokenHash=await hash(`${env.RATE_SECRET}:${token}`);
  // Per token and per account before the global quota, so one account cannot spend it.
  await limit(env,`enroll:${tokenHash}`,3600,3);await limit(env,`enroll-account:${session.account_id}`,3600,20);
  await limit(env,"enroll-global",3600,300);
  const id=crypto.randomUUID(), challenge=random();
  await env.DB.prepare("INSERT INTO challenges(id,token,token_hash,public_key,nonce,challenge_hash,account_id,session_id,expires) VALUES(?,?,?,?,?,?,?,?,?)").bind(id,token,tokenHash,publicKey,nonce,await hash(challenge),session.account_id,session.id,now()+600).run();
  // Only the app receiving this APNs token can learn the proof challenge. A
  // public registration request alone never authorizes alert delivery.
  const result=await apns(env,token,{aps:{"content-available":1},wonderPush:{challengeId:id,challenge,nonce}},id,true);
  if(result.status!==200){await env.DB.prepare("DELETE FROM challenges WHERE id=?").bind(id).run();throw new Failure(result.status===400 || result.status===410?400:503,"enrollment_delivery_failed");}
  return json({id,expiresIn:600},202);
 }
 if(path==="/v1/registrations") {
  const {id,challenge,senderSecret,signature}=data;
  requireValue(typeof id==="string" && uuid.test(id) && typeof challenge==="string" && secret.test(challenge) && typeof senderSecret==="string" && secret.test(senderSecret));
  const pending=await env.DB.prepare("SELECT * FROM challenges WHERE id=? AND expires>?").bind(id,now()).first<Challenge>();
  if(!pending || pending.challenge_hash!==await hash(challenge) || !await verify(pending.public_key,`register|${id}|${challenge}|${senderSecret}`,signature))throw new Failure(403,"invalid_proof");
  // Idempotent proof: the client chose and retained the sender secret before
  // submitting. The old token registration is replaced only after APNs proof.
  const registered=await env.DB.prepare("INSERT INTO registrations(id,token,token_hash,public_key,sender_hash,account_id,session_id,updated) SELECT id,token,token_hash,public_key,?,account_id,session_id,? FROM challenges WHERE id=? AND expires>? AND EXISTS(SELECT 1 FROM sessions s WHERE s.id=challenges.session_id) AND (EXISTS(SELECT 1 FROM registrations r WHERE r.token_hash=challenges.token_hash AND r.public_key=challenges.public_key) OR (SELECT COUNT(*) FROM registrations r WHERE r.token_hash=challenges.token_hash)<8) ON CONFLICT(token_hash,public_key) DO UPDATE SET id=excluded.id,token=excluded.token,public_key=excluded.public_key,sender_hash=excluded.sender_hash,account_id=excluded.account_id,session_id=excluded.session_id,updated=excluded.updated RETURNING id").bind(await hash(senderSecret),now(),id,now()).first();
  if(!registered)throw new Failure(409,"registration_unavailable");
  return json({id});
 }
 const match=path.match(/^\/v1\/registrations\/([0-9a-f-]{36})\/(send|revoke|status|alerts|activity|activity-state)$/);
 if(!match)throw new Failure(404,"not_found");
 const [,id,action]=match;
 // A registration is usable only while the signed-in session that enrolled it
 // exists; signing out or deleting the account ends it.
 const registration=await env.DB.prepare("SELECT r.* FROM registrations r JOIN sessions s ON s.id=r.session_id WHERE r.id=?").bind(id).first<Registration>();
 if(!registration)throw new Failure(410,"registration_gone");
 const bearer=request.headers.get("Authorization")?.replace(/^Bearer /,"")??"";
 if(!secret.test(bearer) || await hash(bearer)!==registration.sender_hash)throw new Failure(403,"unauthorized");
 if(action==="status") return json({id,account:true});
 if(action==="revoke") {
  await env.DB.batch([env.DB.prepare("DELETE FROM registrations WHERE id=?").bind(id),env.DB.prepare("DELETE FROM deliveries WHERE registration_id=?").bind(id),env.DB.prepare("DELETE FROM activity_rows WHERE registration_id=?").bind(id),env.DB.prepare("DELETE FROM challenges WHERE public_key=?").bind(registration.public_key)]);
  return json({revoked:true});
 }
 if(action==="alerts") {
  requireValue(typeof data.enabled==="boolean");
  await env.DB.prepare("UPDATE registrations SET alerts=? WHERE id=?").bind(data.enabled?1:0,id).run();
  return json({alerts:data.enabled});
 }
 if(action==="activity" || action==="activity-state")return handleActivity(env,action,registration,data,activity);
 const {eventId,routeId,kind,preview}=data;
 requireValue(preview===undefined || (typeof preview==="string" && preview.length>=60 && preview.length<=3240 && /^[A-Za-z0-9_-]+$/.test(preview)),"invalid_preview");
 requireValue(typeof eventId==="string" && uuid.test(eventId) && typeof routeId==="string" && uuid.test(routeId) && (kind==="completed" || kind==="attention"));
 const previous=await env.DB.prepare("SELECT * FROM deliveries WHERE registration_id=? AND event_id=?").bind(id,eventId).first<{route_id:string;kind:string;state:string;attempts:number;updated:number}>();
 if(previous && (previous.route_id!==routeId || previous.kind!==kind))throw new Failure(409,"event_conflict");
 if(previous?.state==="delivered")return json({delivered:true,duplicate:true});
 if(previous && (previous.attempts>=5 || (previous.state==="sending" && previous.updated>now()-30)))throw new Failure(429,"retry_later");
 // Notifications off: Live Activities still update, banners do not.
 if(registration.alerts===0)return json({delivered:true,suppressed:true});
 await limit(env,`send:${id}`,3600,60);await limit(env,"send-global",3600,30000);
 const claimed=await env.DB.prepare("INSERT INTO deliveries VALUES(?,?,?,?, 'sending',1,?) ON CONFLICT(registration_id,event_id) DO UPDATE SET state='sending',attempts=attempts+1,updated=excluded.updated WHERE attempts<5 AND route_id=excluded.route_id AND kind=excluded.kind AND (state='retry' OR (state='sending' AND updated<=?)) RETURNING event_id").bind(id,eventId,routeId,kind,now(),now()-30).first();
 if(!claimed)throw new Failure(429,"retry_later");
 try {
  const result=await apns(env,registration.token,{aps:{alert:{title:"Cupola",body:kind==="completed"?"A reply finished.":"A task needs your attention."},sound:"default",...(preview?{"mutable-content":1}:{})},wonderPush:{registrationId:id,routeId,...(preview?{eventId,preview}:{})}},eventId);
  if(result.status===410 || ["BadDeviceToken","DeviceTokenNotForTopic","Unregistered"].includes(result.reason)) {
   await env.DB.prepare("DELETE FROM registrations WHERE id=?").bind(id).run();throw new Failure(410,"registration_gone");
  }
  if(result.status!==200)throw new Failure(503,"delivery_unavailable");
  await env.DB.prepare("UPDATE deliveries SET state='delivered',updated=? WHERE registration_id=? AND event_id=?").bind(now(),id,eventId).run();
  return json({delivered:true});
 }catch(error){await env.DB.prepare("UPDATE deliveries SET state='retry',updated=? WHERE registration_id=? AND event_id=?").bind(now(),id,eventId).run();throw error;}
}
export default {
 async fetch(request:Request,env:Env,ctx:ExecutionContext):Promise<Response> {
  try{return await handle(request,env,ctx);}catch(error){const failure=error instanceof Failure?error:new Failure(503,"temporarily_unavailable");const response=json({error:failure.code},failure.status);if(failure.status===429 || failure.status===503)response.headers.set("Retry-After",failure.code==="relay_full"?"600":"60");return response;}
 },
 async scheduled(_event:ScheduledController,env:Env) {
  // Receipt expiry deliberately scans the seven-day retention window once an
  // hour, avoiding a second index write for every insert/update/delete. Quota
  // rows are reused across hours and reclaimed only after a week of inactivity.
  if(_event.cron!=="*/5 * * * *")await env.DB.batch([env.DB.prepare("DELETE FROM challenges WHERE expires<=?").bind(now()),env.DB.prepare("DELETE FROM limits WHERE expires<?").bind(now()-7*86400),env.DB.prepare("DELETE FROM deliveries WHERE updated<?").bind(now()-7*86400),env.DB.prepare("DELETE FROM registrations WHERE updated<?").bind(now()-90*86400),env.DB.prepare("DELETE FROM activity_cards WHERE updated<?").bind(now()-90*86400),...purgeSessions(env,"refresh_expires<=?",now())]);
  await reapActivity(env,activity);
  await reapRelay(env,_event.cron!=="*/5 * * * *");
 }
} satisfies ExportedHandler<Env>;
