#!/usr/bin/env python3
"""Real Git + Zola process acceptance for live source generations (F03/F04).

Run with --build-eips /absolute/candidate/binary --evidence /new/scratch/path.
All source edits, Git operations and servers belong to disposable fixtures.
Servers bind loopback, are bounded, and are terminated in finally blocks.
"""
from pathlib import Path
import subprocess, tempfile, os, json, hashlib, time, signal, socket
from http.client import HTTPConnection
import argparse, sys
parser=argparse.ArgumentParser()
parser.add_argument('--build-eips',type=Path,required=True)
parser.add_argument('--evidence',type=Path,required=True)
args=parser.parse_args()
BIN=args.build_eips.resolve(strict=True)
AUDIT=args.evidence.resolve(); AUDIT.mkdir(parents=True,exist_ok=False)
ENV=os.environ.copy(); ENV['PATH']=str(BIN.parent)+os.pathsep+ENV['PATH']
RESULTS={}
def cmd(args,cwd=None,check=True,env=None,timeout=30):
 p=subprocess.run([str(a) for a in args],cwd=cwd,env=env or ENV,text=True,encoding='utf-8',errors='replace',stdout=subprocess.PIPE,stderr=subprocess.STDOUT,timeout=timeout,**({'creationflags':subprocess.CREATE_NO_WINDOW} if os.name=='nt' else {}))
 if check and p.returncode: raise RuntimeError(f'{args}: {p.returncode}\n{p.stdout}')
 return p

def write(p,s):
 p.parent.mkdir(parents=True,exist_ok=True); p.write_text(s,encoding='utf-8',newline='\n')
def git(p,*args): return cmd(['git','-C',p,*args]).stdout.strip()
def commit(p): git(p,'add','-A'); git(p,'commit','-qm','audit fixture')
def initgit(p):
 git(p,'init','-q','-b','master'); git(p,'config','user.email','audit@example.invalid'); git(p,'config','user.name','Audit Fixture'); commit(p)
def md(n,body='Original body',kind='Standards Track'):
 return f'---\neip: {n}\ntitle: Audit {n}\ndescription: Audit fixture {n}\nauthor: Audit\nstatus: Draft\ntype: {kind}\ncreated: 2020-01-01\n---\n\n{body}\n'
def complete_sources(repo):
 # Keep these fixtures semantically complete, as in live::fixture::complete_sources.
 # Registry changes must update the fixture, not disable derived-data checks.
 legacy=[606,608,607,609,1013,1679,2387]
 modern=[7773,8081,7569,7600,7607]
 empty=[779,1716,7568]
 members=[90099,2565,2929,2718,2930,1559,3198,3529,3541,3554,4345,5133,3675,4399,3651,3855,3860,4895,6049]
 for n in members: write(repo/'content'/f'{n:05}.md',md(n,'Fixture member'))
 for n in legacy+modern+empty:
  body='### Included EIPs\n\n* [EIP-90099](./90099.md)\n' if n in legacy else ('### Included EIPs\n' if n in modern else '')
  write(repo/'content'/f'{n:05}.md',md(n,body,'Meta'))

def manifest(repo,name,theme,pin,others=None):
 s=f'name = "{name}"\n[locations.{name}]\nrepository = "{repo.as_uri()}"\nbase-url = "https://example.invalid/manifest-base/"\n'
 for key,r in (others or {}).items(): s+=f'[locations.{key}]\nrepository = "{r.as_uri()}"\nbase-url = "https://example.invalid/{key}/"\n'
 return s+f'[theme]\nrepository = "{theme.as_uri()}"\ncommit = "{pin}"\n'
def fixture(label):
 w=Path(tempfile.mkdtemp(prefix=label+'-',dir=AUDIT)); r=w/'EIPs'; t=w/'theme'
 write(w/'.build-eips.toml','[server]\ninterface="127.0.0.1"\nport=1111\n')
 write(t/'theme.toml','name="audit"\ndescription="audit fixture"\n')
 write(t/'config/zola.toml','base_url="https://example.invalid/"\ntheme="eips-theme"\ncompile_sass=false\nbuild_search_index=false\ntaxonomies=[{name="status"},{name="type"},{name="category"}]\n')
 for name,body in [('index.html','<html>{{ section.content | safe }}</html>'),('section.html','<html>{{ section.content | safe }}</html>'),('page.html','<html>{{ page.content | safe }}</html>'),('eip.html','<html>{{ page.content | safe }}</html>'),('hardforks.html','<html>Hardfork fixture</html>'),('taxonomy_list.html','<html>Taxonomy fixture</html>'),('taxonomy_single.html','<html>Taxonomy term fixture</html>')]: write(t/'templates'/name,body)
 write(t/'config/eipw.toml','schema-version="0.10.0+793e8459b9ab9b4d955d2f98bf4283b94926f6a3e94d5462e392d98b67ac9729"\n[fetch]\nproposal-format="{:05}"\n'); initgit(t)
 write(r/'Build.toml',manifest(r,'EIPs',t,git(t,'rev-parse','HEAD')))
 write(r/'content/_index.md','+++\ntitle="Home"\n+++\n\nHome\n'); write(r/'content/00001.md',md(1)); complete_sources(r); initgit(r)
 return w,r,t

