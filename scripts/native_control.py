# SPDX-License-Identifier: MIT
"""Local control sockets, including Windows Python without a Unix path codec."""
import os
import socket


def control_socket(path):
    stream = socket.socket(getattr(socket, 'AF_UNIX', 1), socket.SOCK_STREAM)
    try:
        stream.settimeout(3)
        if os.name != 'nt' or hasattr(socket, 'AF_UNIX'):
            stream.connect(str(path))
        else:
            # Windows Python can create the Winsock socket but lacks its path codec.
            import ctypes
            import select
            import struct
            encoded = os.fsencode(path)
            assert len(encoded) < 108, 'private socket path exceeds the native limit'
            address = struct.pack('H', 1) + encoded + b'\0'
            storage = ctypes.create_string_buffer(address, len(address))
            winsock = ctypes.WinDLL('Ws2_32.dll')
            winsock.connect.argtypes = [ctypes.c_size_t, ctypes.c_void_p, ctypes.c_int]
            winsock.connect.restype = ctypes.c_int
            winsock.WSAGetLastError.restype = ctypes.c_int
            if winsock.connect(stream.fileno(), storage, len(address)) != 0:
                error = winsock.WSAGetLastError()
                if error not in (10035, 10036, 10037):
                    raise OSError(error, 'native local connection failed')
                _, writable, failed = select.select([], [stream], [stream], 3)
                if not writable and not failed:
                    raise TimeoutError('native local connection timed out')
                error = stream.getsockopt(socket.SOL_SOCKET, socket.SO_ERROR)
                if error:
                    raise OSError(error, 'native local connection failed')
        return stream
    except BaseException:
        stream.close()
        raise
