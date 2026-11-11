#!/usr/bin/env python3
"""One contact sheet per screen, with unchanged native captures in labeled cells."""
from pathlib import Path
from PIL import Image, ImageDraw, ImageFont, ImageOps
import argparse
import hashlib
import json
import shutil
p = argparse.ArgumentParser()
p.add_argument('--evidence', type=Path, required=True)
p.add_argument('--output', type=Path, required=True)
p.add_argument('--after-build-info', type=Path, help='Require every after capture to match this final build')
a = p.parse_args()
root=a.evidence
a.output.mkdir(parents=True, exist_ok=True)
manifest=[]
expected_after = json.loads(a.after_build_info.read_text()) if a.after_build_info else None
groups={s:[s] for s in ['chat','chats','files','terminal','computer','settings','settings-voice']}
groups['chat']=['chat','chat-hidden']
for s in ['details','pull-requests','reply-changes','agent-task']: groups[s]=[s,s+'-expanded']
# Validate the complete matrix before replacing any existing contact sheet.
for states in groups.values():
    for state in states:
        for device in ['iphone','ipad','duo-folded','duo-unfolded']:
            if device == 'duo-folded' and state.endswith('-expanded'): continue
            for theme in ['light','dark']:
                for phase in ['before','after']:
                    capture=root/phase/device/theme/(state+'.png')
                    metadata=capture.with_name(state+'-build.json')
                    if not metadata.exists(): metadata=capture.parent/'build.json'
                    assert capture.exists() and metadata.exists(), f'Missing capture or provenance: {capture}'
                    build=json.loads(metadata.read_text())
                    assert build.get('sourceHead') and build.get('sha256'), f'Incomplete provenance: {metadata}'
                    if phase == 'after' and expected_after:
                        assert all(build.get(k) == expected_after.get(k) for k in ('sourceHead', 'sha256')), f'Stale after capture; recapture with the final build: {capture}'
                    manifest.append({'capture':str(capture.relative_to(root)), 'sha256':hashlib.sha256(capture.read_bytes()).hexdigest(), 'build':build})
font=ImageFont.truetype('/System/Library/Fonts/Supplemental/Arial.ttf',24)
for screen,states in groups.items():
    sheet=Image.new('RGB',(1600*len(states),8*535+60),'#eeeeee'); draw=ImageDraw.Draw(sheet)
    draw.text((24,15),screen+' — native simulator captures, before / after'+(' (alternate states included)' if len(states)>1 else ''),font=font,fill='black')
    for row,(device,theme) in enumerate((d,t) for d in ['iphone','ipad','duo-folded','duo-unfolded'] for t in ['light','dark']):
        for mode,state in enumerate(states):
            for col,phase in enumerate(['before','after']):
                x,y=(mode*2+col)*800,row*535+60
                draw.text((x+15,y+10),f'{device} · {theme} · {phase} · '+state,font=font,fill='black')
                file=root/phase/device/theme/(state+'.png')
                if file.exists():
                    img=ImageOps.contain(Image.open(file).convert('RGB'),(776,456))
                    sheet.paste(img,(x+(800-img.width)//2,y+47))
                    metadata=file.with_name(state+'-build.json')
                    if not metadata.exists(): metadata=file.parent/'build.json'
                    build=json.loads(metadata.read_text())
                    draw.text((x+15,y+507),f"source {build['sourceHead'][:9]} / binary {build['sha256'][:12]}",font=font,fill='#555555')
                elif device == 'duo-folded' and mode:
                    draw.text((x+25,y+100),'N/A: folded uses the phone presentation',font=font,fill='#555555')
                else: draw.text((x+25,y+100),'NOT CAPTURED',font=font,fill='#aa0000')
    out=a.output;out.mkdir(parents=True, exist_ok=True)
    sheet.save(out/(screen+'.jpg'),quality=90)

(a.output.parent/'capture-manifest.json').write_text(json.dumps(manifest,indent=2)+'\n')
# Preserve full-resolution hinge evidence, without committing every AX dump.
for entry in manifest:
    relative=Path(entry['capture'])
    if relative.parts[1] == 'duo-unfolded' and relative.stem in {'chat','chat-hidden','chats','settings','settings-voice','details','files','terminal'}:
        target=a.output.parent/'captures'/relative
        target.parent.mkdir(parents=True,exist_ok=True)
        shutil.copy2(root/relative,target)
print(f'Packaged {len(manifest)} captures in {len(groups)} contact sheets with source and binary provenance.')
