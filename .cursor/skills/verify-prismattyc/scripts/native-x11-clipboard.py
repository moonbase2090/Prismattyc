#!/usr/bin/env python3
# SPDX-License-Identifier: MIT
"""Read the isolated host clipboard after an X11 SelectionClear notification."""
import ctypes as c
import select
import time


class SelectionEvent(c.Structure):
    _fields_ = [('type', c.c_int), ('serial', c.c_ulong), ('send_event', c.c_int),
                ('display', c.c_void_p), ('requestor', c.c_ulong),
                ('selection', c.c_ulong), ('target', c.c_ulong),
                ('property', c.c_ulong), ('time', c.c_ulong)]


class Event(c.Union):
    _fields_ = [('type', c.c_int), ('selection', SelectionEvent), ('pad', c.c_long * 24)]


class Clipboard:
    def __init__(self, display):
        self.x = c.CDLL('libX11.so.6')
        signatures = {
            'XOpenDisplay': (c.c_void_p, [c.c_char_p]),
            'XDefaultRootWindow': (c.c_ulong, [c.c_void_p]),
            'XCreateSimpleWindow': (c.c_ulong, [c.c_void_p, c.c_ulong, c.c_int, c.c_int,
                                             c.c_uint, c.c_uint, c.c_uint, c.c_ulong, c.c_ulong]),
            'XInternAtom': (c.c_ulong, [c.c_void_p, c.c_char_p, c.c_int]),
            'XSetSelectionOwner': (c.c_int, [c.c_void_p, c.c_ulong, c.c_ulong, c.c_ulong]),
            'XGetSelectionOwner': (c.c_ulong, [c.c_void_p, c.c_ulong]),
            'XSync': (c.c_int, [c.c_void_p, c.c_int]),
            'XFlush': (c.c_int, [c.c_void_p]),
            'XPending': (c.c_int, [c.c_void_p]),
            'XConnectionNumber': (c.c_int, [c.c_void_p]),
            'XNextEvent': (c.c_int, [c.c_void_p, c.POINTER(Event)]),
            'XConvertSelection': (c.c_int, [c.c_void_p, c.c_ulong, c.c_ulong,
                                          c.c_ulong, c.c_ulong, c.c_ulong]),
            'XGetWindowProperty': (c.c_int, [c.c_void_p, c.c_ulong, c.c_ulong,
                                           c.c_long, c.c_long, c.c_int, c.c_ulong,
                                           c.POINTER(c.c_ulong), c.POINTER(c.c_int),
                                           c.POINTER(c.c_ulong), c.POINTER(c.c_ulong),
                                           c.POINTER(c.c_void_p)]),
            'XFree': (c.c_int, [c.c_void_p]),
            'XDestroyWindow': (c.c_int, [c.c_void_p, c.c_ulong]),
            'XCloseDisplay': (c.c_int, [c.c_void_p]),
        }
        for name, (result, arguments) in signatures.items():
            function = getattr(self.x, name)
            function.restype, function.argtypes = result, arguments
        self.display = self.x.XOpenDisplay(display.encode())
        assert self.display, 'cannot open the owned X11 display'
        self.window = self.x.XCreateSimpleWindow(self.display, self.x.XDefaultRootWindow(self.display),
                                                0, 0, 1, 1, 0, 0, 0)
        self.clipboard = self.x.XInternAtom(self.display, b'CLIPBOARD', 0)
        self.utf8 = self.x.XInternAtom(self.display, b'UTF8_STRING', 0)
        self.property = self.x.XInternAtom(self.display, b'NATIVE_PROOF_CLIPBOARD', 0)
        self.x.XSetSelectionOwner(self.display, self.clipboard, self.window, 0)
        self.x.XSync(self.display, 0)
        assert self.x.XGetSelectionOwner(self.display, self.clipboard) == self.window

    def wait(self, kind):
        deadline = time.monotonic() + 10
        while True:
            while self.x.XPending(self.display):
                event = Event()
                self.x.XNextEvent(self.display, c.byref(event))
                if event.type == kind:
                    return event
            remaining = deadline - time.monotonic()
            assert remaining > 0 and select.select([self.x.XConnectionNumber(self.display)], [], [], remaining)[0], \
                f'X11 clipboard event {kind} did not arrive'

    def copied_text(self):
        self.wait(29)  # SelectionClear: the host took ownership after Copy.
        owner = self.x.XGetSelectionOwner(self.display, self.clipboard)
        assert owner and owner != self.window
        self.x.XConvertSelection(self.display, self.clipboard, self.utf8, self.property, self.window, 0)
        self.x.XFlush(self.display)
        event = self.wait(31).selection  # SelectionNotify from the actual selection owner.
        assert (event.requestor, event.selection, event.target, event.property) == \
            (self.window, self.clipboard, self.utf8, self.property)
        actual_type, actual_format = c.c_ulong(), c.c_int()
        items, remaining, data = c.c_ulong(), c.c_ulong(), c.c_void_p()
        code = self.x.XGetWindowProperty(self.display, self.window, self.property, 0, 16384, 1, self.utf8,
                                         c.byref(actual_type), c.byref(actual_format), c.byref(items),
                                         c.byref(remaining), c.byref(data))
        try:
            assert code == 0 and actual_type.value == self.utf8 and actual_format.value == 8
            assert remaining.value == 0, 'clipboard proof must read the entire selection'
            return c.string_at(data, items.value).decode('utf-8')
        finally:
            if data:
                self.x.XFree(data)

    def owns_selection(self):
        return self.x.XGetSelectionOwner(self.display, self.clipboard) == self.window

    def close(self):
        self.x.XDestroyWindow(self.display, self.window)
        self.x.XCloseDisplay(self.display)
