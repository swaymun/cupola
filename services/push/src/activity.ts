// One Live Activity per phone, built here from every computer's chats. Each
// computer reports only its own rows (one per chat); this file keeps them per
// registration, rebuilds ONE combined state on every report and sends one
// APNs Live Activity push. iOS renders the content itself, so it is rebuilt
// from validated fields: short titles, fixed statuses, random routes.
import {type Env,Failure,hash,json,limit,now,requireValue,uuid} from "./util";

export const ROW_CAP=5, MAX_REPORT_ROWS=20, RUNNING_TTL=7200, WAITING_TTL=86400, FINISHED_TTL=900;
// A phone in front starts its own card; a computer's push-to-start waits this
// long for that, and the same chat work is never started twice within a minute and a half.
export const START_GRACE=10, START_RETRY=90;
// Running work refreshes its card this often so iOS never marks it stale.
export const HEARTBEAT=240, STALE=600;
const TITLE_CHARS=40, PROJECT_CHARS=24, PAYLOAD_BYTES=3800;
const statuses=new Set(["running","needsApproval","needsInput","done","failed","stopped"]);
const active=new Set(["running","needsApproval","needsInput"]);
const rank:Record<string,number>={needsApproval:0,needsInput:1,running:2,failed:3,done:4};
const timestamp=(value:unknown)=>typeof value==="number" && Number.isInteger(value) && value>=1_600_000_000 && value<=4_000_000_000;
const gone=["BadDeviceToken","DeviceTokenNotForTopic","Unregistered","ExpiredToken"];

export interface Row {registration_id:string;route_id:string;session_id:string;title:string;project:string;status:string;started:number;updated:number}
interface Card {session_id:string;update_token:string|null;start_token:string|null;dismissed:number;sent_hash:string|null;sent_at:number;start_sent_at:number;work_since:number;updated:number}
export interface Content {phase:string;running:number;needsInput:number;threads:{title:string;project:string;status:string;startedAt:number;routeId:string;registrationId:string}[];more?:number}
export type Push=(env:Env,token:string,payload:unknown,priority:"5"|"10")=>Promise<{status:number;reason:string}>;

// Titles come from the computer: flatten whitespace and controls, cut on a character.
export function clip(raw:string,limit:number) {
 const flat=[...raw.replace(/[\u0000-\u001f\u007f-\u009f]/g," ").split(/\s+/).filter(Boolean).join(" ")];
 return flat.length<=limit?flat.join(""):flat.slice(0,limit-1).join("").trimEnd()+"…";
}
const ttl=(status:string)=>status==="running"?RUNNING_TTL:active.has(status)?WAITING_TTL:FINISHED_TTL;
export const isLive=(row:Pick<Row,"status"|"updated">,t:number)=>row.updated+ttl(row.status)>t;

// The combined card: most urgent first, capped, with the others as "+N more".
// Nothing live is undefined; only finished rows is a terminal state, whose end
// push dismisses the card when the last finished row would have dropped.
export function aggregate(rows:Row[],t:number) {
 const live=rows.filter(row=>isLive(row,t)).sort((a,b)=>rank[a.status]-rank[b.status] || a.started-b.started || (a.route_id<b.route_id?-1:a.route_id>b.route_id?1:0));
 if(live.length===0)return undefined;
 const working=live.filter(row=>active.has(row.status)),waiting=working.filter(row=>row.status!=="running");
 const phase=working.length===0?(live.some(row=>row.status==="failed")?"failed":"done"):live[0].status;
 const state:Content={phase,running:working.length,needsInput:waiting.length,threads:live.slice(0,ROW_CAP).map(row=>({title:clip(row.title,TITLE_CHARS),project:clip(row.project,PROJECT_CHARS),status:row.status,startedAt:row.started,routeId:row.route_id,registrationId:row.registration_id}))};
 if(live.length>ROW_CAP)state.more=live.length-ROW_CAP;
 // ActivityKit allows 4 KB per push; drop rows rather than send one it rejects.
 while(state.threads.length>1 && new TextEncoder().encode(JSON.stringify({aps:{timestamp:4e9,event:"start","content-state":state,"stale-date":4e9,"attributes-type":"AgentActivityAttributes",attributes:{registrationId:"0".repeat(36)},alert:{title:"Cupola",body:"Agents are working on your computer."}}})).length>PAYLOAD_BYTES){state.threads.pop();state.more=live.length-state.threads.length;}
 return {state,working:working.length,until:Math.max(...live.filter(row=>!active.has(row.status)).map(row=>row.updated+FINISHED_TTL),0)};
}
export async function sessionRows(env:Env,sessionId:string) {
 // Rows of a registration that no longer exists (re-enrolled, revoked) never count.
 return (await env.DB.prepare("SELECT r.* FROM activity_rows r JOIN registrations g ON g.id=r.registration_id WHERE r.session_id=?").bind(sessionId).all<Row>()).results;
}

