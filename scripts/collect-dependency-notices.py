#!/usr/bin/env python3
"""Collect license text from the dependency sources used by the local build.
Fails when a locked registry dependency lacks a local manifest or license text.
Review the generated inventory before publishing; this does not grant licenses.
"""
import hashlib,json,sys,tomllib,re,subprocess
from pathlib import Path
root=Path(__file__).resolve().parents[1]
out=Path(sys.argv[1] if len(sys.argv)>1 else root/'.local/dependency-notices')
out.mkdir(parents=True,exist_ok=True)
registry=list((Path.home()/'.cargo/registry/src').glob('*'))
packages={}
for lock in [root/'Cargo.lock']:
 for p in tomllib.loads(lock.read_text())['package']:
  if p.get('source','').startswith('registry+'):
   packages[(p['name'],p['version'])]=p
selected=set()
# The shipped bundle holds wonderd and the `wonder` CLI (the experimental-headless feature).
for manifest, package in [('Cargo.toml','wonderd')]:
 command=['cargo','tree','--locked','--manifest-path',str(root/manifest),'--target','aarch64-apple-darwin','--features','experimental-headless','-e','normal','--prefix','none','--format','{p}']
 if package:command+=['-p',package]
 for line in subprocess.check_output(command,text=True).splitlines():
  match=re.match(r'([^ ]+) v([^ ]+)',line)
  if match:selected.add((match[1],match[2]))
packages={key:value for key,value in packages.items() if key in selected}
upstream=json.loads((root/'licenses/upstream/index.json').read_text())
records=[];missing=[];texts={}
for name,version in sorted(packages):
 source=next((r/f'{name}-{version}' for r in registry if (r/f'{name}-{version}/Cargo.toml').exists()),None)
 if source is None:
  missing.append(f'{name} {version}: source not cached');continue
 manifest=tomllib.loads((source/'Cargo.toml').read_text())['package']
 files=set()
 for p in source.iterdir():
  if p.is_file() and any(p.name.lower().startswith(x) for x in ['license','licence','copying','notice']):files.add(p)
 if manifest.get('license-file'):
  p=source/manifest['license-file']
  if p.is_file():files.add(p)
 hashes=[]
 for path in sorted(files):
  data=path.read_bytes();digest=hashlib.sha256(data).hexdigest();texts[digest]=data;hashes.append(digest)
 if not hashes:
  for entry in upstream.get(name+'@'+version,{}).get('licenses',[]):
   data=(root/'licenses/upstream'/entry['file']).read_bytes()
   digest=hashlib.sha256(data).hexdigest()
   if digest!=entry['sha256']:raise SystemExit('Upstream notice hash mismatch: '+name)
   texts[digest]=data;hashes.append(digest)
 if not hashes:missing.append(f'{name} {version}: no license text')
 records.append({'name':name,'version':version,'license':manifest.get('license','license-file'),'source':f'https://crates.io/crates/{name}/{version}','licenseTextHashes':hashes})
(out/'texts').mkdir(exist_ok=True)
for digest,data in texts.items():(out/'texts'/(digest+'.txt')).write_bytes(data)
(out/'inventory.json').write_text(json.dumps({'packages':records,'unresolved':missing},indent=2)+'\n')
with (out/'RUST-NOTICES.txt').open('w') as f:
 for p in records:
  f.write(f"\n{'='*60}\n{p['name']} {p['version']} — {p['license']}\n{p['source']}\n")
  f.write('License texts: '+', '.join(p['licenseTextHashes'])+'\n')
 for digest,data in sorted(texts.items()):
  f.write(f"\n{'='*60}\nLicense text SHA-256: {digest}\n")
  f.write(data.decode('utf-8',errors='replace')+'\n')
print(f'{len(records)} package manifests; {len(texts)} distinct notice texts; {len(missing)} unresolved entries.')
if missing:print('\n'.join(missing));sys.exit(1)