def runbuild(w,r,*flags,env=None):
 p=cmd([BIN,'-C',r,'--build-root',w/'generated','build',*flags],check=False,env=env)
 write(w/'build.log',p.stdout); return p

def save(key,data):
 RESULTS[key]=data; write(AUDIT/'probe-results.json',json.dumps(RESULTS,indent=2)); print(key,json.dumps(data),flush=True)

def freeport():
 s=socket.socket(); s.bind(('127.0.0.1',0)); port=s.getsockname()[1]; s.close(); return port

def http(port,path):
 c=HTTPConnection('127.0.0.1',port,timeout=2)
 try: c.request('GET',path); r=c.getresponse(); return r.status,r.read().decode(errors='replace')
 finally: c.close()

def startserver(w,r,operation='serve',flags=()):
 port=freeport(); f=(w/(operation+'.log')).open('w',encoding='utf-8')
 p=subprocess.Popen([str(BIN),'-C',str(r),'--build-root',str(w/'generated'),operation,'--interface','127.0.0.1','--port',str(port),*flags],stdout=f,stderr=subprocess.STDOUT,env=ENV,**({'creationflags':subprocess.CREATE_NO_WINDOW} if os.name=='nt' else {'start_new_session':True}))
 for _ in range(600):
  if p.poll() is not None: f.close(); raise RuntimeError((w/(operation+'.log')).read_text(encoding='utf-8',errors='replace'))
  try:
   http(port,'/'); return p,f,port
  except OSError: time.sleep(.1)
 stopserver(p,f); raise RuntimeError('server failed to become ready')
def stopserver(p,f):
 try:
  if p.poll() is None:
   if os.name == 'nt':
    # Only the process tree launched by this fixture; never match by name/port.
    subprocess.run(['taskkill.exe','/PID',str(p.pid),'/T','/F'],stdout=subprocess.PIPE,stderr=subprocess.STDOUT,check=False,timeout=15,creationflags=subprocess.CREATE_NO_WINDOW)
   else:
    try: os.killpg(p.pid,signal.SIGTERM)
    except ProcessLookupError: pass
   try: p.wait(timeout=5)
   except subprocess.TimeoutExpired:
    if os.name != 'nt': os.killpg(p.pid,signal.SIGKILL)
    else: p.kill()
    p.wait(timeout=5)
 finally: f.close()


p=sys.modules[__name__]
results = []

def record(case, w, **kw):
    results.append({'case': case, 'passed': True, 'fixture': str(w), **kw})
    (AUDIT / 'results.json').write_text(json.dumps(results, indent=2)+'\n')
    print('PASS', case, kw, flush=True)

def eventually(test, w, label, seconds=30):
    end=time.monotonic()+seconds
    while time.monotonic()<end:
        try:
            if test(): return
        except (OSError, ValueError): pass
        time.sleep(.12)
    raise AssertionError(label + '\n' + (w/'serve.log').read_text(encoding='utf-8',errors='replace'))

def content(w, path):
    f=w/'generated/repo/content'/path
    return f.read_text() if f.is_file() else ''

def live_case(w, r, case, action, flags=()):
    process, stream, port=p.startserver(w,r,flags=flags)
    try:
        action(w,r,port)
        assert process.poll() is None
        record(case,w,server_alive=True)
    finally: p.stopserver(process,stream)


def staging(w,r,port):
    p.write(r/'content/00002.md',p.md(2,'NEW STAGED'))
    time.sleep(.6);assert not (w/'generated/repo/content/00002.md').exists()
    p.git(r,'add','content/00002.md')
    index=Path(p.git(r,'rev-parse','--path-format=absolute','--git-path','index'));before=index.read_bytes()
    eventually(lambda:'NEW STAGED' in content(w,'00002.md'),w,'git-add not observed')
    assert index.read_bytes()==before,'source index changed by serve'
    eventually(lambda:'NEW STAGED' in p.http(port,'/2/')[1],w,'Zola did not render staged addition')
    p.git(r,'reset','-q','HEAD','--','content/00002.md')
    eventually(lambda:not (w/'generated/repo/content/00002.md').exists(),w,'unstaged addition stayed published')
    assert (r/'content/00002.md').exists()
w,r,t=p.fixture('watch-staging');live_case(w,r,'create-stage-unstage-without-unrelated-edit',staging)

def branches(w,r,port):
    for branch,text in [('other','OTHER BRANCH'),('master','Original body')]:
        p.git(r,'checkout','-q',branch)
        eventually(lambda:text in content(w,'00001.md'),w,'clean branch change not synchronized')
        eventually(lambda:text in p.http(port,'/1/')[1],w,'branch not rendered')
        assert not p.git(r,'status','--porcelain')
