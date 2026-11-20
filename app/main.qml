// The on-TV Jellyfin app for the Steam Link. One file, no network except sljfd's local interface (API).
// Look: the theme in themes/ (theme.js is generated from themes/<name>.yaml by build-app.sh).
// Input: every source ends up in key(k) with k = up/down/left/right/select/back/home:
//   keyboard and game controllers (main.cpp), and remote commands through Jellyfin or the LAN API (sljfd /events long poll).
import QtQuick 2.12
import "theme.js" as T

Rectangle {
    id: root
    color: T.C.bg
    focus: true
    property real u: height / 720                      // 1 unit = 1 px on a 720p layout
    property string api: "http://127.0.0.1:8097"

    // ---- state ----
    property var rows: []                              // home: [{title, items}]
    property int hRow: 0
    property var hCols: []
    property int rev: 0                                // bump to refresh bindings that read hCols
    property var pages: []                             // browse stack: [{title, parent, items, index, wide, done}]
    property int depth: 0                              // 0 = home, n = pages[n-1]
    property var page: depth > 0 ? pages[depth - 1] : null
    property bool searching: false
    property string term: ""
    property var results: []
    property int sZone: 0                              // search: 0 keyboard, 1 results
    property int sKey: 0
    property int sRes: 0
    property var detail: null                          // item shown in the play dialog
    property int dBtn: 0
    property var dOpts: []                             // dialog choice rows: [{key, title, choices:[{index,name}], sel}]
    property int dRow: 0                               // focused dialog row: 0..dOpts.length-1, dOpts.length = buttons
    property string note: "LOADING"
    property bool starting: false
    property double seq: 0
    property var saved: null                           // state from before playback (restoreState), used up once

    // ---- where the user was: kept across the app's exit during playback (main.cpp `store`, run.sh --restore) ----
    // Only the path is kept (row titles + focused item ids, page parents + index, search term); the lists themselves
    // are fetched again so progress and watched marks are fresh.
    function saveState() {
        if (typeof store === "undefined") return           // qml-qt5 preview on the build machine
        var ids = {}
        for (var i = 0; i < rows.length; i++) {
            var it = rows[i].items[hCols[i] || 0]
            ids[rows[i].title] = {id: it ? it.id : "", col: hCols[i] || 0}
        }
        var ps = []
        for (i = 0; i < depth; i++) {
            var p = pages[i], cur = p.items[p.index]
            ps.push({title: p.title, parent: p.parent, index: p.index, id: cur ? cur.id : ""})
        }
        console.log("state saved: depth " + depth + (depth ? " page " + pages[depth - 1].title + " #" + pages[depth - 1].index : " row " + hRow) + (searching ? " search " + term : ""))
        store.save(JSON.stringify({row: rows[hRow] ? rows[hRow].title : "", rows: ids, pages: ps, searching: searching,
                                   term: term, sZone: sZone, sKey: sKey, sRes: sRes, sId: results[sRes] ? results[sRes].id : ""}))
    }
    function restoreState() {
        if (typeof store === "undefined") return
        var v = null
        try { v = JSON.parse(store.load()) } catch (e) {}
        if (!v) return
        saved = v
        var ps = []
        for (var i = 0; i < v.pages.length; i++) {
            var q = v.pages[i]
            ps.push({title: q.title, parent: q.parent, items: [], index: q.index, want: q.id, wide: false, done: false})
        }
        pages = ps; depth = ps.length
        if (depth) loadMore()
        if (v.searching) {
            searching = true; term = v.term; sZone = v.sZone; sKey = v.sKey
            search(v.sId, v.sRes)
        }
    }
    /// After a reload, put the focus back on the item the user had (by id; else the same position).
    function refocus(items, id, fallback) {
        for (var i = 0; i < items.length; i++) if (items[i].id === id) return i
        return Math.max(0, Math.min(items.length - 1, fallback))
    }

    function get(path, cb) {
        var x = new XMLHttpRequest()
        x.onreadystatechange = function () {
            if (x.readyState !== 4) return
            var v = null
            try { v = JSON.parse(x.responseText) } catch (e) {}
            cb(x.status === 200 ? v : null, x.status)
        }
        x.open("GET", api + path)
        x.send()
    }

    function loadHome() {
        get("/home", function (d, status) {
            if (!d) {
                note = status === 502 ? "JELLYFIN IS NOT ANSWERING (NETWORK?), TRYING AGAIN" : "NO ANSWER FROM THE PLAYER DAEMON, TRYING AGAIN"
                retry.start(); return
            }
            var r = []
            if (d.resume.length) r.push({title: "CONTINUE WATCHING", items: d.resume})
            if (d.nextup.length) r.push({title: "NEXT UP", items: d.nextup})
            if (d.channels && d.channels.length) r.push({title: "LIVE TV", items: d.channels})
            var libs = [{id: "", name: "SEARCH", type: "Search", folder: false}]
            for (var i = 0; i < d.libraries.length; i++)
                if (d.libraries[i].name !== "Live TV") libs.push(d.libraries[i])
            r.push({title: "LIBRARIES", items: libs})
            var c = []
            for (i = 0; i < r.length; i++) c.push(hCols[i] || 0)
            if (saved) {                                   // back from playback: same row, same item if still there
                for (i = 0; i < r.length; i++) {
                    var w = saved.rows[r[i].title]
                    if (w) c[i] = refocus(r[i].items, w.id, w.col)
                    if (r[i].title === saved.row) hRow = i
                }
                saved = null
                console.log("restored home: row " + hRow + " (" + r[hRow].title + ") col " + c[hRow] + " " + (r[hRow].items[c[hRow]] || {}).name)
            }
            hCols = c; rows = r; note = ""; rev++
            if (hRow >= r.length) hRow = 0
        })
    }

    function openFolder(it) {
        var p = {title: (it.series && it.type === "Season" ? it.series + " / " : "") + it.name, parent: it.id,
                 items: [], index: 0, wide: false, done: false}
        var ps = pages.slice(0, depth); ps.push(p); pages = ps; depth = ps.length
        loadMore()
        console.log("open " + p.title)
    }

    function loadMore() {
        var p = pages[depth - 1]
        if (!p || p.done || p.loading) return
        p.loading = true
        var d = depth
        get("/items?parent=" + p.parent + "&start=" + p.items.length, function (v) {
            p.loading = false
            if (!v || !v.length) { p.done = true; if (!p.items.length) p.empty = true }
            else {
                p.items = p.items.concat(v)
                p.wide = p.items[0].type === "Episode"
                if (v.length < 60) p.done = true
            }
            if (p.want !== undefined) {                    // restored page: load up to the remembered item
                var at = -1
                for (var i = 0; i < p.items.length; i++) if (p.items[i].id === p.want) { at = i; break }
                if (at < 0 && !p.done && p.items.length <= p.index + 60) { if (depth === d) { var q = pages.slice(); pages = q }; loadMore(); return }
                p.index = at >= 0 ? at : Math.max(0, Math.min(p.items.length - 1, p.index))
                delete p.want
                console.log("restored page " + p.title + ": #" + p.index + " " + (p.items[p.index] || {}).name + (at < 0 ? " (item gone, same position)" : ""))
            }
            if (depth === d) { var ps = pages.slice(); pages = ps; grid.currentIndex = p.index }      // re-publish
        })
    }

    function activate(it) {
        if (!it) return
        if (it.type === "Search") { searching = true; sZone = 0; return }
        if (it.folder) { openFolder(it); return }
        detail = it; dBtn = 0; dOpts = []; dRow = 0
        if (it.type === "TvChannel") { play(false); return }   // a channel just starts (the dialog says STARTING)
        get("/detail?id=" + it.id, function (v) {
            if (!v || detail !== it) return
            var o = []
            if (v.audio.length > 1) {
                var sel = 0
                for (var i = 0; i < v.audio.length; i++) if (v.audio[i].index === v.audio_default) sel = i
                o.push({key: "audio", title: "AUDIO", choices: v.audio, sel: sel, def: sel})
            }
            if (v.subs.length) {                           // picture subtitles (PGS, DVD) are burned in by the server
                var ch = [{index: -1, name: "Off"}]
                sel = 0
                for (i = 0; i < v.subs.length; i++) {
                    var t = v.subs[i]
                    ch.push({index: t.index, name: t.name + (t.text ? "" : " [burned in]")})
                    if (t.index === v.sub_default) sel = ch.length - 1
                }
                o.push({key: "sub", title: "SUBTITLES", choices: ch, sel: sel, def: sel})
            }
            dOpts = o; dRow = o.length
        })
    }
    // audio is always sent (the queue keeps its language); subtitles only when changed, so later episodes still get
    // Jellyfin's default (forced subtitles) when nobody touched the row
    function optArg(key, onlyChanged) {
        for (var i = 0; i < dOpts.length; i++) {
            var o = dOpts[i]
            if (o.key === key && (!onlyChanged || o.sel !== o.def)) return "&" + key + "=" + o.choices[o.sel].index
        }
        return ""
    }

    function play(fromStart) {
        starting = true
        startGiveUp.restart()
        get("/play?id=" + detail.id + (fromStart ? "&from=start" : "") + optArg("audio") + optArg("sub", true), function (v) {
            if (!v) { starting = false; note = "COULD NOT START PLAYBACK" }
        })
    }

    function search(keepId, keepAt) {
        if (term.length < 2) { results = []; return }
        var t = term
        get("/search?q=" + encodeURIComponent(t), function (v) {
            if (t !== term) return
            results = v || []
            sRes = keepId ? refocus(results, keepId, keepAt) : 0
            if (keepId) console.log("restored search " + t + ": result #" + sRes + " zone " + sZone)
            if (!results.length) sZone = 0
            else if (goWanted) { sZone = 1; sRes = 0 }
            goWanted = false
        })
    }

    readonly property string keysStr: "ABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789"
    function sType(ch) {
        if (ch === "DEL") term = term.slice(0, -1)
        else if (ch === "SPACE") term += " "
        else if (ch === "CLEAR") term = ""
        else if (ch === "GO") { debounce.stop(); goWanted = true; search(); return }   // search now, then the results
        else term += ch
        debounce.restart()
    }
    property bool goWanted: false
    // text from the LAN API (e.g. a Home Assistant text box) or a Jellyfin app: Search with it typed, focus on the results
    function typedElsewhere(t) {
        detail = null; starting = false; searching = true; depth = 0
        term = t.toUpperCase().replace(/[^A-Z0-9 ]/g, "")
        sKey = 39; sZone = 0; debounce.stop(); goWanted = true; search()
        console.log("search from outside: " + term)
    }
    function sLabel(i) { return i < 36 ? keysStr[i] : ["SPACE", "DEL", "CLEAR", "GO ▸"][i - 36] }

    // ---- input ----
    function key(k) {
        if (starting) return
        if (k === "home") { detail = null; searching = false; depth = 0; return }
        if (detail) {
            var n = detail.resume ? 2 : 1
            if (k === "back") { detail = null; return }
            if (dRow < dOpts.length) {                     // a choice row: left/right change it
                var o = dOpts[dRow]
                if (k === "up") dRow = Math.max(0, dRow - 1)
                else if (k === "down" || k === "select") dRow++
                else if (k === "left" || k === "right") {
                    o.sel = Math.max(0, Math.min(o.choices.length - 1, o.sel + (k === "left" ? -1 : 1)))
                    dOpts = dOpts.slice()
                }
                return
            }
            if (k === "left") dBtn = Math.max(0, dBtn - 1)
            else if (k === "right") dBtn = Math.min(n - 1, dBtn + 1)
            else if (k === "up") dRow = Math.max(0, dOpts.length - 1)
            else if (k === "select") play(detail.resume ? dBtn === 1 : false)
            return
        }
        if (searching && depth === 0) {               // a show opened from the results is a page over Search
            if (k === "back") { searching = false; return }
            if (sZone === 0) {
                var cols = 13, total = 40
                if (k === "left") sKey = Math.max(0, sKey - 1)
                else if (k === "right") sKey = Math.min(total - 1, sKey + 1)
                else if (k === "up") sKey = sKey >= cols ? sKey - cols : sKey
                else if (k === "down") { if (sKey + cols < total) sKey += cols; else if (results.length) sZone = 1 }
                else if (k === "select") sType(sKey === 39 ? "GO" : sLabel(sKey))
            } else {
                if (k === "left") sRes = Math.max(0, sRes - 1)
                else if (k === "right") sRes = Math.min(results.length - 1, sRes + 1)
                else if (k === "up") sZone = 0
                else if (k === "select") activate(results[sRes])
            }
            return
        }
        if (depth > 0) {
            var p = page, per = grid.perRow
            if (k === "back") { depth = depth - 1; if (depth > 0 && !pages[depth - 1].items.length) loadMore(); return }
            if (!p.items.length) return
            var i = p.index
            if (k === "left") i = Math.max(0, i - 1)
            else if (k === "right") i = Math.min(p.items.length - 1, i + 1)
            else if (k === "up") i = i >= per ? i - per : i
            else if (k === "down") i = Math.min(p.items.length - 1, i + per)
            else if (k === "select") { activate(p.items[i]); return }
            p.index = i; grid.currentIndex = i
            if (i > p.items.length - 2 * per - 1) loadMore()
            return
        }
        if (!rows.length) return
        if (k === "back") { Qt.quit(); return }
        var c = hCols[hRow] || 0
        if (k === "up") hRow = Math.max(0, hRow - 1)
        else if (k === "down") hRow = Math.min(rows.length - 1, hRow + 1)
        else if (k === "left") hCols[hRow] = Math.max(0, c - 1)
        else if (k === "right") hCols[hRow] = Math.min(rows[hRow].items.length - 1, c + 1)
        else if (k === "select") activate(rows[hRow].items[c])
        rev++
    }

    Keys.onPressed: {
        var m = {}
        m[Qt.Key_Up] = "up"; m[Qt.Key_Down] = "down"; m[Qt.Key_Left] = "left"; m[Qt.Key_Right] = "right"
        m[Qt.Key_Return] = "select"; m[Qt.Key_Enter] = "select"; m[Qt.Key_Escape] = "back"; m[Qt.Key_Home] = "home"
        if (searching && depth === 0 && !detail && event.key === Qt.Key_Backspace) { sType("DEL"); event.accepted = true; return }
        if (searching && depth === 0 && !detail && event.text.length === 1 && /[a-zA-Z0-9 ]/.test(event.text)) {
            sType(event.text === " " ? "SPACE" : event.text.toUpperCase()); event.accepted = true; return
        }
        if (m[event.key]) { key(m[event.key]); event.accepted = true }
    }

    // remote keys and "playback started" arrive here
    function poll() {
        get("/events?since=" + seq, function (v) {
            if (!v) { pollRetry.start(); return }
            var first = seq === 0
            seq = v.seq
            if (v.playing) { saveState(); Qt.exit(42); return }
            if (!first) for (var i = 0; i < v.keys.length; i++) key(v.keys[i])
            if (v.search) typedElsewhere(v.search)
            poll()
        })
    }
    Timer { id: pollRetry; interval: 2000; onTriggered: poll() }
    Timer { id: retry; interval: 3000; onTriggered: loadHome() }
    Timer { id: debounce; interval: 600; onTriggered: search() }
    // the daemon accepted the play but nothing started (item or channel failed to open): let the user go on
    Timer { id: startGiveUp; interval: 60000; onTriggered: if (starting) { starting = false; note = "COULD NOT START PLAYBACK" } }
    Component.onCompleted: { if (typeof restoring !== "undefined" && restoring) restoreState(); loadHome(); poll() }

    // ---- pieces ----
    function img(it, w, wide) {
        var id = wide ? it.id : (it.poster || it.id)
        return id ? api + "/image?id=" + id + "&w=" + w : ""
    }
    function label(it) {
        if (it.type === "Episode" && it.episode !== null && it.episode !== undefined)
            return "S" + it.season + "E" + it.episode + " " + it.name
        return it.name || ""
    }
    function sub(it) {
        if (it.type === "Episode") return it.series || ""
        if (it.type === "Series" && it.unplayed) return it.unplayed + " UNWATCHED"
        if (it.type === "TvChannel") return (it.number ? "CH " + it.number + "  " : "") + (it.program || "")
        var s = it.year ? "" + it.year : ""
        if (it.minutes) s += (s ? "  " : "") + it.minutes + " MIN"
        return s
    }

    Component {
        id: card
        Item {
            id: c
            property var it: ({})
            property bool on: false
            property bool wide: false
            property real pw: 150 * root.u
            width: pw + 16 * root.u
            height: (wide ? pw * 9 / 16 : pw * 1.5) + 62 * root.u
            Rectangle {                                    // hard offset shadow
                x: 8 * root.u + (c.on ? 5 * root.u : 0); y: (c.on ? 5 * root.u : 0)
                width: c.pw; height: frame.height; color: T.C.shadow; visible: c.on
            }
            Rectangle {
                id: frame
                x: 8 * root.u; width: c.pw; height: c.wide ? c.pw * 9 / 16 : c.pw * 1.5
                color: T.C.card; border.width: (c.on ? 3 : 1) * root.u; border.color: c.on ? T.C.accent : T.C.edge
                Text {
                    anchors.centerIn: parent; width: parent.width - 12 * root.u; horizontalAlignment: Text.AlignHCenter
                    wrapMode: Text.WordWrap; maximumLineCount: 3; elide: Text.ElideRight
                    text: c.it.type === "Search" ? "⌕\nSEARCH" : (c.it.name || "")
                    visible: pic.status !== Image.Ready
                    color: c.it.type === "Search" ? T.C.accent : T.C.dim
                    font.family: T.FONT; font.pixelSize: (c.it.type === "Search" ? 22 : 13) * root.u; font.bold: true
                }
                Image {
                    id: pic
                    anchors.fill: parent; anchors.margins: frame.border.width
                    source: c.it.type === "Search" ? "" : root.img(c.it, c.wide ? 320 : 240, c.wide)
                    asynchronous: true; fillMode: c.it.type === "TvChannel" ? Image.PreserveAspectFit : Image.PreserveAspectCrop
                    sourceSize.width: c.wide ? 320 : 240
                }
                Rectangle {                                // progress
                    visible: (c.it.pct || 0) > 0 && !c.it.played
                    anchors.bottom: parent.bottom; anchors.left: parent.left; anchors.margins: frame.border.width
                    height: 5 * root.u; width: (parent.width - 2 * frame.border.width) * (c.it.pct || 0) / 100; color: T.C.accent
                }
                Rectangle {                                // watched
                    visible: c.it.played === true
                    anchors.top: parent.top; anchors.right: parent.right; anchors.margins: 5 * root.u
                    width: 20 * root.u; height: 20 * root.u; color: T.C.good
                    Text { anchors.centerIn: parent; text: "✓"; color: T.C.on_accent; font.pixelSize: 14 * root.u; font.bold: true }
                }
            }
            Text {
                x: 8 * root.u; y: frame.height + 6 * root.u; width: c.pw; elide: Text.ElideRight
                text: root.label(c.it).toUpperCase(); color: c.on ? T.C.bright : T.C.fg
                font.family: T.FONT; font.pixelSize: 13 * root.u; font.bold: c.on
            }
            Text {
                x: 8 * root.u; y: frame.height + 26 * root.u; width: c.pw; elide: Text.ElideRight
                text: root.sub(c.it).toUpperCase(); color: T.C.dim; font.family: T.FONT; font.pixelSize: 11 * root.u
            }
        }
    }

    // ---- header ----
    Rectangle {
        id: head
        x: 40 * u; y: 26 * u; height: 40 * u; width: title.width + 28 * u; color: T.C.accent
        Text {
            id: title; anchors.centerIn: parent; color: T.C.on_accent
            text: depth > 0 ? page.title.toUpperCase() : searching ? "SEARCH" : "JELLYFIN"
            font.family: T.FONT; font.pixelSize: 22 * u; font.bold: true
        }
    }
    Rectangle { x: 40 * u; y: 70 * u; width: parent.width - 80 * u; height: 2 * u; color: T.C.edge }
    Text {
        anchors.right: parent.right; anchors.rightMargin: 40 * u; y: 36 * u; color: T.C.dim
        text: Qt.formatTime(clock.now, "h:mm AP"); font.family: T.FONT; font.pixelSize: 18 * u
        Timer { id: clock; property var now: new Date(); interval: 20000; running: true; repeat: true; onTriggered: now = new Date() }
    }

    // ---- home ----
    ListView {
        id: home
        visible: depth === 0 && !searching
        x: 32 * u; y: 86 * u; width: parent.width - 64 * u; height: parent.height - 126 * u
        clip: true; spacing: 6 * u; model: rows.length
        currentIndex: hRow; highlightMoveDuration: 150
        preferredHighlightBegin: 0; preferredHighlightEnd: height; highlightRangeMode: ListView.ApplyRange
        delegate: Item {
            id: rowBox
            property int r: index
            property var items: root.rows[r] ? root.rows[r].items : []
            property bool tall: root.rows[r] && root.rows[r].title !== "LIBRARIES" && root.rows[r].title !== "LIVE TV"
            width: home.width; height: (tall ? 150 * 1.5 : 150 * 9 / 16) * u + 100 * u
            Text {
                x: 8 * u; text: root.rows[r] ? root.rows[r].title : ""; font.family: T.FONT; font.pixelSize: 16 * u; font.bold: true
                color: r === root.hRow ? T.C.accent : T.C.dim
            }
            ListView {
                y: 28 * u; width: parent.width; height: parent.height - 28 * u
                orientation: ListView.Horizontal; clip: true; model: rowBox.items.length
                currentIndex: { root.rev; return root.hCols[rowBox.r] || 0 }
                highlightMoveDuration: 150
                preferredHighlightBegin: 0; preferredHighlightEnd: width; highlightRangeMode: ListView.ApplyRange
                delegate: Loader {
                    sourceComponent: card
                    onLoaded: item.wide = !rowBox.tall
                    // a binding, not a copy at load: a refreshed list of the same length reuses the delegates
                    Binding { target: item; property: "it"; value: rowBox.items[index] || ({}) }
                    Binding { target: item; property: "on"; value: { root.rev; return rowBox.r === root.hRow && index === (root.hCols[rowBox.r] || 0) } }
                }
            }
        }
    }

    // ---- browse ----
    GridView {
        id: grid
        visible: depth > 0
        property bool wide: root.page ? root.page.wide : false
        property real pw: (wide ? 270 : 150) * u
        property int perRow: Math.max(1, Math.floor(width / cellWidth))
        x: 32 * u; y: 86 * u; width: parent.width - 64 * u; height: parent.height - 126 * u
        cellWidth: pw + 16 * u; cellHeight: (wide ? pw * 9 / 16 : pw * 1.5) + 70 * u
        clip: true; model: root.page ? root.page.items.length : 0
        onModelChanged: currentIndex = root.page ? root.page.index : 0
        highlightMoveDuration: 150
        delegate: Loader {
            sourceComponent: card
            onLoaded: { item.pw = grid.pw; item.wide = grid.wide }
            Binding { target: item; property: "it"; value: (root.page && root.page.items[index]) || ({}) }
            Binding { target: item; property: "on"; value: grid.currentIndex === index }
        }
    }
    Text {
        visible: depth > 0 && root.page && root.page.items.length === 0
        anchors.centerIn: parent; text: root.page && root.page.empty ? "NOTHING HERE" : "LOADING"
        color: T.C.dim; font.family: T.FONT; font.pixelSize: 22 * u
    }

    // ---- search ----
    Item {
        visible: searching && depth === 0
        x: 40 * u; y: 86 * u; width: parent.width - 80 * u; height: parent.height - 126 * u
        Rectangle {
            width: parent.width; height: 46 * u; color: T.C.raised; border.width: 2 * u; border.color: T.C.edge_hi
            Text {
                x: 14 * u; anchors.verticalCenter: parent.verticalCenter; font.family: T.FONT; font.pixelSize: 22 * u
                text: term.length ? term + "_" : "TYPE A TITLE_"; color: term.length ? T.C.bright : T.C.dim
            }
        }
        Grid {
            y: 58 * u; columns: 13; spacing: 6 * u
            Repeater {
                model: 40
                Rectangle {
                    property bool on: sZone === 0 && sKey === index
                    width: 86 * u; height: 44 * u; color: on ? T.C.accent : index === 39 ? T.C.raised : T.C.card
                    border.width: 1 * u; border.color: T.C.edge
                    Text {
                        anchors.centerIn: parent; text: root.sLabel(index)
                        font.family: T.FONT; font.pixelSize: (index < 36 ? 20 : 14) * u; font.bold: true
                        color: parent.on ? T.C.on_accent : index === 39 ? T.C.accent : T.C.fg
                    }
                }
            }
        }
        Text {
            y: 264 * u; x: 8 * u; font.family: T.FONT; font.pixelSize: 16 * u; font.bold: true
            color: sZone === 1 ? T.C.accent : T.C.dim
            text: term.length < 2 ? "" : results.length ? "RESULTS" : "NO RESULTS"
        }
        ListView {
            y: 292 * u; width: parent.width; height: 150 * 1.5 * u + 70 * u
            orientation: ListView.Horizontal; clip: true; model: results.length; currentIndex: sRes
            highlightMoveDuration: 150
            preferredHighlightBegin: 0; preferredHighlightEnd: width; highlightRangeMode: ListView.ApplyRange
            delegate: Loader {
                sourceComponent: card
                // a binding: "da" -> "day" both give 32 results, same model count, so the cards were never refreshed
                Binding { target: item; property: "it"; value: root.results[index] || ({}) }
                Binding { target: item; property: "on"; value: sZone === 1 && sRes === index }
            }
        }
    }

    // ---- play dialog ----
    Rectangle { anchors.fill: parent; color: T.C.bg_deep; opacity: 0.85; visible: detail !== null }
    Rectangle {
        visible: detail !== null
        id: dlg
        anchors.centerIn: parent; width: 820 * u; height: (360 + 46 * dOpts.length) * u
        color: T.C.card; border.width: 3 * u; border.color: T.C.accent
        Rectangle { z: -1; x: 8 * u; y: 8 * u; width: parent.width; height: parent.height; color: T.C.shadow }
        Image {
            id: dImg
            x: 24 * u; y: 24 * u; width: 208 * u; height: 312 * u; fillMode: Image.PreserveAspectFit
            source: detail ? root.img(detail, 240, false) : ""; asynchronous: true
        }
        Column {
            x: 256 * u; y: 24 * u; width: 540 * u; spacing: 8 * u
            Text {
                width: parent.width; wrapMode: Text.WordWrap; maximumLineCount: 2; elide: Text.ElideRight
                text: detail ? root.label(detail).toUpperCase() : ""; color: T.C.bright
                font.family: T.FONT; font.pixelSize: 24 * u; font.bold: true
            }
            Text { text: detail ? root.sub(detail).toUpperCase() : ""; color: T.C.accent; font.family: T.FONT; font.pixelSize: 15 * u }
            Text {
                width: parent.width; height: 150 * u; wrapMode: Text.WordWrap; elide: Text.ElideRight; clip: true
                text: detail && detail.overview ? detail.overview : ""; color: T.C.fg; font.family: T.FONT; font.pixelSize: 14 * u
            }
        }
        Column {                                       // choice rows (audio track; subtitles)
            x: 256 * u; y: dlg.height - 70 * u - 46 * dOpts.length * u; spacing: 8 * u
            Repeater {
                model: dOpts.length
                Rectangle {
                    property var o: dOpts[index]
                    property bool on: dRow === index
                    width: 540 * u; height: 38 * u; color: on ? T.C.raised : T.C.card
                    border.width: 2 * u; border.color: on ? T.C.accent : T.C.edge
                    Text {
                        x: 12 * u; anchors.verticalCenter: parent.verticalCenter; text: o ? o.title : ""
                        color: parent.on ? T.C.accent : T.C.dim; font.family: T.FONT; font.pixelSize: 14 * u; font.bold: true
                    }
                    Text {
                        x: 120 * u; width: parent.width - 220 * u; anchors.verticalCenter: parent.verticalCenter; elide: Text.ElideRight
                        text: o ? o.choices[o.sel].name.toUpperCase() : ""
                        color: parent.on ? T.C.bright : T.C.fg; font.family: T.FONT; font.pixelSize: 15 * u
                    }
                    Text {                             // "‹ 2/3 ›": where you are in the list, which ways you can go
                        anchors.right: parent.right; anchors.rightMargin: 12 * u; anchors.verticalCenter: parent.verticalCenter
                        text: o ? (o.sel > 0 ? "‹ " : "  ") + (o.sel + 1) + "/" + o.choices.length + (o.sel < o.choices.length - 1 ? " ›" : "  ") : ""
                        color: parent.on ? T.C.accent : T.C.dim; font.family: T.FONT; font.pixelSize: 15 * u; font.bold: true
                    }
                }
            }
        }
        Row {
            x: 256 * u; y: dlg.height - 70 * u; spacing: 16 * u
            Repeater {
                model: detail && detail.resume ? ["RESUME", "FROM START"] : ["PLAY"]
                Rectangle {
                    property bool on: dBtn === index && dRow >= dOpts.length
                    width: 190 * u; height: 46 * u; color: on ? T.C.accent : T.C.raised
                    border.width: 2 * u; border.color: on ? T.C.accent : T.C.edge_hi
                    Text {
                        anchors.centerIn: parent; text: root.starting && parent.on ? "STARTING" : modelData
                        color: parent.on ? T.C.on_accent : T.C.fg; font.family: T.FONT; font.pixelSize: 18 * u; font.bold: true
                    }
                }
            }
        }
    }

    // ---- footer ----
    Text {
        x: 40 * u; anchors.bottom: parent.bottom; anchors.bottomMargin: 12 * u
        text: note.length ? note : "ARROWS MOVE   OK SELECT   BACK " + (depth > 0 || searching || detail ? "BACK" : "EXIT")
        color: note.length ? T.C.warn : T.C.dim; font.family: T.FONT; font.pixelSize: 13 * u
    }
}
