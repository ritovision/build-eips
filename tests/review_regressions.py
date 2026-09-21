#!/usr/bin/env python3
"""R01–R06 acceptance: real Git, renderer and HTTP in disposable fixtures only.

Uses the same local fixture/process definitions as serve_generation.py, not its
cases. No source checkout or frozen audit artifact is mutated. Requires POSIX.
"""
from pathlib import Path
import argparse
import ast
import hashlib
import json
import os
import time
import traceback

harness = Path(__file__).with_name('serve_generation.py')
functions = {'cmd','write','git','commit','initgit','md','manifest','fixture','runbuild','freeport','http','startserver','stopserver'}
tree = ast.parse(harness.read_text())
selected = [n for n in tree.body if isinstance(n, (ast.Import,ast.ImportFrom)) or isinstance(n,ast.FunctionDef) and n.name in functions]
exec(compile(ast.Module(body=selected,type_ignores=[]),str(harness),'exec'),globals())


def eventually(test, w, description, seconds=12):
    end = time.monotonic()+seconds
    while time.monotonic()<end:
        try:
            if test(): return
        except (OSError, ValueError): pass
        time.sleep(.12)
    raise AssertionError(description+'\n'+(w/'serve.log').read_text())


def record(case, w, **extra):
    RESULTS.append(dict(case=case,passed=True,fixture=str(w),**extra))
    (AUDIT/'results.json').write_text(json.dumps(RESULTS,indent=2)+'\n')
    print('PASS',case,extra,flush=True)


def r01():
    for reuse in (False,True):
        w,r,t=fixture('git-effective-root')
        if reuse: assert runbuild(w,r).returncode==0
        outside=w/'external'; write(outside/'keep.txt','SENTINEL')
        generated=w/'generated/repo';generated.mkdir(parents=True,exist_ok=True)
        if not reuse: git(generated,'init','-q','-b','master')
        git(generated,'config','core.worktree',str(outside))
        config=(generated/'.git/config').read_bytes()
        result=runbuild(w,r)
        assert result.returncode!=0 and 'effective workdir' in result.stdout,result.stdout
        assert (outside/'keep.txt').read_text()=='SENTINEL'
        assert not (outside/'content').exists()
        assert (generated/'.git/config').read_bytes()==config,'rejected metadata was mutated'
        record('R01-redirected-'+('reuse' if reuse else 'empty'),w,exit=result.returncode)
    w,r,t=fixture('git-linked-source'); linked=w/'linked-source'
    git(r,'worktree','add','-b','linked',str(linked))
    result=runbuild(w,linked); assert result.returncode==0,result.stdout
    record('R01-legitimate-linked-source',w)
    w,r,t=fixture('git-metadata-link');out=w/'external';write(out/'keep','SENTINEL')
    generated=w/'generated/repo';generated.mkdir(parents=True);git(generated,'init','-q','-b','master')
    import shutil
    shutil.rmtree(generated/'.git/refs');(generated/'.git/refs').symlink_to(out,target_is_directory=True)
    result=runbuild(w,r);assert result.returncode!=0,result.stdout
    assert list(out.iterdir())==[out/'keep'] and (out/'keep').read_text()=='SENTINEL'
    record('R01-metadata-directory-link',w)


def r03():
    for kind in ('committed','dirty-tracked','staged-new','local-only'):
        w,r,t=fixture('manifest-'+kind)
        if kind in ('staged-new','local-only'):
            git(r,'rm','--cached','Build.toml');git(r,'commit','-qm','manifest is local-only')
            if kind=='staged-new':git(r,'add','Build.toml')
        if kind=='dirty-tracked':write(r/'Build.toml',(r/'Build.toml').read_text()+'\n# local policy comment\n')
        result=runbuild(w,r);assert result.returncode==0,result.stdout
        proc,f,port=startserver(w,r)
        try:
            assert http(port,'/1/')[0]==200
            original=(r/'Build.toml').read_text()
            write(r/'Build.toml',original+'\n# later change\n')
            eventually(lambda:'restart serve' in (w/'serve.log').read_text(),w,'real configuration change not diagnosed')
            write(r/'Build.toml',original);write(r/'content/00001.md',md(1,'MANIFEST RECOVERED'))
            eventually(lambda:'MANIFEST RECOVERED' in http(port,'/1/')[1],w,'restored configuration did not recover')
            record('R03-'+kind,w)
        finally:stopserver(proc,f)


