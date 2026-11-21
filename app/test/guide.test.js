// Host test for app/guide.js (QML JS library): node app/test/guide.test.js
var fs = require("fs"), vm = require("vm"), assert = require("assert"), path = require("path");
var src = fs.readFileSync(path.join(__dirname, "..", "guide.js"), "utf8").replace(/^\.pragma.*$/m, "");
var G = {};
vm.runInNewContext(src + "\nG.windowStart=windowStart;G.blocks=blocks;G.focusIndex=focusIndex;G.nextTime=nextTime;G.hoursFor=hoursFor;G.SPAN=SPAN;", { G: G });

assert.strictEqual(G.SPAN, 7200);
assert.strictEqual(G.windowStart(1800 * 100 + 900, 0), 1800 * 100);
assert.strictEqual(G.windowStart(1800 * 100, 2), 1800 * 102);

var p1 = { id: "1", start: 0, end: 1800 }, p2 = { id: "2", start: 1800, end: 5400 };
function xs(bl) { return JSON.parse(JSON.stringify(bl.map(function (k) { return { x0: k.x0, x1: k.x1, p: k.p }; }))); }
var b = G.blocks([p1, p2], 0, 7200);
assert.deepEqual(xs(b), [
  { x0: 0, x1: 0.25, p: p1 }, { x0: 0.25, x1: 0.75, p: p2 }, { x0: 0.75, x1: 1, p: null }]);
assert.deepEqual(JSON.parse(JSON.stringify(b.map(function (k) { return [k.t0, k.t1]; }))), [[0, 1800], [1800, 5400], [5400, 7200]]);

var c = G.blocks([{ id: "c", start: -600, end: 1200 }], 0, 7200);
assert.strictEqual(c[0].x0, 0);
assert.strictEqual(c[0].t0, 0);
assert.strictEqual(c[0].p.id, "c");

var e = G.blocks([], 0, 7200);
assert.deepEqual(xs(e), [{ x0: 0, x1: 1, p: null }]);

// outside the window dropped; gap between programmes
var d = G.blocks([{ id: "a", start: -900, end: -100 }, { id: "b", start: 1800, end: 3600 },
                  { id: "z", start: 9000, end: 9900 }], 0, 7200);
assert.strictEqual(d.length, 3);
assert.strictEqual(d[0].p, null); assert.strictEqual(d[0].x1, 0.25);
assert.strictEqual(d[1].p.id, "b");

assert.strictEqual(G.focusIndex(b, 2000), 1);
assert.strictEqual(G.focusIndex(b, 0), 0);
assert.strictEqual(G.focusIndex(b, 7200), 2);
assert.strictEqual(G.nextTime(b[1], 0), 1800);
assert.strictEqual(G.nextTime(c[0], 0), 0);
// a window scrolled 4.5 h ahead still asks for enough (the 30 s refresh used to drop back to 3 h)
assert.strictEqual(G.hoursFor(1000 + 4.5 * 3600, 1000), 5);
assert.strictEqual(G.hoursFor(1000 + 3600, 1000), 3);
assert.strictEqual(G.hoursFor(1000 + 9 * 3600, 1000), 6);
console.log("ok");
