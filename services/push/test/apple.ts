// A fake Sign in with Apple: an RSA key published as a JWKS, and identity
// tokens signed with it. Tests route https://appleid.apple.com/auth/keys here.
import {base64url} from "../src/index";
const encoder=new TextEncoder();
export const APPLE_KEYS_URL="https://appleid.apple.com/auth/keys";
export async function fakeApple(kid="test-kid") {
 const pair=await crypto.subtle.generateKey({name:"RSASSA-PKCS1-v1_5",modulusLength:2048,publicExponent:new Uint8Array([1,0,1]),hash:"SHA-256"},true,["sign","verify"]) as CryptoKeyPair;
 const jwk=await crypto.subtle.exportKey("jwk",pair.publicKey) as JsonWebKey;
 const jwks={keys:[{kty:"RSA",kid,use:"sig",alg:"RS256",n:jwk.n,e:jwk.e}]};
 async function token(claims:Record<string,unknown>,header:Record<string,unknown>={alg:"RS256",kid}) {
  const head=base64url(encoder.encode(JSON.stringify(header))),body=base64url(encoder.encode(JSON.stringify(claims)));
  const signature=await crypto.subtle.sign("RSASSA-PKCS1-v1_5",pair.privateKey,encoder.encode(`${head}.${body}`));
  return `${head}.${body}.${base64url(new Uint8Array(signature))}`;
 }
 return {jwks,token};
}
export const nonceHash=async(nonce:string)=>[...new Uint8Array(await crypto.subtle.digest("SHA-256",encoder.encode(nonce)))].map(b=>b.toString(16).padStart(2,"0")).join("");
export async function deviceKey() {
 const pair=await crypto.subtle.generateKey({name:"ECDSA",namedCurve:"P-256"},true,["sign","verify"]) as CryptoKeyPair;
 const publicKey=base64url(new Uint8Array(await crypto.subtle.exportKey("raw",pair.publicKey) as ArrayBuffer));
 const sign=async(text:string)=>base64url(new Uint8Array(await crypto.subtle.sign({name:"ECDSA",hash:"SHA-256"},pair.privateKey,encoder.encode(text))));
 return {publicKey,sign};
}
export type Device=Awaited<ReturnType<typeof deviceKey>>;
export interface Tokens {sessionId:string;accessToken:string;refreshToken:string;accessExpiresIn:number;refreshExpiresIn:number;created:boolean}
// Signs in through the real /v1/auth/apple handler with a fresh nonce.
export async function signIn(post:(path:string,value:unknown)=>Promise<Response>,apple:Awaited<ReturnType<typeof fakeApple>>,device:Device,sub="001234.apple-user",overrides:Record<string,unknown>={}) {
 const nonce=base64url(crypto.getRandomValues(new Uint8Array(32)));
 const at=Math.floor(Date.now()/1000);
 const identityToken=await apple.token({iss:"https://appleid.apple.com",aud:"com.swaymun.wonder",sub,iat:at,exp:at+600,nonce:await nonceHash(nonce),...overrides});
 return post("/v1/auth/apple",{identityToken,nonce,deviceKey:device.publicKey,signature:await device.sign(`signin|apple|${nonce}`)});
}
