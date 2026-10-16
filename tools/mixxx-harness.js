const fs = require('fs');
const vm = require('vm');
// usage: node mixxx-harness.js LIB[,LIB...] SCRIPT XML [--qtscript]
//   LIB       Mixxx library files loaded before the script, in order
//             (lodash.mixxx.js,midi-components-0.0.js for Mixxx 2.3)
//   --qtscript  emulate Mixxx 2.3: its QtScript engine calls a makeConnection
//             callback with the `this` of the function that registered it,
//             which the 2.3 components library relies on instead of bind().
const args = process.argv.slice(2);
const qtscript = args.includes('--qtscript');
const [compPath, scriptPath, xmlPath] = args.filter(a => a !== '--qtscript');
const values = {};
const log = [];
const engine = {
  getValue: (g, k) => values[g + k] || 0,
  setValue: (g, k, v) => { values[g + k] = v; log.push(['set', g, k, v]); },
  getParameter: (g, k) => values[g + k] || 0,
  setParameter: (g, k, v) => { values[g + k] = v; log.push(['setp', g, k, v]); },
  makeConnection: (g, k, f) => ({disconnect() {}, trigger() { f(0, g, k); }}),
  connectControl: () => ({}),
  softTakeover: () => {}, softTakeoverIgnoreNextValue: () => {},
  scratchEnable: (d) => { values['scratch' + d] = 1; }, scratchDisable: (d) => { values['scratch' + d] = 0; },
  scratchTick: (d, t) => { log.push(['tick', d, t]); }, isScratching: (d) => !!values['scratch' + d],
  beginTimer: () => 1, stopTimer: () => {}, getSetting: () => undefined, log: () => {},
};
const midi = { sendShortMsg: (s, n, v) => { log.push(['midi', s, n, v]); }, sendSysexMsg: () => {} };
// Only the globals every Mixxx version provides: no console (missing in 2.3), no print.
const ctx = { engine, midi, script: { midiDebug: () => {} } };
vm.createContext(ctx);
for (const lib of compPath.split(',')) {
  let src = fs.readFileSync(lib, 'utf8');
  if (qtscript) {
    src = src.replace(/engine\.makeConnection\(([^;]*?)\s*,\s*(this\.output|function\s*\([^)]*\)\s*\{[\s\S]*?\n\s*\})\s*\);/g,
      (m, head, cb) => `engine.makeConnection(${head}, (${cb}).bind(this));`);
  }
  vm.runInContext(src, ctx, { filename: require('path').basename(lib) });
}
vm.runInContext(fs.readFileSync(scriptPath, 'utf8'), ctx, { filename: 'script' });
vm.runInContext('NS7II.init("Numark NS7II", false);', ctx);
const xml = fs.readFileSync(xmlPath, 'utf8');
const re = /<group>([^<]+)<\/group>\s*<key>([^<]+)<\/key>\s*<status>(0x[0-9A-F]+)<\/status>\s*<midino>(0x[0-9A-F]+)<\/midino>/g;
let m, n = 0, failures = 0;
const seen = new Set();
while ((m = re.exec(xml))) {
  const [, group, key, st, no] = m;
  const status = parseInt(st, 16), midino = parseInt(no, 16);
  for (const value of [0x7F, 0x00, 0x41, 0x01]) {
    n++;
    try {
      vm.runInContext(`(function(){ var f = ${key}; if (typeof f !== 'function') throw new Error('not a function: ${key}'); f.call(${key.replace(/\.[A-Za-z0-9]+$/, '')}, ${status & 0x0F}, ${midino}, ${value}, ${status}, '${group}'); })()`, ctx);
    } catch (e) {
      if (!seen.has(key)) { seen.add(key); console.log('FAIL', key, e.message); }
      failures++;
    }
  }
}
// Mode switches and pads under each mode, shift, platter touch+turn.
// A partial capture leaves components out; only exercise the ones present.
const d = ctx.NS7II.decks[0];
const press = (c) => { if (d[c]) { d[c].input(0, 0, 0x7F, 0x90, '[Channel1]'); d[c].input(0, 0, 0, 0x90, '[Channel1]'); } };
for (const mode of ['hotcue', 'loop', 'slicer', 'sampler']) {
  d.setPadMode(mode);
  for (let i = 1; i <= 8; i++) { press('pad' + i); }
}
ctx.NS7II.setShift(0, true); press('pad1'); ctx.NS7II.setShift(0, false);
if (d.platterTouch) { d.platterTouch.input(0, 0, 0x7F, 0x90, '[Channel1]'); }
if (d.platterTurn) { for (let i = 0; i < 10; i++) d.platterTurn.input(0, 6, 66); }
if (d.platterTouch) { d.platterTouch.input(0, 0, 0, 0x90, '[Channel1]'); }
// Motor: starting deck 1 must send start + pitch on the deck's channel, stopping must send stop.
values['[Channel1]rate'] = 0;   // earlier sweeps moved the pitch; 0% must encode as 0x2000
const before = log.length;
d.motorStart();
d.sendMotorPitch();
d.motorStop();
const motorMsgs = log.slice(before).filter(l => l[0] === 'midi').map(l => l.slice(1));
const want = [[0xB1, 0x43, 0], [0xB1, 0x44, 0], [0xB1, 0x69, 0x40], [0xB1, 0x49, 0x00]];
for (const w of want) {
  if (!motorMsgs.some(m => m[0] === w[0] && m[1] === w[1] && m[2] === w[2])) { console.log('FAIL motor message missing', w.map(x => x.toString(16))); failures++; }
}
vm.runInContext('NS7II.shutdown();', ctx);
const ticks = log.filter(l => l[0] === 'tick').length;
const sets = log.filter(l => l[0] === 'set' || l[0] === 'setp').length;
console.log(`invocations ${n}, failures ${failures}, scratch ticks ${ticks}, engine writes ${sets}, midi out ${log.filter(l=>l[0]==='midi').length}`);
process.exit(failures ? 1 : 0);
