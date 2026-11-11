// Wonder accounts own notification registrations and optional remote access. An account is an ID
// with one or more linked sign-in identities; only Sign in with Apple exists
// today. Identity subjects are stored as peppered HMACs, never in plaintext.
// Sessions are bound to a device key: refresh needs that key's signature.
import {cleanupRelay} from "./relay";
import {type Env,Failure,base64url,decode,encoder,hash,isPublicKey,json,limit,now,random,requireValue,secret,uuid,verify} from "./util";

export const ACCESS_SECONDS=3600, REFRESH_SECONDS=90*86400;
const APPLE_ISSUER="https://appleid.apple.com", APPLE_KEYS=`${APPLE_ISSUER}/auth/keys`;

export interface Session {id:string;account_id:string;device_key:string;access_expires:number;refresh_hash:string;refresh_expires:number}

let appleKeys:{expires:number,keys:Map<string,CryptoKey>}|undefined;
export function resetAppleKeys() { appleKeys=undefined; }
async function appleKey(kid:string) {
 if(!appleKeys || appleKeys.expires<=now() || !appleKeys.keys.has(kid)) {
  // Refetch on expiry or an unknown key ID, so Apple's key rotation works.
  const response=await fetch(APPLE_KEYS,{signal:AbortSignal.timeout(5000)});
  if(!response.ok)throw new Failure(503,"apple_unavailable");
  const {keys}=await response.json() as {keys?:JsonWebKey[]};
  const imported=new Map<string,CryptoKey>();
  for(const jwk of keys??[]) {
   const id=(jwk as {kid?:string}).kid;
   if(!id || jwk.kty!=="RSA" || (jwk.alg && jwk.alg!=="RS256"))continue;
   imported.set(id,await crypto.subtle.importKey("jwk",{kty:"RSA",n:jwk.n,e:jwk.e,alg:"RS256",ext:true},{name:"RSASSA-PKCS1-v1_5",hash:"SHA-256"},false,["verify"]));
  }
  appleKeys={expires:now()+3600,keys:imported};
 }
 return appleKeys.keys.get(kid);
}
const hex=(bytes:ArrayBuffer)=>[...new Uint8Array(bytes)].map(b=>b.toString(16).padStart(2,"0")).join("");
// Verifies an Apple identity token: RS256 signature against Apple's JWKS,
// issuer, audience (either Wonder bundle ID), expiry and the request nonce.
export async function verifyAppleToken(env:Env,token:unknown,nonce:string) {
 requireValue(typeof token==="string" && token.length<=4096,"invalid_token");
 const parts=token.split(".");
 if(parts.length!==3)throw new Failure(401,"invalid_token");
 let header:{alg?:string,kid?:string},claims:Record<string,unknown>;
 try {header=JSON.parse(new TextDecoder().decode(decode(parts[0])));claims=JSON.parse(new TextDecoder().decode(decode(parts[1])));}catch{throw new Failure(401,"invalid_token");}
 if(header.alg!=="RS256" || typeof header.kid!=="string")throw new Failure(401,"invalid_token");
 const key=await appleKey(header.kid);
 if(!key || !await crypto.subtle.verify("RSASSA-PKCS1-v1_5",key,decode(parts[2]),encoder.encode(`${parts[0]}.${parts[1]}`)))throw new Failure(401,"invalid_token");
 const audiences=env.APPLE_AUDIENCES.split(",").map(a=>a.trim()).filter(Boolean);
 const current=now();
 if(claims.iss!==APPLE_ISSUER || typeof claims.aud!=="string" || !audiences.includes(claims.aud))throw new Failure(401,"invalid_token");
 if(typeof claims.exp!=="number" || claims.exp<current-60 || typeof claims.iat!=="number" || claims.iat>current+60)throw new Failure(401,"token_expired");
 if(typeof claims.sub!=="string" || !claims.sub || claims.sub.length>255)throw new Failure(401,"invalid_token");
 // The app asks Apple to embed SHA-256(nonce); only this request's nonce matches.
 if(claims.nonce!==hex(await crypto.subtle.digest("SHA-256",encoder.encode(nonce))))throw new Failure(401,"invalid_nonce");
 return claims.sub;
}
async function subjectHash(env:Env,provider:string,subject:string) {
 const key=await crypto.subtle.importKey("raw",encoder.encode(env.ACCOUNT_PEPPER),{name:"HMAC",hash:"SHA-256"},false,["sign"]);
 return base64url(new Uint8Array(await crypto.subtle.sign("HMAC",key,encoder.encode(`${provider}:${subject}`))));
}
// Statements that remove sessions matching `where` with everything they own:
// receipts, registrations and pending challenges. Order matters only for
// readability; nothing here relies on SQLite foreign-key enforcement.
export function purgeSessions(env:Env,where:string,...values:unknown[]) {
 const sessions=`SELECT id FROM sessions WHERE ${where}`;
 return [
  // Unredeemed grants require their issuing session. An enabled host belongs to
  // the account and survives credential expiry/replacement on every phone.
  env.DB.prepare(`DELETE FROM relay_grants WHERE session_id IN (${sessions})`).bind(...values),
  env.DB.prepare(`DELETE FROM activity_rows WHERE session_id IN (${sessions})`).bind(...values),
  env.DB.prepare(`DELETE FROM activity_cards WHERE session_id IN (${sessions})`).bind(...values),
  env.DB.prepare(`DELETE FROM deliveries WHERE registration_id IN (SELECT id FROM registrations WHERE session_id IN (${sessions}))`).bind(...values),
  env.DB.prepare(`DELETE FROM registrations WHERE session_id IN (${sessions})`).bind(...values),
  env.DB.prepare(`DELETE FROM challenges WHERE session_id IN (${sessions})`).bind(...values),
  env.DB.prepare(`DELETE FROM sessions WHERE ${where}`).bind(...values),
 ];
}
// Only explicit sign-out/account deletion revokes account-owned remote access.
// Keep this separate from session maintenance and keep tombstones until the
// ordered external purge succeeds, including after the account row is removed.
function revokeAccountRelay(env:Env,accountID:string) {
 return [
  env.DB.prepare("UPDATE relay_hosts SET state='deleting',purge_attempts=0,purge_after=0 WHERE account_id=? RETURNING label").bind(accountID),
  env.DB.prepare("DELETE FROM relay_grants WHERE label IN (SELECT label FROM relay_hosts WHERE account_id=?)").bind(accountID),
  env.DB.prepare("DELETE FROM relay_pending WHERE account_id=?").bind(accountID),
 ];
}
async function issue(env:Env,accountID:string,deviceKey:string) {
 const id=crypto.randomUUID(),access=random(),refresh=random(),current=now();
 // One session per device key: signing in again, to any account, replaces the
 // device's previous session and the registrations it owned.
 await env.DB.batch([...purgeSessions(env,"device_key=?",deviceKey),
  env.DB.prepare("INSERT INTO sessions(id,account_id,device_key,access_hash,access_expires,refresh_hash,refresh_expires,created) VALUES(?,?,?,?,?,?,?,?)").bind(id,accountID,deviceKey,await hash(access),current+ACCESS_SECONDS,await hash(refresh),current+REFRESH_SECONDS,current)]);
 return {sessionId:id,accessToken:access,accessExpiresIn:ACCESS_SECONDS,refreshToken:refresh,refreshExpiresIn:REFRESH_SECONDS};
}
// Resolves a Bearer access token to its live session, or fails with 401.
export async function authenticate(request:Request,env:Env) {
 const bearer=request.headers.get("Authorization")?.replace(/^Bearer /,"")??"";
 if(!secret.test(bearer))throw new Failure(401,"sign_in_required");
 const session=await env.DB.prepare("SELECT * FROM sessions WHERE access_hash=? AND access_expires>?").bind(await hash(bearer),now()).first<Session>();
 if(!session)throw new Failure(401,"sign_in_required");
 return session;
}