w,r,t=p.fixture('watch-clean-branch');p.git(r,'checkout','-qb','other');p.write(r/'content/00001.md',p.md(1,'OTHER BRANCH'));p.commit(r);p.git(r,'checkout','-q','master')
live_case(w,r,'clean-branch-switch-both-directions',branches)

def assets(w,r,port):
    p.write(r/'content/00001/assets/new.txt','NEW ASSET');p.git(r,'add','content/00001/assets/new.txt')
    p.write(r/'content/00001/index.md',p.md(1,'[asset](assets/new.txt)'))
    eventually(lambda:'assets/new.txt' in content(w,'00001/index.md') and content(w,'00001/index.md').startswith('+++'),w,'fresh asset inventory missing')
    eventually(lambda:p.http(port,'/1/assets/new.txt')==(200,'NEW ASSET'),w,'new asset not served')
    good=content(w,'00001/index.md')
    p.write(r/'content/00001/index.md',p.md(1,'[bad](assets/missing.txt)'))
    eventually(lambda:'last-good input retained' in (w/'serve.log').read_text(encoding='utf-8',errors='replace'),w,'invalid input not diagnosed')
    assert content(w,'00001/index.md')==good,'failed preprocessing changed last good input'
    p.write(r/'content/00001/index.md',p.md(1,'RECOVERED [asset](assets/new.txt)'))
    eventually(lambda:'RECOVERED' in content(w,'00001/index.md'),w,'failed generation did not recover')
    p.git(r,'mv','content/00001/assets/new.txt','content/00001/assets/renamed.txt')
    p.write(r/'content/00001/index.md',p.md(1,'RENAMED [asset](assets/renamed.txt)'))
    eventually(lambda:'RENAMED' in content(w,'00001/index.md') and not (w/'generated/repo/content/00001/assets/new.txt').exists(),w,'asset rename failed')
    assert not (w/'generated/repo/content/00002.md').exists(),'unselected proposal leaked'
    (r/'content/00001/index.md').unlink()
    eventually(lambda:not (w/'generated/repo/content/00001/index.md').exists(),w,'selected deletion failed')
    p.write(r/'content/00001/index.md',p.md(1,'RESTORED'))
    eventually(lambda:'RESTORED' in content(w,'00001/index.md'),w,'selected restoration failed')
w,r,t=p.fixture('watch-targeted-assets');(r/'content/00001.md').unlink();p.write(r/'content/00001/index.md',p.md(1));p.write(r/'content/00001/assets/old.txt','OLD');p.write(r/'content/00002.md',p.md(2,'OMITTED'));p.commit(r)
live_case(w,r,'target-assets-rename-failure-recovery-deletion',assets,flags=('--only','1'))

w,r,t=p.fixture('watch-linked-worktree');linked=w/'linked-source';p.git(r,'worktree','add','-b','linked',str(linked));r=linked
live_case(w,r,'linked-worktree-index-staging',staging)

# Local sibling commits are a fixed startup snapshot. Active edits must not
# silently override a conflicting sibling, and deletion must not erase it.
def sibling_collision(w,r,port):
    before=content(w,'00001.md')
    p.write(r/'content/00001.md',p.md(1,'CONFLICTING ACTIVE EDIT'))
    eventually(lambda:'conflicts with startup sibling snapshot' in (w/'serve.log').read_text(encoding='utf-8',errors='replace'),w,'sibling conflict not diagnosed')
    assert content(w,'00001.md')==before
    (r/'content/00001.md').unlink()
    time.sleep(.7)
    assert content(w,'00001.md')==before,'active deletion erased sibling contribution'
    p.write(r/'content/00003.md',p.md(3,'RECOVERY AFTER COLLISION'));p.git(r,'add','content/00003.md')
    eventually(lambda:'RECOVERY AFTER COLLISION' in content(w,'00003.md'),w,'sibling collision prevented recovery')
w,r,t=p.fixture('watch-sibling-collision');s=w/'ERCs'
p.write(s/'content/00001.md',(r/'content/00001.md').read_text());p.initgit(s)
p.write(r/'Build.toml',p.manifest(r,'EIPs',t,p.git(t,'rev-parse','HEAD'),{'ERCs':s}));p.commit(r)
live_case(w,r,'sibling-collision-retention-and-recovery',sibling_collision)

def manifest_change(w,r,port):
    before=content(w,'00001.md');manifest=(r/'Build.toml').read_text()
    p.write(r/'Build.toml',manifest+'\n# Changed source policy\n')
    eventually(lambda:'restart serve' in (w/'serve.log').read_text(encoding='utf-8',errors='replace'),w,'manifest change not diagnosed')
    assert content(w,'00001.md')==before
    p.write(r/'Build.toml',manifest)
    p.write(r/'content/00001.md',p.md(1,'AFTER MANIFEST RESTORE'))
    eventually(lambda:'AFTER MANIFEST RESTORE' in content(w,'00001.md'),w,'restored manifest did not recover')
w,r,t=p.fixture('watch-manifest');live_case(w,r,'manifest-restart-diagnostic-and-recovery',manifest_change)
