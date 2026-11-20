#!/usr/bin/env python3
"""Screenshot the on-TV app on the build machine, without the TV (docs/ARCHITECTURE.md, "The on-TV app").

  ssh -N -L 8097:127.0.0.1:8097 steamlink &        # the app only talks to sljfd's local interface
  app/shot.py out.png [key|wait:MS|call:FN|search:TEXT ...]
      e.g.  app/shot.py /tmp/d.png search:"day after" wait:2000 down down down select wait:1500

Loads out/app/jellyfin/main.qml (build-app.sh) at 1280x720 offscreen, sends the keys through main.qml's key(),
waits 2.5 s after the last one and saves the picture. `call:FN` runs a function of the root item (e.g. call:loadHome),
`search:TEXT` opens Search with that text typed.
"""
import os
import sys

os.environ.setdefault("QT_QPA_PLATFORM", "offscreen")
from PyQt5.QtCore import QMetaObject, Q_ARG, QTimer, QUrl, QVariant  # noqa: E402
from PyQt5.QtGui import QGuiApplication  # noqa: E402
from PyQt5.QtQuick import QQuickView  # noqa: E402

HERE = os.path.dirname(os.path.abspath(__file__))
QML = os.path.join(HERE, "..", "out", "app", "jellyfin", "main.qml")


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    out, steps = sys.argv[1], sys.argv[2:]
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
