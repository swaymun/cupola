#!/usr/bin/env python3
"""Drive the Linux helper's control.input on the desktop display; no WebRTC peer.
Usage: DISPLAY=:0 input_probe.py HELPER OUTDIR. Prints JSON; exit 1 on failure."""
import json, os, subprocess, sys, time, uuid
helper, out = sys.argv[1], sys.argv[2]
cookie = uuid.uuid4().hex
p = subprocess.Popen([helper], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True, bufsize=1,
                     env={**os.environ, 'WONDER_COMPUTER_USE_HANDSHAKE': cookie})
n = 0
def req(method, params=None):
    global n; n += 1
    p.stdin.write(json.dumps({'id': n, 'method': method, 'params': {**(params or {}), 'handshake': cookie}}) + '\n'); p.stdin.flush()
    while True:
        r = json.loads(p.stdout.readline())
        if r.get('id') == n: return r.get('result', r)
def sh(c): return subprocess.run(c, shell=True, capture_output=True, text=True).stdout.strip()
sess = {'sessionID': 'probe-' + uuid.uuid4().hex[:8], 'generation': 1}
req('capture.prepare', {**sess, 'viewerCount': 1, 'videoQuality': 'auto'})
r = {'pick': req('capture.pick', {**sess, 'sourceID': 'display:x11'})}
r['start'] = req('capture.start', sess)
time.sleep(1.5)
st = req('capture.status'); r['state'] = st.get('state')
lease = {**sess, 'leaseID': 'l-' + uuid.uuid4().hex[:6], 'requestID': 'r-' + uuid.uuid4().hex[:6],
         'sourceID': 'display:x11', 'geometryRevision': st['geometryRevision']}
r['consent'] = req('control.consent', lease); r['activate'] = req('control.activate', lease)
seq = 0
def act(a):
    global seq; seq += 1
    return req('control.input', {**lease, 'sequence': seq, 'actions': a})
import re
typed = '/tmp/probe_typed.txt'
text = 'hello from control.input'
sh(f'rm -f {typed}; (GTK_IM_MODULE=simple zenity --entry --title=probe-target --text="linux-cu probe" --width=500 >{typed} 2>/dev/null &)')
wid = ''
for _ in range(50):
    wid = sh('xdotool search --name probe-target | head -1')
    if wid: break
    time.sleep(.2)
time.sleep(1)
g = dict(l.split('=') for l in sh(f'xdotool getwindowgeometry --shell {wid}').split())
sw, sh_ = map(int, re.search(r'(\d+)x(\d+) pixels', sh('xdpyinfo | grep dimensions')).groups())
cx, cy = int(g['X']) + int(g['WIDTH']) // 2, int(g['Y']) + int(g['HEIGHT']) // 2
nx, ny = cx / sw, cy / sh_
r['window'] = {'id': wid, **g, 'screen': [sw, sh_], 'clickPx': [cx, cy], 'clickNorm': [round(nx, 4), round(ny, 4)]}
sh('xdotool mousemove 100 100'); time.sleep(.3)
r['mouseBefore'] = sh('xdotool getmouselocation')
sh(f'scrot -o {out}/before.png')
r['click'] = act([{'type': 'pointer', 'x': nx, 'y': ny, 'phase': 'move'}, {'type': 'pointer', 'x': nx, 'y': ny, 'phase': 'down'}, {'type': 'pointer', 'x': nx, 'y': ny, 'phase': 'up'}])
time.sleep(.4)
r['mouseAfterClick'] = sh('xdotool getmouselocation')
r['type'] = act([{'type': 'text', 'text': text}]); time.sleep(.6)
sh(f'scrot -o {out}/after.png')
r['enter'] = act([{'type': 'key', 'key': 'Return', 'phase': 'press'}]); time.sleep(.6)
r['typedFile'] = open(typed).read().strip() if os.path.exists(typed) else None
req('control.release', lease); req('capture.stop', sess); p.stdin.close()
m = re.search(r'x:(\d+) y:(\d+)', r['mouseAfterClick'])
ok = (m and abs(int(m[1]) - cx) <= 2 and abs(int(m[2]) - cy) <= 2 and r['typedFile'] == text
      and r['click'].get('accepted') and r['type'].get('accepted'))
r['pass'] = bool(ok)
print(json.dumps(r, indent=1))
sys.exit(0 if ok else 1)
