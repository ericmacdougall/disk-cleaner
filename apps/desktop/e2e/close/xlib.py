"""The few Xlib calls the window-close check needs, through ctypes (libX11 ships with GTK, so
nothing is installed for it). Linux and X11 only.

Everything here acts as another X client would: the check finds the app's real top-level
window, then asks the window manager to close it, destroys it, or closes the app's X
connection, exactly as a window manager, a misbehaving tool or a session ending would.
"""

from __future__ import annotations

import ctypes
import ctypes.util

_x = ctypes.CDLL(ctypes.util.find_library("X11") or "libX11.so.6")

Window = ctypes.c_ulong
Atom = ctypes.c_ulong

IS_VIEWABLE = 2
SUBSTRUCTURE_REDIRECT_MASK = 1 << 20
SUBSTRUCTURE_NOTIFY_MASK = 1 << 19
CLIENT_MESSAGE = 33
XA_CARDINAL = 6
XA_WINDOW = 33


class XWindowAttributes(ctypes.Structure):
    _fields_ = [
        ("x", ctypes.c_int),
        ("y", ctypes.c_int),
        ("width", ctypes.c_int),
        ("height", ctypes.c_int),
        ("border_width", ctypes.c_int),
        ("depth", ctypes.c_int),
        ("visual", ctypes.c_void_p),
        ("root", Window),
        ("class_", ctypes.c_int),
        ("bit_gravity", ctypes.c_int),
        ("win_gravity", ctypes.c_int),
        ("backing_store", ctypes.c_int),
        ("backing_planes", ctypes.c_ulong),
        ("backing_pixel", ctypes.c_ulong),
        ("save_under", ctypes.c_int),
        ("colormap", ctypes.c_ulong),
        ("map_installed", ctypes.c_int),
        ("map_state", ctypes.c_int),
        ("all_event_masks", ctypes.c_long),
        ("your_event_mask", ctypes.c_long),
        ("do_not_propagate_mask", ctypes.c_long),
        ("override_redirect", ctypes.c_int),
        ("screen", ctypes.c_void_p),
    ]


class XClientMessageEvent(ctypes.Structure):
    _fields_ = [
        ("type", ctypes.c_int),
        ("serial", ctypes.c_ulong),
        ("send_event", ctypes.c_int),
        ("display", ctypes.c_void_p),
        ("window", Window),
        ("message_type", Atom),
        ("format", ctypes.c_int),
        ("l", ctypes.c_long * 5),
    ]


class XEvent(ctypes.Union):
    _fields_ = [("xclient", XClientMessageEvent), ("pad", ctypes.c_long * 24)]


_x.XOpenDisplay.restype = ctypes.c_void_p
_x.XOpenDisplay.argtypes = [ctypes.c_char_p]
_x.XCloseDisplay.argtypes = [ctypes.c_void_p]
_x.XDefaultRootWindow.restype = Window
_x.XDefaultRootWindow.argtypes = [ctypes.c_void_p]
_x.XInternAtom.restype = Atom
_x.XInternAtom.argtypes = [ctypes.c_void_p, ctypes.c_char_p, ctypes.c_int]
_x.XGetWindowProperty.argtypes = [
    ctypes.c_void_p, Window, Atom, ctypes.c_long, ctypes.c_long, ctypes.c_int, Atom,
    ctypes.POINTER(Atom), ctypes.POINTER(ctypes.c_int), ctypes.POINTER(ctypes.c_ulong),
    ctypes.POINTER(ctypes.c_ulong), ctypes.POINTER(ctypes.c_void_p),
]
_x.XQueryTree.argtypes = [
    ctypes.c_void_p, Window, ctypes.POINTER(Window), ctypes.POINTER(Window),
    ctypes.POINTER(ctypes.POINTER(Window)), ctypes.POINTER(ctypes.c_uint),
]
_x.XGetWindowAttributes.argtypes = [ctypes.c_void_p, Window, ctypes.POINTER(XWindowAttributes)]
_x.XDestroyWindow.argtypes = [ctypes.c_void_p, Window]
_x.XKillClient.argtypes = [ctypes.c_void_p, ctypes.c_ulong]
_x.XSendEvent.argtypes = [ctypes.c_void_p, Window, ctypes.c_int, ctypes.c_long, ctypes.POINTER(XEvent)]
_x.XSync.argtypes = [ctypes.c_void_p, ctypes.c_int]
_x.XFree.argtypes = [ctypes.c_void_p]

# X errors (BadWindow once a window is gone) are counted, not fatal.
_ERRORS: list[int] = []
_HANDLER = ctypes.CFUNCTYPE(ctypes.c_int, ctypes.c_void_p, ctypes.c_void_p)(
    lambda _d, _e: (_ERRORS.append(1), 0)[1]
)
_x.XSetErrorHandler(_HANDLER)