def r02():
    w,r,t=fixture('structural-generation');write(r/'static/entry/child.txt','CHILD');commit(r)
    git(r,'checkout','-qb','file-shape');git(r,'rm','-r','static/entry');write(r/'static/entry','FILE');commit(r);git(r,'checkout','-q','master')
    proc,f,port=startserver(w,r)
    try:
        assert http(port,'/entry/child.txt')==(200,'CHILD')
        for branch,asset,value,marker in [('file-shape','/entry','FILE','AFTER DIRECTORY TO FILE'),('master','/entry/child.txt','CHILD','AFTER FILE TO DIRECTORY')]:
            git(r,'checkout','-q',branch)
            eventually(lambda:http(port,asset)==(200,value),w,'HTTP shape change failed: '+branch)
            write(r/'content/00001.md',md(1,marker))
            eventually(lambda:marker in http(port,'/1/')[1],w,'valid edit remained poisoned after '+branch)
            git(r,'restore','content/00001.md')
        # Inject an actual publication I/O failure only in owned generated data.
        target=w/'generated/repo/static';target.chmod(0o555)
        try:
            write(r/'static/entry/child.txt','RETRY CONTENT')
            # Change a directly contained file so unlink requires parent write access.
            write(r/'static/new.txt','RETRY NEW');git(r,'add','static/new.txt')
            eventually(lambda:'unable to publish validated generation' in (w/'serve.log').read_text(),w,'publication failure was not exercised')
        finally:target.chmod(0o755)
        write(r/'content/00001.md',md(1,'AFTER IO FAILURE'))
        eventually(lambda:'AFTER IO FAILURE' in http(port,'/1/')[1] and http(port,'/new.txt')==(200,'RETRY NEW'),w,'publication retry did not converge')
        record('R02-bidirectional-shapes-and-io-recovery',w)
    finally:stopserver(proc,f)


def r04():
    for kind in ('bytes','executable','link'):
        w,r,t=fixture('sibling-'+kind);sib=w/'ERCs'
        write(r/'content/shared.txt','IDENTICAL');commit(r)
        write(sib/'content/shared.txt','IDENTICAL');write(sib/'Build.toml',manifest(sib,'ERCs',t,git(t,'rev-parse','HEAD')));initgit(sib)
        write(r/'Build.toml',manifest(r,'EIPs',t,git(t,'rev-parse','HEAD'),{'ERCs':sib}));commit(r)
        proc,f,port=startserver(w,r)
        try:
            path=r/'content/shared.txt'
            if kind=='bytes':write(path,'DIFFERENT')
            elif kind=='executable':path.chmod(0o755)
            else:path.unlink();path.symlink_to('00001.md')
            eventually(lambda:'conflicts with startup sibling snapshot' in (w/'serve.log').read_text(),w,'live conflict not rejected')
            result=cmd([BIN,'-C',r,'--build-root',w/'fresh-conflict','build'],check=False)
            assert result.returncode!=0 and 'conflicting path' in result.stdout,result.stdout
            assert (w/'generated/repo/content/shared.txt').read_text()=='IDENTICAL'
            git(r,'restore','content/shared.txt')
            write(r/'content/00001.md',md(1,'AFTER CONFLICT'))
            eventually(lambda:'AFTER CONFLICT' in http(port,'/1/')[1],w,'live conflict did not recover')
            path.unlink();write(r/'content/00001.md',md(1,'AFTER DELETION'))
            eventually(lambda:'AFTER DELETION' in http(port,'/1/')[1],w,'deleting identical active copy failed')
            assert (w/'generated/repo/content/shared.txt').read_text()=='IDENTICAL'
            result=cmd([BIN,'-C',r,'--build-root',w/'fresh-deletion','build'],check=False);assert result.returncode==0,result.stdout
            record('R04-fresh-live-'+kind,w)
        finally:stopserver(proc,f)


