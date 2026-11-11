#!/usr/bin/env python3
"""Check all AX targets/text against the division; retain every violation.

The system keyboard is classified, never exempted. A keyboard crossing still
fails the whole-surface check, even when all application-owned targets pass.
"""
import json, sys
from pathlib import Path

def walk(value):
    if isinstance(value, list):
        for child in value: yield from walk(child)
    elif isinstance(value, dict):
        yield value
        yield from walk(value.get('children', []))

def intersects(a,b):
    def overlap(axis, length):
        if b[length] <= 0.5: # An inactive division may be a line, not a band.
            return a[axis] + 0.5 < b[axis] < a[axis] + a[length] - 0.5
        return min(a[axis] + a[length], b[axis] + b[length]) > max(a[axis], b[axis]) + 0.5
    return overlap('x', 'width') and overlap('y', 'height')

checks=[]
for state_file in sorted(Path(sys.argv[1]).rglob('*-state.json')):
    state=json.loads(state_file.read_text()); fold=state.get('displayDivision')
    if any(part.startswith('duo-unfolded') for part in state_file.parts):
        assert fold, f'Unfolded capture is missing the system division: {state_file}'
    if not fold: continue
    tree_file=state_file.with_name(state_file.name.replace('-state.json','.json'))
    nodes=list(walk(json.loads(tree_file.read_text())))
    keyboard_nodes={id(child) for node in nodes
                    if str(node.get('AXUniqueId', '')).startswith('UIKeyboardLayoutStar')
                    for child in walk(node)}
    targets=[n for n in nodes if n.get('role') in {'AXButton','AXStaticText','AXHeading','AXTextField','AXTextArea','AXLink','AXCheckBox','AXSlider','AXSwitch','AXPopUpButton','AXRadioButton','AXMenuItem','AXDisclosureTriangle'} and n.get('frame',{}).get('width',0)>0 and n['frame'].get('height',0)>0]
    # With Chats closed and no tool open the chat spans both displays: message text and
    # the composer's field may cross the hinge; only controls must stay off it.
    spans = tree_file.parent.name == 'chat-hidden' or tree_file.name.startswith('chat-hidden')
    content = {'AXStaticText','AXTextArea','AXTextField','AXHeading'} if spans else set()
    violations=[{'id':n.get('AXUniqueId'),'label':n.get('AXLabel'),'frame':n['frame'],
                 'owner':'systemKeyboard' if id(n) in keyboard_nodes else 'application'}
                for n in targets if n.get('role') not in content and intersects(n['frame'],fold)]
    assert targets, f'No native targets in {tree_file}'
    # A visible swap target belongs inside the tool, never in its gutter.
    panes=[n['frame'] for n in nodes if n.get('AXUniqueId')=='companion-pane']
    swaps=[n['frame'] for n in nodes if n.get('AXUniqueId')=='companion-pane-flip']
    for f in swaps:
        assert f['width'] >= 44 and f['height'] >= 44
        assert any(p['x'] <= f['x'] and f['x']+f['width'] <= p['x']+p['width']+1 and p['y'] <= f['y'] and f['y']+f['height'] <= p['y']+p['height']+1 for p in panes), 'Swap must belong to its tool pane'
    checks.append({'capture':str(tree_file),'targets':len(targets),'division':fold,
                   'violations':violations,'passed':not violations,
                   'applicationPassed':not any(v['owner']=='application' for v in violations)})
assert checks, 'No unfolded captures with measured system division'
passed=sum(c['passed'] for c in checks)
print(json.dumps({'passed':passed,'total':len(checks),
                  'applicationPassed':sum(c['applicationPassed'] for c in checks),
                  'captures':checks},indent=2))
sys.exit(0 if passed == len(checks) else 1)