// Pushes the combined state to the phone's card. Dismissed cards get nothing:
// not an update, and not a new card from push-to-start. Returns when a
// computer should report again (a push-start waiting out the phone's own).
export async function syncCard(env:Env,sessionId:string,registrationId:string,push:Push):Promise<{revisit?:number}> {
 const card=await env.DB.prepare("SELECT * FROM activity_cards WHERE session_id=?").bind(sessionId).first<Card>();
 if(!card || card.dismissed)return {};
 const t=now(),combined=aggregate(await sessionRows(env,sessionId),t);
 const working=combined?.working??0;
 const work=working>0?(card.work_since||t):0;
 if(work!==card.work_since)await env.DB.prepare("UPDATE activity_cards SET work_since=? WHERE session_id=?").bind(work,sessionId).run();
 const wake=card.update_token??card.start_token;
 if(!wake)return {};
 if(card.update_token) {
  const token=card.update_token;
  if(!combined || working===0) {
   // Nothing running any more: the end is automatic and the token is spent.
   const state:Content=combined?.state??{phase:"done",running:0,needsInput:0,threads:[]};
   const result=await push(env,token,{aps:{timestamp:t,event:"end","content-state":state,"dismissal-date":combined?Math.max(combined.until,t+60):t}},"10");
   if(result.status!==200 && !(result.status===410 || gone.includes(result.reason)))throw new Failure(503,"delivery_unavailable");
   await env.DB.prepare("UPDATE activity_cards SET update_token=NULL,sent_hash=NULL,work_since=0 WHERE session_id=? AND update_token=?").bind(sessionId,token).run();
   return {};
  }
  const digest=await hash(JSON.stringify(combined!.state));
  // Claim the send so two computers reporting together push once.
  const claimed=await env.DB.prepare("UPDATE activity_cards SET sent_hash=?1,sent_at=?2 WHERE session_id=?3 AND update_token=?4 AND dismissed=0 AND (sent_hash IS NOT ?1 OR sent_at<=?5) RETURNING sent_at").bind(digest,t,sessionId,token,t-HEARTBEAT).first();
  if(!claimed)return {};
  const urgent=digest!==card.sent_hash && combined!.state.needsInput>0;
  const result=await push(env,token,{aps:{timestamp:t,event:"update","content-state":combined!.state,"stale-date":t+STALE}},urgent?"10":"5");
  if(result.status===200)return {};
  if(result.status===410 || gone.includes(result.reason)) {
   // iOS no longer has this card: the owner swiped it away, or the system ended it
   // (its 8 hour limit). Either way it stays gone until Cupola is opened again.
   await env.DB.prepare("UPDATE activity_cards SET update_token=NULL,dismissed=1,sent_hash=NULL,work_since=0 WHERE session_id=? AND update_token=?").bind(sessionId,token).run();
   return {};
  }
  await env.DB.prepare("UPDATE activity_cards SET sent_hash=?,sent_at=? WHERE session_id=? AND sent_hash=?").bind(card.sent_hash,card.sent_at,sessionId,digest).run();
  throw new Failure(503,"delivery_unavailable");
 }
 if(working===0 || !card.start_token)return {};
 const waited=t-work;
 if(waited<START_GRACE)return {revisit:START_GRACE-waited+1};
 const started=await env.DB.prepare("UPDATE activity_cards SET start_sent_at=?1 WHERE session_id=?2 AND start_token=?3 AND update_token IS NULL AND dismissed=0 AND start_sent_at<=?4 RETURNING start_sent_at").bind(t,sessionId,card.start_token,t-START_RETRY).first();
 if(!started)return {};
 const result=await push(env,card.start_token,{aps:{timestamp:t,event:"start","content-state":combined!.state,"stale-date":t+STALE,"attributes-type":"AgentActivityAttributes",attributes:{registrationId},alert:{title:"Cupola",body:"Agents are working on your computer."}}},"10");
 if(result.status===200)return {};
 if(result.status===410 || gone.includes(result.reason)) {
  await env.DB.prepare("UPDATE activity_cards SET start_token=NULL WHERE session_id=? AND start_token=?").bind(sessionId,card.start_token).run();
  return {};
 }
 await env.DB.prepare("UPDATE activity_cards SET start_sent_at=0 WHERE session_id=?").bind(sessionId).run();
 throw new Failure(503,"delivery_unavailable");
}