def r04_shapes():
    for direction in ('active-parent','active-child'):
        w,r,t=fixture('sibling-shape-'+direction);sib=w/'ERCs'
        sibling_path='content/00002/assets/child.txt' if direction=='active-parent' else 'content/shared'
        active_path='content/00002' if direction=='active-parent' else 'content/shared/child.txt'
        if direction=='active-parent':write(sib/'content/00002/index.md',md(2))
        write(sib/sibling_path,'SIBLING CONTENT');write(sib/'Build.toml',manifest(sib,'ERCs',t,git(t,'rev-parse','HEAD')));initgit(sib)
        write(r/'Build.toml',manifest(r,'EIPs',t,git(t,'rev-parse','HEAD'),{'ERCs':sib}));commit(r)
        proc,f,port=startserver(w,r)
        try:
            write(r/active_path,'ACTIVE CONTENT');git(r,'add',active_path)
            eventually(lambda:'conflicts with startup sibling snapshot' in (w/'serve.log').read_text(),w,'hierarchical sibling overlap admitted')
            result=cmd([BIN,'-C',r,'--build-root',w/'fresh-conflict','build'],check=False)
            assert result.returncode!=0 and 'conflicting path' in result.stdout,result.stdout
            assert (w/'generated/repo'/sibling_path).read_text()=='SIBLING CONTENT'
            record('R04-'+direction+'-overlap',w)
        finally:stopserver(proc,f)


def r05():
    for linked in (False,True):
        w,r,t=fixture('theme-generations')
        write(t/'templates/eip.html','<html>THEME_A {{ page.content | safe }}</html>')
        write(t/'static/entry/child.txt','CHILD');write(t/'static/kept.txt','KEPT');commit(t)
        git(t,'checkout','-qb','other');write(t/'templates/eip.html','<html>THEME_B {{ page.content | safe }}</html>')
        git(t,'rm','-r','static/entry');write(t/'static/entry','FILE');commit(t);git(t,'checkout','-q','master')
        if linked:
            old=w/'theme-history';t.rename(old);git(old,'worktree','add','-b','linked',str(t))
        proc,f,port=startserver(w,r)
        try:
            assert 'THEME_A' in http(port,'/1/')[1]
            initial_branch=git(t,'branch','--show-current')
            for branch,marker,asset,value in [('other','THEME_B','/entry','FILE'),(initial_branch,'THEME_A','/entry/child.txt','CHILD')]:
                git(t,'checkout','-q',branch)
                eventually(lambda:marker in http(port,'/1/')[1] and http(port,asset)==(200,value),w,'clean theme branch/shape not reflected in HTTP')
            asset=t/'static/new.txt';write(asset,'NEW_THEME');git(t,'add','static/new.txt')
            index=Path(git(t,'rev-parse','--path-format=absolute','--git-path','index'));before=index.read_bytes()
            eventually(lambda:http(port,'/new.txt')==(200,'NEW_THEME'),w,'theme staging not published')
            assert index.read_bytes()==before
            git(t,'reset','-q','HEAD','--','static/new.txt')
            eventually(lambda:http(port,'/new.txt')[0]==404 and not (w/'generated/repo/themes/eips-theme/static/new.txt').exists(),w,'unstaged theme asset remains published')
            assert asset.read_text()=='NEW_THEME'
            (t/'static/kept.txt').unlink()
            eventually(lambda:http(port,'/kept.txt')[0]==404,w,'theme deletion not reflected')
            git(t,'restore','static/kept.txt')
            eventually(lambda:http(port,'/kept.txt')==(200,'KEPT'),w,'theme restoration not reflected')
            write(t/'templates/eip.html','<html>FINAL_THEME {{ page.content | safe }}</html>')
            eventually(lambda:'FINAL_THEME' in http(port,'/1/')[1],w,'theme did not recover after transitions')
            record('R05-theme-generations-'+('linked' if linked else 'ordinary'),w)
        finally:stopserver(proc,f)