class Display:
    """One connection to the X server `name` (":99")."""

    def __init__(self, name: str):
        self.d = _x.XOpenDisplay(name.encode())
        if not self.d:
            raise OSError(f"cannot open display {name}")
        self.root = _x.XDefaultRootWindow(self.d)

    def close(self) -> None:
        if self.d:
            _x.XCloseDisplay(self.d)
            self.d = None

    def atom(self, name: str) -> int:
        return _x.XInternAtom(self.d, name.encode(), 0)

    def _prop(self, w: int, name: str, kind: int) -> list[int]:
        actual, fmt = Atom(), ctypes.c_int()
        n, after, data = ctypes.c_ulong(), ctypes.c_ulong(), ctypes.c_void_p()
        _ERRORS.clear()
        ok = _x.XGetWindowProperty(self.d, w, self.atom(name), 0, 4096, 0, kind,
                                   ctypes.byref(actual), ctypes.byref(fmt), ctypes.byref(n),
                                   ctypes.byref(after), ctypes.byref(data))
        if ok != 0 or _ERRORS or not data.value:
            return []
        try:
            if fmt.value != 32:
                return []
            # Format 32 properties come back as C longs.
            arr = ctypes.cast(data, ctypes.POINTER(ctypes.c_ulong))
            return [arr[i] for i in range(n.value)]
        finally:
            _x.XFree(data)

    def children(self, w: int) -> list[int]:
        root, parent = Window(), Window()
        kids, n = ctypes.POINTER(Window)(), ctypes.c_uint()
        _ERRORS.clear()
        if not _x.XQueryTree(self.d, w, ctypes.byref(root), ctypes.byref(parent),
                             ctypes.byref(kids), ctypes.byref(n)) or _ERRORS:
            return []
        out = [kids[i] for i in range(n.value)]
        if kids:
            _x.XFree(kids)
        return out

    def attributes(self, w: int) -> XWindowAttributes | None:
        a = XWindowAttributes()
        _ERRORS.clear()
        _x.XSync(self.d, 0)
        ok = _x.XGetWindowAttributes(self.d, w, ctypes.byref(a))
        _x.XSync(self.d, 0)
        return a if ok and not _ERRORS else None

    def exists(self, w: int) -> bool:
        return self.attributes(w) is not None

    def wm_running(self) -> bool:
        """A window manager announced itself (EWMH _NET_SUPPORTING_WM_CHECK)."""
        return bool(self._prop(self.root, "_NET_SUPPORTING_WM_CHECK", XA_WINDOW))

    def toplevel(self, pid: int) -> int | None:
        """The app's real top-level window: a viewable, managed (or, without a window manager,
        root-child) window whose _NET_WM_PID is `pid` and which takes WM_DELETE_WINDOW. GTK's
        hidden client-leader window carries the pid too but is never mapped, so it is not
        chosen (destroying it is not a close). The largest candidate wins."""
        if self.wm_running():
            candidates = self._prop(self.root, "_NET_CLIENT_LIST", XA_WINDOW)
        else:
            candidates = self.children(self.root)
        delete = self.atom("WM_DELETE_WINDOW")
        best, area = None, 0
        for w in candidates:
            if self._prop(w, "_NET_WM_PID", XA_CARDINAL) != [pid]:
                continue
            a = self.attributes(w)
            if a is None or a.map_state != IS_VIEWABLE or a.override_redirect:
                continue
            if delete not in self._prop(w, "WM_PROTOCOLS", 4):  # XA_ATOM
                continue
            if a.width * a.height > area:
                best, area = w, a.width * a.height
        return best

    def request_close(self, w: int) -> None:
        """Ask the window manager to close `w` (_NET_CLOSE_WINDOW, as a pager or wmctrl -c
        does); the window manager then sends the app WM_DELETE_WINDOW."""
        ev = XEvent()
        ev.xclient.type = CLIENT_MESSAGE
        ev.xclient.send_event = 1
        ev.xclient.window = w
        ev.xclient.message_type = self.atom("_NET_CLOSE_WINDOW")
        ev.xclient.format = 32
        ev.xclient.l[0] = 0  # CurrentTime
        ev.xclient.l[1] = 2  # source: a pager or other direct user action
        _x.XSendEvent(self.d, self.root, 0,
                      SUBSTRUCTURE_REDIRECT_MASK | SUBSTRUCTURE_NOTIFY_MASK, ctypes.byref(ev))
        _x.XSync(self.d, 0)

    def destroy(self, w: int) -> None:
        """Destroy `w` from outside, with no close request (as xdotool windowclose does)."""
        _x.XDestroyWindow(self.d, w)
        _x.XSync(self.d, 0)

    def kill_client(self, w: int) -> None:
        """Close the X connection of the client that owns `w` (as xkill does, and as the
        server going away at logout does to every client)."""
        _x.XKillClient(self.d, w)
        _x.XSync(self.d, 0)
