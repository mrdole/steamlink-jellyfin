.pragma library
// TV guide layout math for Guide.qml (import "guide.js" as G). All times are epoch seconds.
// Host-tested: node app/test/guide.test.js

var SPAN = 7200;   // the guide shows two hours
var SLOT = 1800;   // half-hour grid

// The half hour at or below `now`, plus `slots` half hours.
function windowStart(now, slots) {
    return Math.floor(now / SLOT) * SLOT + (slots || 0) * SLOT;
}

function makeBlock(x0, x1, p, t0, t1) {
    // t0/t1: absolute epoch seconds of the clipped block (plain fields, survive copying into QML models)
    return { x0: x0, x1: x1, t0: t0, t1: t1, p: p };
}

// Programmes (sorted by start) -> blocks across [start, end): x0/x1 are fractions 0..1 of the window.
// Programmes outside the window are dropped, overlaps/edges clipped, gaps become {p: null}.
function blocks(programs, start, end) {
    var out = [], len = end - start, cur = start, i, p, s, e;
    if (!(len > 0)) return out;
    for (i = 0; i < (programs || []).length; i++) {
        p = programs[i];
        if (p.end <= start || p.start >= end) continue;
        s = Math.max(p.start, cur);
        e = Math.min(p.end, end);
        if (e <= s) continue;                     // zero-width or fully overlapped
        if (s > cur) out.push(makeBlock((cur - start) / len, (s - start) / len, null, cur, s));
        out.push(makeBlock((s - start) / len, (e - start) / len, p, s, e));
        cur = e;
    }
    if (cur < end) out.push(makeBlock((cur - start) / len, 1, null, cur, end));
    return out;
}

// Index of the block covering time t, else the last block starting before t, else 0.
function focusIndex(bl, t) {
    var i, last = -1, t0, t1;
    for (i = 0; i < bl.length; i++) {
        t0 = bl[i].t0;
        t1 = bl[i].t1;
        if (t >= t0 && t < t1) return i;
        if (t0 < t) last = i;
    }
    return last < 0 ? 0 : last;
}

// Time to move focus to after Left/Right lands on `block`: its start, clipped to the window start.
function nextTime(block, start) {
    return Math.max(block.t0, start);
}

// Hours of programmes to ask sljfd for so the window up to `end` is covered: at least 3, at most the 6 it gives.
function hoursFor(end, now) {
    return Math.max(3, Math.min(6, Math.ceil((end - now) / 3600)));
}