def r06():
    secret='EXTERNAL_SENTINEL_NOT_FOR_PUBLICATION'
    for owner in ('active','theme','sibling'):
        for kind in ('file','directory'):
            w,r,t=fixture('read-boundary-'+owner+'-'+kind);outside=w/'outside';write(outside/'secret.txt',secret)
            if owner=='active':source=r;link=source/'static/linked'
            elif owner=='theme':source=t;link=source/'static/linked'
            else:
                source=w/'ERCs';link=source/'content/00002/assets/linked'
                write(source/'content/00002/index.md',md(2));write(source/'Build.toml',manifest(source,'ERCs',t,git(t,'rev-parse','HEAD')))
            link.parent.mkdir(parents=True,exist_ok=True);link.symlink_to(outside/'secret.txt' if kind=='file' else outside,target_is_directory=kind=='directory')
            if owner=='sibling':
                initgit(source);write(r/'Build.toml',manifest(r,'EIPs',t,git(t,'rev-parse','HEAD'),{'ERCs':source}));commit(r)
            else:commit(source)
            for operation,flags in [('build',[]),('build',['--clean']),('build',['--only','1']),('check',[])]:
                result=cmd([BIN,'-C',r,'--build-root',w/'generated',operation,*flags],check=False)
                assert result.returncode!=0 and 'source-read policy' in result.stdout,result.stdout
            output=w/'generated/output';assert not output.exists() or not any(secret.encode() in p.read_bytes() for p in output.rglob('*') if p.is_file())
            output.mkdir(parents=True,exist_ok=True);write(output/'index.html','LAST GOOD PUBLIC')
            proc,f,port=startserver(w,r,operation='preview')
            try:
                assert http(port,'/')[1]=='LAST GOOD PUBLIC'
                assert http(port,'/linked')[0]==404 and http(port,'/linked/secret.txt')[0]==404
            finally:stopserver(proc,f)
            assert (outside/'secret.txt').read_text()==secret
            record('R06-'+owner+'-'+kind+'-rejected',w)
    w,r,t=fixture('internal-link-controls')
    for source,prefix in ((r,'active'),(t,'theme')):
        write(source/'static/data/public.txt',prefix.upper())
        (source/'static'/f'{prefix}-file.txt').symlink_to('data/public.txt')
        (source/'static'/f'{prefix}-directory').symlink_to('data',target_is_directory=True);commit(source)
    result=runbuild(w,r);assert result.returncode==0,result.stdout
    proc,f,port=startserver(w,r,operation='preview')
    try:
        for prefix in ('active','theme'):
            # Both share data/public.txt; explicit alias paths retain their own sources.
            assert http(port,f'/{prefix}-file.txt')==(200,prefix.upper())
            assert http(port,f'/{prefix}-directory/public.txt')==(200,prefix.upper())
        record('R06-internal-file-directory-link-controls',w)
    finally:stopserver(proc,f)
    for owner in ('active','theme'):
        w,r,t=fixture('live-read-rejection-'+owner);source=r if owner=='active' else t
        write(source/'static/linked.txt','PUBLIC');commit(source);outside=w/'outside.txt';write(outside,secret)
        proc,f,port=startserver(w,r)
        try:
            assert http(port,'/linked.txt')==(200,'PUBLIC')
            link=source/'static/linked.txt';link.unlink();link.symlink_to(outside)
            eventually(lambda:'source-read policy' in (w/'serve.log').read_text(),w,'live external link not rejected')
            assert http(port,'/linked.txt')==(200,'PUBLIC')
            link.unlink();write(link,'SAFE RECOVERY')
            eventually(lambda:http(port,'/linked.txt')==(200,'SAFE RECOVERY'),w,'safe live input did not recover')
            assert outside.read_text()==secret
            record('R06-live-'+owner+'-rejection-and-recovery',w)
        finally:stopserver(proc,f)


if __name__=='__main__':
    if os.name!='posix':raise SystemExit('Native Windows acceptance is separate from this POSIX process harness.')
    parser=argparse.ArgumentParser();parser.add_argument('--build-eips',type=Path,required=True);parser.add_argument('--evidence',type=Path,required=True);parser.add_argument('--cases',nargs='+',default=['r01'])
    args=parser.parse_args();BIN=args.build_eips.resolve(strict=True);AUDIT=args.evidence.resolve();AUDIT.mkdir(parents=True,exist_ok=False)
    ENV=os.environ.copy();ENV['PATH']=str(BIN.parent)+os.pathsep+ENV['PATH'];RESULTS=[]
    (AUDIT/'binary.json').write_text(json.dumps({'path':str(BIN),'sha256':hashlib.sha256(BIN.read_bytes()).hexdigest()},indent=2)+'\n')
    for name in args.cases:
        try: globals()[name]()
        except Exception:
            (AUDIT/'failure.txt').write_text(traceback.format_exc());raise
