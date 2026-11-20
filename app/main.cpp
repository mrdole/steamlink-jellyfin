// sljf-ui: the on-TV Jellyfin app for the Steam Link (docs/ARCHITECTURE.md, "The on-TV app").
// A thin Qt Quick host: everything on screen is main.qml next to this binary, which talks only to sljfd's local
// interface (http://127.0.0.1:8097). Game controllers (USB or Bluetooth, through SDL) are turned into the same key
// presses a keyboard sends. Exit code 42 = "playback is starting, give the screen to the player" (run.sh).
// Where the user was (screen, row, focused item) is kept in STATE_FILE across that exit: run.sh starts the app again
// with --restore after playback, and main.qml reads it back through `store`.
#include <QCursor>
#include <QFile>
#include <QDir>
#include <QFontDatabase>
#include <QGuiApplication>
#include <QKeyEvent>
#include <QQmlContext>
#include <QQmlEngine>
#include <QQuickView>
#include <QTimer>
#include <SDL2/SDL.h>

#define STATE_FILE "/tmp/sljf-ui.state"

// A small text store for main.qml: the UI state (JSON) survives the app's exit during playback (tmpfs: gone at reboot).
class Store : public QObject
{
	Q_OBJECT
public:
	Q_INVOKABLE QString load() const
	{
		QFile f(STATE_FILE);
		return f.open(QIODevice::ReadOnly) ? QString::fromUtf8(f.readAll()) : QString();
	}
	Q_INVOKABLE void save(const QString &text) const
	{
		QFile f(STATE_FILE);
		if (f.open(QIODevice::WriteOnly | QIODevice::Truncate)) f.write(text.toUtf8());
	}
};

static void sendKey(QQuickView *view, int key)
{
	QCoreApplication::postEvent(view, new QKeyEvent(QEvent::KeyPress, key, Qt::NoModifier));
	QCoreApplication::postEvent(view, new QKeyEvent(QEvent::KeyRelease, key, Qt::NoModifier));
}

static int keyForButton(int button)
{
	switch (button) {
	case SDL_CONTROLLER_BUTTON_DPAD_UP: return Qt::Key_Up;
	case SDL_CONTROLLER_BUTTON_DPAD_DOWN: return Qt::Key_Down;
	case SDL_CONTROLLER_BUTTON_DPAD_LEFT: return Qt::Key_Left;
	case SDL_CONTROLLER_BUTTON_DPAD_RIGHT: return Qt::Key_Right;
	case SDL_CONTROLLER_BUTTON_A: return Qt::Key_Return;
	case SDL_CONTROLLER_BUTTON_B: return Qt::Key_Escape;
	case SDL_CONTROLLER_BUTTON_BACK: return Qt::Key_Escape;
	case SDL_CONTROLLER_BUTTON_START: return Qt::Key_Home;
	default: return 0;
	}
}

int main(int argc, char *argv[])
{
	SDL_Init(SDL_INIT_VIDEO | SDL_INIT_GAMECONTROLLER);   // video: sets up the Link's display mode and scale
	QGuiApplication app(argc, argv);
	const QString dir = QCoreApplication::applicationDirPath();
	for (const QString &f : QDir(dir + "/fonts").entryList(QStringList() << "*.ttf", QDir::Files))
		QFontDatabase::addApplicationFont(dir + "/fonts/" + f);
	QCursor().setPos(0xFFFF, 0xFFFF);                      // park the mouse pointer off screen

	QQuickView view;
	view.setResizeMode(QQuickView::SizeRootObjectToView);
	view.setColor(Qt::black);
	Store store;
	view.rootContext()->setContextProperty("appDir", dir);
	view.rootContext()->setContextProperty("store", &store);
	view.rootContext()->setContextProperty("restoring", QCoreApplication::arguments().contains("--restore"));
	QObject::connect(view.engine(), &QQmlEngine::exit, &app, &QCoreApplication::exit);
	QObject::connect(view.engine(), &QQmlEngine::quit, &app, &QCoreApplication::quit);
	view.setSource(QUrl::fromLocalFile(dir + "/main.qml"));
	view.showFullScreen();

	// controllers -> keys (d-pad and left stick move, A selects, B/Back goes back, Start = home)
	QTimer pump;
	int stickX = 0, stickY = 0;
	QObject::connect(&pump, &QTimer::timeout, [&]() {
		SDL_Event e;
		while (SDL_PollEvent(&e)) {
			if (e.type == SDL_CONTROLLERDEVICEADDED) {
				SDL_GameControllerOpen(e.cdevice.which);
			} else if (e.type == SDL_CONTROLLERBUTTONDOWN) {
				if (int k = keyForButton(e.cbutton.button)) sendKey(&view, k);
			} else if (e.type == SDL_CONTROLLERAXISMOTION && e.caxis.axis <= SDL_CONTROLLER_AXIS_LEFTY) {
				int &state = e.caxis.axis == SDL_CONTROLLER_AXIS_LEFTX ? stickX : stickY;
				int now = e.caxis.value > 20000 ? 1 : e.caxis.value < -20000 ? -1 : 0;
				if (now != state) {
					state = now;
					if (now && e.caxis.axis == SDL_CONTROLLER_AXIS_LEFTX) sendKey(&view, now > 0 ? Qt::Key_Right : Qt::Key_Left);
					if (now && e.caxis.axis == SDL_CONTROLLER_AXIS_LEFTY) sendKey(&view, now > 0 ? Qt::Key_Down : Qt::Key_Up);
				}
			}
		}
	});
	pump.start(16);
	return app.exec();
}

#include "main.moc"