// Revocation is immediate; the Cloudflare purge continues after the response.
function queueRelayCleanup(env:Env,ctx:ExecutionContext,revoked:D1Result,accountID:string) {
 if(revoked.results.length===0)return {};
 ctx.waitUntil(cleanupRelay(env,accountID).catch(()=>{/* Cron retries the tombstones. */}));
 return {cleanupPending:true};
}
export async function handleAccount(path:string,request:Request,env:Env,ctx:ExecutionContext,data:Record<string,unknown>,ip:string):Promise<Response|undefined> {
 if(path==="/v1/auth/apple") {
  if(!env.ACCOUNT_PEPPER || !env.APPLE_AUDIENCES)throw new Failure(503,"accounts_not_configured");
  const {identityToken,nonce,deviceKey,signature,...extra}=data;
  requireValue(Object.keys(extra).length===0 && typeof nonce==="string" && secret.test(nonce) && isPublicKey(deviceKey));
  await limit(env,`signin:${ip}`,3600,20);
  // Proves possession of the device key this session will be bound to.
  if(!await verify(deviceKey,`signin|apple|${nonce}`,signature))throw new Failure(403,"invalid_proof");
  const subject=await verifyAppleToken(env,identityToken,nonce);
  // Counted only once Apple vouches for the user: anyone can sign a request
  // with a key of their own, so a bad token must not spend everyone's quota.
  await limit(env,"signin-global",3600,2000);
  const subjectID=await subjectHash(env,"apple",subject),candidate=crypto.randomUUID(),current=now();
  await env.DB.batch([
   env.DB.prepare("INSERT INTO accounts(id,created) SELECT ?,? WHERE NOT EXISTS(SELECT 1 FROM identities WHERE provider='apple' AND subject_hash=?)").bind(candidate,current,subjectID),
   env.DB.prepare("INSERT INTO identities(provider,subject_hash,account_id,created) SELECT 'apple',?,?,? WHERE EXISTS(SELECT 1 FROM accounts WHERE id=?) ON CONFLICT DO NOTHING").bind(subjectID,candidate,current,candidate),
  ]);
  const accountID=await env.DB.prepare("SELECT account_id FROM identities WHERE provider='apple' AND subject_hash=?").bind(subjectID).first<string>("account_id");
  if(!accountID)throw new Failure(503,"temporarily_unavailable");
  // A concurrent first sign-in may have won the identity; drop our spare account.
  if(accountID!==candidate)await env.DB.prepare("DELETE FROM accounts WHERE id=? AND NOT EXISTS(SELECT 1 FROM identities WHERE account_id=?)").bind(candidate,candidate).run();
  return json({...await issue(env,accountID,deviceKey),created:accountID===candidate});
 }
 if(path==="/v1/auth/refresh") {
  const {sessionId,refreshToken,timestamp,signature,...extra}=data;
  requireValue(Object.keys(extra).length===0 && typeof sessionId==="string" && uuid.test(sessionId) && typeof refreshToken==="string" && secret.test(refreshToken) && typeof timestamp==="number" && Number.isInteger(timestamp));
  await limit(env,`refresh:${sessionId}`,3600,30);
  const session=await env.DB.prepare("SELECT * FROM sessions WHERE id=? AND refresh_expires>?").bind(sessionId,now()).first<Session>();
  if(!session || session.refresh_hash!==await hash(refreshToken))throw new Failure(401,"sign_in_required");
  // Device binding: a copied refresh token is useless without the device key.
  if(Math.abs(timestamp-now())>300 || !await verify(session.device_key,`refresh|${sessionId}|${timestamp}`,signature))throw new Failure(403,"invalid_proof");
  const access=random(),refresh=random(),current=now();
  // Rotate both tokens; the compare-and-swap rejects a concurrent reuse.
  const rotated=await env.DB.prepare("UPDATE sessions SET access_hash=?,access_expires=?,refresh_hash=?,refresh_expires=? WHERE id=? AND refresh_hash=? RETURNING id").bind(await hash(access),current+ACCESS_SECONDS,await hash(refresh),current+REFRESH_SECONDS,sessionId,session.refresh_hash).first();
  if(!rotated)throw new Failure(401,"sign_in_required");
  return json({sessionId,accessToken:access,accessExpiresIn:ACCESS_SECONDS,refreshToken:refresh,refreshExpiresIn:REFRESH_SECONDS});
 }
 if(path==="/v1/auth/signout") {
  const session=await authenticate(request,env);
  const [revoked]=await env.DB.batch([...revokeAccountRelay(env,session.account_id),...purgeSessions(env,"id=?",session.id)]);
  return json({signedOut:true,...queueRelayCleanup(env,ctx,revoked,session.account_id)});
 }
 if(path==="/v1/account") {
  const session=await authenticate(request,env);
  const created=await env.DB.prepare("SELECT created FROM accounts WHERE id=?").bind(session.account_id).first<number>("created");
  return json({created,providers:["apple"]});
 }
 if(path==="/v1/account/delete") {
  // Apple requires in-app deletion. Everything the account owns goes now:
  // every device session, registration, receipt, challenge and identity.
  const session=await authenticate(request,env);
  const [revoked]=await env.DB.batch([...revokeAccountRelay(env,session.account_id),...purgeSessions(env,"account_id=?",session.account_id),
   env.DB.prepare("DELETE FROM identities WHERE account_id=?").bind(session.account_id),
   env.DB.prepare("DELETE FROM accounts WHERE id=?").bind(session.account_id)]);
  return json({deleted:true,...queueRelayCleanup(env,ctx,revoked,session.account_id)});
 }
 return undefined;
}

