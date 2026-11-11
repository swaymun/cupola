// Run explicitly against an installed CLI: node tests/provider-endpoints/codex.mjs /path/to/codex
// Uses only a temporary HOME and local HTTP stub, never a provider account.
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { spawn } from 'node:child_process';
import { once } from 'node:events';
import { mkdtemp, mkdir, writeFile, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join, resolve } from 'node:path';
const binary = process.argv[2];
if (!binary) throw new Error('Pass the Codex CLI path explicitly');
if (process.platform !== 'darwin') throw new Error('This native probe requires macOS sandbox-exec to enforce localhost-only traffic');
for (const source of ['config.toml']) {
  const root = await mkdtemp(join(tmpdir(), 'cupola-codex-endpoint-'));
  const home = join(root, '.codex');
  await mkdir(home);
  const reached = Promise.withResolvers();
  const server = createServer(async (request, response) => {
    let body = ''; for await (const chunk of request) body += chunk;
    response.writeHead(401, {'content-type':'application/json'});
    response.end(JSON.stringify({error:{message:'fixture endpoint reached',type:'authentication_error'}}));
    reached.resolve({path:request.url, headers:request.headers,body});
  });
  server.listen(0, '127.0.0.1'); await once(server, 'listening');
  const endpoint = `http://127.0.0.1:${server.address().port}/v1`;
  const config = `model = "fixture"\nmodel_provider = "fixture"\n[model_providers.fixture]\nname = "Fixture"\nbase_url = "${endpoint}"\nenv_key = "FIXTURE_API_KEY"\nwire_api = "responses"\n`;
  await writeFile(join(home,'config.toml'), config);
  const child = spawn('/usr/bin/sandbox-exec', ['-p', '(version 1)(allow default)(deny network-outbound)(allow network-outbound (remote ip "localhost:*"))', resolve(binary), 'app-server','--stdio'], { cwd:root, env:{HOME:root,PATH:process.env.PATH,CODEX_HOME:home,
    FIXTURE_API_KEY:'fixture-only'},stdio:['pipe','pipe','pipe']});
  let buffer='', stderr='', nextId=1; const pending=new Map();
  child.stderr.on('data', chunk=>{stderr=(stderr+chunk).slice(-2000)});
  child.stdout.on('data',chunk=>{
    buffer+=chunk; let i; while((i=buffer.indexOf('\n'))!==-1){
      const line=buffer.slice(0,i);buffer=buffer.slice(i+1); if(!line.trim())continue;
      const frame=JSON.parse(line); const waiter=pending.get(frame.id);
      if(waiter){pending.delete(frame.id);frame.error?waiter.reject(new Error(JSON.stringify(frame.error))):waiter.resolve(frame.result);}
    }
  });
  const call=(method,params)=>{const id=nextId++;const waiter=Promise.withResolvers();pending.set(id,waiter);child.stdin.write(JSON.stringify({id,method,params})+'\n');return waiter.promise;};
  const stopped=once(child,'exit');
  let timer;
  const timeout=new Promise((_,reject)=>{timer=setTimeout(()=>reject(new Error(`Codex endpoint deadline (${source}): ${stderr}`)),30000)});
  try {
    await Promise.race([(async()=>{
      await call('initialize',{clientInfo:{name:'cupola_endpoint_fixture',version:'1'},capabilities:{experimentalApi:true}});
      child.stdin.write(JSON.stringify({method:'initialized',params:{}})+'\n');
      const {thread}=await call('thread/start',{cwd:root,model:'fixture',approvalPolicy:'never',sandbox:'read-only',ephemeral:true});
      await call('turn/start',{threadId:thread.id,input:[{type:'text',text:'fixture endpoint probe',text_elements:[]}]});
      const request=await reached.promise;
      assert.ok(request.path.startsWith('/v1/responses'));
      assert.equal(request.headers.authorization,'Bearer fixture-only');
      console.log(JSON.stringify({source,result:'pass',endpoint:'local fixture',path:request.path}));
    })(),timeout]);
  } finally {
    clearTimeout(timer);child.kill('SIGKILL');await stopped;server.closeAllConnections();server.close();
    await rm(root,{recursive:true,force:true});
  }
}
