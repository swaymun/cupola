#!/usr/bin/env python3
"""Compare a helper against the same owned VM fixture and libwebrtc receiver.
This is VM evidence, not simulator/pairing acceptance. Never targets owner chats.
"""
import argparse
import json
import os
from pathlib import Path
import subprocess
import shutil
import threading
import time
import uuid
try:
    import psutil
except ImportError:
    psutil=None

class Peer:
    def __init__(self, command, receiver, out, generation=1, cookie=None, env=None):
        self.out=out;self.out.mkdir(parents=True,exist_ok=True)
        self.session={'sessionID':'parity-'+uuid.uuid4().hex[:12],'generation':generation}
        self.cookie=cookie or uuid.uuid4().hex;self.next_id=0
        self.condition=threading.Condition();self.write_lock=threading.Lock();self.receiver_lock=threading.Lock()
        self.answers={};self.events=[];self.stats=[];self.peer=None;self.pending=[];self.offered=False;self.errors=[]
        self.log=(out/'helper.jsonl').open('w',encoding='utf-8');self.stderr=(out/'helper.stderr.log').open('w',encoding='utf-8')
        self.helper=subprocess.Popen(command,stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=self.stderr,text=True,encoding="utf-8",bufsize=1,
            env={**os.environ,**(env or {}),'WONDER_COMPUTER_USE_HANDSHAKE':self.cookie,'WONDER_COMPUTER_STATS':'1'})
        self.receiver=subprocess.Popen([receiver],stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,encoding="utf-8",bufsize=1,
            env={**os.environ,'BAKEOFF_DUMP':str(out/'decoded.i420')})
        for target in [self.read_helper,self.read_receiver,self.read_stats]:threading.Thread(target=self.safe,args=(target,),daemon=True).start()
    def safe(self,target):
        try:target()
        except Exception as error:self.errors.append(type(error).__name__+': '+str(error))
    def send_receiver(self,value):
        with self.receiver_lock:self.receiver.stdin.write(json.dumps(value)+'\n');self.receiver.stdin.flush()
    def request(self,method,params=None,wait=True,allow_error=False):
        with self.write_lock:
            self.next_id+=1;rid=self.next_id
            self.helper.stdin.write(json.dumps({'id':rid,'method':method,'params':{**(params or {}),'handshake':self.cookie}})+'\n');self.helper.stdin.flush()
        if not wait:return
        with self.condition:
            if not self.condition.wait_for(lambda:rid in self.answers or self.helper.poll() is not None,timeout=15):raise RuntimeError('request timed out: '+method)
            reply=self.answers.pop(rid,None)
        if reply is None:raise RuntimeError('helper exited: '+method)
        if 'error'in reply:
            if allow_error:return {'accepted':False,'errorCode':reply['error'].get('code')}
            raise RuntimeError(method+': '+str(reply['error']))
        return reply['result']
    def read_helper(self):
        for line in self.helper.stdout:
            self.log.write(line);self.log.flush();message=json.loads(line)
            if 'id'in message:
                with self.condition:self.answers[message['id']]=message;self.condition.notify_all()
                continue
            self.events.append(message)
            if message.get('event')=='signal.peerReady':self.peer=message['peerRevision']
            elif message.get('event')=='signal.offer':
                self.send_receiver({'offer':message['sdp']});self.offered=True
                for candidate in self.pending:self.send_receiver(candidate)
                self.pending=[]
            elif message.get('event')=='signal.candidate':
                candidate={'candidate':{k:message[k] for k in ['candidate','sdpMid','sdpMLineIndex']}}
                if self.offered:self.send_receiver(candidate)
                else:self.pending.append(candidate)
    def read_receiver(self):
        for line in self.receiver.stdout:
            message=json.loads(line);binding={**self.session,'peerRevision':self.peer}
            if 'answer'in message:self.request('signal.answer',{**binding,'sdp':message['answer'],'type':'answer'},False)
            elif 'candidate'in message:self.request('signal.candidate',{**binding,**message['candidate']},False)
    def read_stats(self):
        with (self.out/'receiver.jsonl').open('w',encoding='utf-8') as log:
            for line in self.receiver.stderr:
                if line.startswith('BENCH '):
                    value=json.loads(line[6:]);self.stats.append(value);log.write(json.dumps(value)+'\n');log.flush()
    def close(self):
        # EOF owns whole-process shutdown. In particular it lets the Python
        # baseline's stdin reader finish before interpreter finalization.
        # capture.stop's protocol response is checked by the scenario itself.
        try:self.helper.stdin.close()
        except (OSError,ValueError):pass
        for process in [self.helper,self.receiver]:
            try:
                if process is self.receiver:self.send_receiver({'stop':True})
                process.wait(timeout=4)
            except Exception:process.kill();process.wait()
        self.log.close();self.stderr.close()

