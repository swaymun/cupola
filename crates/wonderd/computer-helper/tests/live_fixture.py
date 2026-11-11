#!/usr/bin/env python3
"""Owned, normal (never floating) windows for native/Python parity on a VM."""
import argparse
import ctypes
import json
from pathlib import Path
import time
if hasattr(ctypes,'windll'):
    ctypes.windll.user32.SetProcessDpiAwarenessContext(ctypes.c_void_p(-4))
import tkinter as tk

parser=argparse.ArgumentParser()
parser.add_argument('--state',type=Path,required=True)
parser.add_argument('--size',default='520x360',help='First window capture size')
parser.add_argument('--text-only',action='store_true',help='Text editor surface for the simulator window-control scenario')
parser.add_argument('--verify-window-control',metavar='TOKEN',help='Check recorded first/second-window text, then exit')
args=parser.parse_args()
if args.verify_window_control:
    saved=json.loads(args.state.read_text())
    checks={f'typed_{name}':saved['windows'][name]['text']==args.verify_window_control+suffix
            for name,suffix in [('A','-first'),('B','-second')]}
    print(json.dumps(dict(checks=checks,passed=sum(checks.values()),total=len(checks))))
    raise SystemExit(0 if all(checks.values()) else 1)
roots=[]
state={'windows':{},'started':time.time()}

def bounds(root):
    if hasattr(ctypes,'windll'):
        class Rect(ctypes.Structure): _fields_=[('left',ctypes.c_long),('top',ctypes.c_long),('right',ctypes.c_long),('bottom',ctypes.c_long)]
        user=ctypes.windll.user32
        user.GetAncestor.restype=ctypes.c_void_p
        h=user.GetAncestor(ctypes.c_void_p(root.winfo_id()),2)
        r=Rect()
        if ctypes.windll.dwmapi.DwmGetWindowAttribute(ctypes.c_void_p(h),9,ctypes.byref(r),ctypes.sizeof(r))!=0:
            user.GetWindowRect(ctypes.c_void_p(h),ctypes.byref(r))
        return [r.left,r.top,r.right-r.left,r.bottom-r.top]
    return [root.winfo_rootx(),root.winfo_rooty(),root.winfo_width(),root.winfo_height()]

for index,name in enumerate(['A','B']):
    root=tk.Tk(className='WonderParity') if index==0 else tk.Toplevel(roots[0][0])
    root.title('Computer parity '+name)
    x=40+index*580 if root.winfo_screenwidth()>1150 else 30+index*40
    y=120 if root.winfo_screenheight()>720 else 30+index*40
    size=args.size if index==0 else '520x360'
    root.geometry(f'{size}+{x}+{y}')
    root.configure(bg='#172033')
    row={'title':root.title(),'clicks':0,'scroll':0,'keys':[],'mouse':[],'text':''}
    state['windows'][name]=row
    tk.Label(root,text='Computer parity '+name,font=('Arial',18),bg='#172033',fg='white').pack(pady=8)
    entry=tk.Text(root,font=('Arial',16),height=2,wrap='word') if hasattr(ctypes,'windll') or args.text_only else tk.Entry(root,font=('Arial',16))
    entry.pack(fill='both' if args.text_only else 'x',expand=args.text_only,padx=18)
    # Use the same normal text-field shortcuts on both platforms; Tk's X11
    # defaults include Emacs Ctrl+A and a paste that does not replace selection.
    def select_all(event,e=entry):
        if isinstance(e,tk.Text):e.tag_add('sel','1.0','end-1c');e.mark_set('insert','end-1c')
        else:e.selection_range(0,'end');e.icursor('end')
        return 'break'
    def copy(event,e=entry,r=root):
        if (e.tag_ranges('sel') if isinstance(e,tk.Text) else e.selection_present()):r.clipboard_clear();r.clipboard_append(e.selection_get())
        return 'break'
    def paste(event,e=entry,r=root):
        try:text=r.clipboard_get()
        except tk.TclError:return 'break'
        if (e.tag_ranges('sel') if isinstance(e,tk.Text) else e.selection_present()):e.delete('sel.first','sel.last')
        e.insert('insert',text);return 'break'
    entry.bind('<Control-a>',select_all);entry.bind('<Control-c>',copy);entry.bind('<Control-v>',paste)
    def click(r=row):r['clicks']+=1
    button=tk.Button(root,text='Count a click',font=('Arial',14),command=click)
    if not args.text_only:button.pack(pady=8)
    def mouse(event,r=row):r['mouse']=(r['mouse']+[{'type':str(event.type),'widget':str(event.widget),'x':event.x_root,'y':event.y_root,'time':time.time()}])[-20:]
    root.bind('<ButtonPress-1>',mouse,add='+');root.bind('<ButtonRelease-1>',mouse,add='+')
    canvas=tk.Canvas(root,bg='#273957',highlightthickness=0,**({'height':24} if args.text_only else {}))
    canvas.pack(fill='x' if args.text_only else 'both',expand=not args.text_only,padx=18,pady=8)
    def key(event,r=row):r['keys']=(r['keys']+[event.keysym])[-40:]
    # Widget bindings run before Text/Entry class bindings can consume Tab.
    for widget in (entry,button,canvas):widget.bind('<KeyPress>',key,add='+')
    def wheel(event,r=row):r['scroll']+=getattr(event,'delta',0) or (1 if event.num==4 else -1)
    root.bind('<KeyPress>',key,add='+')
    canvas.bind('<ButtonPress-1>',lambda event,e=entry:e.focus_set(),add='+')
    canvas.bind('<MouseWheel>',wheel)
    canvas.bind('<Button-4>',wheel);canvas.bind('<Button-5>',wheel)
    if args.text_only:
        entry.bind('<MouseWheel>',wheel,add='+')
        entry.bind('<Button-4>',wheel,add='+');entry.bind('<Button-5>',wheel,add='+')
    roots.append((root,row,entry,button,canvas))

