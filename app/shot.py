#!/usr/bin/env python3
"""Screenshot the on-TV app on the build machine, without the TV (docs/ARCHITECTURE.md, "The on-TV app").

  ssh -N -L 8097:127.0.0.1:8097 steamlink &        # the app only talks to sljfd's local interface
  app/shot.py out.png [key|wait:MS|call:FN|search:TEXT ...]
      e.g.  app/shot.py /tmp/d.png search:"day after" wait:2000 down down down select wait:1500

Loads out/app/jellyfin/main.qml (build-app.sh) at 1280x720 offscreen, sends the keys through main.qml's key(),
waits 2.5 s after the last one and saves the picture. `call:FN` runs a function of the root item (e.g. call:loadHome),
`search:TEXT` opens Search with that text typed, `js:CODE` evaluates CODE in main.qml's root (e.g. fake why
playback ended: js:"wake({after: 'guide', channel: '<id>'})"; js:"woke = {screen: 'guide'}; wake({after: ''})").
Careful: `select` on a channel or guide block calls the real /play and takes over the TV.
"""
import os
import sys

os.environ["QT_QPA_PLATFORM"] = "offscreen"   # a desktop session sets wayland: the window would be tiled
from PyQt5.QtCore import QMetaObject, Q_ARG, QTimer, QUrl, QVariant, qInstallMessageHandler  # noqa: E402
from PyQt5.QtGui import QGuiApplication  # noqa: E402
from PyQt5.QtQml import QQmlExpression  # noqa: E402
from PyQt5.QtQuick import QQuickView  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
QML = os.path.join(HERE, "..", "out", "app", "jellyfin", "main.qml")


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out, steps = sys.argv[1], sys.argv[2:]
    qInstallMessageHandler(lambda kind, ctx, msg: print(msg, file=sys.stderr))   # console.log + QML warnings
    app = QGuiApplication(sys.argv[:1])
    v = QQuickView()
    v.setResizeMode(QQuickView.SizeRootObjectToView)
    v.resize(1280, 720)
    v.setSource(QUrl.fromLocalFile(os.path.abspath(QML)))
    if v.status() != QQuickView.Ready:
        sys.exit("\n".join(e.toString() for e in v.errors()))
    v.show()
    root = v.rootObject()
    t = 2500                                         # let the home rows load first
    for s in steps:
        if s.startswith("wait:"):
            t += int(s[5:])
            continue
        if s.startswith("search:"):
            def typed(text=s[7:].upper()):
                root.setProperty("searching", True)
                root.setProperty("term", text)
                QMetaObject.invokeMethod(root, "search", Q_ARG(QVariant, ""), Q_ARG(QVariant, 0))
            QTimer.singleShot(t, typed)
        elif s.startswith("js:"):
            def run(code=s[3:]):
                e = QQmlExpression(v.rootContext(), root, code)
                e.evaluate()
                if e.hasError():
                    print("js:", e.error().toString(), file=sys.stderr)
            QTimer.singleShot(t, run)
        elif s.startswith("call:"):
            QTimer.singleShot(t, lambda f=s[5:]: QMetaObject.invokeMethod(root, f))
        else:
            QTimer.singleShot(t, lambda k=s: QMetaObject.invokeMethod(root, "key", Q_ARG(QVariant, k)))
        t += 300
    QTimer.singleShot(t + 2500, lambda: (v.grabWindow().save(out), app.quit()))
    app.exec_()
    print(out)


if __name__ == "__main__":
    main()
