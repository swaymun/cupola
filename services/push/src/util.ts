// Shared request, encoding and quota helpers for the push and account routes.
export interface Env extends PushBindings {
 APNS_ENVIRONMENT: "production" | "sandbox";
 // Optional until a separately authorized relay deployment. This credential is
 // Worker-held, scoped to tunnel/DNS management, never a host process variable.
 CF_TUNNEL_TOKEN?:string;
 CF_ACCOUNT_ID?:string;
 CF_ZONE_ID?:string;
 RELAY_ZONE?:string;
 RELAY_ALLOWED_ACCOUNT_IDS?:string;
}
export const encoder = new TextEncoder();
export const uuid = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
export const secret = /^[A-Za-z0-9_-]{43}$/;
export const now = () => Math.floor(Date.now()/1000);
export const base64url = (bytes: Uint8Array) => btoa(String.fromCharCode(...bytes)).replaceAll("+","-").replaceAll("/","_").replace(/=+$/,"");
export const decode = (text: string) => Uint8Array.from(atob(text.replaceAll("-","+").replaceAll("_","/")), c=>c.charCodeAt(0));
export const random = () => base64url(crypto.getRandomValues(new Uint8Array(32)));
export async function hash(text: string) { return base64url(new Uint8Array(await crypto.subtle.digest("SHA-256", encoder.encode(text)))); }
export const json = (body: unknown, status=200) => Response.json(body,{status,headers:{"Cache-Control":"no-store"}});
export class Failure extends Error { constructor(readonly status:number, readonly code:string) {super(code);} }
export function requireValue(ok: unknown, code="invalid_request"): asserts ok { if(!ok)throw new Failure(400,code); }
export async function body(request:Request,max=4096):Promise<Record<string,unknown>> {
 if(!request.headers.get("Content-Type")?.startsWith("application/json"))throw new Failure(415,"json_required");
 // Bound the streamed body too; Content-Length is untrusted and optional.
 const reader=request.body?.getReader();if(!reader)throw new Failure(400,"empty_body");
 let total=0;const chunks:Uint8Array[]=[];
 try {for(;;){const {value,done}=await reader.read();if(done)break;total+=value.byteLength;if(total>max){await reader.cancel();throw new Failure(413,"body_too_large");}chunks.push(value);}}
 finally {reader.releaseLock();}
 const data=new Uint8Array(total);let offset=0;for(const c of chunks){data.set(c,offset);offset+=c.length;}
 try {const value=JSON.parse(new TextDecoder().decode(data));requireValue(value && typeof value==="object" && !Array.isArray(value));return value;}catch(error){if(error instanceof Failure)throw error;throw new Failure(400,"invalid_json");}
}
export async function limit(env:Env,key:string,seconds:number,max:number) {
 const current=now(),expires=(Math.floor(current/seconds)+1)*seconds;
 // Reuse one row per quota. Reset expired counters atomically, including when
 // requests arrive concurrently at different Cloudflare locations.
 const result=await env.DB.prepare("INSERT INTO limits(key,count,expires) VALUES(?,1,?) ON CONFLICT(key) DO UPDATE SET count=CASE WHEN expires<=? THEN 1 ELSE count+1 END,expires=excluded.expires WHERE expires<=? OR count<? RETURNING count").bind(key,expires,current,current,max).first();
 if(!result)throw new Failure(429,"rate_limited");
}
// P-256 public keys are raw X9.63 points (87 base64url characters) and
// signatures are raw r||s, matching CryptoKit's rawRepresentation.
export const isPublicKey = (value:unknown):value is string => typeof value==="string" && value.length===87;
export async function verify(key:string,payload:string,signature:unknown) {
 if(typeof signature!=="string" || signature.length>128)return false;
 try {const publicKey=await crypto.subtle.importKey("raw",decode(key),{name:"ECDSA",namedCurve:"P-256"},false,["verify"]);return await crypto.subtle.verify({name:"ECDSA",hash:"SHA-256"},publicKey,decode(signature),encoder.encode(payload));}catch{return false;}
}
