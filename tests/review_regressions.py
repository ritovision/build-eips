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