def wait_for(check,timeout=8):
    end=time.monotonic()+timeout
    while time.monotonic()<end:
        if check():return True
        time.sleep(.05)
    return False

def classify(previous,current):
    seconds=max(.001,current['t']-previous['t'])
    fps=max(0,current.get('frames_decoded',0)-previous.get('frames_decoded',0))/seconds
    dropped=max(0,current.get('frames_dropped',0)-previous.get('frames_dropped',0))
    lost=max(0,current.get('packets_lost',0)-previous.get('packets_lost',0))
    received=max(0,current.get('packets_received',0)-previous.get('packets_received',0))
    fraction=lost/max(1,lost+received)
    # Same thresholds as ComputerStreamQuality; RTT is reported separately by the app.
    return dict(seconds=seconds,fps=fps,dropped=dropped,lossFraction=fraction,
                choppy=fraction<=.08 and (fraction>.02 or (dropped>3 and fps<12)))

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--helper',required=True);parser.add_argument('--python')
    parser.add_argument('--receiver',required=True);parser.add_argument('--fixture-state',type=Path,required=True)
    parser.add_argument('--out',type=Path,required=True);parser.add_argument('--seconds',type=float,default=120)
    parser.add_argument('--quality',choices=['standard','auto','medium','high','benchmark30'],default='auto')
    parser.add_argument('--measure-only',action='store_true',help='Cadence benchmark without the control parity phase')
    parser.add_argument('--capture-source',choices=['window','display'],default='window')
    parser.add_argument('--helper-path',help='Isolated native runtime PATH containing only documented prerequisites')
    args=parser.parse_args();args.out.mkdir(parents=True,exist_ok=True)
    if args.capture_source=='display' and not args.measure_only:parser.error('Display cadence requires --measure-only; control targets the owned windows.')
    if (args.out/'summary.json').exists() or (args.out/'helper.jsonl').exists():raise SystemExit('Refusing to overwrite parity evidence')
    if args.helper_path:os.environ['PATH']=args.helper_path
    command=([args.python] if args.python else [])+[args.helper]
    summary={'surface':'VM helper + libwebrtc receiver','checks':{},'quality':args.quality,'captureSource':args.capture_source,'secondsRequested':args.seconds,'samples':[]}
    checks=summary['checks'];peer=None
    if args.helper_path:
        checks['python_absent_from_helper_path']=shutil.which('python')is None and shutil.which('python3')is None
        summary['helperPath']=args.helper_path
    def fixture():
        deadline=time.monotonic()+1
        while time.monotonic()<deadline:
            try:
                value=json.loads(args.fixture_state.read_text())
                if time.time()-value['time']>2:raise RuntimeError('fixture state exporter stalled')
                return value
            except (FileNotFoundError,PermissionError,json.JSONDecodeError):time.sleep(.01)
        raise RuntimeError('fixture state exporter unavailable')
    def require(name,condition):
        checks[name]=bool(condition)
        if not condition:raise RuntimeError('failed: '+name)
    try:
        token=uuid.uuid4().hex
        command_path=args.fixture_state.with_suffix('.command.json')
        command_path.write_text(json.dumps({'id':token,'action':'reset'}))
        require('fixture_reset',wait_for(lambda:fixture().get('commandID')==token and fixture().get('ready')))
        discovery=subprocess.run(command+['--discover-sources'],input='{"allWindows":true,"apps":[]}\n',text=True,encoding="utf-8",capture_output=True,timeout=15)
        inventory=json.loads(discovery.stdout.splitlines()[0]);summary['inventory']=inventory
        sources={name:next(s for s in inventory['sources'] if s.get('windowTitle')=='Computer parity '+name) for name in ['A','B']}
        require('window_menu',inventory['available'] and len(sources)==2)
        peer=Peer(command,args.receiver,args.out)
        start=time.monotonic();cap=peer.request('capabilities');summary['capabilities']=cap
        peer.request('capture.prepare',{**peer.session,'viewerCount':1,'videoQuality':args.quality})
        capture_id=('display:1' if os.name=='nt' else 'display:x11') if args.capture_source=='display' else sources['A']['id']
        picked=peer.request('capture.pick',{**peer.session,'sourceID':capture_id})
        require('window_pick',picked['accepted'])
        require('window_start',peer.request('capture.start',peer.session)['accepted'])
        require('live_view',wait_for(lambda:any(s.get('k')=='content_frame'for s in peer.stats),20))
        summary['firstDecodedSeconds']=time.monotonic()-start
        require('capture_live',wait_for(lambda:peer.request('capture.status')['state']=='capturing'))
        # Observe continued motion before input so the late-cadence window is independent.
        deadline=time.monotonic()+args.seconds
        proc=psutil.Process(peer.helper.pid) if psutil else None
        previous_cpu={}
        def totals(process):
            times=process.cpu_times();return times.user+times.system
        if proc:
            previous_cpu={p.pid:totals(p)for p in [proc]+proc.children(recursive=True)}
            psutil.cpu_percent()
        sampled=time.monotonic()
        while time.monotonic()<deadline:
            time.sleep(1)
            if proc:
                tree=[proc]+proc.children(recursive=True);now=time.monotonic()
                current={p.pid:totals(p)for p in tree}
                cpu=sum(max(0,value-previous_cpu.get(pid,value))for pid,value in current.items())/max(.001,now-sampled)*100
                previous_cpu=current;sampled=now
                capture=peer.request('capture.status')
                summary['samples'].append({'t':time.time(),'cpu':cpu,'rss':sum(p.memory_info().rss for p in tree),
                    'systemCpu':psutil.cpu_percent(),'captureSequence':capture['frameSequence']})
            if peer.errors:raise RuntimeError(str(peer.errors))
        status=peer.request('capture.status');require('constant_motion_still_live',status['state']=='capturing')
        if args.measure_only:
            summary['mode']='cadence benchmark only'
            return 0
        lease={**peer.session,'leaseID':'lease-'+uuid.uuid4().hex[:8],'requestID':'request-'+uuid.uuid4().hex[:8],
               'sourceID':sources['A']['id'],'geometryRevision':status['geometryRevision']}
        require('consent',peer.request('control.consent',lease)['granted'])
        require('activate',peer.request('control.activate',lease)['activated'])
        sequence=0
        def action(actions):
            nonlocal sequence
            sequence+=1
            result=peer.request('control.input',{**lease,'sequence':sequence,'actions':actions})
            require('input_'+str(sequence),result.get('accepted'));return result
        def heartbeat():require('heartbeat_'+str(sequence),peer.request('control.heartbeat',lease)['renewed'])
        def point(name,target):
            row=fixture()['windows'][name];ox,oy,w,h=row['bounds'];x,y=row['targets'][target]
            return (x-ox)/max(1,w-1),(y-oy)/max(1,h-1)
        def click(name,target):
            x,y=point(name,target);action([{'type':'pointer','x':x,'y':y,'phase':phase}for phase in ['down','up']])
        click('A','button');require('tap_reaches_A',wait_for(lambda:fixture()['windows']['A']['clicks']>=1))
        click('A','entry');action([{'type':'text','text':'parity'}]);require('text_reaches_A',wait_for(lambda:fixture()['windows']['A']['text']=='parity'))
        action([{'type':'key','key':key,'phase':'press'} for key in ['home','forwarddelete','end','left','backspace','right']])
        require('named_keys_edit_text',wait_for(lambda:fixture()['windows']['A']['text']=='ariy'))
        action([{'type':'key','key':'a','phase':'press','modifiers':2},{'type':'clipboard','operation':'pasteFromPhone','text':'clip✓'}])
        require('clipboard_paste',wait_for(lambda:fixture()['windows']['A']['text']=='clip✓'))
        action([{'type':'key','key':'a','phase':'press','modifiers':2},{'type':'key','key':'c','phase':'press','modifiers':2}])
        time.sleep(.2)
        result=action([{'type':'clipboard','operation':'copyToPhone'}]);require('clipboard_copy',result.get('clipboardText')=='clip✓')
        heartbeat()
        x,y=point('A','scroll');action([{'type':'pointer','x':x,'y':y,'phase':'move'},{'type':'scroll','deltaX':0,'deltaY':80}])
        require('scroll_reaches_A',wait_for(lambda:fixture()['windows']['A']['scroll']!=0))
        beforeA=fixture()['windows']['A']['text']
        rebound=peer.request('capture.rebind',{**lease,'nextSourceID':sources['B']['id']})
        require('same_lease_window_switch',rebound.get('accepted'))
        lease.update(sourceID=sources['B']['id'],geometryRevision=rebound['status']['geometryRevision'])
        heartbeat();time.sleep(.2);click('B','entry');action([{'type':'text','text':'window B'}])
        require('switch_input_reaches_B_only',wait_for(lambda:fixture()['windows']['B']['text']=='window B')and fixture()['windows']['A']['text']==beforeA)
        display='display:1' if os.name=='nt' else 'display:x11'
        rebound=peer.request('capture.rebind',{**lease,'nextSourceID':display});require('same_lease_whole_screen_switch',rebound.get('accepted'))
        lease.update(sourceID=display,geometryRevision=rebound['status']['geometryRevision']);heartbeat()
        require('whole_screen_live',wait_for(lambda:bool(peer.request('capture.status')['lastFrame'])))
        require('release',peer.request('control.release',lease)['released'])
        require('stop',peer.request('capture.stop',peer.session)['accepted'])
        # New peer and capture generation in the same helper process.
        peer.request('signal.close',{**peer.session,'peerRevision':peer.peer})
        peer.close();peer=None
        reconnect=Peer(command,args.receiver,args.out/'reconnect',generation=2)
        peer=reconnect
        peer.request('capture.prepare',{**peer.session,'viewerCount':1,'videoQuality':args.quality})
        peer.request('capture.pick',{**peer.session,'sourceID':display});peer.request('capture.start',peer.session)
        require('reconnect',wait_for(lambda:any(s.get('k')=='content_frame'for s in peer.stats),20))
        # Follow a moved exact window, then refuse a covered drag and a closed
        # source. These are independent of the helper's own unit-test mocks.
        status=peer.request('capture.status')
        rebound=peer.request('capture.rebind',{**peer.session,'sourceID':display,'geometryRevision':status['geometryRevision'],'nextSourceID':sources['A']['id']})
        require('reconnect_window',rebound.get('accepted'))
        lease={**peer.session,'leaseID':'guard-'+uuid.uuid4().hex[:8],'requestID':'guard-'+uuid.uuid4().hex[:8],
               'sourceID':sources['A']['id'],'geometryRevision':rebound['status']['geometryRevision']}
        require('guard_consent',peer.request('control.consent',lease)['granted'])
        require('guard_activate',peer.request('control.activate',lease)['activated']);sequence=0
        def fixture_command(action,wait=True):
            token=uuid.uuid4().hex;command_path.write_text(json.dumps({'id':token,'action':action}))
            if wait:require('fixture_'+action,wait_for(lambda:fixture().get('commandID')==token))
        before=fixture()['windows']['A']['clicks'];fixture_command('moveA')
        click('A','button');require('moved_window_input',wait_for(lambda:fixture()['windows']['A']['clicks']==before+1))
        x,y=point('A','scroll');action([{'type':'pointer','x':x,'y':y,'phase':'down'}])
        fixture_command('coverA');time.sleep(.2);sequence+=1
        refused=peer.request('control.input',{**lease,'sequence':sequence,'actions':[{'type':'pointer','x':x,'y':y,'phase':'move'}]},allow_error=True)
        require('covered_window_refused',not refused.get('accepted') and refused.get('errorCode') in ['window_covered','window_not_focused'])
        require('covered_capture_stays_exact',peer.request('capture.status')['sourceID']==sources['A']['id'])
        fixture_command('closeA',wait=False)
        removed=wait_for(lambda:peer.request('capture.status')['state']=='sourceRemoved',timeout=8 if not args.python else 2)
        summary['closedWindowStatus']=peer.request('capture.status')
        if not args.python:require('native_closed_window_removed',removed)
        rejected=peer.request('capture.pick',{**peer.session,'sourceID':sources['A']['id']})
        require('closed_window_cannot_be_picked',not rejected.get('accepted') and rejected.get('errorCode')=='source_not_found')
        require('closed_source_never_becomes_display',peer.request('capture.status')['sourceID']!=display)
    except Exception as error:
        summary['error']=type(error).__name__+': '+str(error)
    finally:
        if peer:
            peer.close()
        path=args.out/'receiver.jsonl'
        stats=[json.loads(line)for line in path.read_text().splitlines()] if path.exists() else []
        stats=[s for s in stats if s.get('k')=='rx_stats']
        # The app samples every two seconds; the drop threshold applies to that
        # whole interval, not each of this receiver's one-second log samples.
        quality_samples=stats[::2]
        intervals=[classify(a,b)for a,b in zip(quality_samples,quality_samples[1:])]
        summary['receiverIntervals']=intervals
        summary['choppyEstimateSeconds']=sum(i['seconds']for i in intervals if i['choppy'])
        elapsed=0;first_minute=0
        for interval in intervals:
            if interval['choppy']:first_minute+=max(0,min(interval['seconds'],60-elapsed))
            elapsed+=interval['seconds']
        summary['choppyEstimateFirstMinuteSeconds']=first_minute
        summary['choppyEstimateLimitation']='Local libwebrtc receiver thresholds; actual simulator Choppy must be measured separately.'
        summary['codec']=stats[-1].get('codec')if stats else None
        (args.out/'summary.json').write_text(json.dumps(summary,indent=2)+'\n')
        print(json.dumps({'passed':sum(checks.values()),'checks':len(checks),'error':summary.get('error'),'out':str(args.out)}))
    return 1 if summary.get('error')else 0
if __name__=='__main__':raise SystemExit(main())
