// The TV guide (main.qml's screen "guide"): channels down the side, about the next two hours across, the focused
// programme in a strip at the top. Data from sljfd GET /guide; layout math in guide.js (host-tested).
// Keys come from main.qml's key(): up/down change channel, left/right move between programmes (Right past the end
// moves the window on by half an hour, Left never before the current half hour), select tunes, back leaves.
import QtQuick 2.12
import "theme.js" as T
import "guide.js" as G

Item {
    id: g
    property real u: 1
    property string api: ""
    signal tune(string channelId)
    signal back()

    property var chans: []                             // /guide channels: [{id, number, name, programs}]
    property bool loaded: false
    property string note: ""                           // fetch failed: the app's usual text (main.qml shows it too)
    property string want: ""                           // channel to focus once the data is in
    property int row: 0
    property real skew: 0                              // daemon clock - this clock (s): the daemon's "now" counts
    property real now: 0
    property real base: 0                              // the current half hour (window start with no shift)
    property int slots: 0                              // half hours the window is moved on from base
    property real ws: base + slots * G.SLOT            // window start / end
    property real we: ws + G.SPAN
    property real loadedTo: 0                          // programmes are loaded up to here
    property real fT: 0                                // focus time: Up/Down keep it, Left/Right move it
    property var cur: chans.length ? G.blocks((chans[row] || {}).programs || [], ws, we) : []
    property int bi: G.focusIndex(cur, fT)
    property var blk: cur.length ? cur[bi] : null
    property string focusId: chans.length && chans[row] ? chans[row].id : want
    readonly property real maxAhead: 6 * 3600          // sljfd gives at most 6 h
    property int seq: 0                                // the newest /guide request: older answers are dropped

    function clockNow() { return Date.now() / 1000 + skew }

    /// Show the guide focused on `channelId` (else the first channel) at the current time.
    function open(channelId) {
        want = channelId || ""
        slots = 0; loaded = false; chans = []; row = 0; note = ""
        load(3)
    }

    // hours that cover the window as it is now (at least 3, at most what sljfd gives)
    function need() { return G.hoursFor(we, clockNow()) }

    function load(hours) {
        var x = new XMLHttpRequest(), my = ++seq
        x.onreadystatechange = function () {
            if (x.readyState !== 4) return
            if (my !== seq) return                     // a newer request is on its way (or the guide was reopened)
            var d = null
            try { d = JSON.parse(x.responseText) } catch (e) {}
            if (x.status !== 200 || !d || !d.channels) {
                note = x.status === 502 ? "JELLYFIN IS NOT ANSWERING (NETWORK?), TRYING AGAIN" : "NO ANSWER FROM THE PLAYER DAEMON, TRYING AGAIN"
                retry.hours = hours; retry.restart()
                return
            }
            var keep = chans.length ? chans[row].id : want
            skew = d.now - Date.now() / 1000
            now = d.now
            if (!loaded) { base = G.windowStart(d.now, 0); fT = d.now }
            loadedTo = d.now + hours * 3600
            var r = 0
            for (var i = 0; i < d.channels.length; i++) if (d.channels[i].id === keep) r = i
            chans = d.channels; row = r; loaded = true; note = ""
            console.log("guide: " + d.channels.length + " channels, " + hours + " h, focus " + (d.channels[r] || {}).name)
        }
        x.open("GET", api + "/guide?hours=" + hours)
        x.send()
    }
    Timer { id: retry; property int hours: 3; interval: 3000; onTriggered: if (g.visible) load(hours) }
    Timer {                                            // the clock moves on: "now" line, and the window with it
        interval: 30000; running: g.visible && g.loaded; repeat: true
        onTriggered: {
            now = clockNow()
            var b = G.windowStart(now, 0)
            if (b > base) { slots = Math.max(0, slots - Math.round((b - base) / G.SLOT)); base = b; fT = Math.max(fT, now); load(need()) }
        }
    }

    // window moved on: fetch more when it runs past what is loaded
    function moved() {
        if (we > loadedTo) load(need())
    }

    function key(k) {
        if (k === "back") { back(); return }
        if (!chans.length) return
        if (k === "up") row = Math.max(0, row - 1)
        else if (k === "down") row = Math.min(chans.length - 1, row + 1)
        else if (k === "select") tune(chans[row].id)
        else if (k === "right") {
            if (bi < cur.length - 1) {
                fT = G.nextTime(cur[bi + 1], ws)
                // landed on a sliver cut off by the window end: bring it into view
                if (cur[bi].t1 >= we && we - cur[bi].t0 < G.SLOT / 2 && we + G.SLOT <= now + maxAhead) { slots++; moved() }
            } else if (we + G.SLOT <= now + maxAhead) { var end = we; slots++; fT = end; moved() }
        } else if (k === "left") {
            if (bi > 0) fT = G.nextTime(cur[bi - 1], ws)
            else if (slots > 0) {
                var start = ws
                slots--
                fT = G.nextTime(cur[G.focusIndex(cur, start - 1)], ws)
            }
        }
    }

    // "h" is 12-hour only with AP in the same format: split "8:30 PM"
    function hm(t) { return Qt.formatTime(new Date(t * 1000), "h:mm AP").split(" ")[0] }
    function ap(t) { return Qt.formatTime(new Date(t * 1000), "h:mm AP").split(" ")[1] || "" }
    function span(p) {                                 // "8:30–9:00 PM" (or "11:30 AM–12:30 PM")
        return ap(p.start) === ap(p.end) ? hm(p.start) + "–" + hm(p.end) + " " + ap(p.end)
                                         : hm(p.start) + " " + ap(p.start) + "–" + hm(p.end) + " " + ap(p.end)
    }

    readonly property real side: 250 * u               // channel column
    readonly property real gridW: width - side

    // ---- top strip: the focused programme ----
    Item {
        id: strip
        width: parent.width; height: 116 * u; visible: g.loaded && g.chans.length > 0
        Text {
            id: sTitle; width: parent.width; elide: Text.ElideRight
            text: g.blk && g.blk.p ? g.blk.p.name.toUpperCase() : "NO INFORMATION"
            color: T.C.bright; font.family: T.FONT; font.pixelSize: 26 * g.u; font.bold: true
        }
        Text {
            id: sWhen; y: 36 * g.u; width: parent.width; elide: Text.ElideRight
            text: {
                if (!g.chans.length) return ""
                var c = g.chans[g.row] || {}, s = (c.number ? "CH " + c.number + "  " : "") + (c.name || "").toUpperCase()
                if (g.blk && g.blk.p) {
                    s += "    " + g.span(g.blk.p)
                    if (g.blk.p.episode) s += "    " + g.blk.p.episode.toUpperCase()
                }
                return s
            }
            color: T.C.accent; font.family: T.FONT; font.pixelSize: 16 * g.u; font.bold: true
        }
        Text {
            y: 62 * g.u; width: parent.width; height: 48 * g.u; wrapMode: Text.WordWrap; maximumLineCount: 2
            elide: Text.ElideRight; clip: true
            text: g.blk && g.blk.p && g.blk.p.overview ? g.blk.p.overview : ""
            color: T.C.fg; font.family: T.FONT; font.pixelSize: 15 * g.u
        }
    }

    // ---- time ruler ----
    Item {
        id: ruler
        y: 122 * u; width: parent.width; height: 28 * u; visible: strip.visible
        Repeater {
            model: G.SPAN / G.SLOT
            Item {
                x: g.side + index * g.gridW * G.SLOT / G.SPAN; height: ruler.height
                Rectangle { width: 2 * g.u; height: parent.height; color: T.C.edge }
                Text {
                    x: 8 * g.u; anchors.verticalCenter: parent.verticalCenter
                    text: g.hm(g.ws + index * G.SLOT) + " " + g.ap(g.ws + index * G.SLOT)
                    color: T.C.dim; font.family: T.FONT; font.pixelSize: 15 * g.u; font.bold: true
                }
            }
        }
        Text {
            x: 8 * g.u; anchors.verticalCenter: parent.verticalCenter
            text: g.slots > 0 ? "‹ EARLIER" : "TODAY"
            color: T.C.dim; font.family: T.FONT; font.pixelSize: 13 * g.u; font.bold: true
        }
    }

    // ---- channels x programmes ----
    ListView {
        id: list
        y: 156 * u; width: parent.width; height: parent.height - 156 * u
        clip: true; model: g.chans.length; currentIndex: g.row; highlightMoveDuration: 150
        preferredHighlightBegin: 0; preferredHighlightEnd: height; highlightRangeMode: ListView.ApplyRange
        delegate: Item {
            id: rowItem
            property var ch: g.chans[index] || ({})
            property bool on: index === g.row
            property var bl: G.blocks(ch.programs || [], g.ws, g.we)
            width: list.width; height: 68 * g.u
            Rectangle {                                // channel cell
                width: g.side - 8 * g.u; height: parent.height - 6 * g.u
                color: rowItem.on ? T.C.raised : T.C.card; border.width: (rowItem.on ? 2 : 1) * g.u
                border.color: rowItem.on ? T.C.accent : T.C.edge
                Image {
                    id: logo
                    x: 8 * g.u; anchors.verticalCenter: parent.verticalCenter; width: 72 * g.u; height: 46 * g.u
                    source: rowItem.ch.id ? g.api + "/image?id=" + rowItem.ch.id + "&w=160" : ""
                    asynchronous: true; fillMode: Image.PreserveAspectFit; sourceSize.width: 160
                }
                Text {
                    x: 90 * g.u; y: 8 * g.u; width: parent.width - 98 * g.u; elide: Text.ElideRight
                    text: rowItem.ch.number || ""
                    color: rowItem.on ? T.C.accent : T.C.dim; font.family: T.FONT; font.pixelSize: 14 * g.u; font.bold: true
                }
                Text {
                    x: 90 * g.u; y: 28 * g.u; width: parent.width - 98 * g.u; elide: Text.ElideRight
                    text: (rowItem.ch.name || "").toUpperCase()
                    color: rowItem.on ? T.C.bright : T.C.fg; font.family: T.FONT; font.pixelSize: 15 * g.u; font.bold: rowItem.on
                }
            }
            Repeater {                                 // programme blocks
                model: rowItem.bl.length
                Rectangle {
                    property var b: rowItem.bl[index]
                    property bool on: rowItem.on && index === g.bi
                    x: g.side + b.x0 * g.gridW; width: Math.max(2 * g.u, (b.x1 - b.x0) * g.gridW - 4 * g.u)
                    height: parent.height - 6 * g.u
                    color: on ? T.C.accent : b.p ? T.C.card : T.C.bg_deep
                    border.width: (on ? 0 : 1) * g.u; border.color: T.C.edge
                    clip: true
                    Text {
                        x: 10 * g.u; y: (b.p && b.p.episode ? 9 : 20) * g.u; width: parent.width - 16 * g.u; elide: Text.ElideRight
                        text: (b.p && b.p.start < b.t0 ? "‹ " : "") + (b.p ? b.p.name : "No information")   // ‹ = started earlier
                        color: parent.on ? T.C.on_accent : b.p ? T.C.fg : T.C.dim
                        font.family: T.FONT; font.pixelSize: 16 * g.u; font.bold: parent.on
                    }
                    Text {
                        visible: !!(b.p && b.p.episode)
                        x: 10 * g.u; y: 33 * g.u; width: parent.width - 16 * g.u; elide: Text.ElideRight
                        text: b.p && b.p.episode ? b.p.episode : ""
                        color: parent.on ? T.C.on_accent : T.C.dim; font.family: T.FONT; font.pixelSize: 13 * g.u
                    }
                }
            }
        }
    }
    Rectangle {                                        // the "now" line, over the rows
        visible: strip.visible && g.now >= g.ws && g.now < g.we
        x: g.side + (g.now - g.ws) / G.SPAN * g.gridW - 1 * u; y: ruler.y; width: 3 * u; height: parent.height - ruler.y
        color: T.C.warn
    }

    Text {                                             // nothing to show yet, or the fetch failed: never blank
        visible: !strip.visible
        anchors.centerIn: parent; width: parent.width; horizontalAlignment: Text.AlignHCenter; wrapMode: Text.WordWrap
        text: g.note.length ? g.note : g.loaded ? "NO LIVE TV CHANNELS" : "LOADING"
        color: g.note.length ? T.C.warn : T.C.dim; font.family: T.FONT; font.pixelSize: 22 * g.u
    }
}