frame=0

def tick():
    global frame
    frame+=1
    command_path=args.state.with_suffix('.command.json')
    try:
        command=json.loads(command_path.read_text())
        if command.get('id')!=state.get('commandID'):
            if command.get('action')=='reset':
                for root,row,entry,button,canvas in roots:
                    entry.delete('1.0' if isinstance(entry,tk.Text) else 0,'end');row.update(clicks=0,scroll=0,keys=[],mouse=[],text='')
            elif command.get('action')=='clipboard':
                roots[0][0].clipboard_clear();roots[0][0].clipboard_append(command['text'])
            elif command.get('action')=='moveA':
                root=roots[0][0];root.geometry(f'+{root.winfo_x()+40}+{root.winfo_y()+30}')
            elif command.get('action')=='place':
                for name,at in command['windows'].items():roots['AB'.index(name)][0].geometry(f"+{at[0]}+{at[1]}")
            elif command.get('action')=='coverA':
                a,b=roots[0][0],roots[1][0]
                b.geometry(f'{a.winfo_width()}x{a.winfo_height()}+{a.winfo_x()}+{a.winfo_y()}');b.lift()
            elif command.get('action')=='closeA':
                roots[0][0].destroy();return
            state['commandID']=command.get('id')
    except (FileNotFoundError,json.JSONDecodeError):pass
    for root,row,entry,button,canvas in roots:
        if not root.winfo_exists():continue
        row['text']=entry.get('1.0','end-1c') if isinstance(entry,tk.Text) else entry.get();row['bounds']=bounds(root)
        row['targets']={name:[widget.winfo_rootx()+widget.winfo_width()/2,widget.winfo_rooty()+widget.winfo_height()/2] for name,widget in [('entry',entry),('button',button),('scroll',canvas)]}
        canvas.delete('all')
        width,height=canvas.winfo_width(),canvas.winfo_height()
        for y in range(0,height,24):
            canvas.create_text(12,y,text=f'{frame:06d}  Wonder screen/control parity',fill='#c5e4ff',anchor='nw',font=('Courier',12))
        x=frame*7%max(1,width)
        canvas.create_rectangle(x,0,x+35,height,fill='#d39c51',outline='')
        canvas.create_text(width/2,height/2,text=f"Clicks {row['clicks']}  Scroll {row['scroll']}",fill='white',font=('Arial',16))
    state['ready']=all(root.winfo_viewable() and root.winfo_width()>=80 for root,*_ in roots)
    state['frame']=frame;state['time']=time.time()
    args.state.parent.mkdir(parents=True,exist_ok=True)
    temporary=args.state.with_suffix('.tmp')
    try:
        temporary.write_text(json.dumps(state));temporary.replace(args.state)
    except PermissionError:
        # Windows readers need not share delete access. Keep rendering and
        # retry the atomic export on the next frame; never stop the callback.
        state['exportRetries']=state.get('exportRetries',0)+1
    roots[0][0].after(33,tick)

tick()
roots[0][0].mainloop()