interface Registration {id:string;session_id:string}
export async function handleActivity(env:Env,action:string,registration:Registration,data:Record<string,unknown>,push:Push):Promise<Response> {
 const sessionId=registration.session_id,id=registration.id,t=now();
 if(action==="activity") {
  // A computer's own chats, in full: rows it leaves out that were working are gone;
  // rows it reports as finished stay for the finished window.
  const {rows,...extra}=data;
  requireValue(Object.keys(extra).length===0 && Array.isArray(rows) && rows.length<=MAX_REPORT_ROWS,"invalid_rows");
  const parsed=(rows as unknown[]).map(value=>{
   requireValue(value && typeof value==="object" && !Array.isArray(value),"invalid_rows");
   const {routeId,title,project,status,startedAt,...more}=value as Record<string,unknown>;
   requireValue(Object.keys(more).length===0 && typeof routeId==="string" && uuid.test(routeId) && typeof title==="string" && title.length<=400 && typeof project==="string" && project.length<=400 && typeof status==="string" && statuses.has(status) && timestamp(startedAt),"invalid_rows");
   return {routeId,title:clip(title,TITLE_CHARS),project:clip(project,PROJECT_CHARS),status:status==="stopped"?"done":status,startedAt:startedAt as number};
  });
  requireValue(new Set(parsed.map(row=>row.routeId)).size===parsed.length,"invalid_rows");
  await limit(env,`activity:${id}`,3600,180);await limit(env,"activity-global",3600,60000);
  // Nothing is stored for a phone that has not asked for a card (or turned it off).
  const card=await env.DB.prepare("SELECT 1 AS present FROM activity_cards WHERE session_id=?").bind(sessionId).first();
  if(!card)return json({stored:false});
  const keep=parsed.map(()=>"?").join(",");
  // The card can be turned off between that read and these writes, so every
  // insert checks for the card again inside its own statement: turning off
  // deletes the card and the rows together, and nothing private lands after it.
  await env.DB.batch([
   env.DB.prepare(`DELETE FROM activity_rows WHERE registration_id=? AND status IN ('running','needsApproval','needsInput')${parsed.length?` AND route_id NOT IN (${keep})`:""}`).bind(id,...parsed.map(row=>row.routeId)),
   // A finished row keeps the time it finished, however often it is reported again.
   ...parsed.map(row=>env.DB.prepare("INSERT INTO activity_rows(registration_id,route_id,session_id,title,project,status,started,updated) SELECT ?1,?2,?3,?4,?5,?6,?7,?8 WHERE EXISTS(SELECT 1 FROM activity_cards WHERE session_id=?3) ON CONFLICT(registration_id,route_id) DO UPDATE SET session_id=excluded.session_id,title=excluded.title,project=excluded.project,status=excluded.status,started=excluded.started,updated=CASE WHEN excluded.status IN ('done','failed') AND activity_rows.status IN ('done','failed') THEN activity_rows.updated ELSE excluded.updated END").bind(id,row.routeId,sessionId,row.title,row.project,row.status,row.startedAt,t)),
  ]);
  const stored=await env.DB.prepare("SELECT 1 AS present FROM activity_cards WHERE session_id=?").bind(sessionId).first();
  if(!stored)return json({stored:false});
  return json({stored:true,...await syncCard(env,sessionId,id,push)});
 }
 // The phone's own word about its card, over the sender capability it holds.
 const {kind,token,...extra}=data;
 requireValue(Object.keys(extra).length===0 && typeof kind==="string" && ["update","start","ended","off","dismissed","foreground"].includes(kind) && (kind==="update" || kind==="start" || kind==="ended"?typeof token==="string" && /^[0-9a-f]{32,512}$/.test(token):token===undefined),"invalid_request");
 await limit(env,`activity-state:${id}`,3600,240);
 if(kind==="off") {
  await env.DB.batch([env.DB.prepare("DELETE FROM activity_rows WHERE session_id=?").bind(sessionId),env.DB.prepare("DELETE FROM activity_cards WHERE session_id=?").bind(sessionId)]);
  return json({registered:false});
 }
 // Only a token registration creates the card record (and so starts storing
 // titles); ended, dismissed and foreground only touch a record that exists.
 if(kind==="start" || kind==="update")await env.DB.prepare("INSERT INTO activity_cards(session_id,updated) VALUES(?,?) ON CONFLICT(session_id) DO UPDATE SET updated=excluded.updated").bind(sessionId,t).run();
 if(kind==="start") {
  await env.DB.prepare("UPDATE activity_cards SET start_token=? WHERE session_id=?").bind(token,sessionId).run();
  return json({registered:true});
 }
 if(kind==="ended") {
  await env.DB.prepare("UPDATE activity_cards SET update_token=NULL,sent_hash=NULL WHERE session_id=? AND update_token=?").bind(sessionId,token).run();
  return json({registered:false});
 }
 if(kind==="dismissed") {
  // The owner swiped the card away: nothing pushes to it, or starts one, until they return.
  await env.DB.prepare("UPDATE activity_cards SET update_token=NULL,dismissed=1,sent_hash=NULL,work_since=0 WHERE session_id=?").bind(sessionId).run();
  return json({registered:false,dismissed:true});
 }
 if(kind==="foreground") {
  await env.DB.prepare("UPDATE activity_cards SET dismissed=0 WHERE session_id=?").bind(sessionId).run();
  return json({dismissed:false});
 }
 const card=await env.DB.prepare("SELECT dismissed FROM activity_cards WHERE session_id=?").bind(sessionId).first<{dismissed:number}>();
 if(card?.dismissed)return json({registered:false,dismissed:true});
 // The card exists on the phone now: it shows the combined state at once.
 const combined=aggregate(await sessionRows(env,sessionId),t);
 const digest=combined?await hash(JSON.stringify(combined.state)):null;
 await env.DB.prepare("UPDATE activity_cards SET update_token=?,sent_hash=?,sent_at=? WHERE session_id=? AND dismissed=0").bind(token,combined?.working?digest:null,t,sessionId).run();
 return json({registered:true,...(combined?.working?{state:combined.state}:{})});
}

// A card whose computers went quiet still ends or drops expired rows: rows
// expire by time, and nothing reports when a computer is off.
export const pruneRows=(env:Env,t:number)=>env.DB.prepare("DELETE FROM activity_rows WHERE (status='running' AND updated<=?1) OR (status IN ('needsApproval','needsInput') AND updated<=?2) OR (status IN ('done','failed') AND updated<=?3) OR registration_id NOT IN (SELECT id FROM registrations)").bind(t-RUNNING_TTL,t-WAITING_TTL,t-FINISHED_TTL);
export async function reapActivity(env:Env,push:Push) {
 // Titles leave the database when their window closes, not when the card next changes.
 await pruneRows(env,now()).run();
 const due=await env.DB.prepare("SELECT session_id FROM activity_cards WHERE update_token IS NOT NULL AND dismissed=0 AND sent_at<=? LIMIT 50").bind(now()-HEARTBEAT).all<{session_id:string}>();
 for(const {session_id} of due.results)await syncCard(env,session_id,"",push).catch(()=>{/* The next run retries. */});
}
