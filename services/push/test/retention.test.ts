import {env} from "cloudflare:workers";
import {applyD1Migrations} from "cloudflare:test";
import {expect,it} from "vitest";

it("sweeps a ten-million-per-month receipt window within D1's query limit",async()=>{
 await applyD1Migrations(env.DB,env.TEST_MIGRATIONS);
 // Seven days at 10M/month. No device tokens, network calls or APNs sends.
 const count=2_333_334,now=Math.floor(Date.now()/1000);
 try {
  for(let offset=0;offset<count;offset+=100_000){
   const length=Math.min(100_000,count-offset);
   await env.DB.prepare("WITH RECURSIVE n(i) AS (SELECT ? UNION ALL SELECT i+1 FROM n WHERE i<?) INSERT INTO deliveries SELECT printf('device-%08d',i/1000),printf('event-%012d',i),'opaque-route','completed','delivered',1,? FROM n").bind(offset,offset+length-1,now).run();
  }
  const result=await env.DB.prepare("DELETE FROM deliveries WHERE updated<?").bind(now-7*86400).run();
  expect(result.meta.rows_read).toBe(count);
  expect(result.meta.rows_written).toBe(0);
  expect(result.meta.duration).toBeLessThan(30_000);
  console.info("Local workerd receipt sweep",{rows:count,durationMs:result.meta.duration,bytes:result.meta.size_after});
 }finally{await env.DB.prepare("DELETE FROM deliveries").run();}
},60_000);
