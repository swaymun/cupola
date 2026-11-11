#!/usr/bin/env python3
"""Record one reproducible VM run with fresh owned, non-floating fixture windows."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import time

HERE=Path(__file__).resolve().parent

def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--helper',required=True);parser.add_argument('--python')
    parser.add_argument('--receiver',required=True);parser.add_argument('--ffmpeg',required=True)
    parser.add_argument('--out',type=Path,required=True);parser.add_argument('--seconds',type=float,default=120)
    parser.add_argument('--quality',default='auto');parser.add_argument('--size',default='520x360')
    parser.add_argument('--measure-only',action='store_true')
    parser.add_argument('--capture-source',choices=['window','display'],default='window')
    parser.add_argument('--motion-video',type=Path,help='Linux-only owned full-screen motion fixture for display cadence')
    parser.add_argument('--display-size',default='1280x800');parser.add_argument('--helper-path')
    args=parser.parse_args();args.out.mkdir(parents=True,exist_ok=False)
    if args.motion_video and (os.name=='nt' or args.capture_source!='display' or not args.measure_only):parser.error('Motion fixture requires Linux display --measure-only')
    fixture=record=motion=None
    state=args.out/'fixture-state.json'
    start=time.time()
    with (args.out/'fixture.log').open('w')as fixture_log,(args.out/'record.log').open('w')as record_log:
        try:
            fixture=subprocess.Popen([sys.executable,str(HERE/'live_fixture.py'),'--state',str(state),'--size',args.size],stdout=fixture_log,stderr=subprocess.STDOUT)
            deadline=time.monotonic()+10
            while time.monotonic()<deadline:
                if fixture.poll()is not None:raise RuntimeError('fixture exited before mapping')
                try:
                    data=json.loads(state.read_text())
                    if data.get('ready')and data['started']>=start:break
                except (FileNotFoundError,json.JSONDecodeError):pass
                time.sleep(.05)
            else:raise RuntimeError('fixture did not map')
            if args.motion_video:
                ffplay=shutil.which('ffplay')
                if not ffplay:raise RuntimeError('ffplay is required only for this verification fixture')
                motion=subprocess.Popen([ffplay,'-hide_banner','-loglevel','error','-loop','0','-an','-sn','-window_title','Computer parity motion','-fs',str(args.motion_video.resolve())],stdout=fixture_log,stderr=subprocess.STDOUT)
                deadline=time.monotonic()+10
                while time.monotonic()<deadline:
                    if motion.poll()is not None:raise RuntimeError('motion fixture exited')
                    found=subprocess.run(['xdotool','search','--onlyvisible','--name','^Computer parity motion$'],capture_output=True)
                    if found.returncode==0:break
                    time.sleep(.05)
                else:raise RuntimeError('motion fixture did not map')
                (args.out/'motion-fixture.json').write_text(json.dumps({'sha256':hashlib.sha256(args.motion_video.read_bytes()).hexdigest(),'bytes':args.motion_video.stat().st_size,'mode':'normal full-screen ffplay on the owned Xvfb display'},indent=2))
            inputs=['-f','gdigrab','-framerate','15','-i','desktop']if os.name=='nt'else['-f','x11grab','-framerate','15','-video_size',args.display_size,'-i',os.environ['DISPLAY']]
            record=subprocess.Popen([args.ffmpeg,'-nostats','-loglevel','warning','-y',*inputs,'-c:v','libx264','-preset','ultrafast','-crf','25',str(args.out/'vm.mp4')],stdin=subprocess.PIPE,stdout=record_log,stderr=subprocess.STDOUT)
            command=[sys.executable,str(HERE/'parity.py'),'--helper',args.helper,'--receiver',args.receiver,'--fixture-state',str(state),'--out',str(args.out),'--seconds',str(args.seconds),'--quality',args.quality]
            command+=['--capture-source',args.capture_source]
            if args.measure_only:command+=['--measure-only']
            if args.python:command+=['--python',args.python]
            if args.helper_path:command+=['--helper-path',args.helper_path]
            (args.out/'command.json').write_text(json.dumps(command,indent=2)+'\n')
            with open(args.helper,'rb')as binary:sha=hashlib.file_digest(binary,'sha256').hexdigest()
            (args.out/'binary.json').write_text(json.dumps({'helper':args.helper,'sha256':sha,'size':Path(args.helper).stat().st_size,'runtime':'system Python'if args.python else'native','fixtureSha256':hashlib.sha256((HERE/'live_fixture.py').read_bytes()).hexdigest()},indent=2)+'\n')
            result=subprocess.run(command,timeout=args.seconds+120)
            return result.returncode
        finally:
            if record:
                try:record.stdin.write(b'q\n');record.stdin.flush();record.stdin.close();record.wait(timeout=10)
                except (BrokenPipeError,subprocess.TimeoutExpired):record.kill();record.wait()
            if motion:
                motion.terminate()
                try:motion.wait(timeout=5)
                except subprocess.TimeoutExpired:motion.kill();motion.wait()
            if fixture:
                fixture.terminate()
                try:fixture.wait(timeout=5)
                except subprocess.TimeoutExpired:fixture.kill();fixture.wait()

if __name__=='__main__':raise SystemExit(main())
