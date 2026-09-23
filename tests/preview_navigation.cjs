/* Local rendered-site acceptance. Requires Playwright and an installed browser.
 * All preview processes and isolated browser contexts are owned by this runner.
 * Usage: node tests/preview_navigation.cjs --build-eips BIN --profiles JSON
 *        --evidence NEW_DIRECTORY [--chrome EXECUTABLE]
 */
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const {spawn, spawnSync} = require('node:child_process');
const assert = require('node:assert/strict');
const net = require('node:net');
const {chromium} = require('playwright');
const options = Object.fromEntries(Array.from({length:(process.argv.length-2)/2},(_,i)=>[process.argv[2+2*i],process.argv[3+2*i]]));
for (const key of ['--build-eips','--profiles','--evidence']) if (!options[key]) throw Error(`Missing ${key}`);
const evidence = path.resolve(options['--evidence']);
fs.mkdirSync(evidence); // No accidental evidence replacement.
const profiles = JSON.parse(fs.readFileSync(options['--profiles'],'utf8'));
const results = [];
const save = () => fs.writeFileSync(path.join(evidence,'results.json'),JSON.stringify({platform:process.platform,results},null,2));
const delay = ms => new Promise(resolve=>setTimeout(resolve,ms));
async function ready(server, url) {
  for(let i=0;i<100;i++) {
    if(server.exitCode!==null) throw Error(`Preview exited ${server.exitCode}`);
    try { if((await fetch(url,{signal:AbortSignal.timeout(1000)})).status===200) return; } catch {}
    await delay(200);
  }
  throw Error('Preview did not become ready');
}
function stop(server) {
  if(!server || server.exitCode!==null || !server.pid) return;
  if(process.platform==='win32') spawnSync('taskkill.exe',['/PID',String(server.pid),'/T','/F'],{windowsHide:true,timeout:15000});
  else { try { process.kill(-server.pid,'SIGTERM'); } catch(e) { if(e.code!=='ESRCH') throw e; } }
}
(async()=>{
 let browser;
 try {
  browser=await chromium.launch({headless:true,executablePath:options['--chrome'],timeout:30000});
  for (const profile of profiles) {
   let server,context,stream;
   const result={name:profile.name,base:profile.base,passed:false};
   try {
    const url=new URL(profile.base);
    assert.equal(url.hostname,'127.0.0.1','This runner only starts loopback previews');
    await new Promise((resolve,reject)=>{const probe=net.createServer();probe.once('error',reject);probe.listen(Number(url.port),'127.0.0.1',()=>probe.close(resolve));});
    stream=fs.openSync(path.join(evidence,`${profile.name}-preview.log`),'w');
    server=spawn(options['--build-eips'],['-C',profile.source,'--build-root',profile.build_root,'preview','--interface','127.0.0.1','--port',url.port],{stdio:['ignore',stream,stream],windowsHide:true,detached:process.platform!=='win32'});
    server.on('error',e=>{result.spawn_error=String(e);});
    await ready(server,profile.base);
    context=await browser.newContext({viewport:{width:1280,height:900}});
    const page=await context.newPage();
    const errors=[];
    page.on('pageerror',e=>errors.push(String(e)));
    // Readiness is the rendered UI, not unrelated requests becoming idle.
    // Search and popover assertions below still wait for their actual results.
    const homeResponse=await page.goto(profile.base,{waitUntil:'domcontentloaded'});
    assert.equal(homeResponse.status(),200);
    await page.locator('h1').first().waitFor({state:'visible',timeout:15000});
    await page.screenshot({path:path.join(evidence,`${profile.name}-home.png`)});
    await page.goto(profile.base+'search/?q=mynonce1',{waitUntil:'domcontentloaded'});
    const destination=page.locator('[data-search-page-results] a[href$="/5573/"]').first();
    await destination.waitFor({state:'visible',timeout:30000});
    await page.screenshot({path:path.join(evidence,`${profile.name}-search.png`)});
    await destination.click();
    await page.waitForURL(profile.base+'5573/',{timeout:15000});
    result.search_destination=page.url();
    await page.goto(profile.base+'hardforks/',{waitUntil:'domcontentloaded'});
    const metadata=await page.evaluate(async base => (await (await fetch(base+'assets/data/proposals.json')).json()).proposals,profile.base);
    result.popovers=[];
    for(const number of [606,1559]) {
     const record=metadata[`eip-${number}`];
     assert.ok(record,`Missing EIP-${number} metadata`);
     const anchor=page.locator('.page-content a[href]').filter({hasText:new RegExp(`^EIP-${number}$`)}).first();
     await anchor.hover();
     const popup=page.locator('.proposal-popover.show');
     await popup.locator('.popover-header').waitFor({state:'visible',timeout:15000});
     const actual={title:(await popup.locator('.popover-header').innerText()).trim(),description:(await popup.locator('.proposal-popover-description').allTextContents()).map(s=>s.trim()),fields:(await popup.locator('.proposal-popover-meta-item').allTextContents()).map(s=>s.trim())};
     const expected={title:`${record.prefix.toUpperCase()}-${record.number}: ${record.title.trim()}`,description:record.description?.trim()?[record.description.trim()]:[],fields:[`Status: ${record.status.trim()}`,`Type: ${record.type.trim()}`,...(record.category?.trim()?[`Category: ${record.category.trim()}`]:[])]};
     assert.deepEqual(actual,expected);
     result.popovers.push({key:`eip-${number}`,actual,expected});
     await page.screenshot({path:path.join(evidence,`${profile.name}-popover-${number}.png`)});
     await page.mouse.move(0,0);
     await popup.waitFor({state:'hidden',timeout:5000});
    }
    assert.deepEqual(errors,[]);
    result.page_errors=errors;
    result.passed=true;
   } catch(e) {result.error=e.stack||String(e);}
   finally {if(context) await context.close();stop(server);if(stream!==undefined)fs.closeSync(stream);results.push(result);save();console.log(result.name,result.passed?'PASS':'FAIL');}
  }
 } catch(e) {results.push({name:'browser-startup',passed:false,error:e.stack||String(e)});save();}
 finally {if(browser)await browser.close();}
 process.exitCode=results.some(result=>!result.passed)?1:0;
})();
