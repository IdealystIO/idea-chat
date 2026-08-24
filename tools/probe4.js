const { chromium } = require('playwright-core');
const fs=require('fs');
const EXE = process.env.HOME + '/.cache/ms-playwright/chromium-1228/chrome-linux64/chrome';
const OUT='/tmp/claude-1000/-home-nicho-Development-idea-chat/fdd16d72-f0f1-473a-b74f-c73c7af87b1a/scratchpad/';
(async () => {
  const token=(fs.readFileSync('/tmp/hearth-server.log','utf8').match(/claim\/([a-f0-9]+)/)||[])[1];
  const b = await chromium.launch({ executablePath: EXE, headless: true });
  const p = await b.newPage({ viewport: { width: 1440, height: 900 } });
  p.on('console', m => { if (m.type()==='error') console.log('CONSOLE:', m.text()); });
  await p.goto('http://127.0.0.1:8090', { waitUntil: 'load' });
  await p.waitForTimeout(3500);
  const fill = async (label, value) => {
    await p.evaluate(({label}) => { for (const i of document.querySelectorAll('input')) { const box=i.parentElement; if (box && (box.innerText||'').trim().startsWith(label)) { i.dataset.t='1'; return; } } }, {label});
    const el = await p.$('input[data-t="1"]'); if(!el) { console.log('NO FIELD:',label); return false; }
    await el.click(); await el.type(value,{delay:5});
    await p.evaluate(()=>document.querySelector('input[data-t="1"]')?.removeAttribute('data-t'));
    return true;
  };
  const click = async (label) => {
    const at = await p.evaluate(({label})=>{ const els=[...document.querySelectorAll('div,span,button')].filter(e=>(e.innerText||'').trim()===label && e.getBoundingClientRect().width>0); const el=els[els.length-1]; if(!el) return null; const r=el.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; },{label});
    if(!at){console.log('NO BUTTON:',label);return false;}
    await p.mouse.click(at.x,at.y); await p.waitForTimeout(900); return true;
  };
  await fill('Claim link', token); await fill('Handle','ana'); await fill('Display name','Ana Ruiz'); await fill('Password','a good passphrase');
  await click('Claim this instance'); await p.waitForTimeout(2500);
  console.log('composer present after claim:', await p.evaluate(()=>!!document.querySelector('input[placeholder^="Message"]')));

  const plus = await p.evaluate(()=>{ const rail=document.querySelector('#app').firstElementChild.children[0]; const btn=[...rail.querySelectorAll('[role=button]')].find(e=>parseFloat(getComputedStyle(e).borderTopWidth)>0); if(!btn) return null; const r=btn.getBoundingClientRect(); return {x:r.x+r.width/2,y:r.y+r.height/2}; });
  await p.mouse.click(plus.x, plus.y); await p.waitForTimeout(900);
  await p.screenshot({path:OUT+'probe-join.png'});
  await click('Start a new one');
  await p.screenshot({path:OUT+'probe-join2.png'});
  console.log('fields in dialog:', await p.evaluate(()=>[...document.querySelectorAll('input')].map(i=>`ph="${i.placeholder}" label="${(i.parentElement.innerText||'').split('\n')[0]}"`).join(' | ')));
  await fill('Name your space','The Cabin');
  await click('Create space'); await p.waitForTimeout(2500);
  await p.screenshot({path:OUT+'probe-join3.png'});
  console.log('after create:', (await p.evaluate(()=>document.body.innerText)).slice(0,220).replace(/\n/g,' | '));
  await b.close();
})();
