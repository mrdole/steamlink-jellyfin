CONFIG += marvell release
QT += core gui qml quick
TARGET = sljf-ui
QMAKE_CXXFLAGS += -Wno-unused-parameter
QMAKE_LFLAGS += '-Wl,-rpath,\'/home/steam/lib\''
SOURCES += main.cpp
LIBS += -lSDL2
marvell {
	INCLUDEPATH += $(MARVELL_ROOTFS)/usr/include/SDL2
}