export const PRIVACY_HTML=`<!doctype html><html lang="en"><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Cupola services privacy</title>
<style>:root{color-scheme:light dark}body{font:17px/1.5 -apple-system,system-ui,sans-serif;max-width:40rem;margin:0 auto;padding:24px 16px;background:Canvas;color:CanvasText}h1{font-size:1.5rem}h2{font-size:1.1rem;margin-top:1.5rem}</style></head><body>
<h1>Cupola services privacy</h1>
<p>Cupola's notification service runs on Cloudflare. It delivers notifications and Live Activity updates, and optionally lets your phone reach your computer away from home. These features need a Cupola account. Pairing and private connections need no account.</p>
<h2>What is stored</h2>
<ul>
<li><strong>Your account:</strong> a random account ID, the date it was created, and a keyed hash of your Sign in with Apple user ID. Your name and email are not requested or stored.</li>
<li><strong>Your signed-in phones:</strong> a device public key and hashes of short-lived sign-in tokens.</li>
<li><strong>Notification registrations:</strong> each phone's Apple push token, hashes of the per-Mac sending credential, and delivery receipts (random IDs, state and time) kept for seven days.</li>
<li><strong>Live Activity:</strong> so one Lock Screen card can list the chats running on all your computers, each computer sends this service one short row per working chat: the chat's title and its Project name (each cut to a few dozen characters), a status (working, needs approval, needs input, done or failed), a start time and a random route ID. Rows are kept only while the card needs them: working rows are deleted after 2 hours without a refresh, rows waiting on you after 24 hours and finished rows after 15 minutes. They are deleted at once when a computer stops reporting a working chat, when you turn Live Activities off, remove a computer or sign out. The service also keeps your phone's Live Activity push tokens and whether you dismissed the card.</li>
<li><strong>Reach from anywhere:</strong> your computer’s public key, a random public address, your account association and connection times. One-time approvals are stored as hashes and expire after ten minutes. Temporary cleanup records remain until external deletion succeeds.</li>
<li><strong>Abuse limits:</strong> counters keyed by hashes, removed after seven days without use.</li>
</ul>
<h2>What is not stored</h2>
<p>No conversation text, files, screenshots or local Mac addresses are stored by this service. The only chat and Project names it holds are the short-lived Live Activity rows described above. Notification previews are end-to-end encrypted between your Mac and your phone; this service forwards them without being able to read them and does not keep them. Live Activity updates carry the short titles, statuses, counts and times described above, and are rendered by iOS itself.</p>
<h2>Reach from anywhere</h2>
<p>This optional feature carries chats, approvals and requests through Cloudflare. During the owner-only preview, Cloudflare terminates HTTPS and could read this traffic. It does not have the end-to-end encryption used by notification previews. Screen sharing needs a direct connection and does not use this feature.</p>
<h2>Deleting your account</h2>
<p>In Cupola, open Settings, then Notifications, then Delete Account. Your account, identity hash, sessions, registrations and receipts are removed immediately. Signing out removes that phone's session and registrations and disables the account's remote access. Deletion of external resources retries if Cloudflare is unavailable.</p>
<h2>Other processors</h2>
<p>Cloudflare and Apple process network and delivery metadata under their own policies.</p>
</body></html>`;
